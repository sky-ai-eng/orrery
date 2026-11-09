//! Kernel conformance for the collaboration objects: the single-row-write
//! rule (returned row == point read), the state machine, and the action-log
//! entries every transition owes. Shas here are caller-supplied strings —
//! the kernel takes no view on git; the git face owns resolution.

use orrery_world::{
    ActorKind, CheckConclusion, CheckRunPatch, CheckStatus, DEFAULT_T0_UNIX, NewCheckRun, NewPull,
    Pull, PullState, RefMove, ReviewState, TimelineKind, World, WorldError, WorldSecret,
};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

struct Fixture {
    _dir: tempfile::TempDir,
    w: World,
    alice: i64,
    bob: i64,
    repo: i64,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let w = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .expect("create world");
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let (bob, _) = w.create_actor("bob", ActorKind::User).unwrap();
    let repo = w
        .create_repo(alice.id, "webapp", None, false, "main", alice.id)
        .unwrap();
    Fixture {
        _dir: dir,
        w,
        alice: alice.id,
        bob: bob.id,
        repo: repo.id,
    }
}

fn open_pull(f: &Fixture, head: &str, sha: &str) -> Pull {
    f.w.create_pull(NewPull {
        repo_id: f.repo,
        author_id: f.alice,
        title: "add feature",
        body: Some("does the thing"),
        head_ref: head,
        head_sha: sha,
        base_ref: "main",
        base_sha: SHA_A,
        draft: false,
    })
    .unwrap()
}

#[test]
fn create_pull_returns_the_stored_row_and_numbers_sequentially() {
    let f = fixture();
    let created = open_pull(&f, "feature", SHA_B);
    let read = f.w.pull_by_number(f.repo, created.number).unwrap().unwrap();
    assert_eq!(created, read);
    assert_eq!(created.number, 1);
    assert_eq!(created.state, PullState::Open);
    assert_eq!(created.author_login, "alice");
    assert_eq!(created.head_sha, SHA_B);
    assert_eq!(created.base_sha, SHA_A);

    let second = open_pull(&f, "other", SHA_C);
    assert_eq!(second.number, 2);

    // One open pull per (head, base); a different base is a different pull.
    assert!(matches!(
        f.w.create_pull(NewPull {
            repo_id: f.repo,
            author_id: f.bob,
            title: "dup",
            body: None,
            head_ref: "feature",
            head_sha: SHA_B,
            base_ref: "main",
            base_sha: SHA_A,
            draft: false,
        }),
        Err(WorldError::PullExists(_))
    ));

    let log = f.w.log_after(0, 100).unwrap();
    let opened = log.iter().find(|e| e.action == "pull.open").unwrap();
    assert_eq!(opened.details["repo"], "alice/webapp");
    assert_eq!(opened.details["number"], 1);
    assert_eq!(opened.actor_login, "alice");
}

#[test]
fn close_reopen_merge_state_machine() {
    let f = fixture();
    let pull = open_pull(&f, "feature", SHA_B);

    let closed = f.w.close_pull(pull.id, f.bob).unwrap();
    assert_eq!(closed.state, PullState::Closed);
    assert!(closed.closed_clock_unix.is_some());
    assert!(!closed.merged);
    assert!(matches!(
        f.w.close_pull(pull.id, f.bob),
        Err(WorldError::InvalidTransition(_))
    ));

    let reopened = f.w.reopen_pull(pull.id, f.bob).unwrap();
    assert_eq!(reopened.state, PullState::Open);
    assert_eq!(reopened.closed_clock_unix, None);
    assert!(matches!(
        f.w.reopen_pull(pull.id, f.bob),
        Err(WorldError::InvalidTransition(_))
    ));

    let merged = f.w.record_merge(pull.id, f.bob, SHA_C).unwrap();
    assert!(merged.merged);
    assert_eq!(merged.state, PullState::Closed);
    assert_eq!(merged.merge_commit_sha.as_deref(), Some(SHA_C));
    assert_eq!(merged.merged_by_login.as_deref(), Some("bob"));
    let read = f.w.pull_by_number(f.repo, pull.number).unwrap().unwrap();
    assert_eq!(merged, read);

    // A merged pull is terminal: no reopen, no second merge.
    assert!(matches!(
        f.w.reopen_pull(pull.id, f.bob),
        Err(WorldError::InvalidTransition(_))
    ));
    assert!(matches!(
        f.w.record_merge(pull.id, f.bob, SHA_C),
        Err(WorldError::InvalidTransition(_))
    ));

    // The close and the merge each land on the timeline as a close, and the
    // refused transitions add nothing.
    let items = f.w.list_pull_timeline(pull.id).unwrap();
    assert_eq!(
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
        [&TimelineKind::Closed, &TimelineKind::Closed]
    );
    assert!(items.iter().all(|i| i.actor_login == "bob"));

    let actions: Vec<String> =
        f.w.log_after(0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();
    for expected in ["pull.open", "pull.close", "pull.reopen", "pull.merge"] {
        assert!(actions.iter().any(|a| a == expected), "missing {expected}");
    }
}

#[test]
fn sync_pull_heads_repins_open_pulls_only() {
    let f = fixture();
    let open = open_pull(&f, "feature", SHA_B);
    let closed = open_pull(&f, "stale", SHA_B);
    f.w.close_pull(closed.id, f.alice).unwrap();

    let sys = f.w.actor_by_login("orrery").unwrap().unwrap();
    f.w.advance_clock(60, sys.id).unwrap();

    let touched = f.w.sync_pull_heads(f.repo, "feature", SHA_C).unwrap();
    assert_eq!(touched, vec![open.number]);
    let read = f.w.pull_by_number(f.repo, open.number).unwrap().unwrap();
    assert_eq!(read.head_sha, SHA_C);
    assert_eq!(read.updated_clock_unix, DEFAULT_T0_UNIX + 60);

    // The closed pull keeps its last pinned sha, and a no-op move touches
    // nothing.
    let stale = f.w.pull_by_number(f.repo, closed.number).unwrap().unwrap();
    assert_eq!(stale.head_sha, SHA_B);
    assert_eq!(
        f.w.sync_pull_heads(f.repo, "feature", SHA_C).unwrap(),
        vec![] as Vec<i64>
    );
}

#[test]
fn a_push_repins_the_pulls_it_names_and_only_open_ones() {
    let f = fixture();
    let closed = open_pull(&f, "feature", SHA_B);
    f.w.close_pull(closed.id, f.alice).unwrap();
    let open = open_pull(&f, "feature", SHA_B);
    let elsewhere = open_pull(&f, "other", SHA_B);

    // What a move of `feature` re-pins: its open pulls, and none already at
    // the new sha.
    assert_eq!(
        f.w.pulls_to_sync(f.repo, "feature", SHA_C).unwrap(),
        vec![open.number]
    );
    assert_eq!(
        f.w.pulls_to_sync(f.repo, "feature", SHA_B).unwrap(),
        vec![] as Vec<i64>
    );

    let moves = [RefMove {
        name: "refs/heads/feature",
        old: SHA_B,
        new: SHA_C,
        repins: vec![open.number],
    }];
    f.w.record_push(f.repo, f.alice, &moves, None).unwrap();
    let head = |n| f.w.pull_by_number(f.repo, n).unwrap().unwrap().head_sha;
    assert_eq!(head(open.number), SHA_C);
    assert_eq!(head(closed.number), SHA_B);
    assert_eq!(head(elsewhere.number), SHA_B);

    // Naming a pull that is not open refuses the whole push: no entry, and
    // no pull moves.
    let mark = f.w.log_after(0, 1000).unwrap().last().unwrap().seq;
    let stale = [RefMove {
        name: "refs/heads/feature",
        old: SHA_C,
        new: SHA_A,
        repins: vec![open.number, closed.number],
    }];
    assert!(matches!(
        f.w.record_push(f.repo, f.alice, &stale, None),
        Err(WorldError::InvalidTransition(_))
    ));
    assert_eq!(head(open.number), SHA_C);
    assert!(f.w.log_after(mark, 1000).unwrap().is_empty());
}

#[test]
fn edit_pull_distinguishes_clear_from_keep() {
    let f = fixture();
    let pull = open_pull(&f, "feature", SHA_B);
    let log_before = f.w.log_after(0, 1000).unwrap().len();

    // All-None is a no-op: same row back, nothing logged.
    let unchanged = f.w.edit_pull(pull.id, f.alice, None, None, None).unwrap();
    assert_eq!(unchanged, pull);
    assert_eq!(f.w.log_after(0, 1000).unwrap().len(), log_before);

    let retitled =
        f.w.edit_pull(pull.id, f.alice, Some("new title"), Some(None), None)
            .unwrap();
    assert_eq!(retitled.title, "new title");
    assert_eq!(retitled.body, None, "explicit null cleared the body");
    let read = f.w.pull_by_number(f.repo, pull.number).unwrap().unwrap();
    assert_eq!(retitled, read);

    let edit =
        f.w.log_after(0, 1000)
            .unwrap()
            .into_iter()
            .find(|e| e.action == "pull.edit")
            .unwrap();
    assert_eq!(edit.details["fields"], serde_json::json!(["title", "body"]));
}

#[test]
fn review_and_comment_return_stored_rows_and_bump_the_pull() {
    let f = fixture();
    let pull = open_pull(&f, "feature", SHA_B);
    let sys = f.w.actor_by_login("orrery").unwrap().unwrap();
    f.w.advance_clock(60, sys.id).unwrap();

    let review =
        f.w.create_review(pull.id, f.bob, ReviewState::Approved, Some("lgtm"), None)
            .unwrap();
    assert_eq!(review.author_login, "bob");
    assert_eq!(review.state, ReviewState::Approved);
    assert_eq!(review.commit_sha, SHA_B, "pinned to the pull's head");
    assert_eq!(review.submitted_clock_unix, DEFAULT_T0_UNIX + 60);

    let comment = f.w.create_issue_comment(pull.id, f.bob, "ship it").unwrap();
    assert_eq!(comment.author_login, "bob");
    assert_eq!(comment.body, "ship it");

    let read = f.w.pull_by_number(f.repo, pull.number).unwrap().unwrap();
    assert_eq!(
        read.updated_clock_unix,
        DEFAULT_T0_UNIX + 60,
        "activity bumps the pull"
    );

    let actions: Vec<String> =
        f.w.log_after(0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();
    assert!(actions.iter().any(|a| a == "review.submit"));
    assert!(actions.iter().any(|a| a == "comment.create"));

    // An explicit commit pin wins over the pull's head.
    let pinned =
        f.w.create_review(pull.id, f.bob, ReviewState::Commented, None, Some(SHA_A))
            .unwrap();
    assert_eq!(pinned.commit_sha, SHA_A);

    assert!(matches!(
        f.w.create_review(999, f.bob, ReviewState::Approved, None, None),
        Err(WorldError::NoSuchPull(999))
    ));
}

#[test]
fn check_run_create_and_patch_return_stored_rows() {
    let f = fixture();
    let (ci, _) = f.w.create_actor("ci-bot", ActorKind::Bot).unwrap();
    let created =
        f.w.create_check_run(NewCheckRun {
            repo_id: f.repo,
            creator_id: ci.id,
            head_sha: SHA_B,
            name: "ci/test",
            status: CheckStatus::Queued,
            conclusion: None,
            details_url: None,
            external_id: Some("run-1"),
            output: Some(&serde_json::json!({ "title": "queued" })),
            started_clock_unix: Some(DEFAULT_T0_UNIX),
            completed_clock_unix: None,
        })
        .unwrap();
    let read = f.w.check_run_by_id(created.id).unwrap().unwrap();
    assert_eq!(created, read);
    assert_eq!(read.status, CheckStatus::Queued);
    assert_eq!(read.creator_login, "ci-bot");
    assert_eq!(read.output, Some(serde_json::json!({ "title": "queued" })));

    let updated =
        f.w.update_check_run(
            created.id,
            ci.id,
            CheckRunPatch {
                status: Some(CheckStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                completed_clock_unix: Some(DEFAULT_T0_UNIX + 120),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(updated.status, CheckStatus::Completed);
    assert_eq!(updated.conclusion, Some(CheckConclusion::Success));
    assert_eq!(updated.completed_clock_unix, Some(DEFAULT_T0_UNIX + 120));
    // Patch is field-by-field: untouched fields survive.
    assert_eq!(updated.external_id.as_deref(), Some("run-1"));
    assert_eq!(updated.name, "ci/test");
    let read = f.w.check_run_by_id(created.id).unwrap().unwrap();
    assert_eq!(updated, read);

    assert!(matches!(
        f.w.update_check_run(999, ci.id, CheckRunPatch::default()),
        Err(WorldError::NoSuchCheckRun(999))
    ));

    let actions: Vec<String> =
        f.w.log_after(0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect();
    assert!(actions.iter().any(|a| a == "check_run.create"));
    assert!(actions.iter().any(|a| a == "check_run.update"));
}

fn timeline_kinds(f: &Fixture, pull_id: i64) -> Vec<TimelineKind> {
    f.w.list_pull_timeline(pull_id)
        .unwrap()
        .into_iter()
        .map(|i| i.kind)
        .collect()
}

#[test]
fn labels_added_and_removed_land_on_the_pull_and_its_timeline() {
    let f = fixture();
    let pull = open_pull(&f, "feature", SHA_B);
    f.w.advance_clock(60, f.alice).unwrap();

    let labelled =
        f.w.add_pull_labels(pull.id, f.bob, &["bug", "Bug", "needs-review"])
            .unwrap();
    assert_eq!(labelled, f.w.pull_by_id(pull.id).unwrap().unwrap());
    assert_eq!(
        labelled.labels,
        vec!["bug", "needs-review"],
        "a repeat in another case is the same label"
    );
    assert_eq!(labelled.updated_clock_unix, f.w.clock().unwrap());

    let again = f.w.add_pull_labels(pull.id, f.bob, &["BUG"]).unwrap();
    assert_eq!(
        again, labelled,
        "a label the pull carries is skipped without a write"
    );

    let removed = f.w.remove_pull_label(pull.id, f.bob, "BUG").unwrap();
    assert_eq!(removed, f.w.pull_by_id(pull.id).unwrap().unwrap());
    assert_eq!(removed.labels, vec!["needs-review"]);
    assert!(matches!(
        f.w.remove_pull_label(pull.id, f.bob, "bug"),
        Err(WorldError::NoSuchLabel(_))
    ));

    assert_eq!(
        timeline_kinds(&f, pull.id),
        vec![
            TimelineKind::Labeled("bug".into()),
            TimelineKind::Labeled("needs-review".into()),
            TimelineKind::Unlabeled("bug".into()),
        ],
        "the unlabel records the stored name, not the request's casing"
    );
    let items = f.w.list_pull_timeline(pull.id).unwrap();
    assert!(
        items
            .iter()
            .all(|i| i.actor_login == "bob" && i.clock_unix == f.w.clock().unwrap())
    );

    let actions: Vec<(String, serde_json::Value)> =
        f.w.log_after(0, 100)
            .unwrap()
            .into_iter()
            .filter(|e| e.action.starts_with("pull.") && e.action.contains("label"))
            .map(|e| (e.action, e.details))
            .collect();
    assert_eq!(
        actions.len(),
        2,
        "one entry per write that changed something: {actions:?}"
    );
    assert_eq!(actions[0].0, "pull.label");
    assert_eq!(
        actions[0].1["added"],
        serde_json::json!(["bug", "needs-review"])
    );
    assert_eq!(actions[1].0, "pull.unlabel");
    assert_eq!(actions[1].1["removed"], "bug");
}

#[test]
fn an_invalid_label_name_refuses_the_whole_request() {
    let f = fixture();
    let pull = open_pull(&f, "feature", SHA_B);
    let long = "x".repeat(51);
    for bad in ["", "   ", long.as_str()] {
        assert!(
            matches!(
                f.w.add_pull_labels(pull.id, f.alice, &["ok", bad]),
                Err(WorldError::InvalidLabel(_))
            ),
            "{bad:?} accepted"
        );
    }
    assert!(f.w.pull_by_id(pull.id).unwrap().unwrap().labels.is_empty());
    assert!(timeline_kinds(&f, pull.id).is_empty());
    assert!(matches!(
        f.w.add_pull_labels(9999, f.alice, &["ok"]),
        Err(WorldError::NoSuchPull(_))
    ));
}

#[test]
fn ready_for_review_lands_on_the_timeline_and_convert_to_draft_does_not() {
    let f = fixture();
    let draft =
        f.w.create_pull(NewPull {
            repo_id: f.repo,
            author_id: f.alice,
            title: "wip",
            body: None,
            head_ref: "wip",
            head_sha: SHA_B,
            base_ref: "main",
            base_sha: SHA_A,
            draft: true,
        })
        .unwrap();
    f.w.set_pull_draft(draft.id, f.alice, false).unwrap();
    f.w.set_pull_draft(draft.id, f.alice, true).unwrap();
    assert!(f.w.set_pull_draft(draft.id, f.alice, true).is_err());
    assert_eq!(
        timeline_kinds(&f, draft.id),
        vec![TimelineKind::ReadyForReview]
    );
}
