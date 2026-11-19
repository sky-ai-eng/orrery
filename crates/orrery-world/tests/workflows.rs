//! Workflow runs: which jobs a run lists and when, how a job's steps go,
//! and what the run comes to.

use orrery_world::{
    ActorKind, Attribution, CheckConclusion, CheckRun, CheckStatus, Door, JobSpec, NewWorkflow,
    NewWorkflowRun, StepConclusion, StepSpec, StepStatus, World, WorldError, WorldSecret,
    check_jobs,
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
        s.w.list_workflow_runs(s.repo_id)
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
        },
        s.system,
    );
    assert!(matches!(again, Err(WorldError::WorkflowExists(_))));
}
