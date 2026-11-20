//! Workflows, their runs, and the jobs of a run. The kernel does what a
//! workflow's orchestrator does: it lists a job once the jobs it needs have
//! finished, as queued or as skipped, and walks a job's steps as its runner
//! reports them. A job is a check run of the run's suite, so what a run has
//! come to is what its suite has.

use rusqlite::params;

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, log, objects, queries, suites};

pub struct NewWorkflow<'a> {
    pub repo_id: i64,
    pub path: &'a str,
    pub name: &'a str,
    pub jobs: &'a [JobSpec],
    /// The inputs a dispatch of it may give, or none for a workflow that
    /// cannot be dispatched.
    pub dispatch_inputs: Option<&'a [String]>,
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
                        "INSERT INTO workflows (repo_id, path, name, jobs, created_clock_unix,
                                                dispatch_inputs)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         RETURNING {}",
                        queries::WORKFLOW_COLUMNS
                    ),
                    params![
                        new.repo_id,
                        new.path,
                        new.name,
                        serde_json::to_string(new.jobs).expect("job specs serialize"),
                        queries::clock(tx)?,
                        new.dispatch_inputs
                            .map(|inputs| serde_json::to_string(inputs).expect("names serialize"))
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
            // The run reads its status through its suite, which is queued
            // now and still is once the first jobs are, so the row the
            // insert returns is the run as the write leaves it.
            let run = tx.query_row(
                &format!(
                    "INSERT INTO workflow_runs (repo_id, workflow_id, suite_id, run_number,
                                                event, head_branch, head_sha, title, actor_id,
                                                attempt, created_clock_unix, started_clock_unix,
                                                triggering_actor_id)
                     VALUES (?1, ?2, ?3,
                             (SELECT COALESCE(MAX(run_number), 0) + 1 FROM workflow_runs
                              WHERE workflow_id = ?2),
                             ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9, ?8)
                     RETURNING {}",
                    queries::WORKFLOW_RUN_COLUMNS
                ),
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
                queries::row_to_workflow_run,
            )?;
            w.operation(EventBody::WorkflowRunCreated(events::WorkflowRunCreated {
                subject: objects::workflow_run_subject(tx, run.id)?,
                object: objects::workflow_run_object(tx, run.id)?,
            }));
            list_ready_jobs(w, run.id)?;
            suites::settle(w, suite)?;
            Ok(run)
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

    /// Ask a run that has not finished to cancel, or cancel it by force.
    ///
    /// Asked, each queued job is cancelled, and each job in progress has
    /// its step in progress cancelled and goes on to its next step that
    /// runs regardless, finishing as cancelled when it has none. Jobs are
    /// then listed as they are for a run asked to cancel. By force, every
    /// unfinished job is cancelled with its steps left as they are.
    ///
    /// Asking again, or asking after a cancel by force, changes nothing.
    pub fn cancel_workflow_run(&self, run_id: i64, force: bool, by: Attribution) -> Result<()> {
        self.write(by, |w| {
            let tx = w.tx;
            let run = queries::workflow_run_by_id(tx, run_id)?
                .ok_or(WorldError::NoSuchWorkflowRun(run_id))?;
            if run.status == CheckStatus::Completed {
                return Err(WorldError::WorkflowRunCompleted(run_id));
            }
            let cancel = match force {
                true => RunCancel::Forced,
                false => RunCancel::Requested,
            };
            if run.cancel == Some(cancel) || run.cancel == Some(RunCancel::Forced) {
                return Ok(());
            }
            let before = objects::workflow_run_object(tx, run_id)?;
            tx.execute(
                "UPDATE workflow_runs SET cancel = ?2 WHERE id = ?1",
                params![run_id, cancel.as_str()],
            )?;
            w.operation(EventBody::WorkflowRunCancelled(
                events::WorkflowRunChanged {
                    subject: objects::workflow_run_subject(tx, run_id)?,
                    changes: events::WorkflowRunChanges::between(
                        &before,
                        &objects::workflow_run_object(tx, run_id)?,
                    ),
                },
            ));
            let specs = specs(tx, run_id)?;
            let clock = queries::clock(tx)?;
            let unfinished = queries::list_workflow_jobs(tx, run_id)?
                .into_iter()
                .filter(|job| job.job.as_ref().is_some_and(|j| j.attempt == run.attempt))
                .filter(|job| job.status != CheckStatus::Completed);
            for job in unfinished {
                let steps = job.job.as_ref().map_or(Vec::new(), |j| j.steps.clone());
                let spec = specs.iter().find(|spec| spec.name == job.name);
                match (job.status, spec) {
                    (CheckStatus::InProgress, Some(spec)) if !force => {
                        let mut steps = steps;
                        advance(&mut steps, spec, StepConclusion::Cancelled, clock);
                        write_steps(w, &job, spec, steps, clock, As::Consequence)?;
                    }
                    _ => {
                        write_job(
                            w,
                            &job,
                            CheckStatus::Completed,
                            Some(CheckConclusion::Cancelled),
                            None,
                            Some(clock),
                            &steps,
                            As::Consequence,
                        )?;
                    }
                }
            }
            list_ready_jobs(w, run_id)?;
            suites::settle(w, run.suite_id)
        })
    }

    /// Begin a new attempt of a run that has finished. The jobs `which`
    /// names run again, with every job that needs one of them, and each
    /// other job is listed in the new attempt as it finished in the last.
    /// The attempt it replaces is kept.
    pub fn rerun_workflow_run(
        &self,
        run_id: i64,
        which: Rerun,
        actor_id: i64,
        by: Attribution,
    ) -> Result<WorkflowRun> {
        self.write(by, |w| {
            let tx = w.tx;
            let run = queries::workflow_run_by_id(tx, run_id)?
                .ok_or(WorldError::NoSuchWorkflowRun(run_id))?;
            if run.status != CheckStatus::Completed {
                return Err(WorldError::WorkflowRunInProgress(run_id));
            }
            let specs = specs(tx, run_id)?;
            let last: Vec<CheckRun> = queries::list_workflow_jobs(tx, run_id)?
                .into_iter()
                .filter(|job| job.job.as_ref().is_some_and(|j| j.attempt == run.attempt))
                .collect();
            let mut again: Vec<&str> = match which {
                Rerun::All => specs.iter().map(|spec| spec.name.as_str()).collect(),
                Rerun::Failed => {
                    let failed: Vec<&str> = last
                        .iter()
                        .filter(|job| {
                            matches!(
                                job.conclusion,
                                Some(
                                    CheckConclusion::Failure
                                        | CheckConclusion::Cancelled
                                        | CheckConclusion::TimedOut
                                )
                            )
                        })
                        .map(|job| job.name.as_str())
                        .collect();
                    if failed.is_empty() {
                        return Err(WorldError::NothingToRerun(run_id));
                    }
                    failed
                }
                Rerun::Job(job_id) => {
                    let job = last
                        .iter()
                        .find(|job| job.id == job_id)
                        .ok_or(WorldError::NoSuchWorkflowJob(job_id))?;
                    vec![job.name.as_str()]
                }
            };
            // Every job that needs one that runs again runs again too.
            loop {
                let more: Vec<&str> = specs
                    .iter()
                    .filter(|spec| !again.contains(&spec.name.as_str()))
                    .filter(|spec| spec.needs.iter().any(|need| again.contains(&need.as_str())))
                    .map(|spec| spec.name.as_str())
                    .collect();
                if more.is_empty() {
                    break;
                }
                again.extend(more);
            }
            let before = objects::workflow_run_object(tx, run_id)?;
            let clock = queries::clock(tx)?;
            tx.execute(
                "INSERT INTO workflow_run_attempts (run_id, attempt, triggering_actor_id,
                                                    started_clock_unix)
                 SELECT id, attempt, COALESCE(triggering_actor_id, actor_id), started_clock_unix
                 FROM workflow_runs WHERE id = ?1",
                params![run_id],
            )?;
            tx.execute(
                "UPDATE workflow_runs
                 SET attempt = attempt + 1, started_clock_unix = ?2, triggering_actor_id = ?3,
                     cancel = NULL
                 WHERE id = ?1",
                params![run_id, clock, actor_id],
            )?;
            w.operation(EventBody::WorkflowRunRerun(events::WorkflowRunChanged {
                subject: objects::workflow_run_subject(tx, run_id)?,
                changes: events::WorkflowRunChanges::between(
                    &before,
                    &objects::workflow_run_object(tx, run_id)?,
                ),
            }));
            for job in last
                .iter()
                .filter(|job| !again.contains(&job.name.as_str()))
            {
                let id: i64 = tx.query_row(
                    "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion,
                                             creator_id, started_clock_unix,
                                             completed_clock_unix, created_clock_unix, suite_id,
                                             workflow_run_id, run_attempt, steps)
                     SELECT repo_id, head_sha, name, status, conclusion, creator_id,
                            started_clock_unix, completed_clock_unix, ?2, suite_id,
                            workflow_run_id, run_attempt + 1, steps
                     FROM check_runs WHERE id = ?1
                     RETURNING id",
                    params![job.id, clock],
                    |r| r.get(0),
                )?;
                w.emit(EventBody::CheckRunCreated(events::CheckRunCreated {
                    subject: objects::check_run_subject(tx, id)?,
                    object: objects::check_run_object(tx, id)?,
                }));
            }
            list_ready_jobs(w, run_id)?;
            suites::settle(w, run.suite_id)?;
            // The run reads its status through its suite, which the jobs
            // listed above have just brought to something else, so it is
            // read once they have.
            queries::workflow_run_by_id(tx, run_id)?.ok_or(WorldError::NoSuchWorkflowRun(run_id))
        })
    }

    /// A run as one of its attempts left it: who started that attempt and
    /// when, and what its jobs came to. The current attempt is the run as it
    /// is. `None` for an attempt the run has not had.
    pub fn workflow_run_at_attempt(
        &self,
        run_id: i64,
        attempt: i64,
    ) -> Result<Option<WorkflowRun>> {
        self.with_conn(|conn| {
            let Some(run) = queries::workflow_run_by_id(conn, run_id)? else {
                return Ok(None);
            };
            if attempt == run.attempt {
                return Ok(Some(run));
            }
            let Some(kept) = queries::workflow_run_attempt(conn, run_id, attempt)? else {
                return Ok(None);
            };
            let jobs: Vec<CheckRun> = queries::list_workflow_jobs(conn, run_id)?
                .into_iter()
                .filter(|job| job.job.as_ref().is_some_and(|j| j.attempt == attempt))
                .collect();
            let updated = jobs
                .iter()
                .filter_map(|job| job.completed_clock_unix)
                .max()
                .unwrap_or(kept.started_clock_unix);
            let (status, conclusion) = suites::rollup(jobs);
            Ok(Some(WorkflowRun {
                attempt,
                triggering_actor_login: kept.triggering_actor_login,
                started_clock_unix: kept.started_clock_unix,
                updated_clock_unix: updated,
                status,
                conclusion,
                cancel: None,
                ..run
            }))
        })
    }

    /// An attempt of a run that a re-run has replaced.
    pub fn workflow_run_attempt(
        &self,
        run_id: i64,
        attempt: i64,
    ) -> Result<Option<WorkflowRunAttempt>> {
        self.with_conn(|conn| queries::workflow_run_attempt(conn, run_id, attempt))
    }

    pub fn workflow_by_id(&self, id: i64) -> Result<Option<Workflow>> {
        self.with_conn(|conn| queries::workflow_by_id(conn, id))
    }

    /// A repository's workflows, oldest first.
    pub fn list_workflows_for_repo(&self, repo_id: i64) -> Result<Vec<Workflow>> {
        self.with_conn(|conn| queries::list_workflows_for_repo(conn, repo_id))
    }

    pub fn workflow_run_by_id(&self, id: i64) -> Result<Option<WorkflowRun>> {
        self.with_conn(|conn| queries::workflow_run_by_id(conn, id))
    }

    /// The run whose jobs a suite holds, for a suite that is a run's.
    pub fn workflow_run_by_suite(&self, suite_id: i64) -> Result<Option<WorkflowRun>> {
        self.with_conn(|conn| queries::workflow_run_by_suite(conn, suite_id))
    }

    /// A repository's workflow runs, newest first.
    pub fn list_workflow_runs_for_repo(&self, repo_id: i64) -> Result<Vec<WorkflowRun>> {
        self.with_conn(|conn| queries::list_workflow_runs_for_repo(conn, repo_id))
    }

    /// The jobs of a run in every attempt, oldest first.
    pub fn list_workflow_jobs(&self, run_id: i64) -> Result<Vec<CheckRun>> {
        self.with_conn(|conn| queries::list_workflow_jobs(conn, run_id))
    }
}

/// The bot whose check runs a run's jobs are, made if the world has no
/// actor of that login.
fn runner(w: &mut log::Write<'_>, login: &str) -> Result<i64> {
    match crate::apps::actor_by_login(w.tx, login)? {
        Some(held) if held.kind == ActorKind::Bot => Ok(held.id),
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
/// that does not run regardless when one has failed or been cancelled, and
/// begin the next that runs.
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
    let stopped = steps.iter().any(|s| {
        matches!(
            s.conclusion,
            Some(StepConclusion::Failure | StepConclusion::Cancelled)
        )
    });
    for (step, said) in steps.iter_mut().zip(&spec.steps).skip(current + 1) {
        step.started_unix = Some(clock);
        if stopped && !said.always {
            step.status = StepStatus::Completed;
            step.conclusion = Some(StepConclusion::Skipped);
            step.completed_unix = Some(clock);
        } else {
            step.status = StepStatus::InProgress;
            return;
        }
    }
}

/// Whether a write to a job is its write's operation or a consequence of
/// another.
#[derive(Clone, Copy, PartialEq, Eq)]
enum As {
    Operation,
    Consequence,
}

/// What a job's steps bring it to once none is in progress: its closing
/// steps listed, and its conclusion, which is `cancelled` when a step was,
/// a failure when one failed, and otherwise a success.
fn finished(spec: &JobSpec, mut steps: Vec<Step>, clock: i64) -> (Vec<Step>, CheckConclusion) {
    let any = |conclusion| steps.iter().any(|s| s.conclusion == Some(conclusion));
    let conclusion = if any(StepConclusion::Cancelled) {
        CheckConclusion::Cancelled
    } else if any(StepConclusion::Failure) {
        CheckConclusion::Failure
    } else {
        CheckConclusion::Success
    };
    steps.extend(spec.closing_steps.iter().map(|name| Step {
        name: name.clone(),
        status: StepStatus::Completed,
        conclusion: Some(StepConclusion::Success),
        started_unix: Some(clock),
        completed_unix: Some(clock),
    }));
    (steps, conclusion)
}

/// Store a job's steps after a step finished, as the write's operation. A
/// job with no step left in progress finishes, and the jobs that were
/// waiting for it are listed.
fn finish_or_store(
    w: &mut log::Write<'_>,
    job: &CheckRun,
    spec: &JobSpec,
    steps: Vec<Step>,
    clock: i64,
) -> Result<CheckRun> {
    let stored = write_steps(w, job, spec, steps, clock, As::Operation)?;
    if let (CheckStatus::Completed, Some(held)) = (stored.status, &job.job) {
        list_ready_jobs(w, held.run_id)?;
    }
    suites::settle(w, job.suite_id)?;
    Ok(stored)
}

/// Write a job's steps, and its finish when none is in progress.
fn write_steps(
    w: &mut log::Write<'_>,
    job: &CheckRun,
    spec: &JobSpec,
    steps: Vec<Step>,
    clock: i64,
    event: As,
) -> Result<CheckRun> {
    if steps.iter().any(|s| s.status == StepStatus::InProgress) {
        return write_job(w, job, job.status, None, None, None, &steps, event);
    }
    let (steps, conclusion) = finished(spec, steps, clock);
    write_job(
        w,
        job,
        CheckStatus::Completed,
        Some(conclusion),
        None,
        Some(clock),
        &steps,
        event,
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
    let stored = write_job(
        w,
        job,
        status,
        conclusion,
        started,
        completed,
        steps,
        As::Operation,
    )?;
    if let (CheckStatus::Completed, Some(held)) = (status, &job.job) {
        list_ready_jobs(w, held.run_id)?;
    }
    suites::settle(w, job.suite_id)?;
    Ok(stored)
}

/// Write a job's state and steps, with the event that says so.
#[allow(clippy::too_many_arguments)]
fn write_job(
    w: &mut log::Write<'_>,
    job: &CheckRun,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    started: Option<i64>,
    completed: Option<i64>,
    steps: &[Step],
    event: As,
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
    let body = EventBody::CheckRunUpdated(events::CheckRunChanged {
        subject: objects::check_run_subject(tx, job.id)?,
        changes: events::CheckRunChanges::between(&before, &objects::check_run_object(tx, job.id)?),
    });
    match event {
        As::Operation => w.operation(body),
        As::Consequence => w.emit(body),
    }
    Ok(stored)
}

/// List each job of the run's current attempt that is not listed yet and
/// whose needs have all finished: queued when each succeeded or the job
/// runs regardless, and otherwise skipped. A job listed as skipped can
/// finish the needs of another, so this goes on until it lists nothing. A
/// run asked to cancel queues only what runs regardless, and lists the rest
/// as cancelled. One cancelled by force lists nothing more.
fn list_ready_jobs(w: &mut log::Write<'_>, run_id: i64) -> Result<()> {
    let tx = w.tx;
    let run =
        queries::workflow_run_by_id(tx, run_id)?.ok_or(WorldError::NoSuchWorkflowRun(run_id))?;
    if run.cancel == Some(RunCancel::Forced) {
        return Ok(());
    }
    let unrun = match run.cancel {
        Some(_) => CheckConclusion::Cancelled,
        None => CheckConclusion::Skipped,
    };
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
                    || run.cancel.is_none()
                        && spec.needs.iter().all(|need| {
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
                false => (CheckStatus::Completed, Some(unrun), Some(clock)),
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
