use orrery_world::{
    ActorKind, DEFAULT_T0_UNIX, World, WorldError, WorldReader, WorldSecret, WorldView,
};
use sha2::{Digest, Sha256};

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
    let to = w.advance_clock(3600, sys.id).unwrap();
    assert_eq!(to, DEFAULT_T0_UNIX + 3600);
    assert!(matches!(
        w.advance_clock(0, sys.id),
        Err(WorldError::ClockRegression(0))
    ));
    assert!(matches!(
        w.advance_clock(-5, sys.id),
        Err(WorldError::ClockRegression(-5))
    ));

    let log = w.log_after(0, 100).unwrap();
    let advance = log
        .iter()
        .find(|e| e.action == "clock.advance")
        .expect("advance logged");
    assert_eq!(advance.details["from_unix"], DEFAULT_T0_UNIX);
    assert_eq!(advance.details["to_unix"], DEFAULT_T0_UNIX + 3600);
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
        token.starts_with("orr_"),
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
        "orr_{}",
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
        let (_, issued_token) = w.issue_actor_token("alice", alice.id).unwrap();
        let app = w
            .create_oauth_app("Sign-in", "https://auth.test/callback", alice.id)
            .unwrap();
        let issued_secret = w
            .issue_oauth_client_secret(&app.client_id, alice.id)
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
        w.log_after(0, 1).unwrap()[0].details,
        serde_json::json!({ "t0_unix": DEFAULT_T0_UNIX })
    );
    assert!(!log.contains(&"ab".repeat(32)), "{log}");
    assert!(!log.contains(&token), "{log}");
}

#[test]
fn an_issued_token_acts_as_its_actor_beside_every_earlier_one() {
    let (_dir, w) = fresh();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    let (alice, created) = w.create_actor("alice", ActorKind::User).unwrap();
    let (actor, first) = w.issue_actor_token("alice", sys.id).unwrap();
    assert_eq!(actor, alice);
    // The login is matched as GitHub matches it, and the derivation uses the
    // stored one.
    let (_, second) = w.issue_actor_token("ALICE", sys.id).unwrap();
    assert!(
        first.starts_with("orr_"),
        "tokens are visibly fake: {first}"
    );
    assert_ne!(first, second);
    for token in [&created, &first, &second] {
        assert_eq!(w.actor_by_token(token).unwrap().as_ref(), Some(&alice));
    }

    // Another actor's count is its own, and its tokens are its own.
    let (bob, _) = w.create_actor("bob", ActorKind::User).unwrap();
    let (_, bobs) = w.issue_actor_token("bob", sys.id).unwrap();
    assert_eq!(w.actor_by_token(&bobs).unwrap(), Some(bob));

    assert!(matches!(
        w.issue_actor_token("ghost", sys.id),
        Err(WorldError::NoSuchActor(_))
    ));
    assert!(matches!(
        w.issue_actor_token("orrery", sys.id),
        Err(WorldError::SystemActorToken)
    ));

    let log = w.log_after(0, 100).unwrap();
    let issued: Vec<_> = log
        .iter()
        .filter(|e| e.action == "actor.token.issue")
        .map(|e| (e.actor_login.as_str(), e.details.clone()))
        .collect();
    assert_eq!(
        issued,
        [("alice", 1), ("alice", 2), ("bob", 1)]
            .map(|(login, n)| ("orrery", serde_json::json!({ "login": login, "n": n })))
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
    assert!(w.actor_by_token("orr_bogus").unwrap().is_none());
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
        .create_repo(acme.id, "webapp", Some("the app"), false, "main", alice.id)
        .unwrap();
    let read = w.repo_by_owner_name("acme", "webapp").unwrap().unwrap();
    assert_eq!(created, read);
    assert_eq!(read.owner_login, "acme");
    assert_eq!(read.default_branch, "main");
    assert_eq!(read.pushed_at_clock_unix, None);

    // Case-insensitive lookup and uniqueness, like GitHub's.
    assert!(w.repo_by_owner_name("ACME", "WebApp").unwrap().is_some());
    assert!(matches!(
        w.create_repo(acme.id, "WEBAPP", None, false, "main", alice.id),
        Err(WorldError::RepoExists(_))
    ));
    assert!(matches!(
        w.create_repo(acme.id, "bad name", None, false, "main", alice.id),
        Err(WorldError::InvalidRepoName(_))
    ));
    assert!(matches!(
        w.create_repo(acme.id, "trap.git", None, false, "main", alice.id),
        Err(WorldError::InvalidRepoName(_))
    ));
}

#[test]
fn pushed_at_and_default_branch_updates() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    let repo = w
        .create_repo(alice.id, "dots", None, true, "main", alice.id)
        .unwrap();
    let sys = w.actor_by_login("orrery").unwrap().unwrap();
    w.advance_clock(60, sys.id).unwrap();
    w.touch_pushed(repo.id).unwrap();
    w.set_default_branch(repo.id, "trunk").unwrap();
    let read = w.repo_by_id(repo.id).unwrap().unwrap();
    assert_eq!(read.pushed_at_clock_unix, Some(DEFAULT_T0_UNIX + 60));
    assert_eq!(read.default_branch, "trunk");
    assert!(matches!(
        w.touch_pushed(999),
        Err(WorldError::NoSuchRepo(999))
    ));
}

#[test]
fn log_is_ordered_and_keyset_paginated() {
    let (_dir, w) = fresh();
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    for i in 0..5 {
        w.log_action(alice.id, "test.tick", &serde_json::json!({ "i": i }))
            .unwrap();
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
    // Every entry is actor-attributed and clock-stamped.
    assert!(
        all.iter()
            .all(|e| !e.actor_login.is_empty() && e.clock_unix >= DEFAULT_T0_UNIX)
    );
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
