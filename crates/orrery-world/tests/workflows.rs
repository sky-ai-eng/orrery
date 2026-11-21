//! Workflow runs: which jobs a run lists and when, how a job's steps go,
//! and what the run comes to.

use orrery_world::{
    ActorKind, Attribution, CheckConclusion, CheckRun, CheckRunPatch, CheckStatus, Door, JobSpec,
    NewCheckRun, NewWorkflow, NewWorkflowRun, Rerun, RunCancel, StepConclusion, StepSpec,
    StepStatus, World, WorldError, WorldSecret, check_jobs,
};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn step(name: &str, always: bool) -> StepSpec {
    StepSpec {
        name: name.to_string(),
        always,
    }
}

fn job(name: &str, needs: &[&str], always: bool, steps: Vec<StepSpec>) -> JobSpec {
    JobSpec {
        name: name.to_string(),
        needs: needs.iter().map(|n| n.to_string()).collect(),
        always,
        labels: vec!["ubuntu-latest".to_string()],
        steps,
        closing_steps: vec!["Complete job".to_string()],
    }
}

/// A workflow like the one observed on GitHub: two jobs that need nothing,
/// one that needs both and runs whatever they came to, and one that needs
/// both and does not.
fn ci() -> Vec<JobSpec> {
    let steps = || {
        vec![
            step("Set up job", false),
            step("Run tests", false),
            step("Never reached on failure", false),
            step("Dump logs", true),
        ]
    };
    vec![
        job("test (a)", &[], false, steps()),
        job("test (b)", &[], false, steps()),
        job("report", &["test (a)", "test (b)"], true, steps()),
        job("build", &["test (a)", "test (b)"], false, steps()),
    ]
}

struct Seeded {
    _dir: tempfile::TempDir,
    w: World,
    system: Attribution,
    repo_id: i64,
    alice_id: i64,
}

fn seeded() -> Seeded {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(dir.path(), 1_700_000_000, &WorldSecret::LOCAL).unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let me = Attribution::new(alice.id, Door::Rest);
    let repo = w
        .create_repo(alice.id, "web", None, false, "main", me)
        .unwrap();
    Seeded {
        _dir: dir,
        w,
        system: Attribution::new(orrery_world::SYSTEM_ACTOR_ID, Door::Control),
        repo_id: repo.id,
        alice_id: alice.id,
    }
}

impl Seeded {
    fn workflow(&self, path: &str, jobs: &[JobSpec]) -> i64 {
        self.w
            .create_workflow(
                NewWorkflow {
                    repo_id: self.repo_id,
                    path,
                    name: "ci",
                    jobs,
                    dispatch_inputs: None,
                },
                self.system,
            )
            .unwrap()
            .id
    }

    fn start(&self, workflow_id: i64) -> orrery_world::WorkflowRun {
        self.w
            .start_workflow_run(
                NewWorkflowRun {
                    workflow_id,
                    event: "push",
                    head_sha: SHA,
                    head_branch: Some("main"),
                    title: "Add a thing",
                    actor_id: self.alice_id,
                    runner: "runner",
                },
                self.system,
            )
            .unwrap()
    }

    fn jobs(&self, run_id: i64) -> Vec<CheckRun> {
        self.w.list_workflow_jobs(run_id).unwrap()
    }

    /// The job of this name, which the run lists.
    fn job(&self, run_id: i64, name: &str) -> CheckRun {
        self.jobs(run_id)
            .into_iter()
            .find(|job| job.name == name)
            .unwrap_or_else(|| panic!("no job {name}"))
    }

    fn state(&self, run_id: i64) -> (CheckStatus, Option<CheckConclusion>) {
        let run = self.w.workflow_run_by_id(run_id).unwrap().unwrap();
        (run.status, run.conclusion)
    }
}

/// A job's steps as `(name, status, conclusion)`.
fn steps(job: &CheckRun) -> Vec<(&str, StepStatus, Option<StepConclusion>)> {
    job.job
        .as_ref()
        .expect("a job")
        .steps
        .iter()
        .map(|s| (s.name.as_str(), s.status, s.conclusion))
        .collect()
}

fn names(jobs: &[CheckRun]) -> Vec<(&str, CheckStatus, Option<CheckConclusion>)> {
    jobs.iter()
        .map(|job| (job.name.as_str(), job.status, job.conclusion))
        .collect()
}

#[test]
fn a_run_lists_each_job_when_its_needs_finish_and_ends_when_all_have() {
    let s = seeded();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));

    // The write answers the run a read does.
    assert_eq!(s.w.workflow_run_by_id(run.id).unwrap(), Some(run.clone()));

    // The run has a suite of its own, which the runner's bot made, and the
    // jobs that need nothing are queued check runs of it.
    assert_eq!((run.run_number, run.attempt), (1, 1));
    assert_eq!(s.state(run.id), (CheckStatus::Queued, None));
    let suite = s.w.check_suite_by_id(run.suite_id).unwrap().unwrap();
    assert_eq!(suite.creator_login, "runner");
    assert_eq!(suite.head_branch.as_deref(), Some("main"));
    assert_eq!(suite.pusher_login.as_deref(), Some("alice"));
    let listed = s.jobs(run.id);
    assert_eq!(
        names(&listed),
        [
            ("test (a)", CheckStatus::Queued, None),
            ("test (b)", CheckStatus::Queued, None),
        ]
    );
    assert!(listed.iter().all(|job| job.suite_id == run.suite_id));
    assert!(steps(&listed[0]).is_empty(), "a queued job lists no steps");

    // A started job lists its steps, the first in progress.
    let a = listed[0].id;
    let b = listed[1].id;
    let started = s.w.start_workflow_job(a, s.system).unwrap();
    assert_eq!(started.status, CheckStatus::InProgress);
    assert_eq!(
        steps(&started),
        [
            ("Set up job", StepStatus::InProgress, None),
            ("Run tests", StepStatus::Pending, None),
            ("Never reached on failure", StepStatus::Pending, None),
            ("Dump logs", StepStatus::Pending, None),
        ]
    );
    assert_eq!(s.state(run.id), (CheckStatus::InProgress, None));
    assert!(matches!(
        s.w.start_workflow_job(a, s.system),
        Err(WorldError::JobNotQueued(_))
    ));

    // A finishes with every step succeeding. B has not, so nothing that
    // needs both is listed, and the run waits for B, which is queued.
    let done = s.w.complete_workflow_job(a, true, s.system).unwrap();
    assert_eq!(done.conclusion, Some(CheckConclusion::Success));
    assert_eq!(steps(&done).len(), 5, "its closing step is listed");
    assert_eq!(s.jobs(run.id).len(), 2);
    assert_eq!(s.state(run.id), (CheckStatus::Queued, None));

    // B fails at its second step. The step after is skipped, and the one
    // that runs regardless begins.
    s.w.start_workflow_job(b, s.system).unwrap();
    s.w.complete_workflow_step(b, StepConclusion::Success, s.system)
        .unwrap();
    let failing =
        s.w.complete_workflow_step(b, StepConclusion::Failure, s.system)
            .unwrap();
    assert_eq!(
        steps(&failing),
        [
            (
                "Set up job",
                StepStatus::Completed,
                Some(StepConclusion::Success)
            ),
            (
                "Run tests",
                StepStatus::Completed,
                Some(StepConclusion::Failure)
            ),
            (
                "Never reached on failure",
                StepStatus::Completed,
                Some(StepConclusion::Skipped)
            ),
            ("Dump logs", StepStatus::InProgress, None),
        ]
    );
    assert_eq!(failing.status, CheckStatus::InProgress);

    // Its last step finishes the job, as a failure. The job that runs
    // regardless is queued and the other is listed as skipped, so the run
    // is queued again.
    let failed =
        s.w.complete_workflow_step(b, StepConclusion::Success, s.system)
            .unwrap();
    assert_eq!(failed.conclusion, Some(CheckConclusion::Failure));
    assert_eq!(
        steps(&failed).last(),
        Some(&(
            "Complete job",
            StepStatus::Completed,
            Some(StepConclusion::Success)
        ))
    );
    assert_eq!(
        names(&s.jobs(run.id))[2..],
        [
            ("report", CheckStatus::Queued, None),
            (
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped)
            ),
        ]
    );
    let skipped = s.job(run.id, "build");
    assert!(steps(&skipped).is_empty());
    assert_eq!(skipped.started_clock_unix, skipped.completed_clock_unix);
    assert_eq!(s.state(run.id), (CheckStatus::Queued, None));

    // The run ends when its last job does, with what its jobs came to.
    let report = s.job(run.id, "report").id;
    s.w.start_workflow_job(report, s.system).unwrap();
    assert_eq!(s.state(run.id), (CheckStatus::InProgress, None));
    s.w.complete_workflow_job(report, true, s.system).unwrap();
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Failure))
    );
    assert!(matches!(
        s.w.complete_workflow_job(report, true, s.system),
        Err(WorldError::JobNotInProgress(_))
    ));
}

#[test]
fn a_job_that_fails_outright_runs_the_steps_that_run_regardless() {
    let s = seeded();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));
    let a = s.job(run.id, "test (a)").id;
    s.w.start_workflow_job(a, s.system).unwrap();
    let failed = s.w.complete_workflow_job(a, false, s.system).unwrap();
    assert_eq!(failed.conclusion, Some(CheckConclusion::Failure));
    let concluded: Vec<_> = steps(&failed).into_iter().map(|(_, _, c)| c).collect();
    assert_eq!(
        concluded,
        [
            Some(StepConclusion::Failure),
            Some(StepConclusion::Skipped),
            Some(StepConclusion::Skipped),
            Some(StepConclusion::Success),
            Some(StepConclusion::Success),
        ]
    );
}

#[test]
fn a_skipped_job_skips_what_needs_it() {
    let s = seeded();
    let one = vec![step("Set up job", false)];
    let jobs = vec![
        job("first", &[], false, one.clone()),
        job("second", &["first"], false, one.clone()),
        job("third", &["second"], false, one.clone()),
        job("always", &["third"], true, one),
    ];
    let run = s.start(s.workflow(".github/workflows/chain.yml", &jobs));
    let first = s.job(run.id, "first").id;
    s.w.start_workflow_job(first, s.system).unwrap();
    s.w.complete_workflow_job(first, false, s.system).unwrap();
    assert_eq!(
        names(&s.jobs(run.id))[1..],
        [
            (
                "second",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped)
            ),
            (
                "third",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped)
            ),
            ("always", CheckStatus::Queued, None),
        ]
    );
}

#[test]
fn runs_of_a_workflow_are_numbered_and_each_has_its_own_suite() {
    let s = seeded();
    let ci_id = s.workflow(".github/workflows/ci.yml", &ci());
    let other = s.workflow(".github/workflows/other.yml", &ci());
    let first = s.start(ci_id);
    let second = s.start(ci_id);
    let elsewhere = s.start(other);
    assert_eq!(
        (first.run_number, second.run_number, elsewhere.run_number),
        (1, 2, 1)
    );
    assert_ne!(first.suite_id, second.suite_id);
    let listed: Vec<i64> =
        s.w.list_workflow_runs_for_repo(s.repo_id)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
    assert_eq!(listed, [elsewhere.id, second.id, first.id], "newest first");
    assert_eq!(
        s.w.workflow_run_by_suite(second.suite_id)
            .unwrap()
            .map(|r| r.id),
        Some(second.id)
    );

    // A check run the runner's bot makes through the checks API is in a
    // suite of its own, not a run's, and a job is not written that way.
    let bot = s.w.actor_by_login("runner").unwrap().unwrap();
    let as_bot = Attribution::new(bot.id, Door::Rest);
    let at = s.w.clock().unwrap();
    let plain =
        s.w.create_check_run(
            as_bot,
            NewCheckRun {
                repo_id: s.repo_id,
                head_sha: SHA,
                name: "extra",
                status: CheckStatus::Queued,
                conclusion: None,
                details_url: None,
                external_id: None,
                output: None,
                started_clock_unix: None,
                completed_clock_unix: None,
                annotations: &[],
            },
            at,
        )
        .unwrap();
    assert!(plain.job.is_none());
    assert!(![first.suite_id, second.suite_id, elsewhere.suite_id].contains(&plain.suite_id));
    let job = s.jobs(first.id)[0].id;
    let patch = CheckRunPatch {
        status: Some(CheckStatus::Completed),
        ..Default::default()
    };
    assert!(matches!(
        s.w.update_check_run(job, as_bot, patch, at),
        Err(WorldError::CheckRunIsJob(_))
    ));

    // The runner is one bot, made by the first run, and a login that is a
    // person's cannot be one.
    assert_eq!(
        s.w.actor_by_login("runner").unwrap().map(|a| a.kind),
        Some(ActorKind::Bot)
    );
    let as_alice = s.w.start_workflow_run(
        NewWorkflowRun {
            workflow_id: ci_id,
            event: "push",
            head_sha: SHA,
            head_branch: None,
            title: "t",
            actor_id: s.alice_id,
            runner: "alice",
        },
        s.system,
    );
    assert!(matches!(as_alice, Err(WorldError::RunnerNotABot(_))));
}

#[test]
fn a_workflow_whose_jobs_cannot_run_is_refused() {
    let s = seeded();
    let one = || vec![step("Set up job", false)];
    let fields = |jobs: &[JobSpec]| -> Vec<String> {
        check_jobs(jobs)
            .into_iter()
            .map(|(field, _)| field)
            .collect()
    };
    assert_eq!(fields(&[]), ["jobs"]);
    assert_eq!(
        fields(&[job("a", &[], false, one()), job("a", &[], false, one())]),
        ["jobs[1].name"]
    );
    assert_eq!(
        fields(&[job("a", &["a", "nope"], false, Vec::new())]),
        ["jobs[0].needs[0]", "jobs[0].needs[1]", "jobs[0].steps"]
    );
    let cycle = [
        job("a", &["b"], false, one()),
        job("b", &["a"], false, one()),
        job("c", &[], false, one()),
    ];
    assert_eq!(fields(&cycle), ["jobs[0].needs", "jobs[1].needs"]);
    let refused = s.w.create_workflow(
        NewWorkflow {
            repo_id: s.repo_id,
            path: ".github/workflows/ci.yml",
            name: "ci",
            jobs: &cycle,
            dispatch_inputs: None,
        },
        s.system,
    );
    assert!(matches!(refused, Err(WorldError::InvalidWorkflow { .. })));

    // A repository holds one workflow at a path.
    s.workflow(".github/workflows/ci.yml", &ci());
    let again = s.w.create_workflow(
        NewWorkflow {
            repo_id: s.repo_id,
            path: ".github/workflows/ci.yml",
            name: "again",
            jobs: &ci(),
            dispatch_inputs: None,
        },
        s.system,
    );
    assert!(matches!(again, Err(WorldError::WorkflowExists(_))));
}

/// The jobs of the run's current attempt.
fn current(s: &Seeded, run_id: i64) -> Vec<CheckRun> {
    let attempt = s.w.workflow_run_by_id(run_id).unwrap().unwrap().attempt;
    s.w.list_workflow_jobs_of_attempt(run_id, attempt).unwrap()
}

/// The types of the events the log holds after `seq`.
fn logged_after(s: &Seeded, seq: i64) -> Vec<&'static str> {
    s.w.log_after(seq, 10_000)
        .unwrap()
        .iter()
        .map(|event| event.body.type_name())
        .collect()
}

fn last_seq(s: &Seeded) -> i64 {
    s.w.log_after(0, 10_000).unwrap().last().unwrap().seq
}

fn current_job(s: &Seeded, run_id: i64, name: &str) -> CheckRun {
    current(s, run_id)
        .into_iter()
        .find(|job| job.name == name)
        .unwrap_or_else(|| panic!("no job {name} in the current attempt"))
}

#[test]
fn a_run_asked_to_cancel_stops_its_jobs_and_runs_what_runs_regardless() {
    let s = seeded();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));
    let a = s.job(run.id, "test (a)").id;
    let b = s.job(run.id, "test (b)").id;
    s.w.start_workflow_job(a, s.system).unwrap();
    s.w.complete_workflow_step(a, StepConclusion::Success, s.system)
        .unwrap();

    s.w.cancel_workflow_run(run.id, false, s.system).unwrap();
    let held = s.w.workflow_run_by_id(run.id).unwrap().unwrap();
    assert_eq!(held.cancel, Some(RunCancel::Requested));
    // The queued job is cancelled where it stood.
    let queued = s.job(run.id, "test (b)");
    assert_eq!(
        (queued.id, queued.status, queued.conclusion),
        (b, CheckStatus::Completed, Some(CheckConclusion::Cancelled))
    );
    assert!(steps(&queued).is_empty());
    // The job in progress has its step cancelled and the next skipped, and
    // goes on to the step that runs regardless.
    let running = s.job(run.id, "test (a)");
    assert_eq!(running.status, CheckStatus::InProgress);
    assert_eq!(
        steps(&running),
        [
            (
                "Set up job",
                StepStatus::Completed,
                Some(StepConclusion::Success)
            ),
            (
                "Run tests",
                StepStatus::Completed,
                Some(StepConclusion::Cancelled)
            ),
            (
                "Never reached on failure",
                StepStatus::Completed,
                Some(StepConclusion::Skipped)
            ),
            ("Dump logs", StepStatus::InProgress, None),
        ]
    );
    assert_eq!(s.jobs(run.id).len(), 2);
    assert_eq!(s.state(run.id), (CheckStatus::InProgress, None));

    // Asking again changes nothing.
    let events = s.w.log_after(0, 10_000).unwrap().len();
    s.w.cancel_workflow_run(run.id, false, s.system).unwrap();
    assert_eq!(s.w.log_after(0, 10_000).unwrap().len(), events);

    // The job finishes as cancelled. The job that runs regardless is
    // queued, and the one that does not is listed as cancelled, not
    // skipped.
    let done =
        s.w.complete_workflow_step(a, StepConclusion::Success, s.system)
            .unwrap();
    assert_eq!(done.conclusion, Some(CheckConclusion::Cancelled));
    assert_eq!(
        names(&s.jobs(run.id))[2..],
        [
            ("report", CheckStatus::Queued, None),
            (
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Cancelled)
            ),
        ]
    );
    let report = s.job(run.id, "report").id;
    s.w.start_workflow_job(report, s.system).unwrap();
    s.w.complete_workflow_job(report, true, s.system).unwrap();
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Cancelled))
    );
    assert!(matches!(
        s.w.cancel_workflow_run(run.id, false, s.system),
        Err(WorldError::WorkflowRunCompleted(_))
    ));
}

/// What GitHub was observed to do with a run asked to cancel while a job
/// that runs regardless was queued, another was in progress, and a third
/// job was at a step that runs regardless.
#[test]
fn a_run_asked_to_cancel_leaves_what_runs_regardless_running() {
    let s = seeded();
    let work = || vec![step("Set up job", false), step("Work", false)];
    let jobs = [
        job("plain", &[], false, work()),
        job("hold", &[], true, work()),
        job("waiting", &[], true, work()),
        job(
            "steps",
            &[],
            false,
            vec![
                step("Set up job", false),
                step("Regardless", true),
                step("After", false),
                step("After regardless", true),
            ],
        ),
    ];
    let run = s.start(s.workflow(".github/workflows/cancel.yml", &jobs));
    let id = |name: &str| s.job(run.id, name).id;
    for name in ["plain", "hold", "steps"] {
        s.w.start_workflow_job(id(name), s.system).unwrap();
        s.w.complete_workflow_step(id(name), StepConclusion::Success, s.system)
            .unwrap();
    }
    let before = |name: &str| s.job(run.id, name);
    let (hold, steps_job) = (before("hold"), before("steps"));

    s.w.cancel_workflow_run(run.id, false, s.system).unwrap();
    // The job that does not run regardless is cancelled at its step.
    let plain = s.job(run.id, "plain");
    assert_eq!(plain.conclusion, Some(CheckConclusion::Cancelled));
    assert_eq!(
        steps(&plain)[1..],
        [
            (
                "Work",
                StepStatus::Completed,
                Some(StepConclusion::Cancelled)
            ),
            (
                "Complete job",
                StepStatus::Completed,
                Some(StepConclusion::Success)
            ),
        ]
    );
    // The jobs that run regardless are as they were, queued or in progress,
    // and so is the job whose step in progress runs regardless.
    assert_eq!(s.job(run.id, "hold"), hold);
    assert_eq!(s.job(run.id, "waiting").status, CheckStatus::Queued);
    assert_eq!(s.job(run.id, "steps"), steps_job);

    // That step finishes as it would have. The job is cancelled from the
    // step after it: one skipped, one that runs regardless run.
    s.w.complete_workflow_step(id("steps"), StepConclusion::Success, s.system)
        .unwrap();
    let done =
        s.w.complete_workflow_step(id("steps"), StepConclusion::Success, s.system)
            .unwrap();
    assert_eq!(done.conclusion, Some(CheckConclusion::Cancelled));
    assert_eq!(
        steps(&done)
            .iter()
            .map(|(name, _, conclusion)| (*name, conclusion.unwrap()))
            .collect::<Vec<_>>(),
        [
            ("Set up job", StepConclusion::Success),
            ("Regardless", StepConclusion::Success),
            ("After", StepConclusion::Skipped),
            ("After regardless", StepConclusion::Success),
            ("Complete job", StepConclusion::Success),
        ]
    );
    // A job that runs regardless runs every step, and succeeds.
    let held =
        s.w.complete_workflow_step(id("hold"), StepConclusion::Success, s.system)
            .unwrap();
    assert_eq!(held.conclusion, Some(CheckConclusion::Success));
    // The run waits for the job still queued, which a cancel by force ends.
    assert_eq!(s.state(run.id), (CheckStatus::Queued, None));
    s.w.cancel_workflow_run(run.id, true, s.system).unwrap();
    assert_eq!(
        s.job(run.id, "waiting").conclusion,
        Some(CheckConclusion::Cancelled)
    );
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Cancelled))
    );
}

#[test]
fn a_run_cancelled_by_force_ends_where_it_stands() {
    let s = seeded();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));
    let a = s.job(run.id, "test (a)").id;
    s.w.start_workflow_job(a, s.system).unwrap();
    // Asked first, then forced: the force is what holds.
    s.w.cancel_workflow_run(run.id, false, s.system).unwrap();
    s.w.cancel_workflow_run(run.id, true, s.system).unwrap();
    assert_eq!(
        s.w.workflow_run_by_id(run.id).unwrap().unwrap().cancel,
        Some(RunCancel::Forced)
    );
    let frozen = s.job(run.id, "test (a)");
    assert_eq!(frozen.conclusion, Some(CheckConclusion::Cancelled));
    // Its steps are as the cancel left them: the one that runs regardless
    // was in progress, and is still listed so.
    assert_eq!(
        steps(&frozen).last(),
        Some(&("Dump logs", StepStatus::InProgress, None))
    );
    assert_eq!(
        s.jobs(run.id).len(),
        2,
        "no job is listed after a cancel by force"
    );
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Cancelled))
    );
}

#[test]
fn a_rerun_is_a_new_attempt_that_copies_what_it_does_not_run_again() {
    let s = seeded();
    let (bob, _) = s.w.create_actor("bob", ActorKind::User).unwrap();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));
    assert!(matches!(
        s.w.rerun_workflow_run(run.id, Rerun::All, bob.id, s.system),
        Err(WorldError::WorkflowRunInProgress(_))
    ));
    // Attempt 1: (a) succeeds, (b) fails, the report runs, the build is
    // skipped.
    let finish = |name: &str, succeeded: bool| {
        let id = current_job(&s, run.id, name).id;
        s.w.start_workflow_job(id, s.system).unwrap();
        s.w.complete_workflow_job(id, succeeded, s.system).unwrap()
    };
    let first_a = finish("test (a)", true);
    finish("test (b)", false);
    finish("report", true);
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Failure))
    );

    // Re-running what failed: (b) and the two jobs that need it run again,
    // and (a) is copied as it finished, under a new id.
    s.w.advance_clock(60, s.system).unwrap();
    let seq = last_seq(&s);
    let again =
        s.w.rerun_workflow_run(run.id, Rerun::Failed, bob.id, s.system)
            .unwrap();
    assert_eq!(s.w.workflow_run_by_id(run.id).unwrap(), Some(again.clone()));
    assert_eq!((again.id, again.attempt), (run.id, 2));
    assert_eq!(again.triggering_actor_login, "bob");
    assert_eq!(again.actor_login, "alice");
    assert_eq!(again.started_clock_unix, run.started_clock_unix + 60);
    assert_eq!(again.created_clock_unix, run.created_clock_unix);
    assert_eq!(again.suite_id, run.suite_id);
    assert_eq!(
        (again.status, again.conclusion),
        (CheckStatus::Queued, None)
    );
    let kept = s.w.workflow_run_attempt(run.id, 1).unwrap().unwrap();
    assert_eq!(kept.triggering_actor_login, "alice");
    assert_eq!(kept.started_clock_unix, run.started_clock_unix);
    assert_eq!(s.w.workflow_run_attempt(run.id, 2).unwrap(), None);
    // The job that runs is listed before the one carried over, and each
    // is logged as what it is.
    assert_eq!(
        names(&current(&s, run.id)),
        [
            ("test (b)", CheckStatus::Queued, None),
            (
                "test (a)",
                CheckStatus::Completed,
                Some(CheckConclusion::Success)
            ),
        ]
    );
    assert_eq!(
        logged_after(&s, seq),
        [
            "workflow_run:rerun",
            "check_suite:updated",
            "check_run:created",
            "check_run:carried",
        ]
    );
    let copy = current_job(&s, run.id, "test (a)");
    assert_ne!(copy.id, first_a.id);
    assert_eq!(copy.job.as_ref().unwrap().steps, first_a.job.unwrap().steps);
    assert_eq!(copy.started_clock_unix, first_a.started_clock_unix);
    assert_eq!(copy.completed_clock_unix, first_a.completed_clock_unix);
    assert_eq!(copy.created_clock_unix, first_a.created_clock_unix + 60);
    assert_eq!(s.jobs(run.id).len(), 6, "every attempt's jobs are kept");

    // This time (b) succeeds, so both jobs that need it run.
    finish("test (b)", true);
    finish("report", true);
    let build = finish("build", true);
    assert_eq!(
        s.state(run.id),
        (CheckStatus::Completed, Some(CheckConclusion::Success))
    );
    assert!(matches!(
        s.w.rerun_workflow_run(run.id, Rerun::Failed, bob.id, s.system),
        Err(WorldError::NothingToRerun(_))
    ));

    // Re-running one job runs it and what needs it, which here is nothing.
    let third =
        s.w.rerun_workflow_run(run.id, Rerun::Job(build.id), bob.id, s.system)
            .unwrap();
    assert_eq!(third.attempt, 3);
    let listed = current(&s, run.id);
    assert_eq!(listed.len(), 4);
    assert_eq!(
        listed
            .iter()
            .filter(|job| job.status == CheckStatus::Queued)
            .map(|job| job.name.as_str())
            .collect::<Vec<_>>(),
        ["build"]
    );
    assert!(matches!(
        s.w.rerun_workflow_run(run.id, Rerun::Job(build.id), bob.id, s.system),
        Err(WorldError::WorkflowRunInProgress(_))
    ));
    finish("build", true);
    // A job of an earlier attempt cannot be re-run, and a check run that
    // is no job of the run is not found.
    assert!(matches!(
        s.w.rerun_workflow_run(run.id, Rerun::Job(build.id), bob.id, s.system),
        Err(WorldError::JobOfEarlierAttempt(_))
    ));
    assert!(matches!(
        s.w.rerun_workflow_run(run.id, Rerun::Job(i64::MAX), bob.id, s.system),
        Err(WorldError::NoSuchWorkflowJob(_))
    ));

    // Re-running all of it copies nothing.
    let fourth =
        s.w.rerun_workflow_run(run.id, Rerun::All, bob.id, s.system)
            .unwrap();
    assert_eq!(fourth.attempt, 4);
    assert_eq!(
        names(&current(&s, run.id)),
        [
            ("test (a)", CheckStatus::Queued, None),
            ("test (b)", CheckStatus::Queued, None),
        ]
    );
}

/// A re-run can list every job already finished: the one job it runs again
/// needs a job that failed. The attempt still begins and ends, so the suite
/// is brought back to queued and to its conclusion in the one write, and a
/// job the attempt before never listed is listed as its needs have it.
#[test]
fn a_rerun_that_runs_nothing_still_begins_and_ends() {
    let s = seeded();
    let run = s.start(s.workflow(".github/workflows/ci.yml", &ci()));
    let finish = |name: &str, succeeded: bool| {
        let id = current_job(&s, run.id, name).id;
        s.w.start_workflow_job(id, s.system).unwrap();
        s.w.complete_workflow_job(id, succeeded, s.system).unwrap()
    };
    finish("test (a)", true);
    finish("test (b)", false);
    finish("report", true);
    let build = current_job(&s, run.id, "build");
    assert_eq!(build.conclusion, Some(CheckConclusion::Skipped));
    let ended = s.w.workflow_run_by_id(run.id).unwrap().unwrap();

    s.w.advance_clock(60, s.system).unwrap();
    let seq = last_seq(&s);
    let again =
        s.w.rerun_workflow_run(run.id, Rerun::Job(build.id), s.alice_id, s.system)
            .unwrap();
    assert_eq!(again.attempt, 2);
    assert_eq!(
        (again.status, again.conclusion),
        (CheckStatus::Completed, Some(CheckConclusion::Failure))
    );
    assert_eq!(again.updated_clock_unix, ended.updated_clock_unix + 60);
    assert_eq!(
        logged_after(&s, seq),
        [
            "workflow_run:rerun",
            "check_suite:updated",
            "check_run:carried",
            "check_run:carried",
            "check_run:carried",
            "check_run:created",
            "check_suite:updated",
        ]
    );
    assert_eq!(
        current_job(&s, run.id, "build").conclusion,
        Some(CheckConclusion::Skipped)
    );

    // After a cancel by force the report was never listed. Re-running one
    // test lists it once the tests it needs have finished.
    let forced = s.start(s.workflow(".github/workflows/again.yml", &ci()));
    s.w.cancel_workflow_run(forced.id, true, s.system).unwrap();
    assert_eq!(current(&s, forced.id).len(), 2);
    let a = current_job(&s, forced.id, "test (a)").id;
    s.w.rerun_workflow_run(forced.id, Rerun::Job(a), s.alice_id, s.system)
        .unwrap();
    assert_eq!(
        names(&current(&s, forced.id)),
        [
            ("test (a)", CheckStatus::Queued, None),
            (
                "test (b)",
                CheckStatus::Completed,
                Some(CheckConclusion::Cancelled)
            ),
        ]
    );
    let id = current_job(&s, forced.id, "test (a)").id;
    s.w.start_workflow_job(id, s.system).unwrap();
    s.w.complete_workflow_job(id, true, s.system).unwrap();
    assert_eq!(
        names(&current(&s, forced.id))[2..],
        [
            ("report", CheckStatus::Queued, None),
            (
                "build",
                CheckStatus::Completed,
                Some(CheckConclusion::Skipped)
            ),
        ]
    );
}

/// A job's log as `(step, text)`, which was not deleted.
fn log_of(s: &Seeded, job_id: i64) -> Vec<(i64, String)> {
    s.w.job_log(job_id)
        .unwrap()
        .expect("the log was not deleted")
        .into_iter()
        .map(|line| (line.step, line.text))
        .collect()
}

#[test]
fn a_runner_writes_a_jobs_log_and_a_run_and_its_logs_are_deleted() {
    let s = seeded();
    let workflow = s.workflow(".github/workflows/ci.yml", &ci());
    let run = s.start(workflow);
    let lines = |texts: &[&str]| texts.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let a = s.job(run.id, "test (a)").id;
    // A job no runner has taken has no step to write in.
    assert!(matches!(
        s.w.append_job_log(a, &lines(&["early"]), s.system),
        Err(WorldError::JobNotInProgress(_))
    ));
    s.w.start_workflow_job(a, s.system).unwrap();
    s.w.append_job_log(a, &lines(&["one"]), s.system).unwrap();
    s.w.complete_workflow_step(a, StepConclusion::Success, s.system)
        .unwrap();
    s.w.append_job_log(a, &lines(&["two", "three"]), s.system)
        .unwrap();
    s.w.complete_workflow_job(a, true, s.system).unwrap();
    let written = [
        (1, "one".to_string()),
        (2, "two".to_string()),
        (2, "three".to_string()),
    ];
    assert_eq!(log_of(&s, a), written);
    assert!(matches!(
        s.w.append_job_log(a, &lines(&["late"]), s.system),
        Err(WorldError::JobNotInProgress(_))
    ));

    let finish = |name: &str, succeeded: bool| {
        let id = current_job(&s, run.id, name).id;
        s.w.start_workflow_job(id, s.system).unwrap();
        s.w.append_job_log(id, &[format!("{name} ran")], s.system)
            .unwrap();
        s.w.complete_workflow_job(id, succeeded, s.system).unwrap()
    };
    finish("test (b)", false);
    finish("report", true);

    // A job a re-run carries over names the job that ran, and has its log.
    s.w.rerun_workflow_run(run.id, Rerun::Failed, s.alice_id, s.system)
        .unwrap();
    let copy = current_job(&s, run.id, "test (a)");
    assert_eq!(copy.job.as_ref().unwrap().carried_from, Some(a));
    assert_eq!(log_of(&s, copy.id), written);
    assert_eq!(s.job(run.id, "test (a)").job.unwrap().carried_from, None);

    // Neither the logs nor the run can be deleted while the run runs.
    for refused in [
        s.w.delete_workflow_run_logs(run.id, s.system),
        s.w.delete_workflow_run(run.id, s.system),
    ] {
        assert!(matches!(refused, Err(WorldError::WorkflowRunInProgress(_))));
    }
    finish("test (b)", true);
    finish("report", true);
    finish("build", true);

    // Deleting the logs deletes every attempt's, and moves the run's time.
    let ended = s.w.workflow_run_by_id(run.id).unwrap().unwrap();
    assert_eq!(ended.logs_deleted_attempt, None);
    s.w.advance_clock(60, s.system).unwrap();
    let seq = last_seq(&s);
    s.w.delete_workflow_run_logs(run.id, s.system).unwrap();
    let after = s.w.workflow_run_by_id(run.id).unwrap().unwrap();
    assert_eq!(after.logs_deleted_attempt, Some(2));
    assert_eq!(after.updated_clock_unix, ended.updated_clock_unix + 60);
    assert_eq!(
        (after.status, after.conclusion),
        (ended.status, ended.conclusion)
    );
    for gone in [a, copy.id] {
        assert_eq!(s.w.job_log(gone).unwrap(), None);
        assert!(s.w.job_log_deleted(gone).unwrap());
    }
    let logged = logged_after(&s, seq);
    assert_eq!(logged[0], "workflow_run:logs_deleted");
    // One for each job that had lines of its own: three in the first
    // attempt, and three that ran in the second.
    assert_eq!(logged[1..], ["check_run:log_deleted"; 6]);
    let seq = last_seq(&s);
    s.w.delete_workflow_run_logs(run.id, s.system).unwrap();
    assert!(logged_after(&s, seq).is_empty(), "deleted already");

    // A later attempt has logs again, and the mark stays on the attempts
    // before it. The job it runs has a log. A job it carries over has the
    // log of the job that ran, which was deleted, and a job carried twice
    // still names the job that ran.
    let build = current_job(&s, run.id, "build").id;
    s.w.rerun_workflow_run(run.id, Rerun::Job(build), s.alice_id, s.system)
        .unwrap();
    let third = finish("build", true);
    assert_eq!(log_of(&s, third.id), [(1, "build ran".to_string())]);
    assert!(!s.w.job_log_deleted(third.id).unwrap());
    let twice = current_job(&s, run.id, "test (a)");
    assert_eq!(twice.job.as_ref().unwrap().carried_from, Some(a));
    assert_eq!(s.w.job_log(twice.id).unwrap(), None);
    assert!(s.w.job_log_deleted(twice.id).unwrap());
    let again = s.w.workflow_run_by_id(run.id).unwrap().unwrap();
    assert_eq!((again.attempt, again.logs_deleted_attempt), (3, Some(2)));

    // Deleting the run deletes its jobs of every attempt, their logs, and
    // its suite.
    let newest_job = s.jobs(run.id).iter().map(|job| job.id).max().unwrap();
    let seq = last_seq(&s);
    s.w.delete_workflow_run(run.id, s.system).unwrap();
    assert_eq!(s.w.workflow_run_by_id(run.id).unwrap(), None);
    assert!(s.jobs(run.id).is_empty());
    assert_eq!(s.w.check_run_by_id(a).unwrap(), None);
    assert_eq!(s.w.check_suite_by_id(run.suite_id).unwrap(), None);
    assert_eq!(s.w.workflow_run_attempt(run.id, 1).unwrap(), None);
    let logged = logged_after(&s, seq);
    assert_eq!(logged[0], "workflow_run:deleted");
    assert_eq!(logged[logged.len() - 1], "check_suite:deleted");
    assert_eq!(logged[1..logged.len() - 1], ["check_run:deleted"; 12]);
    assert!(matches!(
        s.w.delete_workflow_run(run.id, s.system),
        Err(WorldError::NoSuchWorkflowRun(_))
    ));

    // What was deleted is named by nothing later: the workflow's next run
    // takes the next number, not the deleted run's, and its jobs take ids
    // past the deleted jobs'.
    assert_eq!(run.run_number, 1);
    let next = s.start(workflow);
    assert_eq!(next.run_number, 2);
    assert!(next.id > run.id);
    assert!(s.jobs(next.id).iter().all(|job| job.id > newest_job));
    s.w.cancel_workflow_run(next.id, true, s.system).unwrap();
    s.w.delete_workflow_run(next.id, s.system).unwrap();
    assert_eq!(s.start(workflow).run_number, 3);
}
