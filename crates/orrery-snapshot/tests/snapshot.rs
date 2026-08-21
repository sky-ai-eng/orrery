//! Snapshot mechanics against real world directories: create/list/info,
//! restore (including that the world keeps working afterwards), fork
//! isolation, and the semantic diff — all at the crate API level; the HTTP
//! routes over these are exercised in orrery-server's E2E suite.

use orrery_snapshot::{DiffSide, SnapshotError, diff};
use orrery_world::{ActorKind, DEFAULT_T0_UNIX, NewPull, World, WorldView};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Instance {
    dir: tempfile::TempDir,
    world: World,
}

impl Instance {
    fn new() -> Instance {
        let dir = tempfile::tempdir().expect("tempdir");
        let world =
            World::create(&dir.path().join("world"), DEFAULT_T0_UNIX).expect("create world");
        Instance { dir, world }
    }

    fn snapshots(&self) -> std::path::PathBuf {
        self.dir.path().join("snapshots")
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

    inst.world.advance_clock(3600, alice.id).unwrap();
    let after = orrery_snapshot::create(&inst.world, &inst.snapshots(), "after").unwrap();
    assert_eq!(after.clock_unix, DEFAULT_T0_UNIX + 3600);

    // The listing reads each copy's own database — names sorted, tmp junk
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
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "before").unwrap();

    inst.world.advance_clock(3600, alice.id).unwrap();
    let (bob, bob_token) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let _ = bob;

    orrery_snapshot::restore(&inst.world, &inst.snapshots(), "before").unwrap();

    // The clock rewound, bob never happened, and bob's token resolves to
    // nothing — restore is the one legal way time moves backwards.
    assert_eq!(inst.world.clock().unwrap(), DEFAULT_T0_UNIX);
    assert!(inst.world.actor_by_login("bob").unwrap().is_none());
    assert!(inst.world.actor_by_token(&bob_token).unwrap().is_none());
    assert!(inst.world.actor_by_login("alice").unwrap().is_some());

    // The restored world is fully live: writes, and further snapshots, work.
    inst.world.create_actor("carol", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "again").unwrap();

    assert!(matches!(
        orrery_snapshot::restore(&inst.world, &inst.snapshots(), "ghost"),
        Err(SnapshotError::NotFound(_))
    ));
}

#[test]
fn fork_is_isolated_from_the_source() {
    let inst = Instance::new();
    inst.world.create_actor("alice", ActorKind::User).unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "seed").unwrap();

    let dest = tempfile::tempdir().unwrap();
    let forked_root = orrery_snapshot::fork(&inst.snapshots(), "seed", dest.path()).unwrap();
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
    assert!(orrery_snapshot::fork(&inst.snapshots(), "seed", dest.path()).is_err());
}

#[test]
fn diff_reports_the_interval_between_states() {
    let inst = Instance::new();
    let (alice, _) = inst.world.create_actor("alice", ActorKind::User).unwrap();
    let repo = inst
        .world
        .create_repo(alice.id, "webapp", None, false, "main", alice.id)
        .unwrap();
    orrery_snapshot::create(&inst.world, &inst.snapshots(), "before").unwrap();

    inst.world.advance_clock(3600, alice.id).unwrap();
    let (bob, _) = inst.world.create_actor("bob", ActorKind::User).unwrap();
    let pull = inst
        .world
        .create_pull(NewPull {
            repo_id: repo.id,
            author_id: alice.id,
            title: "add feature",
            body: None,
            head_ref: "feature",
            head_sha: SHA_B,
            base_ref: "main",
            base_sha: SHA_A,
            draft: false,
        })
        .unwrap();
    inst.world
        .create_review(
            pull.id,
            bob.id,
            orrery_world::ReviewState::Approved,
            None,
            None,
        )
        .unwrap();
    inst.world.record_merge(pull.id, bob.id, SHA_A).unwrap();
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
    assert_eq!(d.log.to_seq - d.log.from_seq, d.log.appended);

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
