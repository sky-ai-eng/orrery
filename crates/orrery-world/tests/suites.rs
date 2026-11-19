//! Check suites: when one is made, and what its check runs bring it to.

use orrery_world::{
    ActorKind, Attribution, CheckConclusion, CheckRunPatch, CheckStatus, Door, NewCheckRun,
    RefChange, RefMove, World, WorldSecret,
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NONE: &str = "0000000000000000000000000000000000000000";

fn world() -> (tempfile::TempDir, World) {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(dir.path(), 1_700_000_000, &WorldSecret::LOCAL).unwrap();
    (dir, w)
}

fn run<'a>(repo_id: i64, sha: &'a str, name: &'a str, status: CheckStatus) -> NewCheckRun<'a> {
    NewCheckRun {
        repo_id,
        head_sha: sha,
        name,
        status,
        conclusion: None,
        details_url: None,
        external_id: None,
        output: None,
        started_clock_unix: None,
        completed_clock_unix: None,
    }
}

#[test]
fn a_suite_comes_to_what_its_newest_runs_of_each_name_do() {
    let (_dir, w) = world();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (ci, _) = w.create_actor("ci", ActorKind::Bot).unwrap();
    let (other, _) = w.create_actor("other", ActorKind::Bot).unwrap();
    let me = Attribution::new(alice.id, Door::Rest);
    let bot = Attribution::new(ci.id, Door::Rest);
    let repo = w
        .create_repo(alice.id, "web", None, false, "main", me)
        .unwrap();
    let at = w.clock().unwrap();
    let state = |w: &World, id: i64| {
        let suite = w.check_suite_by_id(id).unwrap().unwrap();
        (suite.status, suite.conclusion)
    };

    // A creator's first run on a commit makes its suite, which records no
    // push. The second joins it.
    let a = w
        .create_check_run(bot, run(repo.id, A, "a", CheckStatus::Queued), at)
        .unwrap();
    let b = w
        .create_check_run(bot, run(repo.id, A, "b", CheckStatus::Queued), at)
        .unwrap();
    assert_eq!(a.suite_id, b.suite_id);
    let suite = w.check_suite_by_id(a.suite_id).unwrap().unwrap();
    assert_eq!(
        (suite.head_branch, suite.before_sha, suite.pusher_login),
        (None, None, None)
    );
    assert_eq!(suite.creator_login, "ci");
    assert_eq!(state(&w, a.suite_id), (CheckStatus::Queued, None));
    // Another creator's run on the commit, and this creator's on another
    // commit, are each in a suite of their own.
    let elsewhere = w
        .create_check_run(
            Attribution::new(other.id, Door::Rest),
            run(repo.id, A, "a", CheckStatus::Queued),
            at,
        )
        .unwrap();
    let later = w
        .create_check_run(bot, run(repo.id, B, "a", CheckStatus::Queued), at)
        .unwrap();
    assert_ne!(elsewhere.suite_id, a.suite_id);
    assert_ne!(later.suite_id, a.suite_id);
    assert_eq!(w.list_check_suites_for_sha(repo.id, A).unwrap().len(), 2);
    assert_eq!(w.list_check_runs_for_suite(a.suite_id).unwrap().len(), 2);

    let set = |id: i64, status: CheckStatus, conclusion: Option<CheckConclusion>| {
        let patch = CheckRunPatch {
            status: Some(status),
            conclusion,
            ..Default::default()
        };
        w.update_check_run(id, bot, patch, at).unwrap();
    };
    // In progress while any run is, queued while any is queued, and
    // complete once every run is.
    set(a.id, CheckStatus::InProgress, None);
    assert_eq!(state(&w, a.suite_id), (CheckStatus::InProgress, None));
    set(a.id, CheckStatus::Completed, Some(CheckConclusion::Success));
    assert_eq!(state(&w, a.suite_id), (CheckStatus::Queued, None));
    // The conclusion is the first of these any run holds.
    use CheckConclusion::*;
    for (one, two, both) in [
        (Success, Success, Success),
        (Success, Skipped, Success),
        (Success, Neutral, Success),
        (Success, Cancelled, Cancelled),
        (Cancelled, Failure, Failure),
        (Failure, TimedOut, TimedOut),
        (TimedOut, ActionRequired, ActionRequired),
        (Failure, ActionRequired, ActionRequired),
        (Skipped, Neutral, Neutral),
        (Skipped, Skipped, Skipped),
    ] {
        set(a.id, CheckStatus::Completed, Some(one));
        set(b.id, CheckStatus::Completed, Some(two));
        assert_eq!(
            state(&w, a.suite_id),
            (CheckStatus::Completed, Some(both)),
            "{one:?} and {two:?}"
        );
    }
    // A newer run of a name takes the older one's place.
    set(a.id, CheckStatus::Completed, Some(Failure));
    set(b.id, CheckStatus::Completed, Some(Success));
    assert_eq!(
        state(&w, a.suite_id),
        (CheckStatus::Completed, Some(Failure))
    );
    let again = w
        .create_check_run(bot, run(repo.id, A, "a", CheckStatus::Queued), at)
        .unwrap();
    assert_eq!(again.suite_id, a.suite_id);
    assert_eq!(state(&w, a.suite_id), (CheckStatus::Queued, None));
    set(again.id, CheckStatus::Completed, Some(Success));
    assert_eq!(
        state(&w, a.suite_id),
        (CheckStatus::Completed, Some(Success))
    );

    // Each change of what a suite comes to is an event, and a write that
    // changes neither its status nor its conclusion is none.
    let events = |w: &World| {
        w.log_after(0, 10_000)
            .unwrap()
            .iter()
            .filter(|e| e.body.type_name() == "check_suite:updated")
            .count()
    };
    let before = events(&w);
    w.update_check_run(
        b.id,
        bot,
        CheckRunPatch {
            details_url: Some("https://ci.test/b"),
            ..Default::default()
        },
        at,
    )
    .unwrap();
    assert_eq!(events(&w), before);
}

#[test]
fn a_push_makes_a_suite_for_each_app_that_may_write_checks() {
    let (_dir, w) = world();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let me = Attribution::new(alice.id, Door::Rest);
    let system = Attribution::new(orrery_world::SYSTEM_ACTOR_ID, Door::Control);
    let repo = w
        .create_repo(alice.id, "web", None, false, "main", me)
        .unwrap();
    let app = |slug: &'static str, checks: Option<orrery_world::AccessLevel>| {
        let mut permissions = orrery_world::Permissions::new();
        if let Some(level) = checks {
            permissions.insert("checks".to_string(), level);
        }
        let new = orrery_world::NewApp {
            slug,
            name: slug,
            owner: "alice",
            callback_urls: &["https://app.test/callback".to_string()],
            permissions: &permissions,
            events: &[],
            hook_url: None,
            hook_active: false,
            setup_url: None,
            request_oauth_on_install: false,
        };
        let made = w.create_app(&new, system).unwrap();
        w.create_installation(slug, "alice", None, system).unwrap();
        made
    };
    app("writer", Some(orrery_world::AccessLevel::Write));
    app("reader", Some(orrery_world::AccessLevel::Read));
    app("neither", None);

    let push = |name: &str, old: &str, new: &str, change: RefChange| {
        let moves = [RefMove {
            name,
            old,
            new,
            repins: Vec::new(),
            change,
            forced: false,
            commits: Vec::new(),
            commit_count: 0,
        }];
        w.record_push(repo.id, me, &moves, None, &[]).unwrap();
    };
    push("refs/heads/main", "", A, RefChange::Created);
    let suites = w.list_check_suites_for_sha(repo.id, A).unwrap();
    assert_eq!(suites.len(), 1);
    let suite = &suites[0];
    assert_eq!(suite.creator_login, "writer");
    assert_eq!(suite.head_branch.as_deref(), Some("main"));
    assert_eq!(suite.before_sha.as_deref(), Some(NONE));
    assert_eq!(suite.pusher_login.as_deref(), Some("alice"));
    assert_eq!(
        (suite.status, suite.conclusion),
        (CheckStatus::Queued, None)
    );

    // A move records what the branch pointed at, a tag makes nothing, and a
    // commit that already has the app's suite keeps the one it has.
    push("refs/heads/main", A, B, RefChange::Moved);
    let moved = w.list_check_suites_for_sha(repo.id, B).unwrap();
    assert_eq!(moved[0].before_sha.as_deref(), Some(A));
    push("refs/tags/v1", "", B, RefChange::Created);
    push("refs/heads/other", "", B, RefChange::Created);
    assert_eq!(w.list_check_suites_for_sha(repo.id, B).unwrap(), moved);
    push("refs/heads/other", B, "", RefChange::Deleted);
    assert_eq!(w.list_check_suites_for_sha(repo.id, B).unwrap(), moved);

    // The app's first run on the commit joins the suite the push made.
    let bot = w.actor_by_login("writer").unwrap().unwrap();
    let at = w.clock().unwrap();
    let made = w
        .create_check_run(
            Attribution::new(bot.id, Door::Rest),
            run(repo.id, B, "build", CheckStatus::InProgress),
            at,
        )
        .unwrap();
    assert_eq!(made.suite_id, moved[0].id);
    assert_eq!(
        w.check_suite_by_id(made.suite_id).unwrap().unwrap().status,
        CheckStatus::InProgress
    );
}
