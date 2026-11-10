use std::sync::{Arc, Mutex};

use orrery_world::{
    ActorKind, Attribution, DEFAULT_T0_UNIX, Door, Event, LogOrder, RefChange, RefMove, Signal,
    World, WorldError, WorldReader, WorldSecret, WorldView, WriteSpan,
};
use sha2::{Digest, Sha256};

/// A write by `actor_id` as the REST face makes it.
fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

/// An event without its envelope: its type and its type's fields.
fn body(e: &Event) -> serde_json::Value {
    serde_json::to_value(&e.body).unwrap()
}

fn fresh() -> (tempfile::TempDir, World) {
    keyed(&WorldSecret::LOCAL)
}

fn keyed(secret: &WorldSecret) -> (tempfile::TempDir, World) {
    let dir = tempfile::tempdir().expect("tempdir");
    let world =
        World::create(&dir.path().join("world"), DEFAULT_T0_UNIX, secret).expect("create world");
    (dir, world)
}

#[test]
fn create_then_reopen_preserves_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("world");
    {
        let w = World::create(&root, 1000, &WorldSecret::LOCAL).unwrap();
        w.create_actor("alice", ActorKind::User).unwrap();
        assert_eq!(w.clock().unwrap(), 1000);
    }
    let w = World::open(&root).unwrap();
    assert_eq!(w.clock().unwrap(), 1000);
    let alice = w
        .actor_by_login("alice")
        .unwrap()
        .expect("alice survives reopen");
    assert_eq!(alice.kind, ActorKind::User);
    assert!(matches!(
        World::create(&root, 1000, &WorldSecret::LOCAL),
        Err(WorldError::AlreadyExists(_))
    ));
}

#[test]
fn clock_is_forward_only_and_logged() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let to = w.advance_clock(3600, by(sys.id)).unwrap();
    assert_eq!(to, DEFAULT_T0_UNIX + 3600);
    assert!(matches!(
        w.advance_clock(0, by(sys.id)),
        Err(WorldError::ClockRegression(0))
    ));
    assert!(matches!(
        w.advance_clock(-5, by(sys.id)),
        Err(WorldError::ClockRegression(-5))
    ));

    let log = w.log_after(0, 100).unwrap();
    let advance = log
        .iter()
        .find(|e| e.body.type_name() == "world:clock_advanced")
        .expect("advance logged");
    assert_eq!(
        body(advance)["changes"]["clock_unix"],
        serde_json::json!({ "from": DEFAULT_T0_UNIX, "to": DEFAULT_T0_UNIX + 3600 })
    );
}

#[test]
fn the_clock_stops_at_the_latest_time_it_can_render() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let latest = jiff::Timestamp::MAX.as_second();
    let log_len = w.log_after(0, 100).unwrap().len();

    for seconds in [latest - DEFAULT_T0_UNIX + 1, i64::MAX] {
        match w.advance_clock(seconds, by(sys.id)) {
            Err(WorldError::ClockOutOfRange {
                seconds: refused,
                remaining,
                ..
            }) => {
                assert_eq!(refused, seconds);
                assert_eq!(remaining, latest - DEFAULT_T0_UNIX);
            }
            other => panic!("advance of {seconds}: {other:?}"),
        }
    }
    assert_eq!(w.clock().unwrap(), DEFAULT_T0_UNIX);
    assert_eq!(w.log_after(0, 100).unwrap().len(), log_len);

    assert_eq!(
        w.advance_clock(latest - DEFAULT_T0_UNIX, by(sys.id))
            .unwrap(),
        latest
    );
    let refused = w.advance_clock(1, by(sys.id)).unwrap_err();
    assert!(
        matches!(refused, WorldError::ClockOutOfRange { remaining: 0, .. }),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains(" 9999-12-30T22:00:00Z, "),
        "{refused}"
    );
    assert_eq!(w.clock().unwrap(), latest);
}

// The single-row-write rule (CLAUDE.md): the row a write returns must equal the
// row a point read sees, with no follow-up read to "correct" it.
#[test]
fn create_actor_returns_the_stored_row() {
    let (_dir, w) = fresh();
    let (created, token) = w.create_actor("alice", ActorKind::User).unwrap();
    let read = w.actor_by_login("alice").unwrap().unwrap();
    assert_eq!(created, read);
    assert!(
        token.starts_with("orrfake_"),
        "tokens are visibly fake: {token}"
    );
}

#[test]
fn tokens_are_unguessable_without_the_world_secret() {
    let (_d1, w1) = keyed(&WorldSecret::from_bytes([1; 32]));
    let (_d2, w2) = keyed(&WorldSecret::from_bytes([2; 32]));
    let (alice1, token1) = w1.create_actor("alice", ActorKind::User).unwrap();
    let (alice2, token2) = w2.create_actor("alice", ActorKind::User).unwrap();
    assert_eq!((alice1.id, alice2.id), (2, 2));
    assert_ne!(token1, token2);

    // The derivation's inputs are public; without the key they compute nothing.
    let bare = format!(
        "orrfake_{}",
        hex::encode(&Sha256::digest(format!("alice:{}", alice1.id))[..16])
    );
    assert_ne!(token1, bare);
    assert_ne!(token2, bare);
    assert!(w1.actor_by_token(&bare).unwrap().is_none());
    assert!(w1.actor_by_token(&token2).unwrap().is_none());
}

#[test]
fn the_same_secret_and_calls_mint_the_same_credentials() {
    let secret = WorldSecret::from_bytes([7; 32]);
    let run = || {
        let (_dir, w) = keyed(&secret);
        let (alice, token) = w.create_actor("alice", ActorKind::User).unwrap();
        let (_, issued_token) = w.issue_actor_token("alice", by(alice.id)).unwrap();
        let app = w
            .create_oauth_app("Sign-in", "https://auth.test/callback", by(alice.id))
            .unwrap();
        let issued_secret = w
            .issue_oauth_client_secret(&app.client_id, by(alice.id))
            .unwrap();
        let code = w
            .issue_oauth_code(&app, &alice, "https://auth.test/callback", "user")
            .unwrap();
        let oauth = w
            .exchange_oauth_code(&app.client_id, &issued_secret, &code, None)
            .unwrap();
        (
            [token, issued_token, oauth.token],
            [app.client_id, app.client_secret, issued_secret, code],
        )
    };
    assert_eq!(run(), run());
}

#[test]
fn the_world_secret_never_reaches_the_log() {
    let secret = WorldSecret::from_bytes([0xab; 32]);
    let (_dir, w) = keyed(&secret);
    let (_, token) = w.create_actor("alice", ActorKind::User).unwrap();
    let log = serde_json::to_string(&w.log_after(0, 100).unwrap()).unwrap();
    assert_eq!(
        body(&w.log_after(0, 1).unwrap()[0])["object"],
        serde_json::json!({ "clock_unix": DEFAULT_T0_UNIX })
    );
    assert!(!log.contains(&"ab".repeat(32)), "{log}");
    assert!(!log.contains(&token), "{log}");
}

#[test]
fn an_issued_token_acts_as_its_actor_beside_every_earlier_one() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let (alice, created) = w.create_actor("alice", ActorKind::User).unwrap();
    let (actor, first) = w.issue_actor_token("alice", by(sys.id)).unwrap();
    assert_eq!(actor, alice);
    // The login is matched as GitHub matches it, and the derivation uses the
    // stored one.
    let (_, second) = w.issue_actor_token("ALICE", by(sys.id)).unwrap();
    assert!(
        first.starts_with("orrfake_"),
        "tokens are visibly fake: {first}"
    );
    assert_ne!(first, second);
    for token in [&created, &first, &second] {
        assert_eq!(w.actor_by_token(token).unwrap().as_ref(), Some(&alice));
    }

    // Another actor's count is its own, and its tokens are its own.
    let (bob, _) = w.create_actor("bob", ActorKind::User).unwrap();
    let (_, bobs) = w.issue_actor_token("bob", by(sys.id)).unwrap();
    assert_eq!(w.actor_by_token(&bobs).unwrap(), Some(bob));

    assert!(matches!(
        w.issue_actor_token("ghost", by(sys.id)),
        Err(WorldError::NoSuchActor(_))
    ));
    assert!(matches!(
        w.issue_actor_token("orrery", by(sys.id)),
        Err(WorldError::SystemActorToken)
    ));

    let log = w.log_after(0, 100).unwrap();
    let issued: Vec<_> = log
        .iter()
        .filter(|e| e.body.type_name() == "actor:token_issued")
        .map(|e| {
            let b = body(e);
            (
                e.actor.login.clone(),
                b["subject"]["login"].clone(),
                b["n"].clone(),
            )
        })
        .collect();
    assert_eq!(
        issued,
        [("alice", 1), ("alice", 2), ("bob", 1)].map(|(login, n)| (
            "orrery".to_string(),
            login.into(),
            n.into()
        ))
    );
    let wire = serde_json::to_string(&log).unwrap();
    for token in [&first, &second, &bobs] {
        assert!(!wire.contains(token.as_str()), "the log leaks {token}");
    }
}

#[test]
fn token_resolves_to_its_actor() {
    let (_dir, w) = fresh();
    let (actor, token) = w.create_actor("ci-bot", ActorKind::Bot).unwrap();
    let resolved = w.actor_by_token(&token).unwrap().expect("token resolves");
    assert_eq!(resolved, actor);
    assert!(w.actor_by_token("orrfake_bogus").unwrap().is_none());
}

#[test]
fn actor_validation_and_conflicts() {
    let (_dir, w) = fresh();
    w.create_actor("alice", ActorKind::User).unwrap();
    assert!(matches!(
        w.create_actor("alice", ActorKind::User),
        Err(WorldError::ActorExists(_))
    ));
    // Logins are case-insensitively unique, like GitHub's.
    assert!(matches!(
        w.create_actor("ALICE", ActorKind::User),
        Err(WorldError::ActorExists(_))
    ));
    assert!(matches!(
        w.create_actor("-x", ActorKind::User),
        Err(WorldError::InvalidLogin(_))
    ));
    assert!(matches!(
        w.create_actor("a b", ActorKind::User),
        Err(WorldError::InvalidLogin(_))
    ));
    assert!(matches!(
        w.create_actor("orrery", ActorKind::User),
        Err(WorldError::InvalidLogin(_))
    ));
    assert!(matches!(
        w.create_actor("sneaky", ActorKind::System),
        Err(WorldError::InvalidActorKind)
    ));
}

#[test]
fn create_repo_returns_the_stored_row() {
    let (_dir, w) = fresh();
    let (acme, _) = w.create_actor("acme", ActorKind::Org).unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let created = w
        .create_repo(
            acme.id,
            "webapp",
            Some("the app"),
            false,
            "main",
            by(alice.id),
        )
        .unwrap();
    let read = w.repo_by_owner_name("acme", "webapp").unwrap().unwrap();
    assert_eq!(created, read);
    assert_eq!(read.owner_login, "acme");
    assert_eq!(read.default_branch, "main");
    assert_eq!(read.pushed_at_clock_unix, None);

    // Case-insensitive lookup and uniqueness, like GitHub's.
    assert!(w.repo_by_owner_name("ACME", "WebApp").unwrap().is_some());
    assert!(matches!(
        w.create_repo(acme.id, "WEBAPP", None, false, "main", by(alice.id)),
        Err(WorldError::RepoExists(_))
    ));
    assert!(matches!(
        w.create_repo(acme.id, "bad name", None, false, "main", by(alice.id)),
        Err(WorldError::InvalidRepoName(_))
    ));
    assert!(matches!(
        w.create_repo(acme.id, "trap.git", None, false, "main", by(alice.id)),
        Err(WorldError::InvalidRepoName(_))
    ));
}

#[test]
fn create_repo_with_an_unknown_actor_is_no_such_actor() {
    let (_dir, w) = fresh();
    let (acme, _) = w.create_actor("acme", ActorKind::Org).unwrap();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let unknown = alice.id + 1000;
    assert!(matches!(
        w.create_repo(unknown, "webapp", None, false, "main", by(alice.id)),
        Err(WorldError::NoSuchActorId(id)) if id == unknown
    ));
    assert!(matches!(
        w.create_repo(acme.id, "webapp", None, false, "main", by(unknown)),
        Err(WorldError::NoSuchActorId(id)) if id == unknown
    ));
    assert!(w.repo_by_owner_name("acme", "webapp").unwrap().is_none());
}

const ABSENT: &str = "0000000000000000000000000000000000000000";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A first push into an empty repository: a new branch and a new tag.
fn first_push() -> [RefMove<'static>; 2] {
    [
        RefMove {
            name: "refs/heads/trunk",
            old: ABSENT,
            new: SHA_A,
            repins: Vec::new(),
            change: RefChange::Created,
            forced: false,
            commits: vec![SHA_A.to_string()],
            commit_count: 1,
        },
        RefMove {
            name: "refs/tags/v1",
            old: ABSENT,
            new: SHA_B,
            repins: Vec::new(),
            change: RefChange::Created,
            forced: false,
            commits: vec![SHA_B.to_string()],
            commit_count: 1,
        },
    ]
}

#[test]
fn a_push_records_its_events_pushed_at_and_default_branch() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let repo = w
        .create_repo(alice.id, "dots", None, true, "main", by(alice.id))
        .unwrap();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    w.advance_clock(60, by(sys.id)).unwrap();
    let mark = w.log_after(0, 1000).unwrap().last().unwrap().seq;

    w.record_push(repo.id, by(alice.id), &first_push(), Some("trunk"), &[])
        .unwrap();
    let read = w.repo_by_id(repo.id).unwrap().unwrap();
    assert_eq!(read.pushed_at_clock_unix, Some(DEFAULT_T0_UNIX + 60));
    assert_eq!(read.default_branch, "trunk");
    let events = w.log_after(mark, 1000).unwrap();
    for e in &events {
        assert_eq!(
            e.write,
            WriteSpan {
                seq: mark + 1,
                events: 3
            }
        );
        assert_eq!((e.actor.login.as_str(), e.door), ("alice", Door::Rest));
        assert_eq!(e.clock_unix, DEFAULT_T0_UNIX + 60);
    }
    let repo_ref = serde_json::json!({ "id": repo.id, "name": "dots" });
    let owner = serde_json::json!({ "id": alice.id, "login": "alice" });
    let created = |name: &str, sha: &str| {
        serde_json::json!({
            "type": "ref:created",
            "subject": { "type": "ref", "name": name, "repo": repo_ref, "owner": owner },
            "object": { "name": name, "sha": sha },
            "commits": [sha],
            "commit_count": 1,
        })
    };
    assert_eq!(
        events.iter().map(body).collect::<Vec<_>>(),
        [
            serde_json::json!({
                "type": "repo:pushed",
                "subject": { "type": "repo", "id": repo.id, "name": "dots", "owner": owner },
                "changes": {
                    "default_branch": { "from": "main", "to": "trunk" },
                    "pushed_unix": { "from": null, "to": DEFAULT_T0_UNIX + 60 },
                },
            }),
            created("refs/heads/trunk", SHA_A),
            created("refs/tags/v1", SHA_B),
        ],
        "the push, then one event per move in the order given"
    );

    // A later push moves `pushed_at` and keeps the default branch.
    w.advance_clock(60, by(sys.id)).unwrap();
    let later = [RefMove {
        name: "refs/heads/trunk",
        old: SHA_A,
        new: SHA_B,
        repins: Vec::new(),
        change: RefChange::Moved,
        forced: true,
        commits: vec![SHA_B.to_string()],
        commit_count: 1,
    }];
    w.record_push(repo.id, by(alice.id), &later, None, &[])
        .unwrap();
    let read = w.repo_by_id(repo.id).unwrap().unwrap();
    assert_eq!(read.pushed_at_clock_unix, Some(DEFAULT_T0_UNIX + 120));
    assert_eq!(read.default_branch, "trunk");

    // A push that moved nothing records nothing, and a push into a
    // repository the world does not hold records none of its events.
    let mark = w.log_after(0, 1000).unwrap().last().unwrap().seq;
    w.record_push(repo.id, by(alice.id), &[], Some("main"), &[])
        .unwrap();
    assert_eq!(
        w.repo_by_id(repo.id).unwrap().unwrap().default_branch,
        "trunk"
    );
    assert!(matches!(
        w.record_push(999, by(alice.id), &first_push(), None, &[]),
        Err(WorldError::NoSuchRepo(999))
    ));
    assert!(w.log_after(mark, 1000).unwrap().is_empty());
}

#[test]
fn the_log_puts_each_ref_at_its_last_ref_event() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let dots = w
        .create_repo(alice.id, "dots", None, true, "main", by(alice.id))
        .unwrap();
    let other = w
        .create_repo(alice.id, "other", None, false, "main", by(alice.id))
        .unwrap();
    w.create_repo(alice.id, "empty", None, false, "main", by(alice.id))
        .unwrap();
    w.record_push(dots.id, by(alice.id), &first_push(), Some("trunk"), &[])
        .unwrap();
    let mv = |name, old, new, change| RefMove {
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
        dots.id,
        by(alice.id),
        &[
            mv("refs/heads/trunk", SHA_A, SHA_B, RefChange::Moved),
            mv("refs/tags/v1", SHA_B, ABSENT, RefChange::Deleted),
        ],
        None,
        &[],
    )
    .unwrap();
    w.record_push(
        other.id,
        by(alice.id),
        &[mv("refs/heads/main", ABSENT, SHA_A, RefChange::Created)],
        None,
        &[],
    )
    .unwrap();

    let refs = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(name, sha)| (name.to_string(), sha.to_string()))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert_eq!(
        w.logged_refs().unwrap(),
        [
            (dots.id, refs(&[("refs/heads/trunk", SHA_B)])),
            (other.id, refs(&[("refs/heads/main", SHA_A)])),
        ]
        .into()
    );
}

/// A world written before its log held events logged each ref a push or a
/// merge moved as `ref.update`. The v9 fixture's log creates and moves
/// `main`, deletes `experiment` and `release`, and creates `experiment`
/// again.
#[test]
fn the_log_of_a_migrated_world_puts_refs_where_its_older_entries_did() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("world");
    std::fs::create_dir_all(&root).unwrap();
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v9.db");
    std::fs::copy(fixture, root.join("world.db")).unwrap();
    let w = World::open(&root).unwrap();
    let webapp = w.repo_by_owner_name("acme", "webapp").unwrap().unwrap();
    assert_eq!(
        w.logged_refs().unwrap(),
        [(
            webapp.id,
            [
                (
                    "refs/heads/experiment".to_string(),
                    "2222222222222222222222222222222222222222".to_string()
                ),
                (
                    "refs/heads/main".to_string(),
                    "3333333333333333333333333333333333333333".to_string()
                ),
            ]
            .into()
        )]
        .into()
    );
}

#[test]
fn a_reader_sees_all_of_a_push_or_none_of_it() {
    let (dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let repo = w
        .create_repo(alice.id, "dots", None, true, "main", by(alice.id))
        .unwrap();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    w.advance_clock(60, by(sys.id)).unwrap();

    // A connection of its own reads only what has committed. Reading through
    // it each time the log may have grown shows what a reader woken by any of
    // the push's events can see.
    let db = dir.path().join("world").join("world.db");
    let read_push = move |conn: &rusqlite::Connection| {
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM action_log WHERE type = 'ref:created'),
                    pushed_at_clock_unix, default_branch
             FROM repos WHERE id = ?1",
            [repo.id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    };
    let open_reader = || {
        rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
    };
    let reader = Mutex::new(open_reader());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    w.listen(move |signal| {
        if signal == Signal::LogMayHaveGrown {
            let state = read_push(&reader.lock().unwrap());
            sink.lock().unwrap().push(state);
        }
    });

    w.record_push(repo.id, by(alice.id), &first_push(), Some("trunk"), &[])
        .unwrap();

    let none = (0, None, "main".to_string());
    let all = (2, Some(DEFAULT_T0_UNIX + 60), "trunk".to_string());
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "one signal per write: {seen:?}");
    for state in seen.iter() {
        assert!(
            *state == none || *state == all,
            "a reader saw part of a push: {state:?}"
        );
    }
    assert_eq!(read_push(&open_reader()), all);
}

#[test]
fn log_is_ordered_and_keyset_paginated() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    for _ in 0..5 {
        w.advance_clock(1, by(alice.id)).unwrap();
    }
    let all = w.log_after(0, 100).unwrap();
    let seqs: Vec<i64> = all.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "log_after returns ascending seq");

    let first_two = w.log_after(0, 2).unwrap();
    assert_eq!(first_two.len(), 2);
    let rest = w.log_after(first_two[1].seq, 100).unwrap();
    assert_eq!(rest.first().unwrap().seq, first_two[1].seq + 1);
    // Every event is actor-attributed and clock-stamped.
    assert!(
        all.iter()
            .all(|e| !e.actor.login.is_empty() && e.clock_unix >= DEFAULT_T0_UNIX)
    );
}

/// How many entries the log of the world at `root` holds, counted by a
/// connection of its own.
fn count_log(root: &std::path::Path) -> i64 {
    rusqlite::Connection::open_with_flags(
        root.join("world.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row("SELECT COUNT(*) FROM action_log", [], |r| r.get(0))
    .unwrap()
}

/// The log's total with no `after_seq`, in each order, read alone as
/// `page_size: 0` reads it.
fn totals(w: &World) -> [i64; 2] {
    [LogOrder::OldestFirst, LogOrder::NewestFirst]
        .map(|order| w.log_page(0, order, None, 0).unwrap().1)
}

#[test]
fn the_logs_total_is_the_number_of_entries_it_holds_after_every_write() {
    let (dir, w) = fresh();
    let root = dir.path().join("world");
    let holds = |w: &World| {
        let n = count_log(&root);
        assert_eq!(totals(w), [n, n]);
        n
    };
    let created = holds(&w);
    assert_eq!(
        created, 2,
        "world:created and the system actor's actor:created"
    );

    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let repo = w
        .create_repo(alice.id, "dots", None, true, "main", by(alice.id))
        .unwrap();
    let before_push = holds(&w);
    // One write, three entries: repo:pushed and a ref:created per ref.
    w.record_push(repo.id, by(alice.id), &first_push(), None, &[])
        .unwrap();
    let pushed = holds(&w);
    assert_eq!(pushed, before_push + 3);

    // A write that fails, and one that changes nothing, append nothing and
    // count nothing.
    assert!(matches!(
        w.create_actor("alice", ActorKind::User),
        Err(WorldError::ActorExists(_))
    ));
    w.record_push(repo.id, by(alice.id), &[], None, &[])
        .unwrap();
    assert_eq!(holds(&w), pushed);

    w.advance_clock(60, by(alice.id)).unwrap();
    let n = holds(&w);
    drop(w);
    let w = World::open(&root).unwrap();
    assert_eq!(holds(&w), n, "a reopened world reports the count it kept");
}

/// Every fixture is a world written at an older version, so opening a copy
/// migrates it and sets its count from the log it holds.
#[test]
fn a_migrated_world_reports_the_count_of_its_log() {
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(fixtures).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "db") {
            continue;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("world");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(&path, root.join("world.db")).unwrap();
        let fixture_log = count_log(&root);
        assert!(fixture_log > 0, "{}", path.display());

        let w = World::open(&root).unwrap();
        assert_eq!(totals(&w), [fixture_log; 2], "{}", path.display());
        let (actor, _) = w.create_actor("after-migration", ActorKind::User).unwrap();
        w.advance_clock(1, by(actor.id)).unwrap();
        assert_eq!(totals(&w), [fixture_log + 2; 2], "{}", path.display());
        assert_eq!(count_log(&root), fixture_log + 2);
        seen += 1;
    }
    assert!(seen > 0, "no fixtures in {fixtures}");
}

#[test]
fn a_reader_opens_a_world_under_any_path() {
    // The reader opens through an SQLite URI, where these characters mean
    // something.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("100% sure? #1 ü");
    {
        let w = World::create(&root, DEFAULT_T0_UNIX, &WorldSecret::LOCAL).unwrap();
        w.create_actor("alice", ActorKind::User).unwrap();
    }
    let reader = WorldReader::open(&root).unwrap();
    assert!(
        reader
            .list_actors()
            .unwrap()
            .iter()
            .any(|a| a.login == "alice")
    );
}
