//! Snapshot mechanics against real world directories: create/list/info,
//! restore (including that the world keeps working afterwards), fork
//! isolation, and the semantic diff — all at the crate API level; the HTTP
//! routes over these are exercised in orrery-server's E2E suite.

use std::collections::{BTreeMap, BTreeSet};

use orrery_snapshot::{DiffSide, SnapshotError, diff};
use orrery_world::{
    ActorEmail, ActorKind, Attribution, DEFAULT_T0_UNIX, Door, EventBody, NewPull, OrgRole, Pull,
    RefChange, RefMove, ReviewState, TeamRole, World, WorldError, WorldSecret, WorldView,
};

/// A write by `actor_id` as the REST face makes it.
fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// The secret every test instance is created with, and its stored form.
const SECRET: WorldSecret = WorldSecret::from_bytes([0x5a; 32]);
const SECRET_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

struct Instance {
    dir: tempfile::TempDir,
    world: World,
}

impl Instance {
    fn new() -> Instance {
        let dir = tempfile::tempdir().expect("tempdir");
        let world = World::create(&dir.path().join("world"), DEFAULT_T0_UNIX, &SECRET)
            .expect("create world");
        Instance { dir, world }
    }

    fn snapshots(&self) -> std::path::PathBuf {
        self.dir.path().join("snapshots")
    }

    fn snapshot(&self, name: &str) {
        orrery_snapshot::create(&self.world, &self.snapshots(), name).expect("create snapshot");
    }

    /// The diff from snapshot `from` to snapshot `to`, or to the live world
    /// when `to` is `None`.
    fn diff(&self, from: &str, to: Option<&str>) -> orrery_snapshot::WorldDiff {
        let open = |name: &str| {
            orrery_world::WorldReader::open(&self.snapshots().join(name)).expect("open snapshot")
        };
        let from_view = open(from);
        let from_repos = self.snapshots().join(from).join("repos");
        let from_side = DiffSide {
            name: from,
            view: &from_view,
            repos_dir: &from_repos,
        };
        match to {
            Some(to) => {
                let to_view = open(to);
                let to_repos = self.snapshots().join(to).join("repos");
                diff::diff(
                    from_side,
                    DiffSide {
                        name: to,
                        view: &to_view,
                        repos_dir: &to_repos,
                    },
                )
                .expect("diff")
            }
            None => diff::diff(
                from_side,
                DiffSide {
                    name: "live",
                    view: &self.world,
                    repos_dir: &self.world.repos_dir(),
                },
            )
            .expect("diff"),
        }
    }

    fn pull(&self, repo_id: i64, author_id: i64, head_ref: &str, draft: bool) -> Pull {
        self.world
            .create_pull(
                by(author_id),
                NewPull {
                    repo_id,
                    title: "change",
                    body: None,
                    head_ref,
                    head_sha: SHA_B,
                    base_ref: "main",
                    base_sha: SHA_A,
                    draft,
                },
            )
            .expect("create pull")
    }
}

fn email(address: &str, primary: bool, verified: bool) -> ActorEmail {
    ActorEmail {
        email: address.to_string(),
        primary,
        verified,
    }
}

#[test]
fn create_list_info_delete() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();

    let before = orrery_snapshot::create(&inst.world, &inst.snapshots(), "before").unwrap();
    assert_eq!(before.name, "before");
    assert_eq!(before.clock_unix, DEFAULT_T0_UNIX);
    assert!(before.last_seq > 0);

    inst.world.advance_clock(3600, by(alice.id)).unwrap();
    let after = orrery_snapshot::create(&inst.world, &inst.snapshots(), "after").unwrap();
    assert_eq!(after.clock_unix, DEFAULT_T0_UNIX + 3600);

    // Whole copies of a snapshot under names no snapshot could have: a
    // crashed `.tmp-*` copy and directories placed by hand.
    for junk in [".tmp-before", "My Snap", &"x".repeat(65)] {
        let copied = std::process::Command::new("cp")
            .arg("-a")
            .arg(inst.snapshots().join("before"))
            .arg(inst.snapshots().join(junk))
            .status()
            .unwrap();
        assert!(copied.success());
    }

    // The listing reads each copy's own database — names sorted, junk
    // excluded, and the info read equals what create returned.
    let listed = orrery_snapshot::list(&inst.snapshots()).unwrap();
    assert_eq!(
        listed,
        vec![after.clone(), before.clone()],
        "sorted by name"
    );
    assert_eq!(
        orrery_snapshot::info(&inst.snapshots(), "before").unwrap(),
        Some(before)
    );

    assert!(matches!(
        orrery_snapshot::create(&inst.world, &inst.snapshots(), "before"),
        Err(SnapshotError::AlreadyExists(_))
    ));
    for bad in ["", "..", "a/b", ".hidden"] {
        assert!(matches!(
            orrery_snapshot::create(&inst.world, &inst.snapshots(), bad),
            Err(SnapshotError::InvalidName(_))
        ));
    }

    orrery_snapshot::delete(&inst.snapshots(), "after").unwrap();
    assert_eq!(orrery_snapshot::list(&inst.snapshots()).unwrap().len(), 1);
    assert!(matches!(
        orrery_snapshot::delete(&inst.snapshots(), "after"),
        Err(SnapshotError::NotFound(_))
    ));
}

#[test]
fn restore_rewinds_and_the_world_keeps_working() {
    let inst = Instance::new();
    let (alice, alice_token) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "before").unwrap();

    inst.world.advance_clock(3600, by(alice.id)).unwrap();
    let (bob, bob_token) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let _ = bob;

    orrery_snapshot::restore(&inst.world, &inst.snapshots(), "before").unwrap();

    // The clock rewound, bob never happened, and bob's token resolves to
    // nothing — restore is the one legal way time moves backwards.
    assert_eq!(inst.world.clock().unwrap(), DEFAULT_T0_UNIX);
    assert!(inst.world.actor_by_login("bob").unwrap().is_none());
    assert!(inst.world.actor_by_token(&bob_token).unwrap().is_none());
    assert!(inst.world.actor_by_login("alice").unwrap().is_some());
    // Restore returns the world to its own state, secret included, so the
    // credentials it had then still work: nothing is re-keyed.
    assert_eq!(
        inst.world.actor_by_token(&alice_token).unwrap(),
        Some(alice)
    );

    // The restored world is fully live: writes, and further snapshots, work.
    inst.world.create_actor("carol", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "again").unwrap();

    // Carol's entry is numbered past the two the restore discarded, and the
    // diff counts the one entry rather than the span of numbers.
    let since = inst.diff("before", Some("again"));
    assert_eq!(since.to.last_seq - since.from.last_seq, 3);
    assert_eq!(since.log.shared_seq, since.from.last_seq);
    assert_eq!(since.log.appended, 1);

    assert!(matches!(
        orrery_snapshot::restore(&inst.world, &inst.snapshots(), "ghost"),
        Err(SnapshotError::NotFound(_))
    ));
}

/// Restores make a world's history a tree, and each state's log the path to
/// it. For every pair of states, a diff's `shared_seq` is the newest entry
/// both logs hold and `appended` counts `to`'s entries above it, including
/// between branches whose numbers interleave.
#[test]
fn a_diffs_shared_seq_is_the_newest_entry_both_logs_hold() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    let log = |world: &World| -> BTreeSet<i64> {
        world
            .log_after(0, 10_000)
            .unwrap()
            .into_iter()
            .map(|e| e.seq)
            .collect()
    };
    let grow = |entries: usize| {
        for _ in 0..entries {
            inst.world.advance_clock(1, by(alice.id)).unwrap();
        }
    };
    let restore = |name: &str| {
        orrery_snapshot::restore(&inst.world, &inst.snapshots(), name).unwrap();
    };
    let mut logs: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    let mut snapshot = |name: &'static str| {
        inst.snapshot(name);
        logs.insert(name, log(&inst.world));
    };

    grow(3);
    snapshot("root");
    grow(20);
    snapshot("left");
    restore("root");
    grow(5);
    snapshot("right");
    // Back on "left"'s branch, numbered above all of "right"'s entries.
    restore("left");
    grow(7);
    snapshot("left-on");
    restore("right");
    grow(1);
    snapshot("right-on");
    restore("root");
    snapshot("root-again");
    restore("left-on");
    grow(2);
    let live = log(&inst.world);

    for (from, from_log) in &logs {
        let tos = logs.iter().map(|(name, l)| (Some(*name), l));
        for (to, to_log) in tos.chain([(None, &live)]) {
            let shared = from_log.intersection(to_log).max().copied().unwrap_or(0);
            let d = inst.diff(from, to);
            assert_eq!(d.log.shared_seq, shared, "{from} to {to:?}");
            assert_eq!(
                d.log.appended,
                to_log.range(shared + 1..).count() as i64,
                "{from} to {to:?}"
            );
        }
    }
}

#[test]
fn fork_is_isolated_from_the_source() {
    let inst = Instance::new();
    inst.world.create_actor("alice", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "seed").unwrap();

    let dest = tempfile::tempdir().unwrap();
    let forked_root =
        orrery_snapshot::fork(&inst.snapshots(), "seed", dest.path(), &SECRET).unwrap();
    let fork = World::open(&forked_root).unwrap();
    assert_eq!(fork.clock().unwrap(), DEFAULT_T0_UNIX);
    assert!(fork.actor_by_login("alice").unwrap().is_some());

    // Divergence stays local on both sides.
    fork.create_actor("fork-only", ActorKind::User).unwrap();
    inst.world
        .create_actor("source-only", ActorKind::User)
        .unwrap();
    assert!(inst.world.actor_by_login("fork-only").unwrap().is_none());
    assert!(fork.actor_by_login("source-only").unwrap().is_none());

    // A second fork into the same instance dir refuses: it already has a
    // world.
    assert!(orrery_snapshot::fork(&inst.snapshots(), "seed", dest.path(), &SECRET).is_err());
}

/// The credentials one scripted history leaves in a world.
struct Seeded {
    alice: orrery_world::Actor,
    alice_token: String,
    issued_token: String,
    client_id: String,
    client_secret: String,
    issued_secret: String,
    oauth_token: String,
    pending_code: String,
}

/// Alice with a token issued beside her first, an OAuth app with a client
/// secret issued beside its first, a code she authorized and exchanged, and a
/// second code still waiting to be.
fn seed(world: &World) -> Seeded {
    let (alice, alice_token) = world.create_actor("alice", ActorKind::User).unwrap();
    let (_, issued_token) = world.issue_actor_token("alice", by(alice.id)).unwrap();
    let app = world
        .create_oauth_app("Sign-in", "https://auth.test/callback", by(alice.id))
        .unwrap();
    let issued_secret = world
        .issue_oauth_client_secret(&app.client_id, by(alice.id))
        .unwrap();
    let issue = || {
        world
            .issue_oauth_code(&app, &alice, "https://auth.test/callback", "user")
            .unwrap()
    };
    let spent = issue();
    let oauth_token = world
        .exchange_oauth_code(&app.client_id, &app.client_secret, &spent, None)
        .unwrap()
        .token;
    let pending_code = issue();
    Seeded {
        alice,
        alice_token,
        issued_token,
        client_id: app.client_id,
        client_secret: app.client_secret,
        issued_secret,
        oauth_token,
        pending_code,
    }
}

#[test]
fn a_fork_is_rekeyed_and_accepts_none_of_its_sources_credentials() {
    let inst = Instance::new();
    let source = seed(&inst.world);
    inst.snapshot("seed");

    // A twin: the same history played into a world created with the fork's
    // secret holds exactly the credentials re-keying must derive.
    let fork_secret = WorldSecret::from_bytes([0xc3; 32]);
    let twin_dir = tempfile::tempdir().unwrap();
    let twin_world = World::create(
        &twin_dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &fork_secret,
    )
    .unwrap();
    let twin = seed(&twin_world);

    let dest = tempfile::tempdir().unwrap();
    let forked_root =
        orrery_snapshot::fork(&inst.snapshots(), "seed", dest.path(), &fork_secret).unwrap();
    let fork = World::open(&forked_root).unwrap();

    // Tokens: the source's are refused, the re-derived ones act as alice.
    for (which, token) in [
        ("alice", &source.alice_token),
        ("issued", &source.issued_token),
        ("oauth", &source.oauth_token),
    ] {
        assert!(
            fork.actor_by_token(token).unwrap().is_none(),
            "the fork accepts the source's {which} token"
        );
    }
    for token in [&twin.alice_token, &twin.issued_token, &twin.oauth_token] {
        assert_eq!(
            fork.actor_by_token(token).unwrap().as_ref(),
            Some(&source.alice)
        );
    }

    // The app keeps its public client id and gets the re-derived secret.
    assert_eq!(twin.client_id, source.client_id);
    let app = fork
        .oauth_app_by_client_id(&source.client_id)
        .unwrap()
        .unwrap();
    assert_eq!(app.client_secret, twin.client_secret);
    assert_ne!(app.client_secret, source.client_secret);
    for secret in [&source.client_secret, &source.issued_secret] {
        assert!(matches!(
            fork.exchange_oauth_code(&source.client_id, secret, &twin.pending_code, None),
            Err(WorldError::OAuthClientCredentials)
        ));
    }

    // The pending code: the source's value is dead in the fork, the
    // re-derived one exchanges under the re-derived issued secret, and the
    // token it buys means nothing to the source.
    assert!(matches!(
        fork.exchange_oauth_code(
            &app.client_id,
            &app.client_secret,
            &source.pending_code,
            None
        ),
        Err(WorldError::OAuthBadCode)
    ));
    let exchanged = fork
        .exchange_oauth_code(
            &app.client_id,
            &twin.issued_secret,
            &twin.pending_code,
            None,
        )
        .unwrap();
    assert_eq!(exchanged.actor, source.alice);
    assert!(
        inst.world
            .actor_by_token(&exchanged.token)
            .unwrap()
            .is_none()
    );

    // Credentials issued in the fork are the twin's, and work only there.
    let (_, fork_token) = fork
        .issue_actor_token("alice", by(source.alice.id))
        .unwrap();
    let (_, twin_token) = twin_world
        .issue_actor_token("alice", by(twin.alice.id))
        .unwrap();
    assert_eq!(fork_token, twin_token);
    assert_eq!(
        fork.actor_by_token(&fork_token).unwrap(),
        Some(source.alice.clone())
    );
    assert!(inst.world.actor_by_token(&fork_token).unwrap().is_none());
    let fork_secret_issued = fork
        .issue_oauth_client_secret(&app.client_id, by(source.alice.id))
        .unwrap();
    assert_eq!(
        fork_secret_issued,
        twin_world
            .issue_oauth_client_secret(&twin.client_id, by(twin.alice.id))
            .unwrap()
    );

    // The source is untouched.
    for token in [&source.alice_token, &source.issued_token] {
        assert_eq!(
            inst.world.actor_by_token(token).unwrap(),
            Some(source.alice.clone())
        );
    }

    // One event, the system actor's through the CLI, naming no credential.
    let log = fork.log_after(0, 500).unwrap();
    let rekeys: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::WorldRekeyed(r) => Some((e, r)),
            _ => None,
        })
        .collect();
    assert_eq!(rekeys.len(), 1);
    assert_eq!(
        (rekeys[0].0.actor.login.as_str(), rekeys[0].0.door),
        ("orrery", Door::Cli)
    );
    assert_eq!(
        serde_json::to_value(rekeys[0].1.rekeyed).unwrap(),
        serde_json::json!({
            "tokens": 3,
            "oauth_apps": 1,
            "oauth_app_secrets": 1,
            "oauth_codes": 1,
        })
    );
    let wire = serde_json::to_string(&log).unwrap();
    for (which, secret) in [
        ("fork's world secret", &"c3".repeat(32)),
        ("alice token", &twin.alice_token),
        ("issued token", &twin.issued_token),
        ("oauth token", &twin.oauth_token),
        ("client secret", &twin.client_secret),
        ("issued secret", &twin.issued_secret),
        ("pending code", &twin.pending_code),
        ("fork token", &fork_token),
        ("fork's issued secret", &fork_secret_issued),
    ] {
        assert!(!wire.contains(secret.as_str()), "the log leaks the {which}");
    }
}

#[test]
fn diff_reports_the_interval_between_states() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    let repo = inst
        .world
        .create_repo(alice.id, "webapp", None, false, "main", by(alice.id))
        .unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "before").unwrap();

    inst.world.advance_clock(3600, by(alice.id)).unwrap();
    let (bob, _) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let pull = inst
        .world
        .create_pull(
            by(alice.id),
            NewPull {
                repo_id: repo.id,
                title: "add feature",
                body: None,
                head_ref: "feature",
                head_sha: SHA_B,
                base_ref: "main",
                base_sha: SHA_A,
                draft: false,
            },
        )
        .unwrap();
    inst.world
        .create_review(
            pull.id,
            by(bob.id),
            orrery_world::ReviewState::Approved,
            None,
            None,
        )
        .unwrap();
    let merge = RefMove {
        name: "refs/heads/main",
        old: SHA_A,
        new: SHA_C,
        repins: Vec::new(),
        change: RefChange::Moved,
        forced: false,
        commits: vec![SHA_C.to_string()],
        commit_count: 1,
    };
    inst.world
        .record_merge(pull.id, by(bob.id), &merge)
        .unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "after").unwrap();

    let open = |name: &str| {
        orrery_world::WorldReader::open(&inst.snapshots().join(name)).expect("open snapshot")
    };
    let (before, after) = (open("before"), open("after"));
    let before_repos = inst.snapshots().join("before").join("repos");
    let after_repos = inst.snapshots().join("after").join("repos");
    let d = diff::diff(
        DiffSide {
            name: "before",
            view: &before,
            repos_dir: &before_repos,
        },
        DiffSide {
            name: "after",
            view: &after,
            repos_dir: &after_repos,
        },
    )
    .unwrap();

    assert_eq!(d.from.clock_unix, DEFAULT_T0_UNIX);
    assert_eq!(d.to.clock_unix, DEFAULT_T0_UNIX + 3600);
    assert_eq!(d.actors_added.len(), 1);
    assert_eq!(d.actors_added[0].login, "bob");
    assert!(d.repos_created.is_empty());
    // Opened and merged within one interval: both facts reported.
    assert_eq!(d.pulls_opened.len(), 1);
    assert_eq!(d.pulls_opened[0].number, pull.number);
    assert_eq!(d.pulls_merged.len(), 1);
    assert_eq!(d.pulls_merged[0].merged_by.as_deref(), Some("bob"));
    assert!(
        d.pulls_closed.is_empty(),
        "merged is not closed-without-merge"
    );
    assert_eq!(d.reviews_added.len(), 1);
    assert_eq!(d.reviews_added[0].reviewer, "bob");
    assert_eq!(d.reviews_added[0].state, "approved");
    assert!(d.log.appended > 0);
    assert_eq!(d.to.last_seq - d.from.last_seq, d.log.appended);

    // The live world equals the "after" snapshot: same diff against "before",
    // and an empty diff against "after".
    let live = diff::diff(
        DiffSide {
            name: "before",
            view: &before,
            repos_dir: &before_repos,
        },
        DiffSide {
            name: "live",
            view: &inst.world,
            repos_dir: &inst.world.repos_dir(),
        },
    )
    .unwrap();
    assert_eq!(live.pulls_merged.len(), 1);
    assert_eq!(live.log.appended, d.log.appended);

    let idle = diff::diff(
        DiffSide {
            name: "after",
            view: &after,
            repos_dir: &after_repos,
        },
        DiffSide {
            name: "live",
            view: &inst.world,
            repos_dir: &inst.world.repos_dir(),
        },
    )
    .unwrap();
    assert!(idle.actors_added.is_empty());
    assert!(idle.pulls_opened.is_empty());
    assert!(idle.ref_changes.is_empty());
    assert_eq!(idle.log.appended, 0);
}

#[test]
fn snapshot_captures_a_quiesced_database() {
    // The copy must be openable standalone — WorldReader reads the copy's
    // main db file; a snapshot taken without checkpointing would strand the
    // fresh rows in a WAL the reader may not replay identically.
    let inst = Instance::new();
    for i in 0..50 {
        inst.world
            .create_actor(&format!("user{i}"), ActorKind::User)
            .unwrap();
    }
    let info = orrery_snapshot::create(&inst.world, &inst.snapshots(), "busy").unwrap();
    let reader =
        orrery_world::WorldReader::open(&inst.snapshots().join("busy")).expect("open copy");
    assert_eq!(reader.list_actors().unwrap().len(), 51, "50 users + system");
    assert_eq!(reader.view_meta().unwrap().last_seq, info.last_seq);
}

#[test]
fn diff_reports_new_actors_with_their_addresses_and_changed_addresses() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    inst.snapshot("before");

    inst.world.create_actor("bob", ActorKind::User).unwrap();
    inst.world
        .set_actor_emails(
            "alice",
            &[
                email("alice@acme.test", true, true),
                email("alice@old.test", false, false),
            ],
            by(alice.id),
        )
        .unwrap();

    let d = inst.diff("before", None);
    assert_eq!(d.actors_added.len(), 1);
    assert_eq!(d.actors_added[0].login, "bob");
    assert_eq!(
        d.actors_added[0].emails,
        vec![email("bob@orrery.invalid", true, true)]
    );
    assert_eq!(d.emails_changed.len(), 1, "bob's addresses arrive with bob");
    let changed = &d.emails_changed[0];
    assert_eq!(changed.login, "alice");
    assert_eq!(
        changed.from,
        vec![email("alice@orrery.invalid", true, true)]
    );
    assert_eq!(
        changed.to,
        vec![
            email("alice@acme.test", true, true),
            email("alice@old.test", false, false)
        ]
    );

    // Writing the same set again, in another case, is no change.
    inst.snapshot("after");
    inst.world
        .set_actor_emails(
            "alice",
            &[
                email("alice@old.test", false, false),
                email("ALICE@acme.test", true, true),
            ],
            by(alice.id),
        )
        .unwrap();
    assert!(inst.diff("after", None).emails_changed.is_empty());
}

#[test]
fn diff_reports_oauth_apps_and_authorizations_without_their_secrets() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    inst.snapshot("before");

    let (_, bob_token) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let app = inst
        .world
        .create_oauth_app("GoTrue", "https://auth.test/callback", by(alice.id))
        .unwrap();
    let code = inst
        .world
        .issue_oauth_code(&app, &alice, "https://auth.test/callback", "user:email")
        .unwrap();
    inst.snapshot("issued");
    let token = inst
        .world
        .exchange_oauth_code(&app.client_id, &app.client_secret, &code, None)
        .unwrap();

    let issued = inst.diff("before", Some("issued"));
    assert_eq!(issued.oauth_apps_added.len(), 1);
    assert_eq!(issued.oauth_apps_added[0].name, "GoTrue");
    assert_eq!(issued.oauth_apps_added[0].client_id, app.client_id);
    assert_eq!(issued.authorizations_issued.len(), 1);
    let grant = &issued.authorizations_issued[0];
    assert_eq!(grant.client_id, app.client_id);
    assert_eq!(grant.app, "GoTrue");
    assert_eq!(grant.actor, "alice");
    assert_eq!(grant.scope, "user:email");
    assert!(issued.authorizations_exchanged.is_empty());

    let exchanged = inst.diff("issued", None);
    assert!(exchanged.oauth_apps_added.is_empty());
    assert!(exchanged.authorizations_issued.is_empty());
    assert_eq!(exchanged.authorizations_exchanged.len(), 1);
    assert_eq!(exchanged.authorizations_exchanged[0].actor, "alice");

    // Issued and exchanged within one interval: both facts reported.
    let whole = inst.diff("before", None);
    assert_eq!(whole.authorizations_issued.len(), 1);
    assert_eq!(whole.authorizations_exchanged.len(), 1);

    let wire = serde_json::to_string(&whole).unwrap();
    for (which, secret) in [
        ("world secret", SECRET_HEX),
        ("bob's token", &bob_token),
        ("client secret", &app.client_secret),
        ("code", &code),
        ("oauth token", &token.token),
    ] {
        assert!(!wire.contains(secret), "diff leaks the {which}");
    }
}

#[test]
fn diff_reports_labels_added_and_removed() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    let repo = inst
        .world
        .create_repo(alice.id, "webapp", None, false, "main", by(alice.id))
        .unwrap();
    let pull = inst.pull(repo.id, alice.id, "feature", false);
    inst.world
        .add_pull_labels(pull.id, by(alice.id), &["bug", "triage"])
        .unwrap();
    inst.snapshot("before");

    inst.world
        .remove_pull_label(pull.id, by(alice.id), "triage")
        .unwrap();
    inst.world
        .add_pull_labels(pull.id, by(alice.id), &["needs-review", "BUG"])
        .unwrap();
    let fresh = inst.pull(repo.id, alice.id, "other", false);
    inst.world
        .add_pull_labels(fresh.id, by(alice.id), &["docs"])
        .unwrap();

    let d = inst.diff("before", None);
    let added: Vec<(i64, &str)> = d
        .labels_added
        .iter()
        .map(|l| {
            assert_eq!(l.repo, "alice/webapp");
            (l.number, l.label.as_str())
        })
        .collect();
    assert_eq!(
        added,
        vec![(pull.number, "needs-review"), (fresh.number, "docs")],
        "a label is one label whatever its case; a new pull's labels count"
    );
    let removed: Vec<(i64, &str)> = d
        .labels_removed
        .iter()
        .map(|l| (l.number, l.label.as_str()))
        .collect();
    assert_eq!(removed, vec![(pull.number, "triage")]);
}

#[test]
fn diff_reports_ready_for_review_and_conversion_to_draft() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    let (bob, _) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let repo = inst
        .world
        .create_repo(alice.id, "webapp", None, false, "main", by(alice.id))
        .unwrap();
    let readied = inst.pull(repo.id, alice.id, "one", true);
    let round_trip = inst.pull(repo.id, alice.id, "two", true);
    let converted = inst.pull(repo.id, alice.id, "three", false);
    inst.snapshot("before");

    inst.world
        .set_pull_draft(readied.id, by(alice.id), false)
        .unwrap();
    inst.world
        .set_pull_draft(round_trip.id, by(bob.id), false)
        .unwrap();
    inst.world
        .set_pull_draft(round_trip.id, by(bob.id), true)
        .unwrap();
    inst.world
        .set_pull_draft(converted.id, by(alice.id), true)
        .unwrap();

    let d = inst.diff("before", None);
    let ready: Vec<(i64, &str)> = d
        .pulls_ready_for_review
        .iter()
        .map(|r| {
            assert_eq!(r.repo, "alice/webapp");
            (r.number, r.by.as_str())
        })
        .collect();
    assert_eq!(
        ready,
        vec![(readied.number, "alice"), (round_trip.number, "bob")]
    );
    let drafted: Vec<i64> = d
        .pulls_converted_to_draft
        .iter()
        .map(|c| c.number)
        .collect();
    assert_eq!(drafted, vec![converted.number]);
    assert!(d.pulls_edited.is_empty(), "a draft flip is not an edit");
}

#[test]
fn diff_reports_org_memberships_and_roles() {
    let inst = Instance::new();
    let (admin, _) = inst.world.create_actor("admin", ActorKind::User).unwrap();
    inst.world.create_actor("acme", ActorKind::Org).unwrap();
    inst.world.create_actor("dave", ActorKind::User).unwrap();
    inst.world.create_actor("carol", ActorKind::User).unwrap();
    inst.world
        .set_org_membership("acme", "dave", OrgRole::Member, by(admin.id))
        .unwrap();
    inst.snapshot("before");

    inst.world
        .set_org_membership("acme", "dave", OrgRole::Admin, by(admin.id))
        .unwrap();
    inst.world
        .set_org_membership("acme", "carol", OrgRole::Member, by(admin.id))
        .unwrap();

    let d = inst.diff("before", None);
    assert_eq!(d.org_memberships_added.len(), 1);
    let added = &d.org_memberships_added[0];
    assert_eq!(
        (
            added.org.as_str(),
            added.login.as_str(),
            added.role.as_str()
        ),
        ("acme", "carol", "member")
    );
    assert_eq!(d.org_roles_changed.len(), 1);
    let changed = &d.org_roles_changed[0];
    assert_eq!(
        (
            changed.login.as_str(),
            changed.from.as_str(),
            changed.to.as_str()
        ),
        ("dave", "member", "admin")
    );
    assert!(d.org_memberships_removed.is_empty());
}

#[test]
fn diff_reports_teams_and_team_memberships() {
    let inst = Instance::new();
    let (admin, _) = inst.world.create_actor("admin", ActorKind::User).unwrap();
    inst.world.create_actor("acme", ActorKind::Org).unwrap();
    for login in ["dave", "carol"] {
        inst.world.create_actor(login, ActorKind::User).unwrap();
        inst.world
            .set_org_membership("acme", login, OrgRole::Member, by(admin.id))
            .unwrap();
    }
    inst.world
        .create_team("acme", "Platform", None, by(admin.id))
        .unwrap();
    inst.world
        .set_team_membership("acme", "platform", "dave", TeamRole::Member, by(admin.id))
        .unwrap();
    inst.world
        .set_team_membership("acme", "platform", "carol", TeamRole::Member, by(admin.id))
        .unwrap();
    inst.snapshot("before");

    inst.world
        .set_team_membership(
            "acme",
            "platform",
            "dave",
            TeamRole::Maintainer,
            by(admin.id),
        )
        .unwrap();
    inst.world
        .remove_team_membership("acme", "platform", "carol", by(admin.id))
        .unwrap();
    inst.world
        .create_team("acme", "Release", None, by(admin.id))
        .unwrap();
    inst.world
        .set_team_membership(
            "acme",
            "release",
            "carol",
            TeamRole::Maintainer,
            by(admin.id),
        )
        .unwrap();

    let d = inst.diff("before", None);
    let created: Vec<(&str, &str, &str)> = d
        .teams_created
        .iter()
        .map(|t| (t.org.as_str(), t.slug.as_str(), t.name.as_str()))
        .collect();
    assert_eq!(created, vec![("acme", "release", "Release")]);
    assert!(d.teams_deleted.is_empty());
    let added: Vec<(&str, &str, &str)> = d
        .team_memberships_added
        .iter()
        .map(|m| (m.team.as_str(), m.login.as_str(), m.role.as_str()))
        .collect();
    assert_eq!(added, vec![("release", "carol", "maintainer")]);
    let removed: Vec<(&str, &str)> = d
        .team_memberships_removed
        .iter()
        .map(|m| (m.team.as_str(), m.login.as_str()))
        .collect();
    assert_eq!(removed, vec![("platform", "carol")]);
    assert_eq!(d.team_roles_changed.len(), 1);
    let changed = &d.team_roles_changed[0];
    assert_eq!(
        (
            changed.org.as_str(),
            changed.team.as_str(),
            changed.login.as_str(),
            changed.from.as_str(),
            changed.to.as_str()
        ),
        ("acme", "platform", "dave", "member", "maintainer")
    );
}

#[test]
fn diff_reports_a_reused_team_slug_as_a_deletion_and_a_creation() {
    let inst = Instance::new();
    let (admin, _) = inst.world.create_actor("admin", ActorKind::User).unwrap();
    inst.world.create_actor("acme", ActorKind::Org).unwrap();
    inst.world.create_actor("dave", ActorKind::User).unwrap();
    inst.world
        .set_org_membership("acme", "dave", OrgRole::Member, by(admin.id))
        .unwrap();
    inst.world
        .create_team("acme", "Reviewers", None, by(admin.id))
        .unwrap();
    inst.world
        .set_team_membership("acme", "reviewers", "dave", TeamRole::Member, by(admin.id))
        .unwrap();
    inst.snapshot("before");

    inst.world
        .delete_team("acme", "reviewers", by(admin.id))
        .unwrap();
    inst.world
        .create_team("acme", "Reviewers", None, by(admin.id))
        .unwrap();
    inst.world
        .set_team_membership("acme", "reviewers", "dave", TeamRole::Member, by(admin.id))
        .unwrap();

    let d = inst.diff("before", None);
    let slugs = |ts: &[orrery_snapshot::diff::TeamRef]| -> Vec<String> {
        ts.iter().map(|t| format!("{}/{}", t.org, t.slug)).collect()
    };
    assert_eq!(slugs(&d.teams_deleted), vec!["acme/reviewers"]);
    assert_eq!(slugs(&d.teams_created), vec!["acme/reviewers"]);
    // Dave's membership ended with the old team and began on the new one;
    // it is not one unchanged membership.
    let names = |ms: &[orrery_snapshot::diff::TeamMembershipChange]| -> Vec<String> {
        ms.iter()
            .map(|m| format!("{}/{} {}", m.org, m.team, m.login))
            .collect()
    };
    assert_eq!(
        names(&d.team_memberships_removed),
        vec!["acme/reviewers dave"]
    );
    assert_eq!(
        names(&d.team_memberships_added),
        vec!["acme/reviewers dave"]
    );
    assert!(d.team_roles_changed.is_empty());
}

#[test]
fn diff_reports_review_requests_added_and_removed() {
    use orrery_snapshot::diff::Reviewer;

    let inst = Instance::new();
    let (admin, _) = inst.world.create_actor("admin", ActorKind::User).unwrap();
    let (acme, _) = inst.world.create_actor("acme", ActorKind::Org).unwrap();
    let mut people = std::collections::BTreeMap::new();
    for login in ["alice", "dave", "carol", "erin"] {
        let (actor, _) = inst.world.create_actor(login, ActorKind::User).unwrap();
        inst.world
            .set_org_membership("acme", login, OrgRole::Member, by(admin.id))
            .unwrap();
        people.insert(login, actor);
    }
    let alice = &people["alice"];
    let repo = inst
        .world
        .create_repo(acme.id, "webapp", None, false, "main", by(alice.id))
        .unwrap();
    inst.world
        .create_team("acme", "Reviewers", None, by(admin.id))
        .unwrap();
    inst.world
        .create_team("acme", "Release", None, by(admin.id))
        .unwrap();
    let pull = inst.pull(repo.id, alice.id, "feature", false);
    inst.snapshot("before");

    inst.world
        .request_reviewers(pull.id, by(alice.id), &["dave", "carol"], &["reviewers"])
        .unwrap();
    inst.snapshot("requested");

    // Dave's review clears his request, carol's is withdrawn, the team's
    // goes with the team, and erin and Release are newly asked.
    inst.world
        .create_review(
            pull.id,
            by(people["dave"].id),
            ReviewState::Commented,
            None,
            None,
        )
        .unwrap();
    inst.world
        .remove_review_requests(pull.id, by(alice.id), &["carol"], &[])
        .unwrap();
    inst.world
        .delete_team("acme", "reviewers", by(admin.id))
        .unwrap();
    inst.world
        .request_reviewers(pull.id, by(alice.id), &["erin"], &["release"])
        .unwrap();

    let names = |rs: &[orrery_snapshot::diff::ReviewRequestChange]| -> Vec<String> {
        rs.iter()
            .map(|r| {
                assert_eq!((r.repo.as_str(), r.number), ("acme/webapp", pull.number));
                match &r.reviewer {
                    Reviewer::User(login) => format!("user:{login}"),
                    Reviewer::Team(team) => format!("team:{team}"),
                }
            })
            .collect()
    };

    let first = inst.diff("before", Some("requested"));
    assert_eq!(
        names(&first.review_requests_added),
        vec!["user:carol", "user:dave", "team:acme/reviewers"]
    );
    assert!(first.review_requests_removed.is_empty());

    let second = inst.diff("requested", None);
    assert_eq!(
        names(&second.review_requests_added),
        vec!["user:erin", "team:acme/release"]
    );
    assert_eq!(
        names(&second.review_requests_removed),
        vec!["user:carol", "user:dave", "team:acme/reviewers"]
    );

    let wire = serde_json::to_value(&second).unwrap();
    assert_eq!(
        wire["review_requests_added"][1]["reviewer"],
        serde_json::json!({ "team": "acme/release" })
    );
}
