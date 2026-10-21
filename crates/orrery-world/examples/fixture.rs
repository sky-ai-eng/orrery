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
    ActorEmail, ActorKind, CheckConclusion, CheckRunPatch, CheckStatus, DEFAULT_T0_UNIX,
    NewCheckRun, NewPull, OrgRole, ReviewState, SCHEMA_VERSION, SYSTEM_ACTOR_LOGIN, TeamRole,
    World, WorldSecret,
};

const BASE_SHA: &str = "1111111111111111111111111111111111111111";
const HEAD_SHA: &str = "2222222222222222222222222222222222222222";
const MERGE_SHA: &str = "3333333333333333333333333333333333333333";
const CALLBACK: &str = "https://auth.example.test/callback";

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
        alice.id,
    )?;

    w.set_org_membership("acme", "alice", OrgRole::Admin, system.id)?;
    w.set_org_membership("acme", "bob", OrgRole::Member, alice.id)?;
    w.set_org_membership("acme", "carol", OrgRole::Member, alice.id)?;
    w.create_team("acme", "Platform", Some("keeps the lights on"), alice.id)?;
    w.set_team_membership("acme", "platform", "bob", TeamRole::Maintainer, alice.id)?;
    w.set_team_membership("acme", "platform", "carol", TeamRole::Member, alice.id)?;
    // Deleted after a review is requested from it, so a timeline row names
    // a team that no longer exists.
    w.create_team("acme", "Interim", None, alice.id)?;

    let repo = w.create_repo(acme.id, "webapp", Some("the app"), false, "main", alice.id)?;
    w.touch_pushed(repo.id)?;
    w.advance_clock(3600, system.id)?;

    let merged = w.create_pull(NewPull {
        repo_id: repo.id,
        author_id: alice.id,
        title: "Add the feature",
        body: Some("Closes nothing; it is a fixture."),
        head_ref: "feature",
        head_sha: HEAD_SHA,
        base_ref: "main",
        base_sha: BASE_SHA,
        draft: true,
    })?;
    w.set_pull_draft(merged.id, alice.id, false)?;
    w.add_pull_labels(merged.id, alice.id, &["bug", "triage"])?;
    w.remove_pull_label(merged.id, alice.id, "triage")?;
    w.request_reviewers(
        merged.id,
        alice.id,
        &["bob", "carol"],
        &["platform", "interim"],
    )?;
    w.remove_review_requests(merged.id, alice.id, &["carol"], &[])?;
    w.delete_team("acme", "interim", alice.id)?;
    w.create_review(
        merged.id,
        bob.id,
        ReviewState::ChangesRequested,
        Some("one nit"),
        None,
    )?;
    w.create_issue_comment(merged.id, carol.id, "Looks close.")?;
    w.advance_clock(600, system.id)?;
    w.create_review(merged.id, bob.id, ReviewState::Approved, None, None)?;
    let check = w.create_check_run(NewCheckRun {
        repo_id: repo.id,
        creator_id: ci.id,
        head_sha: HEAD_SHA,
        name: "build",
        status: CheckStatus::InProgress,
        conclusion: None,
        details_url: Some("https://ci.example.test/runs/1"),
        external_id: Some("run-1"),
        output: Some(&serde_json::json!({ "title": "Building", "summary": "" })),
        started_clock_unix: Some(DEFAULT_T0_UNIX + 3600),
        completed_clock_unix: None,
    })?;
    w.update_check_run(
        check.id,
        ci.id,
        CheckRunPatch {
            status: Some(CheckStatus::Completed),
            conclusion: Some(CheckConclusion::Success),
            completed_clock_unix: Some(DEFAULT_T0_UNIX + 4200),
            ..Default::default()
        },
    )?;
    w.record_merge(merged.id, bob.id, MERGE_SHA)?;

    let closed = w.create_pull(NewPull {
        repo_id: repo.id,
        author_id: bob.id,
        title: "Try another way",
        body: None,
        head_ref: "experiment",
        head_sha: HEAD_SHA,
        base_ref: "main",
        base_sha: BASE_SHA,
        draft: false,
    })?;
    w.request_reviewers(closed.id, bob.id, &["alice"], &[])?;
    w.close_pull(closed.id, bob.id)?;

    let app = w.create_oauth_app("GoTrue", CALLBACK, alice.id)?;
    let code = w.issue_oauth_code(&app, &alice, CALLBACK, "user:email")?;
    w.exchange_oauth_code(&app.client_id, &app.client_secret, &code, Some(CALLBACK))?;
    w.issue_oauth_code(&app, &bob, CALLBACK, "read:org")?;
    Ok(())
}
