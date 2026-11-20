//! The control plane's routes for GitHub Actions: what GitHub's own
//! orchestrator and runners do, and offer no API for. A workflow is
//! registered here, standing for a workflow file a world does not read. A
//! run of one is started here for the events GitHub starts one on by
//! itself. And a runner's work on a job is reported here: taking it,
//! writing to its log, finishing a step, and finishing the job. Which jobs
//! a run lists, and
//! when, follows from those (`spec/surface-github.md`, "Actions"). The
//! reads of workflows, runs, and jobs are the GitHub face's.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post};
use orrery_face_git::{signature, store};
use orrery_world::{
    ActorKind, CheckConclusion, CheckRun, CheckStatus, JobSpec, NewWorkflow, NewWorkflowRun,
    PullState, Repo, StepConclusion, StepSpec, StepStatus, Workflow, WorkflowRun, WorldError,
    check_jobs,
};

use crate::control::{Fault, Fields, SYSTEM, errors, faults, internal};
use crate::{AppState, json};

/// The login of the bot GitHub's runners write check runs as.
const RUNNER: &str = "github-actions";
/// The step a runner lists first in every job, and the one it lists as the
/// job finishes.
const SET_UP: &str = "Set up job";
const COMPLETE: &str = "Complete job";
/// The events a run is started for here. GitHub starts a run for
/// `workflow_dispatch` when its API is asked to, which is the face's.
const EVENTS: [&str; 3] = ["push", "pull_request", "schedule"];

pub(crate) fn router() -> Router<AppState> {
    const JOB: &str = "/repos/{owner}/{repo}/workflow-jobs/{job_id}";
    Router::new()
        .route("/repos/{owner}/{repo}/workflows", post(create_workflow))
        .route("/repos/{owner}/{repo}/workflows/{id}", get(get_workflow))
        .route("/repos/{owner}/{repo}/workflows/{id}/runs", post(start_run))
        .route(&format!("{JOB}/start"), post(start_job))
        .route(&format!("{JOB}/log"), post(write_log))
        .route(&format!("{JOB}/complete-step"), post(complete_step))
        .route(&format!("{JOB}/complete"), post(complete_job))
}

/// A job of a workflow as a body gives it and as a read answers it.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct JobBody {
    name: String,
    /// The jobs that finish before this one is listed.
    #[serde(default)]
    needs: Vec<String>,
    /// Whether it runs when a job it needs did not succeed, as a job with
    /// `if: always()` does.
    #[serde(default)]
    always: bool,
    #[serde(default = "hosted")]
    runs_on: Vec<String>,
    /// Its steps, without the two a runner adds.
    #[serde(default)]
    steps: Vec<StepBody>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct StepBody {
    name: String,
    /// Whether it runs after a step before it failed.
    #[serde(default)]
    always: bool,
}

fn hosted() -> Vec<String> {
    vec!["ubuntu-latest".to_string()]
}

#[derive(serde::Serialize)]
struct WorkflowBody {
    id: i64,
    path: String,
    name: String,
    /// The inputs a dispatch of the workflow may give: null for a workflow
    /// with no `workflow_dispatch` trigger.
    dispatch_inputs: Option<Vec<String>>,
    jobs: Vec<JobBody>,
}

fn workflow_body(workflow: Workflow) -> WorkflowBody {
    let jobs = workflow
        .jobs
        .into_iter()
        .map(|job| JobBody {
            name: job.name,
            needs: job.needs,
            always: job.always,
            runs_on: job.labels,
            // The runner's own first step is not the workflow's to list.
            steps: job
                .steps
                .into_iter()
                .enumerate()
                .filter(|(i, step)| *i > 0 || step.name != SET_UP)
                .map(|(_, step)| StepBody {
                    name: step.name,
                    always: step.always,
                })
                .collect(),
        })
        .collect();
    WorkflowBody {
        id: workflow.id,
        path: workflow.path,
        name: workflow.name,
        dispatch_inputs: workflow.dispatch_inputs,
        jobs,
    }
}

/// A run with the jobs its current attempt lists, which every route that
/// moves a run answers.
#[derive(serde::Serialize)]
struct RunBody {
    id: i64,
    workflow_id: i64,
    run_number: i64,
    attempt: i64,
    event: String,
    head_sha: String,
    head_branch: Option<String>,
    title: String,
    actor: String,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    check_suite_id: i64,
    jobs: Vec<JobState>,
}

#[derive(serde::Serialize)]
struct JobState {
    /// The job's id, which is its check run's.
    id: i64,
    name: String,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    steps: Vec<StepState>,
}

#[derive(serde::Serialize)]
struct StepState {
    number: usize,
    name: String,
    status: StepStatus,
    conclusion: Option<StepConclusion>,
}

fn run_body(state: &AppState, run: WorkflowRun) -> Result<RunBody, WorldError> {
    let jobs = state
        .world
        .list_workflow_jobs_of_attempt(run.id, run.attempt)?
        .into_iter()
        .map(|job| JobState {
            id: job.id,
            name: job.name,
            status: job.status,
            conclusion: job.conclusion,
            steps: job
                .job
                .into_iter()
                .flat_map(|j| j.steps)
                .enumerate()
                .map(|(i, step)| StepState {
                    number: i + 1,
                    name: step.name,
                    status: step.status,
                    conclusion: step.conclusion,
                })
                .collect(),
        })
        .collect();
    Ok(RunBody {
        id: run.id,
        workflow_id: run.workflow_id,
        run_number: run.run_number,
        attempt: run.attempt,
        event: run.event,
        head_sha: run.head_sha,
        head_branch: run.head_branch,
        title: run.title,
        actor: run.actor_login,
        status: run.status,
        conclusion: run.conclusion,
        check_suite_id: run.suite_id,
        jobs,
    })
}

fn not_found(message: &str) -> Response<Body> {
    errors(StatusCode::NOT_FOUND, "NOT_FOUND", message, None)
}

fn invalid(message: String, field: &str) -> Response<Body> {
    errors(
        StatusCode::BAD_REQUEST,
        "INVALID_ARGUMENT",
        &message,
        Some(field),
    )
}

fn repo_at(state: &AppState, owner: &str, name: &str) -> Result<Repo, Box<Response<Body>>> {
    match state.world.repo_by_owner_name(owner, name) {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err(Box::new(not_found(&format!(
            "no repository {owner}/{name}"
        )))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// The repository's workflow with the path's id.
fn workflow_at(state: &AppState, repo: &Repo, raw: &str) -> Result<Workflow, Box<Response<Body>>> {
    let found = match raw.parse::<i64>() {
        Ok(id) => state.world.workflow_by_id(id),
        Err(_) => Ok(None),
    };
    match found {
        Ok(Some(workflow)) if workflow.repo_id == repo.id => Ok(workflow),
        Ok(_) => Err(Box::new(not_found(&format!(
            "no workflow with id {raw:?} in {}",
            repo.full_name()
        )))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// What a write to a run answers: the run as it now is, or why not.
fn answered(
    state: &AppState,
    status: StatusCode,
    run: Result<WorkflowRun, WorldError>,
) -> Response<Body> {
    match run.and_then(|run| run_body(state, run)) {
        Ok(body) => json(status, serde_json::json!(body)),
        Err(e @ WorldError::JobNotQueued(_)) | Err(e @ WorldError::JobNotInProgress(_)) => errors(
            StatusCode::BAD_REQUEST,
            "FAILED_PRECONDITION",
            &e.to_string(),
            None,
        ),
        Err(WorldError::RunnerNotABot(login)) => errors(
            StatusCode::BAD_REQUEST,
            "FAILED_PRECONDITION",
            &format!(
                "the world has an actor {login:?} that is not a bot, and a run's jobs are that \
                 bot's check runs"
            ),
            None,
        ),
        Err(e) => internal(e),
    }
}

/// Register a workflow in a repository: what GitHub does when it reads a
/// workflow file. Each job begins with the runner's "Set up job" and ends
/// with its "Complete job", which a body does not list. `dispatch_inputs`
/// is for a workflow with a `workflow_dispatch` trigger: the inputs it
/// takes, which may be none.
async fn create_workflow(
    State(state): State<AppState>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let path: Option<String> = fields.required("path");
    let named: Option<String> = fields.required("name");
    let jobs: Option<Vec<JobBody>> = fields.required("jobs");
    let dispatch_inputs: Option<Vec<String>> = fields.optional("dispatch_inputs");
    if let Some(inputs) = &dispatch_inputs {
        for (i, input) in inputs.iter().enumerate() {
            if input.trim().is_empty() || inputs[..i].contains(input) {
                fields.fault(Fault::invalid(
                    "an input's name is not blank, and is given once".to_string(),
                    format!("dispatch_inputs[{i}]"),
                ));
            }
        }
    }
    if let Some(path) = &path {
        let file = path.strip_prefix(".github/workflows/").unwrap_or("");
        let stem = file
            .strip_suffix(".yml")
            .or_else(|| file.strip_suffix(".yaml"));
        if !stem.is_some_and(|stem| !stem.is_empty() && !stem.contains('/')) {
            fields.fault(Fault::invalid(
                "path is a .yml or .yaml file directly in .github/workflows/".to_string(),
                "path",
            ));
        }
    }
    if named.as_deref().is_some_and(|n| n.trim().is_empty()) {
        fields.fault(Fault::invalid("name cannot be blank".to_string(), "name"));
    }
    let jobs: Option<Vec<JobSpec>> = jobs.map(|jobs| {
        for (i, job) in jobs.iter().enumerate() {
            if job.name.trim().is_empty() {
                fields.fault(Fault::invalid(
                    "a job's name cannot be blank".to_string(),
                    format!("jobs[{i}].name"),
                ));
            }
            for (n, step) in job.steps.iter().enumerate() {
                if step.name.trim().is_empty() {
                    fields.fault(Fault::invalid(
                        "a step's name cannot be blank".to_string(),
                        format!("jobs[{i}].steps[{n}].name"),
                    ));
                }
            }
        }
        jobs.into_iter()
            .map(|job| JobSpec {
                name: job.name,
                needs: job.needs,
                always: job.always,
                labels: job.runs_on,
                steps: std::iter::once(StepSpec {
                    name: SET_UP.to_string(),
                    always: false,
                })
                .chain(job.steps.into_iter().map(|step| StepSpec {
                    name: step.name,
                    always: step.always,
                }))
                .collect(),
                closing_steps: vec![COMPLETE.to_string()],
            })
            .collect()
    });
    if let Some(jobs) = &jobs {
        for (field, message) in check_jobs(jobs) {
            fields.fault(Fault::invalid(message, field));
        }
    }
    let (path, (named, jobs)) = match fields.finish(path.zip(named.zip(jobs))) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    let repo = match repo_at(&state, &owner, &name) {
        Ok(repo) => repo,
        Err(resp) => return *resp,
    };
    let new = NewWorkflow {
        repo_id: repo.id,
        path: &path,
        name: &named,
        jobs: &jobs,
        dispatch_inputs: dispatch_inputs.as_deref(),
    };
    match state.world.create_workflow(new, SYSTEM) {
        Ok(workflow) => json(
            StatusCode::CREATED,
            serde_json::json!(workflow_body(workflow)),
        ),
        Err(WorldError::WorkflowExists(path)) => errors(
            StatusCode::CONFLICT,
            "ALREADY_EXISTS",
            &format!("{} already has a workflow at {path:?}", repo.full_name()),
            Some("path"),
        ),
        Err(WorldError::InvalidWorkflow { field, message }) => invalid(message, &field),
        Err(e) => internal(e),
    }
}

/// A workflow with its jobs, which no read of the face answers.
async fn get_workflow(
    State(state): State<AppState>,
    UrlPath((owner, name, id)): UrlPath<(String, String, String)>,
) -> Response<Body> {
    let found = repo_at(&state, &owner, &name).and_then(|repo| workflow_at(&state, &repo, &id));
    match found {
        Ok(workflow) => json(StatusCode::OK, serde_json::json!(workflow_body(workflow))),
        Err(resp) => *resp,
    }
}

/// Start a run of a workflow on a commit, as GitHub does on a push, on a
/// change to a pull request, or on a schedule. The jobs that need nothing
/// are queued. The run's title is GitHub's: the commit's subject for a
/// push, the pull request's title for a pull request, and the workflow's
/// name for a schedule.
async fn start_run(
    State(state): State<AppState>,
    UrlPath((owner, name, id)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let event: Option<String> = fields.required("event");
    let head_sha: Option<String> = fields.required("head_sha");
    let head_branch: Option<String> = fields.required("head_branch");
    let actor: Option<String> = fields.required("actor");
    if event.as_deref().is_some_and(|e| !EVENTS.contains(&e)) {
        fields.fault(Fault::invalid(
            format!(
                "event is one of {}; a workflow_dispatch run is started by the face's \
                 POST /repos/{{owner}}/{{repo}}/actions/workflows/{{id}}/dispatches",
                EVENTS.join(", ")
            ),
            "event",
        ));
    }
    if head_sha.as_deref().is_some_and(|sha| !store::is_oid(sha)) {
        fields.fault(Fault::invalid(
            "head_sha is a commit's full id".to_string(),
            "head_sha",
        ));
    }
    if head_branch.as_deref().is_some_and(|b| b.trim().is_empty()) {
        fields.fault(Fault::invalid(
            "head_branch cannot be blank".to_string(),
            "head_branch",
        ));
    }
    let read = event.zip(head_sha).zip(head_branch).zip(actor);
    let (((event, head_sha), head_branch), actor) = match fields.finish(read) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    // The commit is read from the bare repository, which a restore replaces
    // under the gate.
    let _gate = state.git.gate().read_owned().await;
    let found = repo_at(&state, &owner, &name)
        .and_then(|repo| workflow_at(&state, &repo, &id).map(|workflow| (repo, workflow)));
    let (repo, workflow) = match found {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    // Each of the body's names is looked up, and every one that names
    // nothing is reported.
    let mut wrong = Vec::new();
    let actor_id = match state.world.actor_by_login(&actor) {
        Ok(Some(found)) if found.kind == ActorKind::User => Some(found.id),
        Ok(_) => {
            wrong.push(Fault::invalid(format!("no user {actor:?}"), "actor"));
            None
        }
        Err(e) => return internal(e),
    };
    let bare = store::bare_dir(&state.world.repos_dir(), &repo.owner_login, &repo.name);
    let commit = match signature::read_commit(&bare, &head_sha) {
        Ok(Some(commit)) => Some(commit),
        Ok(None) => {
            wrong.push(Fault::invalid(
                format!("{} has no commit {head_sha}", repo.full_name()),
                "head_sha",
            ));
            None
        }
        Err(e) => return internal(e),
    };
    match store::branch_tip(&bare, &head_branch) {
        Ok(Some(_)) => {}
        Ok(None) => wrong.push(Fault::invalid(
            format!("{} has no branch {head_branch:?}", repo.full_name()),
            "head_branch",
        )),
        Err(e) => return internal(e),
    }
    let mut title = workflow.name.clone();
    if event == "pull_request" && wrong.is_empty() {
        let pulls = match state
            .world
            .list_pulls_for_repo(repo.id, Some(PullState::Open))
        {
            Ok(pulls) => pulls,
            Err(e) => return internal(e),
        };
        let of = pulls
            .into_iter()
            .find(|pull| pull.head_sha == head_sha && pull.head_ref == head_branch);
        match of {
            Some(pull) => title = pull.title,
            None => wrong.push(Fault::invalid(
                format!("no open pull request has {head_branch:?} as its head at {head_sha}"),
                "head_sha",
            )),
        }
    }
    let (Some(actor_id), Some(commit), true) = (actor_id, commit, wrong.is_empty()) else {
        return faults(StatusCode::BAD_REQUEST, wrong);
    };
    if event == "push" {
        title = commit.message.lines().next().unwrap_or("").to_string();
    }
    let new = NewWorkflowRun {
        workflow_id: workflow.id,
        event: &event,
        head_sha: &head_sha,
        head_branch: Some(&head_branch),
        title: &title,
        actor_id,
        runner: RUNNER,
    };
    answered(
        &state,
        StatusCode::CREATED,
        state.world.start_workflow_run(new, SYSTEM),
    )
}

/// The repository's job with the path's id, which is a check run that is a
/// job of a workflow run.
fn job_at(
    state: &AppState,
    owner: &str,
    name: &str,
    raw: &str,
) -> Result<CheckRun, Box<Response<Body>>> {
    let repo = repo_at(state, owner, name)?;
    let found = match raw.parse::<i64>() {
        Ok(id) => state.world.check_run_by_id(id),
        Err(_) => Ok(None),
    };
    match found {
        Ok(Some(job)) if job.repo_id == repo.id && job.job.is_some() => Ok(job),
        Ok(_) => Err(Box::new(not_found(&format!(
            "no workflow job with id {raw:?} in {}",
            repo.full_name()
        )))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// The run a job is in, after a write to the job.
fn run_of<T>(state: &AppState, job: &CheckRun, written: Result<T, WorldError>) -> Response<Body> {
    let run_id = job.job.as_ref().map_or(0, |j| j.run_id);
    let run = written.and_then(|_| {
        state
            .world
            .workflow_run_by_id(run_id)?
            .ok_or(WorldError::NoSuchWorkflowRun(run_id))
    });
    answered(state, StatusCode::OK, run)
}

/// The conclusion a body gives a step or a job a runner finished.
fn conclusion(fields: &mut Fields) -> Option<bool> {
    let given: Option<String> = fields.required("conclusion");
    match given.as_deref() {
        Some("success") => Some(true),
        Some("failure") => Some(false),
        Some(_) => {
            fields.fault(Fault::invalid(
                "conclusion is success or failure".to_string(),
                "conclusion",
            ));
            None
        }
        None => None,
    }
}

/// A runner takes a queued job. The job is in progress and lists its
/// steps, the first of them, "Set up job", in progress.
async fn start_job(
    State(state): State<AppState>,
    UrlPath((owner, name, job_id)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    if let Err(resp) = Fields::parse(&body).finish(Some(())) {
        return *resp;
    }
    let job = match job_at(&state, &owner, &name, &job_id) {
        Ok(job) => job,
        Err(resp) => return *resp,
    };
    run_of(&state, &job, state.world.start_workflow_job(job.id, SYSTEM))
}

/// A runner writes lines to the job's log, in the step in progress. GitHub
/// serves them once the step has finished (`spec/surface-github.md`,
/// "Logs").
async fn write_log(
    State(state): State<AppState>,
    UrlPath((owner, name, job_id)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let lines: Option<Vec<String>> = fields.required("lines");
    if lines.as_ref().is_some_and(Vec::is_empty) {
        fields.fault(Fault::invalid(
            "lines holds at least one line".to_string(),
            "lines",
        ));
    }
    for (i, line) in lines.iter().flatten().enumerate() {
        if line.contains(['\n', '\r']) {
            fields.fault(Fault::invalid(
                "a line holds no line break".to_string(),
                format!("lines[{i}]"),
            ));
        }
    }
    let lines = match fields.finish(lines) {
        Ok(lines) => lines,
        Err(resp) => return *resp,
    };
    let job = match job_at(&state, &owner, &name, &job_id) {
        Ok(job) => job,
        Err(resp) => return *resp,
    };
    let written = state.world.append_job_log(job.id, &lines, SYSTEM);
    run_of(&state, &job, written)
}

/// The step in progress finishes, and the next step that runs begins:
/// after a failure, a step that does not run regardless is skipped. When
/// no step is left the job finishes, as a failure if a step failed, and
/// the jobs that were waiting for it are listed.
async fn complete_step(
    State(state): State<AppState>,
    UrlPath((owner, name, job_id)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let succeeded = conclusion(&mut fields);
    let succeeded = match fields.finish(succeeded) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    let job = match job_at(&state, &owner, &name, &job_id) {
        Ok(job) => job,
        Err(resp) => return *resp,
    };
    let conclusion = match succeeded {
        true => StepConclusion::Success,
        false => StepConclusion::Failure,
    };
    let written = state
        .world
        .complete_workflow_step(job.id, conclusion, SYSTEM);
    run_of(&state, &job, written)
}

/// A job in progress finishes outright. A success has every step left
/// succeed. A failure has the step in progress fail, and the steps after
/// it go as they do after a failure.
async fn complete_job(
    State(state): State<AppState>,
    UrlPath((owner, name, job_id)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let succeeded = conclusion(&mut fields);
    let succeeded = match fields.finish(succeeded) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    let job = match job_at(&state, &owner, &name, &job_id) {
        Ok(job) => job,
        Err(resp) => return *resp,
    };
    let written = state.world.complete_workflow_job(job.id, succeeded, SYSTEM);
    run_of(&state, &job, written)
}
