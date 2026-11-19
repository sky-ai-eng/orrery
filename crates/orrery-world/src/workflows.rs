//! Workflows, their runs, and the jobs of a run. The kernel does what a
//! workflow's orchestrator does: it lists a job once the jobs it needs have
//! finished, as queued or as skipped, and walks a job's steps as its runner
//! reports them. A job is a check run of the run's suite, so what a run has
//! come to is what its suite has.

use rusqlite::{OptionalExtension, params};

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, log, objects, queries, suites};

pub struct NewWorkflow<'a> {
    pub repo_id: i64,
    pub path: &'a str,
    pub name: &'a str,
    pub jobs: &'a [JobSpec],
}

pub struct NewWorkflowRun<'a> {
    pub workflow_id: i64,
    /// What started it, stored as given.
    pub event: &'a str,
    pub head_sha: &'a str,
    pub head_branch: Option<&'a str>,
    pub title: &'a str,
    /// Who started it.
    pub actor_id: i64,
    /// The login of the bot whose check runs the jobs are. The first run to
    /// name it makes it.
    pub runner: &'a str,
}

/// What is wrong with a workflow's jobs, as `(field, message)` with the
/// field a path into `jobs`; empty for jobs a workflow can hold. A workflow
/// has a job, its jobs' names differ, each needs only other jobs of the
/// workflow, and no job needs itself through the others.
pub fn check_jobs(jobs: &[JobSpec]) -> Vec<(String, String)> {
    let mut faults = Vec::new();
    if jobs.is_empty() {
        faults.push(("jobs".to_string(), "a workflow has a job".to_string()));
    }
    for (i, job) in jobs.iter().enumerate() {
        if jobs[..i].iter().any(|other| other.name == job.name) {
            faults.push((
                format!("jobs[{i}].name"),
                format!("another job is named {:?}", job.name),
            ));
        }
        for (n, need) in job.needs.iter().enumerate() {
            if *need == job.name || !jobs.iter().any(|other| other.name == *need) {
                faults.push((
                    format!("jobs[{i}].needs[{n}]"),
                    format!("no other job is named {need:?}"),
                ));
            }
        }
        if job.steps.is_empty() {
            faults.push((format!("jobs[{i}].steps"), "a job has a step".to_string()));
        }
    }
    if !faults.is_empty() {
        return faults;
    }
    // Every job is reached by taking, over and over, the jobs whose needs
    // are all taken. What is left needs itself.
    let mut taken: Vec<&str> = Vec::new();
    loop {
        let next: Vec<&str> = jobs
            .iter()
            .filter(|job| !taken.contains(&job.name.as_str()))
            .filter(|job| job.needs.iter().all(|need| taken.contains(&need.as_str())))
            .map(|job| job.name.as_str())
            .collect();
        if next.is_empty() {
            break;
        }
        taken.extend(next);
    }
    for (i, job) in jobs.iter().enumerate() {
        if !taken.contains(&job.name.as_str()) {
            faults.push((
                format!("jobs[{i}].needs"),
                format!("{:?} needs itself, through the jobs it needs", job.name),
            ));
        }
    }
    faults
}

impl World {
    pub fn create_workflow(&self, new: NewWorkflow<'_>, by: Attribution) -> Result<Workflow> {
        if let Some((field, message)) = check_jobs(new.jobs).into_iter().next() {
            return Err(WorldError::InvalidWorkflow { field, message });
        }
        self.write(by, |w| {
            let tx = w.tx;
            let workflow = tx
                .query_row(
                    &format!(
                        "INSERT INTO workflows (repo_id, path, name, jobs, created_clock_unix)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         RETURNING {}",
                        queries::WORKFLOW_COLUMNS
                    ),
                    params![
                        new.repo_id,
                        new.path,
                        new.name,
                        serde_json::to_string(new.jobs).expect("job specs serialize"),
                        queries::clock(tx)?
                    ],
                    queries::row_to_workflow,
                )
                .map_err(|e| match crate::uniqueness_violation(&e) {
                    true => WorldError::WorkflowExists(new.path.to_string()),
                    false => WorldError::from(e),
                })?;
            w.operation(EventBody::WorkflowCreated(events::WorkflowCreated {
                subject: objects::workflow_subject(tx, workflow.id)?,
                object: objects::workflow_object(tx, workflow.id)?,
            }));
            Ok(workflow)
        })
    }

    /// Begin a run of a workflow, and queue the jobs that need nothing.
    pub fn start_workflow_run(
        &self,
        new: NewWorkflowRun<'_>,
        by: Attribution,
    ) -> Result<WorkflowRun> {
        self.write(by, |w| {
            let tx = w.tx;
            let workflow = queries::workflow_by_id(tx, new.workflow_id)?
                .ok_or(WorldError::NoSuchWorkflow(new.workflow_id))?;
            let runner = runner(w, new.runner)?;
            let suite = suites::create(
                w,
                workflow.repo_id,
                new.head_sha,
                runner,
                new.head_branch.map(|branch| suites::Started {
                    branch,
                    before: suites::NO_COMMIT,
                    by: new.actor_id,
                }),
            )?;
            let clock = queries::clock(tx)?;
            let id: i64 = tx.query_row(
                "INSERT INTO workflow_runs (repo_id, workflow_id, suite_id, run_number, event,
                                            head_branch, head_sha, title, actor_id, attempt,
                                            created_clock_unix, started_clock_unix)
                 VALUES (?1, ?2, ?3,
                         (SELECT COALESCE(MAX(run_number), 0) + 1 FROM workflow_runs
                          WHERE workflow_id = ?2),
                         ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9)
                 RETURNING id",
                params![
                    workflow.repo_id,
                    workflow.id,
                    suite,
                    new.event,
                    new.head_branch,
                    new.head_sha,
                    new.title,
                    new.actor_id,
                    clock
                ],
                |r| r.get(0),
            )?;
            w.operation(EventBody::WorkflowRunCreated(events::WorkflowRunCreated {
                subject: objects::workflow_run_subject(tx, id)?,
                object: objects::workflow_run_object(tx, id)?,
            }));
            list_ready_jobs(w, id)?;
            suites::settle(w, suite)?;
            queries::workflow_run_by_id(tx, id)?.ok_or(WorldError::NoSuchWorkflowRun(id))
        })
    }

    /// A runner takes a queued job: it is in progress, with its steps
    /// listed and the first under way.
    pub fn start_workflow_job(&self, job_id: i64, by: Attribution) -> Result<CheckRun> {
        self.write(by, |w| {
            let (job, spec) = job_and_spec(w.tx, job_id)?;
            if job.status != CheckStatus::Queued {
                return Err(WorldError::JobNotQueued(job_id));
            }
            let clock = queries::clock(w.tx)?;
            let mut steps: Vec<Step> = spec
                .steps
                .iter()
                .map(|step| Step {
                    name: step.name.clone(),
                    status: StepStatus::Pending,
                    conclusion: None,
                    started_unix: None,
                    completed_unix: None,
                })
                .collect();
            steps[0].status = StepStatus::InProgress;
            steps[0].started_unix = Some(clock);
            store_job(
                w,
                &job,
                CheckStatus::InProgress,
                None,
                Some(clock),
                None,
                &steps,
            )
        })
    }

    /// The step in progress finishes, and the job goes on to the next step
    /// that runs. When none is left the job finishes too.
    pub fn complete_workflow_step(
        &self,
        job_id: i64,
        conclusion: StepConclusion,
        by: Attribution,
    ) -> Result<CheckRun> {
        self.write(by, |w| {
            let (job, spec) = job_and_spec(w.tx, job_id)?;
            let mut steps = running_steps(&job)?;
            let clock = queries::clock(w.tx)?;
            advance(&mut steps, &spec, conclusion, clock);
            finish_or_store(w, &job, &spec, steps, clock)
        })
    }

    /// The job finishes outright. One that succeeds has every step left
    /// succeed. One that fails has the step in progress fail, and the steps
    /// after it run as they do after a failure.
    pub fn complete_workflow_job(
        &self,
        job_id: i64,
        succeeded: bool,
        by: Attribution,
    ) -> Result<CheckRun> {
        self.write(by, |w| {
            let (job, spec) = job_and_spec(w.tx, job_id)?;
            let mut steps = running_steps(&job)?;
            let clock = queries::clock(w.tx)?;
            let mut conclusion = match succeeded {
                true => StepConclusion::Success,
                false => StepConclusion::Failure,
            };
            while steps.iter().any(|s| s.status == StepStatus::InProgress) {
                advance(&mut steps, &spec, conclusion, clock);
                conclusion = StepConclusion::Success;
            }
            finish_or_store(w, &job, &spec, steps, clock)
        })
    }

    pub fn workflow_by_id(&self, id: i64) -> Result<Option<Workflow>> {
        self.with_conn(|conn| queries::workflow_by_id(conn, id))
    }

    /// A repository's workflows, oldest first.
    pub fn list_workflows(&self, repo_id: i64) -> Result<Vec<Workflow>> {
        self.with_conn(|conn| queries::list_workflows(conn, repo_id))
    }

    pub fn workflow_run_by_id(&self, id: i64) -> Result<Option<WorkflowRun>> {
        self.with_conn(|conn| queries::workflow_run_by_id(conn, id))
    }

    /// The run whose jobs a suite holds, for a suite that is a run's.
    pub fn workflow_run_by_suite(&self, suite_id: i64) -> Result<Option<WorkflowRun>> {
        self.with_conn(|conn| queries::workflow_run_by_suite(conn, suite_id))
    }

    /// A repository's workflow runs, newest first.
    pub fn list_workflow_runs(&self, repo_id: i64) -> Result<Vec<WorkflowRun>> {
        self.with_conn(|conn| queries::list_workflow_runs(conn, repo_id))
    }

    /// The jobs of a run in every attempt, oldest first.
    pub fn list_workflow_jobs(&self, run_id: i64) -> Result<Vec<CheckRun>> {
        self.with_conn(|conn| queries::list_workflow_jobs(conn, run_id))
    }
}

/// The bot whose check runs a run's jobs are, made if the world has no
/// actor of that login.
fn runner(w: &mut log::Write<'_>, login: &str) -> Result<i64> {
    let held: Option<(i64, String)> =
        w.tx.query_row(
            "SELECT id, kind FROM actors WHERE login = ?1",
            params![login],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match held {
        Some((id, kind)) if ActorKind::from_db(&kind) == ActorKind::Bot => Ok(id),
        Some(_) => Err(WorldError::RunnerNotABot(login.to_string())),
        None => {
            let (bot, _) = crate::insert_actor(w.tx, login, ActorKind::Bot)?;
            w.emit(EventBody::ActorCreated(events::ActorCreated {
                subject: objects::actor_subject(w.tx, bot.id)?,
                object: objects::actor_object(w.tx, bot.id)?,
            }));
            Ok(bot.id)
        }
    }
}

/// A job with what its workflow says of it.
fn job_and_spec(conn: &rusqlite::Connection, job_id: i64) -> Result<(CheckRun, JobSpec)> {
    let job = queries::check_run_by_id(conn, job_id)?
        .filter(|run| run.job.is_some())
        .ok_or(WorldError::NoSuchWorkflowJob(job_id))?;
    let run_id = job.job.as_ref().map_or(0, |j| j.run_id);
    let spec = specs(conn, run_id)?
        .into_iter()
        .find(|spec| spec.name == job.name)
        .ok_or(WorldError::NoSuchWorkflowJob(job_id))?;
    Ok((job, spec))
}

/// What a run's workflow says of its jobs.
fn specs(conn: &rusqlite::Connection, run_id: i64) -> Result<Vec<JobSpec>> {
    let run =
        queries::workflow_run_by_id(conn, run_id)?.ok_or(WorldError::NoSuchWorkflowRun(run_id))?;
    Ok(queries::workflow_by_id(conn, run.workflow_id)?
        .ok_or(WorldError::NoSuchWorkflow(run.workflow_id))?
        .jobs)
}

/// The steps of a job that is in progress, one of which is.
fn running_steps(job: &CheckRun) -> Result<Vec<Step>> {
    match &job.job {
        Some(held) if job.status == CheckStatus::InProgress => Ok(held.steps.clone()),
        _ => Err(WorldError::JobNotInProgress(job.id)),
    }
}

/// Finish the step in progress with `conclusion`, skip each step after it
/// that does not run after a failure when one has failed, and begin the
/// next that runs.
fn advance(steps: &mut [Step], spec: &JobSpec, conclusion: StepConclusion, clock: i64) {
    let Some(current) = steps
        .iter()
        .position(|s| s.status == StepStatus::InProgress)
    else {
        return;
    };
    steps[current].status = StepStatus::Completed;
    steps[current].conclusion = Some(conclusion);
    steps[current].completed_unix = Some(clock);
    let failed = steps
        .iter()
        .any(|s| s.conclusion == Some(StepConclusion::Failure));
    for (step, said) in steps.iter_mut().zip(&spec.steps).skip(current + 1) {
        step.started_unix = Some(clock);
        if failed && !said.always {
            step.status = StepStatus::Completed;
            step.conclusion = Some(StepConclusion::Skipped);
            step.completed_unix = Some(clock);
        } else {
            step.status = StepStatus::InProgress;
            return;
        }
    }
}

/// Store a job's steps after a step finished. A job with no step left in
/// progress finishes: its closing steps are listed, it concludes with a
/// failure when a step failed and a success otherwise, and the jobs that
/// were waiting for it are listed.
fn finish_or_store(
    w: &mut log::Write<'_>,
    job: &CheckRun,
    spec: &JobSpec,
    mut steps: Vec<Step>,
    clock: i64,
) -> Result<CheckRun> {
    if steps.iter().any(|s| s.status == StepStatus::InProgress) {
        return store_job(w, job, job.status, None, None, None, &steps);
    }
    let failed = steps
        .iter()
        .any(|s| s.conclusion == Some(StepConclusion::Failure));
    steps.extend(spec.closing_steps.iter().map(|name| Step {
        name: name.clone(),
        status: StepStatus::Completed,
        conclusion: Some(StepConclusion::Success),
        started_unix: Some(clock),
        completed_unix: Some(clock),
    }));
    let conclusion = match failed {
        true => CheckConclusion::Failure,
        false => CheckConclusion::Success,
    };
    store_job(
        w,
        job,
        CheckStatus::Completed,
        Some(conclusion),
        None,
        Some(clock),
        &steps,
    )
}

/// Write a job's state and steps as the write's operation, list the jobs a
/// finished job was holding back, and bring the suite to what its jobs come
/// to.
fn store_job(
    w: &mut log::Write<'_>,
    job: &CheckRun,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    started: Option<i64>,
    completed: Option<i64>,
    steps: &[Step],
) -> Result<CheckRun> {
    let tx = w.tx;
    let before = objects::check_run_object(tx, job.id)?;
    let stored = tx.query_row(
        &format!(
            "UPDATE check_runs SET
                 status = ?1,
                 conclusion = COALESCE(?2, conclusion),
                 started_clock_unix = COALESCE(?3, started_clock_unix),
                 completed_clock_unix = COALESCE(?4, completed_clock_unix),
                 steps = ?5
             WHERE id = ?6
             RETURNING {}",
            queries::CHECK_RUN_COLUMNS
        ),
        params![
            status.as_str(),
            conclusion.map(CheckConclusion::as_str),
            started,
            completed,
            serde_json::to_string(steps).expect("steps serialize"),
            job.id
        ],
        queries::row_to_check_run,
    )?;
    w.operation(EventBody::CheckRunUpdated(events::CheckRunChanged {
        subject: objects::check_run_subject(tx, job.id)?,
        changes: events::CheckRunChanges::between(&before, &objects::check_run_object(tx, job.id)?),
    }));
    if let (CheckStatus::Completed, Some(held)) = (status, &job.job) {
        list_ready_jobs(w, held.run_id)?;
    }
    suites::settle(w, job.suite_id)?;
    Ok(stored)
}

/// List each job of the run's current attempt that is not listed yet and
/// whose needs have all finished: queued when each succeeded or the job
/// runs regardless, and otherwise skipped. A job listed as skipped can
/// finish the needs of another, so this goes on until it lists nothing.
fn list_ready_jobs(w: &mut log::Write<'_>, run_id: i64) -> Result<()> {
    let tx = w.tx;
    let run =
        queries::workflow_run_by_id(tx, run_id)?.ok_or(WorldError::NoSuchWorkflowRun(run_id))?;
    let specs = specs(tx, run_id)?;
    let creator_id: i64 = tx.query_row(
        "SELECT creator_id FROM check_suites WHERE id = ?1",
        params![run.suite_id],
        |r| r.get(0),
    )?;
    loop {
        let listed: Vec<CheckRun> = queries::list_workflow_jobs(tx, run_id)?
            .into_iter()
            .filter(|job| job.job.as_ref().is_some_and(|j| j.attempt == run.attempt))
            .collect();
        let finished = |name: &String| {
            listed
                .iter()
                .find(|job| job.name == *name)
                .filter(|job| job.status == CheckStatus::Completed)
        };
        let ready: Vec<(&JobSpec, bool)> = specs
            .iter()
            .filter(|spec| !listed.iter().any(|job| job.name == spec.name))
            .filter(|spec| spec.needs.iter().all(|need| finished(need).is_some()))
            .map(|spec| {
                let runs = spec.always
                    || spec.needs.iter().all(|need| {
                        finished(need)
                            .is_some_and(|job| job.conclusion == Some(CheckConclusion::Success))
                    });
                (spec, runs)
            })
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let clock = queries::clock(tx)?;
        for (spec, runs) in ready {
            let (status, conclusion, at) = match runs {
                true => (CheckStatus::Queued, None, None),
                false => (
                    CheckStatus::Completed,
                    Some(CheckConclusion::Skipped),
                    Some(clock),
                ),
            };
            let id: i64 = tx.query_row(
                "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion, creator_id,
                                         started_clock_unix, completed_clock_unix,
                                         created_clock_unix, suite_id, workflow_run_id,
                                         run_attempt, steps)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9, ?10, ?11, '[]')
                 RETURNING id",
                params![
                    run.repo_id,
                    run.head_sha,
                    spec.name,
                    status.as_str(),
                    conclusion.map(CheckConclusion::as_str),
                    creator_id,
                    at,
                    clock,
                    run.suite_id,
                    run.id,
                    run.attempt
                ],
                |r| r.get(0),
            )?;
            w.emit(EventBody::CheckRunCreated(events::CheckRunCreated {
                subject: objects::check_run_subject(tx, id)?,
                object: objects::check_run_object(tx, id)?,
            }));
        }
    }
}
