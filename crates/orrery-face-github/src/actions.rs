//! GitHub Actions as REST reads it: workflows, their runs, and a run's
//! jobs with their steps. A job is a check run, whose id it shares.

use std::collections::BTreeMap;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use orrery_face_git::{signature, store};
use orrery_world::{
    Actor, Caller, CheckRun, Door, NewWorkflowRun, Pull, PullState, Repo, Rerun, StepConclusion,
    StepStatus, Workflow, WorkflowRun, WorldError,
};

use crate::reads::query_param as param;
use crate::render::{actor_json, rfc3339, user_json};
use crate::{GithubFace, github_error, internal, json_response, media, resolve_repo};

/// The slug of GitHub's own App, whose bot a run's jobs are check runs of.
pub(crate) const ACTIONS_APP: &str = "github-actions";
/// That App's id on GitHub, which a world answers where it has no App of
/// the slug.
pub(crate) const ACTIONS_APP_ID: i64 = 15368;

/// GitHub's own Actions App, as it appears on a check run or a suite of a
/// world that has no App of the slug. The values are GitHub's.
pub(crate) fn actions_app_json(face: &GithubFace) -> serde_json::Value {
    let web = face.layout.web();
    serde_json::json!({
        "id": ACTIONS_APP_ID,
        "client_id": "Iv1.05c79e9ad1f6bdfa",
        "slug": ACTIONS_APP,
        "node_id": "MDM6QXBwMTUzNjg=",
        "owner": {
            "login": "github",
            "id": 9919,
            "node_id": "MDEyOk9yZ2FuaXphdGlvbjk5MTk=",
            "type": "Organization",
            "site_admin": false,
        },
        "name": "GitHub Actions",
        "description": "Automate your workflow from idea to production",
        "external_url": "https://help.github.com/en/actions",
        "html_url": format!("{web}/apps/{ACTIONS_APP}"),
        "created_at": "2018-07-30T09:30:17Z",
        "updated_at": "2018-07-30T09:30:17Z",
        "permissions": {
            "actions": "write", "administration": "read", "checks": "write",
            "contents": "write", "deployments": "write", "issues": "write",
            "metadata": "read", "packages": "write", "pages": "write",
            "pull_requests": "write", "repository_hooks": "write",
            "repository_projects": "write", "security_events": "write",
            "statuses": "write", "vulnerability_alerts": "read",
        },
        "events": [
            "check_run", "check_suite", "create", "delete", "deployment",
            "deployment_status", "fork", "gollum", "issues", "issue_comment", "label",
            "milestone", "page_build", "public", "pull_request", "pull_request_review",
            "pull_request_review_comment", "push", "registry_package", "release",
            "repository", "repository_dispatch", "status", "watch", "workflow_dispatch",
            "workflow_run",
        ],
    })
}

fn not_found() -> Response<Body> {
    github_error(StatusCode::NOT_FOUND, "Not Found")
}

/// Where REST serves the repository.
fn api(face: &GithubFace, repo: &Repo) -> String {
    format!("{}/repos/{}", face.layout.api(), repo.full_name())
}

/// The page a job's check run and the job itself link to.
pub(crate) fn job_html_url(face: &GithubFace, repo: &Repo, run_id: i64, job_id: i64) -> String {
    format!(
        "{}/{}/actions/runs/{run_id}/job/{job_id}",
        face.layout.web(),
        repo.full_name()
    )
}

/// A time as GitHub writes a workflow's: with milliseconds.
fn rfc3339_millis(unix: i64) -> String {
    rfc3339(unix).replacen('Z', ".000Z", 1)
}

pub(crate) fn workflow_json(
    face: &GithubFace,
    repo: &Repo,
    workflow: &Workflow,
) -> serde_json::Value {
    let web = face.layout.web();
    let full_name = repo.full_name();
    serde_json::json!({
        "id": workflow.id,
        "node_id": crate::nodeid::workflow(workflow.id),
        "name": workflow.name,
        "path": workflow.path,
        "state": "active",
        "created_at": rfc3339_millis(workflow.created_clock_unix),
        "updated_at": rfc3339_millis(workflow.created_clock_unix),
        "url": format!("{}/actions/workflows/{}", api(face, repo), workflow.id),
        "html_url": format!(
            "{web}/{full_name}/blob/{}/{}",
            repo.default_branch, workflow.path
        ),
        "badge_url": format!(
            "{web}/{full_name}/workflows/{}/badge.svg",
            crate::oauth::percent_encode(&workflow.name)
        ),
    })
}

/// The repository as a run carries it: GitHub's short form.
fn short_repo_json(face: &GithubFace, repo: &Repo, owner: &Actor) -> serde_json::Value {
    let web = face.layout.web();
    serde_json::json!({
        "id": repo.id,
        "node_id": crate::nodeid::repo(repo.id),
        "name": repo.name,
        "full_name": repo.full_name(),
        "private": repo.private,
        "owner": actor_json(owner),
        "html_url": format!("{web}/{}", repo.full_name()),
        "description": repo.description,
        "fork": false,
        "url": api(face, repo),
    })
}

/// What a run's JSON takes beside the run: its repository, the workflows
/// of that repository, and the commits its runs are on.
pub(crate) struct RunContext<'a> {
    face: &'a GithubFace,
    repo: &'a Repo,
    owner: &'a Actor,
    workflows: Vec<Workflow>,
    commits: BTreeMap<String, signature::Commit>,
    /// The repository's open pull requests: none for a request that asked
    /// for runs without theirs.
    open: Vec<Pull>,
}

impl<'a> RunContext<'a> {
    /// The context for rendering `runs`, whose commits one read of the bare
    /// repository answers. The caller holds the gate.
    fn read(
        face: &'a GithubFace,
        repo: &'a Repo,
        owner: &'a Actor,
        runs: &[&WorkflowRun],
        pulls: bool,
    ) -> Result<RunContext<'a>, Box<Response<Body>>> {
        let workflows = face
            .world
            .list_workflows_for_repo(repo.id)
            .map_err(|e| Box::new(internal(e)))?;
        let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
        let mut shas: Vec<&str> = runs.iter().map(|run| run.head_sha.as_str()).collect();
        shas.sort_unstable();
        shas.dedup();
        let commits = signature::read_commits(&bare, &shas)
            .map_err(|e| Box::new(internal(e)))?
            .into_iter()
            .map(|commit| (commit.sha.clone(), commit))
            .collect();
        let open = match pulls {
            true => face
                .world
                .list_pulls_for_repo(repo.id, Some(PullState::Open))
                .map_err(|e| Box::new(internal(e)))?,
            false => Vec::new(),
        };
        Ok(RunContext {
            face,
            repo,
            owner,
            workflows,
            commits,
            open,
        })
    }

    /// The context for one run's delivery. A commit the repository cannot
    /// read is left out, as a delivery leaves out what git cannot answer.
    pub(crate) fn for_delivery(
        face: &'a GithubFace,
        repo: &'a Repo,
        owner: &'a Actor,
        run: &WorkflowRun,
    ) -> Result<RunContext<'a>, WorldError> {
        let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
        let commits = signature::read_commits(&bare, &[run.head_sha.as_str()])
            .unwrap_or_default()
            .into_iter()
            .map(|commit| (commit.sha.clone(), commit))
            .collect();
        Ok(RunContext {
            face,
            repo,
            owner,
            workflows: face.world.list_workflows_for_repo(repo.id)?,
            commits,
            open: face
                .world
                .list_pulls_for_repo(repo.id, Some(PullState::Open))?,
        })
    }

    pub(crate) fn workflow(&self, id: i64) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.id == id)
    }

    pub(crate) fn run_json(&self, run: &WorkflowRun) -> serde_json::Value {
        let (face, repo) = (self.face, self.repo);
        let web = face.layout.web();
        let api = api(face, repo);
        let url = format!("{api}/actions/runs/{}", run.id);
        let workflow = self.workflow(run.workflow_id);
        let actor = user_json(face, &run.actor_login);
        let person = |i: &signature::Ident| serde_json::json!({ "name": i.name, "email": i.email });
        let head_commit = self.commits.get(&run.head_sha).map(|commit| {
            serde_json::json!({
                "id": commit.sha,
                "tree_id": commit.tree,
                "message": commit.message.trim_end_matches('\n'),
                "timestamp": rfc3339(commit.committer.unix),
                "author": person(&commit.author),
                "committer": person(&commit.committer),
            })
        });
        let pull_requests = crate::suites::pulls_among(face, repo, &self.open, &run.head_sha);
        let repository = short_repo_json(face, repo, self.owner);
        serde_json::json!({
            "id": run.id,
            "name": workflow.map(|w| w.name.as_str()),
            "node_id": crate::nodeid::workflow_run(run.id),
            "head_branch": run.head_branch,
            "head_sha": run.head_sha,
            "path": workflow.map(|w| w.path.as_str()),
            "display_title": run.title,
            "run_number": run.run_number,
            "event": run.event,
            "status": run.status.as_str(),
            "conclusion": run.conclusion.map(|c| c.as_str()),
            "workflow_id": run.workflow_id,
            "check_suite_id": run.suite_id,
            "check_suite_node_id": crate::nodeid::check_suite(run.suite_id),
            "url": url,
            "html_url": format!("{web}/{}/actions/runs/{}", repo.full_name(), run.id),
            "pull_requests": pull_requests,
            "created_at": rfc3339(run.created_clock_unix),
            "updated_at": rfc3339(run.updated_clock_unix),
            "actor": actor,
            "run_attempt": run.attempt,
            "referenced_workflows": [],
            "run_started_at": rfc3339(run.started_clock_unix),
            "triggering_actor": user_json(face, &run.triggering_actor_login),
            "jobs_url": format!("{url}/jobs"),
            "logs_url": format!("{url}/logs"),
            "check_suite_url": format!("{api}/check-suites/{}", run.suite_id),
            "artifacts_url": format!("{url}/artifacts"),
            "cancel_url": format!("{url}/cancel"),
            "rerun_url": format!("{url}/rerun"),
            "previous_attempt_url": (run.attempt > 1)
                .then(|| format!("{url}/attempts/{}", run.attempt - 1)),
            "workflow_url": format!("{api}/actions/workflows/{}", run.workflow_id),
            "head_commit": head_commit,
            "repository": repository,
            "head_repository": repository,
        })
    }
}

/// A job of `run` as GitHub's `jobs` routes answer it. The machine a
/// started job names is one of GitHub's own, numbered after the job.
pub(crate) fn job_json(
    face: &GithubFace,
    repo: &Repo,
    run: &WorkflowRun,
    workflow: Option<&Workflow>,
    job: &CheckRun,
) -> serde_json::Value {
    let api = api(face, repo);
    let held = job.job.as_ref();
    let steps: Vec<serde_json::Value> = held
        .into_iter()
        .flat_map(|j| &j.steps)
        .enumerate()
        .map(|(i, step)| {
            serde_json::json!({
                "name": step.name,
                "status": match step.status {
                    StepStatus::Pending => "pending",
                    StepStatus::InProgress => "in_progress",
                    StepStatus::Completed => "completed",
                },
                "conclusion": step.conclusion.map(|c| match c {
                    StepConclusion::Success => "success",
                    StepConclusion::Failure => "failure",
                    StepConclusion::Skipped => "skipped",
                    StepConclusion::Cancelled => "cancelled",
                }),
                "number": i + 1,
                "started_at": step.started_unix.map(rfc3339),
                "completed_at": step.completed_unix.map(rfc3339),
            })
        })
        .collect();
    let labels = workflow
        .and_then(|w| w.jobs.iter().find(|spec| spec.name == job.name))
        .map(|spec| spec.labels.clone())
        .unwrap_or_default();
    // A runner took the job if it lists a step: a queued job and a skipped
    // one list none.
    let runner = (!steps.is_empty()).then_some(1_000_000_000 + job.id);
    serde_json::json!({
        "id": job.id,
        "run_id": run.id,
        "workflow_name": workflow.map(|w| w.name.as_str()),
        "head_branch": run.head_branch,
        "run_url": format!("{api}/actions/runs/{}", run.id),
        "run_attempt": held.map(|j| j.attempt),
        "node_id": crate::nodeid::check(job.id),
        "head_sha": job.head_sha,
        "url": format!("{api}/actions/jobs/{}", job.id),
        "html_url": job_html_url(face, repo, run.id, job.id),
        "status": job.status.as_str(),
        "conclusion": job.conclusion.map(|c| c.as_str()),
        "created_at": rfc3339(job.created_clock_unix),
        // A job no runner has taken says it started when it was listed.
        "started_at": rfc3339(job.started_clock_unix.unwrap_or(job.created_clock_unix)),
        "completed_at": job.completed_clock_unix.map(rfc3339),
        "name": job.name,
        "steps": steps,
        "check_run_url": format!("{api}/check-runs/{}", job.id),
        "labels": labels,
        "runner_id": runner,
        "runner_name": runner.map(|id| format!("GitHub Actions {id}")),
        "runner_group_id": runner.map(|_| 0),
        "runner_group_name": runner.map(|_| "GitHub Actions"),
    })
}

/// The page GitHub's Actions lists answer: `per_page` items from `page`,
/// where a `per_page` outside 1 to 100 is the default, 30.
fn page_of(query: &str) -> (usize, usize) {
    let per_page = param(query, "per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| (1..=100).contains(n))
        .unwrap_or(30);
    let page = param(query, "page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    (per_page, page)
}

/// `{total_count, <key>: [...]}` for the page `query` asks of `items`, with
/// GitHub's `Link` header: the list's path under `/repositories/{id}`, the
/// query as it was asked, and `page` last.
fn listed(
    face: &GithubFace,
    repo: &Repo,
    path: &str,
    key: &str,
    query: &str,
    total: usize,
    items: Vec<serde_json::Value>,
) -> Response<Body> {
    let (per_page, page) = page_of(query);
    let mut resp = json_response(
        StatusCode::OK,
        serde_json::json!({ "total_count": total, key: items }),
    );
    let last = total.div_ceil(per_page).max(1);
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && *pair != "page" && !pair.starts_with("page="))
        .collect();
    let api = face.layout.api();
    let to = |n: usize, rel: &str| {
        let mut params = kept.clone();
        let page = format!("page={n}");
        params.push(&page);
        format!(
            "<{api}/repositories/{}/{path}?{}>; rel=\"{rel}\"",
            repo.id,
            params.join("&")
        )
    };
    let mut links = Vec::new();
    if page > 1 {
        links.push(to((page - 1).min(last), "prev"));
    }
    if page < last {
        links.push(to(page + 1, "next"));
        links.push(to(last, "last"));
    }
    if page > 1 {
        links.push(to(1, "first"));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        resp.headers_mut().insert(header::LINK, value);
    }
    resp
}

fn page<T>(items: Vec<T>, query: &str) -> Vec<T> {
    let (per_page, page) = page_of(query);
    crate::reads::paginate(items, per_page, page)
}

/// The repository with the id a `/repositories/{id}` path names.
fn repo_by_id(face: &GithubFace, id: &str) -> Result<(Repo, Actor), Box<Response<Body>>> {
    let repo = match id.parse::<i64>().map(|id| face.world.repo_by_id(id)) {
        Ok(Ok(Some(repo))) => repo,
        Ok(Ok(None)) | Err(_) => return Err(Box::new(not_found())),
        Ok(Err(e)) => return Err(Box::new(internal(e))),
    };
    resolve_repo(face, &repo.owner_login, &repo.name)
}

/// The repository's workflow a path segment names: by its id, or by the
/// name of its file.
fn workflow_at(
    face: &GithubFace,
    repo: &Repo,
    named: &str,
) -> Result<Workflow, Box<Response<Body>>> {
    let workflows = face
        .world
        .list_workflows_for_repo(repo.id)
        .map_err(|e| Box::new(internal(e)))?;
    let id = named.parse::<i64>().ok();
    workflows
        .into_iter()
        .find(|w| Some(w.id) == id || w.path.rsplit('/').next() == Some(named))
        .ok_or_else(|| Box::new(not_found()))
}

/// The repository's run with this id.
fn run_at(face: &GithubFace, repo: &Repo, id: &str) -> Result<WorkflowRun, Box<Response<Body>>> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(Box::new(not_found()));
    };
    match face.world.workflow_run_by_id(id) {
        Ok(Some(run)) if run.repo_id == repo.id => Ok(run),
        Ok(_) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn workflows_page(face: &GithubFace, repo: &Repo, query: &str) -> Response<Body> {
    let workflows = match face.world.list_workflows_for_repo(repo.id) {
        Ok(workflows) => workflows,
        Err(e) => return internal(e),
    };
    let total = workflows.len();
    let items = page(workflows, query)
        .iter()
        .map(|w| workflow_json(face, repo, w))
        .collect();
    listed(
        face,
        repo,
        "actions/workflows",
        "workflows",
        query,
        total,
        items,
    )
}

pub(crate) async fn list_workflows(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Workflow") {
        return refused;
    }
    workflows_page(&face, &repo, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/actions/workflows`: the same list, at the path
/// its own `Link` header names.
pub(crate) async fn list_workflows_by_id(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match repo_by_id(&face, &id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Workflow") {
        return refused;
    }
    workflows_page(&face, &repo, query.as_deref().unwrap_or(""))
}

pub(crate) async fn get_workflow(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, named)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let workflow = match workflow_at(&face, &repo, &named) {
        Ok(workflow) => workflow,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Workflow") {
        return refused;
    }
    json_response(StatusCode::OK, workflow_json(&face, &repo, &workflow))
}

/// GitHub's filters on a list of runs. Each keeps the runs that match, and
/// a value that matches none keeps none. `status` takes a status or a
/// conclusion.
fn filtered(runs: Vec<WorkflowRun>, query: &str) -> Vec<WorkflowRun> {
    let mut runs = runs;
    if let Some(actor) = param(query, "actor") {
        runs.retain(|run| run.actor_login.eq_ignore_ascii_case(&actor));
    }
    if let Some(branch) = param(query, "branch") {
        runs.retain(|run| run.head_branch.as_deref() == Some(branch.as_str()));
    }
    if let Some(event) = param(query, "event") {
        runs.retain(|run| run.event == event);
    }
    if let Some(sha) = param(query, "head_sha") {
        runs.retain(|run| run.head_sha == sha);
    }
    if let Some(suite) = param(query, "check_suite_id") {
        runs.retain(|run| run.suite_id.to_string() == suite);
    }
    if let Some(status) = param(query, "status") {
        runs.retain(|run| {
            run.status.as_str() == status || run.conclusion.is_some_and(|c| c.as_str() == status)
        });
    }
    runs
}

/// One page of a repository's runs, newest first: those of one workflow
/// when `workflow` is given. The caller holds the gate.
fn runs_page(
    face: &GithubFace,
    repo: &Repo,
    owner: &Actor,
    workflow: Option<&Workflow>,
    query: &str,
) -> Response<Body> {
    // GitHub filters on when a run was created, which a world does not, so
    // a caller that asks is told rather than answered every run.
    if param(query, "created").is_some() {
        return crate::orrery_unsupported_field("WorkflowRun", "created");
    }
    let mut runs = match face.world.list_workflow_runs_for_repo(repo.id) {
        Ok(runs) => filtered(runs, query),
        Err(e) => return internal(e),
    };
    if let Some(workflow) = workflow {
        runs.retain(|run| run.workflow_id == workflow.id);
    }
    let total = runs.len();
    let runs = page(runs, query);
    let pulls = param(query, "exclude_pull_requests").as_deref() != Some("true");
    let shown: Vec<&WorkflowRun> = runs.iter().collect();
    let context = match RunContext::read(face, repo, owner, &shown, pulls) {
        Ok(context) => context,
        Err(resp) => return *resp,
    };
    let items = runs.iter().map(|run| context.run_json(run)).collect();
    let path = match workflow {
        Some(workflow) => format!("actions/workflows/{}/runs", workflow.id),
        None => "actions/runs".to_string(),
    };
    listed(face, repo, &path, "workflow_runs", query, total, items)
}

pub(crate) async fn list_runs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, and each run
    // carries its commit.
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    runs_page(&face, &repo, &owner, None, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/actions/runs`: the same list, at the path its
/// own `Link` header names.
pub(crate) async fn list_runs_by_id(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match repo_by_id(&face, &id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    runs_page(&face, &repo, &owner, None, query.as_deref().unwrap_or(""))
}

pub(crate) async fn list_workflow_runs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, named)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let workflow = match workflow_at(&face, &repo, &named) {
        Ok(workflow) => workflow,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let query = query.unwrap_or_default();
    runs_page(&face, &repo, &owner, Some(&workflow), &query)
}

/// `GET /repositories/{id}/actions/workflows/{workflow}/runs`: the same
/// list, at the path its own `Link` header names.
pub(crate) async fn list_workflow_runs_by_id(
    State(face): State<GithubFace>,
    UrlPath((id, named)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match repo_by_id(&face, &id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let workflow = match workflow_at(&face, &repo, &named) {
        Ok(workflow) => workflow,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let query = query.unwrap_or_default();
    runs_page(&face, &repo, &owner, Some(&workflow), &query)
}

pub(crate) async fn get_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let query = query.unwrap_or_default();
    let pulls = param(&query, "exclude_pull_requests").as_deref() != Some("true");
    let context = match RunContext::read(&face, &repo, &owner, &[&run], pulls) {
        Ok(context) => context,
        Err(resp) => return *resp,
    };
    json_response(StatusCode::OK, context.run_json(&run))
}

/// One page of a run's jobs, oldest first: those of the attempt `run` is
/// at, or, for a list that is not of one attempt and asks `filter=all`,
/// those of every attempt.
fn jobs_page(
    face: &GithubFace,
    repo: &Repo,
    run: &WorkflowRun,
    of_attempt: bool,
    query: &str,
) -> Response<Body> {
    let every = !of_attempt && param(query, "filter").as_deref() == Some("all");
    let read = || -> Result<(Vec<CheckRun>, Option<Workflow>), WorldError> {
        let mut jobs = face.world.list_workflow_jobs(run.id)?;
        jobs.retain(|job| every || job.job.as_ref().is_some_and(|j| j.attempt == run.attempt));
        Ok((jobs, face.world.workflow_by_id(run.workflow_id)?))
    };
    let (jobs, workflow) = match read() {
        Ok(read) => read,
        Err(e) => return internal(e),
    };
    let total = jobs.len();
    let items = page(jobs, query)
        .iter()
        .map(|job| job_json(face, repo, run, workflow.as_ref(), job))
        .collect();
    let path = match of_attempt {
        true => format!("actions/runs/{}/attempts/{}/jobs", run.id, run.attempt),
        false => format!("actions/runs/{}/jobs", run.id),
    };
    listed(face, repo, &path, "jobs", query, total, items)
}

pub(crate) async fn list_jobs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    jobs_page(&face, &repo, &run, false, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/actions/runs/{run_id}/jobs`: the same list, at
/// the path its own `Link` header names.
pub(crate) async fn list_jobs_by_id(
    State(face): State<GithubFace>,
    UrlPath((repo_id, id)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match repo_by_id(&face, &repo_id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    jobs_page(&face, &repo, &run, false, query.as_deref().unwrap_or(""))
}

pub(crate) async fn get_job(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let read = || -> Result<Option<(CheckRun, WorkflowRun, Option<Workflow>)>, WorldError> {
        let Ok(id) = id.parse::<i64>() else {
            return Ok(None);
        };
        let Some(job) = face.world.check_run_by_id(id)? else {
            return Ok(None);
        };
        let Some(held) = job.job.as_ref().filter(|_| job.repo_id == repo.id) else {
            return Ok(None);
        };
        let Some(run) = face.world.workflow_run_by_id(held.run_id)? else {
            return Ok(None);
        };
        let workflow = face.world.workflow_by_id(run.workflow_id)?;
        Ok(Some((job, run, workflow)))
    };
    let (job, run, workflow) = match read() {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    json_response(
        StatusCode::OK,
        job_json(&face, &repo, &run, workflow.as_ref(), &job),
    )
}

/// The repository's run as its attempt `n` left it, or GitHub's 404 for an
/// attempt it has not had.
fn attempt_at(
    face: &GithubFace,
    repo: &Repo,
    id: &str,
    n: &str,
) -> Result<WorkflowRun, Box<Response<Body>>> {
    let run = run_at(face, repo, id)?;
    let Ok(n) = n.parse::<i64>() else {
        return Err(Box::new(not_found()));
    };
    match face.world.workflow_run_at_attempt(run.id, n) {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(internal(e))),
    }
}

pub(crate) async fn get_attempt(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id, n)): UrlPath<(String, String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match attempt_at(&face, &repo, &id, &n) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let query = query.unwrap_or_default();
    let pulls = param(&query, "exclude_pull_requests").as_deref() != Some("true");
    let context = match RunContext::read(&face, &repo, &owner, &[&run], pulls) {
        Ok(context) => context,
        Err(resp) => return *resp,
    };
    json_response(StatusCode::OK, context.run_json(&run))
}

pub(crate) async fn list_attempt_jobs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id, n)): UrlPath<(String, String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match attempt_at(&face, &repo, &id, &n) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    jobs_page(&face, &repo, &run, true, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/actions/runs/{run_id}/attempts/{n}/jobs`: the
/// same list, at the path its own `Link` header names.
pub(crate) async fn list_attempt_jobs_by_id(
    State(face): State<GithubFace>,
    UrlPath((repo_id, id, n)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match repo_by_id(&face, &repo_id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match attempt_at(&face, &repo, &id, &n) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    jobs_page(&face, &repo, &run, true, query.as_deref().unwrap_or(""))
}

/// GitHub's answer to a write it has taken and will carry out: an empty
/// object.
fn taken(status: StatusCode) -> Response<Body> {
    json_response(status, serde_json::json!({}))
}

/// What a re-run answers: `201 {}`, or GitHub's refusal. `running` is its
/// message for a run that has not finished.
fn rerun_answer(written: Result<WorkflowRun, WorldError>, running: &str) -> Response<Body> {
    match written {
        Ok(_) => taken(StatusCode::CREATED),
        Err(WorldError::WorkflowRunInProgress(_)) => github_error(StatusCode::FORBIDDEN, running),
        Err(WorldError::NothingToRerun(_)) => {
            github_error(StatusCode::FORBIDDEN, "This workflow run cannot be retried")
        }
        Err(WorldError::NoSuchWorkflowRun(_) | WorldError::NoSuchWorkflowJob(_)) => not_found(),
        Err(e) => internal(e),
    }
}

/// `POST …/actions/runs/{id}/rerun`: every job of the run runs again, in a
/// new attempt.
pub(crate) async fn rerun(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let written = face
        .world
        .rerun_workflow_run(run.id, Rerun::All, caller.actor.id, by);
    rerun_answer(written, "This workflow is already running")
}

/// `POST …/actions/runs/{id}/rerun-failed-jobs`: the jobs that failed or
/// were cancelled run again, with every job that needs one of them.
pub(crate) async fn rerun_failed(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    let written = face
        .world
        .rerun_workflow_run(run.id, Rerun::Failed, caller.actor.id, by);
    rerun_answer(written, "This workflow is already running")
}

/// `POST …/actions/jobs/{id}/rerun`: the job runs again, with every job
/// that needs it. A job of an earlier attempt names the job of its name in
/// the latest.
pub(crate) async fn rerun_job(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let found = || -> Result<Option<(i64, i64)>, WorldError> {
        let Ok(id) = id.parse::<i64>() else {
            return Ok(None);
        };
        let Some(job) = face.world.check_run_by_id(id)? else {
            return Ok(None);
        };
        let Some(held) = job.job.as_ref().filter(|_| job.repo_id == repo.id) else {
            return Ok(None);
        };
        let Some(run) = face.world.workflow_run_by_id(held.run_id)? else {
            return Ok(None);
        };
        let latest = face
            .world
            .list_workflow_jobs(run.id)?
            .into_iter()
            .filter(|other| other.job.as_ref().is_some_and(|j| j.attempt == run.attempt))
            .find(|other| other.name == job.name);
        Ok(latest.map(|latest| (run.id, latest.id)))
    };
    let (run_id, job_id) = match found() {
        Ok(Some(found)) => found,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    let written = face
        .world
        .rerun_workflow_run(run_id, Rerun::Job(job_id), caller.actor.id, by);
    rerun_answer(
        written,
        "The workflow run containing this job is already running",
    )
}

/// What a cancel answers: `202 {}`, or GitHub's `409` for a run that has
/// finished.
fn cancel_answer(written: Result<(), WorldError>) -> Response<Body> {
    match written {
        Ok(()) => taken(StatusCode::ACCEPTED),
        Err(WorldError::WorkflowRunCompleted(_)) => github_error(
            StatusCode::CONFLICT,
            "Cannot cancel a workflow run that is completed.",
        ),
        Err(WorldError::NoSuchWorkflowRun(_)) => not_found(),
        Err(e) => internal(e),
    }
}

/// `POST …/actions/runs/{id}/cancel`: the run is asked to cancel.
pub(crate) async fn cancel(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    cancel_answer(face.world.cancel_workflow_run(run.id, false, by))
}

/// `POST …/actions/runs/{id}/force-cancel`: the run is cancelled where it
/// stands.
pub(crate) async fn force_cancel(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    cancel_answer(face.world.cancel_workflow_run(run.id, true, by))
}

/// What a dispatch gives: the branch to run on, and the inputs it names.
#[derive(serde::Deserialize)]
struct DispatchBody {
    r#ref: Option<String>,
    #[serde(default)]
    inputs: BTreeMap<String, serde_json::Value>,
}

/// `POST …/actions/workflows/{workflow}/dispatches`: a `workflow_dispatch`
/// run of the workflow starts on a branch's tip, started by the caller and
/// titled by the workflow's name.
pub(crate) async fn dispatch(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, named)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    // The branch is read from the bare repository, which a restore
    // replaces under the gate.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let workflow = match workflow_at(&face, &repo, &named) {
        Ok(workflow) => workflow,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Workflow") {
        return refused;
    }
    let parsed: DispatchBody = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    let Some(given) = parsed.r#ref.filter(|r| !r.is_empty()) else {
        return crate::invalid_request("\"ref\" wasn't supplied.");
    };
    let unprocessable = |message: &str| github_error(StatusCode::UNPROCESSABLE_ENTITY, message);
    let Some(inputs) = &workflow.dispatch_inputs else {
        return unprocessable("Workflow does not have 'workflow_dispatch' trigger");
    };
    let branch = given.strip_prefix("refs/heads/").unwrap_or(&given);
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let head_sha = match store::rev_parse(&bare, &format!("refs/heads/{branch}")) {
        Ok(Some(sha)) => sha,
        Ok(None) => return unprocessable(&format!("No ref found for: {given}")),
        Err(e) => return internal(e),
    };
    let unexpected: Vec<&String> = parsed
        .inputs
        .keys()
        .filter(|name| !inputs.contains(name))
        .collect();
    if !unexpected.is_empty() {
        let names = serde_json::to_string(&unexpected).unwrap_or_default();
        return unprocessable(&format!("Unexpected inputs provided: {names}"));
    }
    let new = NewWorkflowRun {
        workflow_id: workflow.id,
        event: "workflow_dispatch",
        head_sha: &head_sha,
        head_branch: Some(branch),
        title: &workflow.name,
        actor_id: caller.actor.id,
        runner: ACTIONS_APP,
    };
    match face.world.start_workflow_run(new, by) {
        Ok(_) => crate::no_content(),
        Err(e) => internal(e),
    }
}
