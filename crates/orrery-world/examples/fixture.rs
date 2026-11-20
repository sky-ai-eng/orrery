//! Builds the world-database fixture for this binary's schema version, at
//! `tests/fixtures/v<version>.db`. When to run it, and why a committed
//! fixture is never rebuilt, is in `tests/fixtures/README.md`.
//!
//! ```text
//! cargo run -p orrery-world --example fixture
//! ```
//!
//! The world puts rows in every table, so each migration has data to carry
//! and the fixture test notices any it loses. A table added to the schema
//! gets rows here in the same change.

use std::path::Path;

use orrery_world::{
    AccessLevel, ActorEmail, ActorKind, Attribution, CheckConclusion, CheckRunPatch, CheckStatus,
    CommitFacts, CommitSignature, DEFAULT_T0_UNIX, Door, JobSpec, NewApp, NewCheckRun,
    NewCommitStatus, NewPull, NewWorkflow, NewWorkflowRun, OrgRole, Permissions, RefChange,
    RefMove, Rerun, ReviewState, SCHEMA_VERSION, SYSTEM_ACTOR_LOGIN, StatusState, StepConclusion,
    StepSpec, TeamRole, World, WorldSecret,
};

/// A write by `actor_id` as the REST face makes it.
fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
const BASE_SHA: &str = "1111111111111111111111111111111111111111";
const HEAD_SHA: &str = "2222222222222222222222222222222222222222";
const MERGE_SHA: &str = "3333333333333333333333333333333333333333";
const CALLBACK: &str = "https://auth.example.test/callback";
const SIGNING_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2wv3 alice@laptop";
const RETIRED_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICXNiiZ0NcLB2s0gYXpNwyFmtzd28Zxng4mIlVSzlRIv";

const APP_KEY: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAyQDurX+SGDbR9X9O5rwY
XND5rbR9L5MHPsh5fy9PCvnjEtf9vd+gbv+MKb5isMFXGXS2Jwc/zZ4mitvK/TUB
FZD5K4Q53vIVpt54aWggRB9a3KaePpA8hMrLDklc2JaVcUtOqR2U8okAqQhagx+V
Hkjnfr2FKrRaIcA/vRvx/o+9yqSP/6TP3Kp5kMpxMBuCIPs3kF2WzxnvSrw+91Qp
CljNv66agiv3iPnNWyeaOe5C/0YFg3PRLVJq03K/LT4cHaUGM/tSFJLGj9BwyxHT
67koh9CtImSoshN5v/41kPYAuxUYzKXmUr9JGENNAIaUkrl7Gr3CzFnUIFfuCpd9
uQIDAQAB
-----END PUBLIC KEY-----
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("v{SCHEMA_VERSION}.db"));
    if dest.exists() {
        return Err(format!(
            "{} already exists; a committed fixture is never rebuilt",
            dest.display()
        )
        .into());
    }
    let dir = tempfile::tempdir()?;
    let world = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )?;
    populate(&world)?;
    // Quiesced, the database file alone is the whole world state.
    world.quiesce(|| std::fs::copy(world.root().join("world.db"), &dest))??;
    println!("wrote {}", dest.display());
    Ok(())
}

fn populate(w: &World) -> orrery_world::Result<()> {
    let system = w
        .actor_by_login(SYSTEM_ACTOR_LOGIN)?
        .expect("every world has the system actor");
    let (alice, _) = w.create_actor("alice", ActorKind::User)?;
    w.issue_actor_token("alice", by(system.id))?;
    let (bob, _) = w.create_actor("bob", ActorKind::User)?;
    let (carol, _) = w.create_actor("carol", ActorKind::User)?;
    let (ci, _) = w.create_actor("ci-bot", ActorKind::Bot)?;
    let (acme, _) = w.create_actor("acme", ActorKind::Org)?;
    w.set_actor_emails(
        "alice",
        &[
            ActorEmail {
                email: "alice@acme.test".to_string(),
                primary: true,
                verified: true,
            },
            ActorEmail {
                email: "alice@old.test".to_string(),
                primary: false,
                verified: false,
            },
        ],
        by(alice.id),
    )?;

    w.set_org_membership("acme", "alice", OrgRole::Admin, by(system.id))?;
    w.set_org_membership("acme", "bob", OrgRole::Member, by(alice.id))?;
    w.set_org_membership("acme", "carol", OrgRole::Member, by(alice.id))?;
    w.create_team(
        "acme",
        "Platform",
        Some("keeps the lights on"),
        by(alice.id),
    )?;
    w.set_team_membership(
        "acme",
        "platform",
        "bob",
        TeamRole::Maintainer,
        by(alice.id),
    )?;
    w.set_team_membership("acme", "platform", "carol", TeamRole::Member, by(alice.id))?;
    // Deleted after a review is requested from it, so a timeline row names
    // a team that no longer exists.
    w.create_team("acme", "Interim", None, by(alice.id))?;

    let repo = w.create_repo(
        acme.id,
        "webapp",
        Some("the app"),
        false,
        "main",
        by(alice.id),
    )?;
    let first_push = RefMove {
        name: "refs/heads/main",
        old: ZERO_SHA,
        new: BASE_SHA,
        repins: Vec::new(),
        change: RefChange::Created,
        forced: false,
        commits: Vec::new(),
        commit_count: 0,
    };
    // A key alice signs with, and one she registered and deleted, so the
    // first push's commit has a stored verdict and a deleted key's id is
    // retired.
    w.add_signing_key(by(alice.id), SIGNING_KEY, Some("laptop"))?;
    let retired = w.add_signing_key(by(alice.id), RETIRED_KEY, None)?;
    w.delete_signing_key(by(alice.id), retired.id)?;
    let signed = CommitFacts {
        sha: BASE_SHA.to_string(),
        committer_email: "alice@acme.test".to_string(),
        signature: CommitSignature::Ssh {
            key: orrery_world::ssh_public_key(SIGNING_KEY).expect("the key parses"),
            verifies: true,
        },
    };
    w.record_push(repo.id, by(alice.id), &[first_push], None, &[signed])?;
    w.advance_clock(3600, by(system.id))?;

    let merged = w.create_pull(
        by(alice.id),
        NewPull {
            repo_id: repo.id,
            title: "Add the feature",
            body: Some("Closes nothing; it is a fixture."),
            head_ref: "feature",
            head_sha: HEAD_SHA,
            base_ref: "main",
            base_sha: BASE_SHA,
            draft: true,
        },
    )?;
    w.set_pull_draft(merged.id, by(alice.id), false)?;
    w.add_pull_labels(merged.id, by(alice.id), &["bug", "triage"])?;
    w.remove_pull_label(merged.id, by(alice.id), "triage")?;
    w.request_reviewers(
        merged.id,
        by(alice.id),
        &["bob", "carol"],
        &["platform", "interim"],
    )?;
    w.remove_review_requests(merged.id, by(alice.id), &["carol"], &[])?;
    w.delete_team("acme", "interim", by(alice.id))?;
    w.create_review(
        merged.id,
        by(bob.id),
        ReviewState::ChangesRequested,
        Some("one nit"),
        None,
    )?;
    w.create_issue_comment(merged.id, by(carol.id), "Looks close.")?;
    w.advance_clock(600, by(system.id))?;
    w.create_review(merged.id, by(bob.id), ReviewState::Approved, None, None)?;
    let check = w.create_check_run(
        by(ci.id),
        NewCheckRun {
            repo_id: repo.id,
            head_sha: HEAD_SHA,
            name: "build",
            status: CheckStatus::InProgress,
            conclusion: None,
            details_url: Some("https://ci.example.test/runs/1"),
            external_id: Some("run-1"),
            output: Some(&serde_json::json!({ "title": "Building", "summary": "" })),
            started_clock_unix: Some(DEFAULT_T0_UNIX + 3600),
            completed_clock_unix: None,
        },
        w.clock()?,
    )?;
    w.update_check_run(
        check.id,
        by(ci.id),
        CheckRunPatch {
            status: Some(CheckStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            completed_clock_unix: Some(DEFAULT_T0_UNIX + 4200),
            ..Default::default()
        },
        w.clock()?,
    )?;
    let jobs = [
        JobSpec {
            name: "test".to_string(),
            needs: Vec::new(),
            always: false,
            labels: vec!["ubuntu-latest".to_string()],
            steps: vec![
                StepSpec {
                    name: "Set up job".to_string(),
                    always: false,
                },
                StepSpec {
                    name: "Run tests".to_string(),
                    always: false,
                },
            ],
            closing_steps: vec!["Complete job".to_string()],
        },
        JobSpec {
            name: "deploy".to_string(),
            needs: vec!["test".to_string()],
            always: false,
            labels: vec!["ubuntu-latest".to_string()],
            steps: vec![StepSpec {
                name: "Set up job".to_string(),
                always: false,
            }],
            closing_steps: Vec::new(),
        },
    ];
    let workflow = w.create_workflow(
        NewWorkflow {
            repo_id: repo.id,
            path: ".github/workflows/ci.yml",
            name: "ci",
            jobs: &jobs,
            dispatch_inputs: None,
        },
        by(system.id),
    )?;
    let run = w.start_workflow_run(
        NewWorkflowRun {
            workflow_id: workflow.id,
            event: "push",
            head_sha: HEAD_SHA,
            head_branch: Some("feature"),
            title: "Add the feature",
            actor_id: alice.id,
            runner: "runner",
        },
        by(system.id),
    )?;
    let test = w.list_workflow_jobs(run.id)?[0].id;
    w.start_workflow_job(test, by(system.id))?;
    w.append_job_log(test, &["Getting ready".to_string()], by(system.id))?;
    w.complete_workflow_step(test, StepConclusion::Success, by(system.id))?;
    w.complete_workflow_job(test, false, by(system.id))?;
    // A second attempt, asked to cancel while its job runs.
    let rerun = w.rerun_workflow_run(run.id, Rerun::Failed, bob.id, by(bob.id))?;
    let again = w
        .list_workflow_jobs(run.id)?
        .into_iter()
        .rfind(|job| job.name == "test")
        .map(|job| job.id)
        .expect("the job runs again");
    w.start_workflow_job(again, by(system.id))?;
    w.cancel_workflow_run(rerun.id, false, by(bob.id))?;
    w.create_commit_status(
        by(bob.id),
        NewCommitStatus {
            repo_id: repo.id,
            sha: HEAD_SHA,
            context: "deploy/preview",
            state: StatusState::Pending,
            description: Some("Deploying"),
            target_url: Some("https://deploy.example/1"),
        },
    )?;

    let merge = RefMove {
        name: "refs/heads/main",
        old: BASE_SHA,
        new: MERGE_SHA,
        repins: Vec::new(),
        change: RefChange::Moved,
        forced: false,
        commits: Vec::new(),
        commit_count: 0,
    };
    w.record_merge(merged.id, by(bob.id), &merge, w.clock()?)?;

    let closed = w.create_pull(
        by(bob.id),
        NewPull {
            repo_id: repo.id,
            title: "Try another way",
            body: None,
            head_ref: "experiment",
            head_sha: HEAD_SHA,
            base_ref: "main",
            base_sha: BASE_SHA,
            draft: false,
        },
    )?;
    w.request_reviewers(closed.id, by(bob.id), &["alice"], &[])?;
    w.close_pull(closed.id, by(bob.id))?;

    // A pull into a branch that is then deleted, and the deletion and
    // restoration of the closed pull's head, so the timeline holds every
    // kind a push records.
    w.create_pull(
        by(carol.id),
        NewPull {
            repo_id: repo.id,
            title: "Document the release",
            body: None,
            head_ref: "docs",
            head_sha: HEAD_SHA,
            base_ref: "release",
            base_sha: BASE_SHA,
            draft: false,
        },
    )?;
    let branch_move = |name, old, new, change| RefMove {
        name,
        old,
        new,
        repins: Vec::new(),
        change,
        forced: false,
        commits: Vec::new(),
        commit_count: 0,
    };
    w.record_push(
        repo.id,
        by(carol.id),
        &[
            branch_move(
                "refs/heads/experiment",
                HEAD_SHA,
                ZERO_SHA,
                RefChange::Deleted,
            ),
            branch_move("refs/heads/release", BASE_SHA, ZERO_SHA, RefChange::Deleted),
        ],
        None,
        &[],
    )?;
    w.record_push(
        repo.id,
        by(bob.id),
        &[branch_move(
            "refs/heads/experiment",
            ZERO_SHA,
            HEAD_SHA,
            RefChange::Created,
        )],
        None,
        &[],
    )?;

    let app = w.create_oauth_app("GoTrue", CALLBACK, by(alice.id))?;
    let issued = w.issue_oauth_client_secret(&app.client_id, by(system.id))?;
    let code = w.issue_oauth_code(&app, &alice, CALLBACK, "user:email")?;
    w.exchange_oauth_code(&app.client_id, &issued, &code, Some(CALLBACK))?;
    w.issue_oauth_code(&app, &bob, CALLBACK, "read:org")?;

    // An app with a second callback URL, a key, an installation on chosen
    // repositories, and a token narrowed to one, so each of its tables has
    // a row.
    let permissions: Permissions = [
        ("contents".to_string(), AccessLevel::Write),
        ("metadata".to_string(), AccessLevel::Read),
    ]
    .into();
    let registered = w.create_app(
        &NewApp {
            slug: "ci-bot",
            name: "CI",
            owner: "acme",
            callback_urls: &[CALLBACK.to_string(), format!("{CALLBACK}/setup")],
            permissions: &permissions,
            events: &["push".to_string(), "pull_request".to_string()],
            hook_url: Some("https://ci.test/hook"),
            hook_active: true,
            setup_url: None,
            request_oauth_on_install: false,
        },
        by(system.id),
    )?;
    w.add_app_key("ci-bot", APP_KEY, by(system.id))?;
    // Two apps a person registered from a manifest: one whose code was
    // traded, which gave it a key, and one whose code is still held.
    for (slug, traded) in [("from-manifest", true), ("manifest-pending", false)] {
        let made = w.create_app_from_manifest(
            &NewApp {
                slug,
                name: slug,
                owner: "alice",
                callback_urls: &[CALLBACK.to_string()],
                permissions: &permissions,
                events: &[],
                hook_url: None,
                hook_active: false,
                setup_url: Some("https://ci.test/setup"),
                request_oauth_on_install: false,
            },
            Attribution::new(alice.id, Door::Web),
        )?;
        if traded {
            w.convert_app_manifest(&made.code)?;
        }
    }
    // Dependency alerts on a repository that has them on: one open with the
    // pull request the updater opened for it, one a person dismissed, and
    // one fixed.
    w.set_dependency_alerts(repo.id, true, by(alice.id))?;
    for (package, severity) in [
        ("lodash", orrery_world::AlertSeverity::High),
        ("minimist", orrery_world::AlertSeverity::Medium),
        ("node-fetch", orrery_world::AlertSeverity::Critical),
    ] {
        w.create_dependency_alert(
            repo.id,
            &orrery_world::NewDependencyAlert {
                ecosystem: "npm",
                package,
                manifest_path: "package-lock.json",
                scope: Some(orrery_world::AlertScope::Runtime),
                version_requirement: Some("1.0.0"),
                advisory_id: "GHSA-0000-0000-0000",
                cve_id: Some("CVE-2021-0000"),
                summary: "A vulnerable dependency",
                description: "What is wrong with it.",
                severity,
                vulnerable_version_range: "< 2.0.0",
                first_patched_version: Some("2.0.0"),
            },
            by(system.id),
        )?;
    }
    w.set_dependency_alert_update(
        repo.id,
        1,
        &orrery_world::NewAlertUpdate::PullRequest {
            number: closed.number,
        },
        by(system.id),
    )?;
    w.move_dependency_alert(
        repo.id,
        2,
        &orrery_world::AlertTransition::Dismiss {
            reason: orrery_world::DismissReason::TolerableRisk,
            comment: Some("accepted"),
        },
        by(alice.id),
    )?;
    w.move_dependency_alert(
        repo.id,
        3,
        &orrery_world::AlertTransition::Fix,
        by(system.id),
    )?;
    // A delivery for the installation, as a face would make one, so that
    // `deliveries` holds one that was answered and one still pending.
    w.set_notifier(std::sync::Arc::new(OnInstallation));
    let repositories = ["webapp".to_string()];
    let installation =
        w.create_installation("ci-bot", "acme", Some(&repositories), by(system.id))?;
    let owed = w.pending_deliveries(1)?;
    w.record_delivery_attempt(
        owed[0].id,
        12,
        &orrery_world::DeliveryOutcome::Answered {
            status_code: 204,
            headers: vec![("content-length".to_string(), "0".to_string())],
            body: String::new(),
            truncated: false,
        },
    )?;
    w.redeliver(registered.app.id, owed[0].id)?;
    w.suspend_installation(registered.app.id, installation.id)?;
    w.unsuspend_installation(registered.app.id, installation.id)?;
    w.create_installation_token(
        registered.app.id,
        installation.id,
        Some(&repositories),
        None,
    )?;
    Ok(())
}

/// Owes the installation's app one delivery when it is installed.
struct OnInstallation;

impl orrery_world::Notifier for OnInstallation {
    fn deliveries(
        &self,
        _world: &World,
        _by: orrery_world::Attribution,
        events: &[orrery_world::EventBody],
    ) -> Vec<orrery_world::NewDelivery> {
        events
            .iter()
            .filter_map(|event| match event {
                orrery_world::EventBody::InstallationCreated(created) => {
                    Some(orrery_world::NewDelivery {
                        app_id: created.subject.app.id,
                        installation_id: Some(created.subject.id),
                        repo_id: None,
                        event: "installation".to_string(),
                        action: Some("created".to_string()),
                        body: "{\"action\":\"created\"}".to_string(),
                    })
                }
                _ => None,
            })
            .collect()
    }
}
