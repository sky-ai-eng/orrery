//! Snapshots across schema versions: an
//! older snapshot restores, forks and diffs through a migrated copy and is
//! never rewritten itself, and every world-database fixture migrates to the
//! current version, opens, and diffs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use orrery_snapshot::{DiffSide, SnapshotError, WorldDiff, diff};
use orrery_world::test_support::with_appended_migrations;
use orrery_world::{
    ActorKind, Attribution, BASELINE_VERSION, DEFAULT_T0_UNIX, Door, SCHEMA_VERSION, World,
    WorldError, WorldReader, WorldSecret, WorldView,
};

/// A write by `actor_id` as the REST face makes it.
fn by(actor_id: i64) -> Attribution {
    Attribution::new(actor_id, Door::Rest)
}

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../orrery-world/tests/fixtures"
);

const FORK_SECRET: WorldSecret = WorldSecret::from_bytes([0xc3; 32]);

/// Appended, it makes every world at this binary's version one version old.
const NEXT: &[&str] = &["CREATE TABLE next_version (id INTEGER PRIMARY KEY) STRICT;"];

/// Every file under `dir`, by path relative to it, with its contents.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn assert_unchanged(dir: &Path, pristine: &BTreeMap<PathBuf, Vec<u8>>, what: &str) {
    let now = files(dir);
    let changed: BTreeSet<&PathBuf> = pristine
        .keys()
        .chain(now.keys())
        .filter(|p| pristine.get(*p) != now.get(*p))
        .collect();
    assert!(changed.is_empty(), "{what} was rewritten: {changed:?}");
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The sections of a diff that report something.
fn changed(d: &WorldDiff) -> Vec<String> {
    let value = serde_json::to_value(d).unwrap();
    value
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, v)| v.as_array().is_some_and(|a| !a.is_empty()))
        .map(|(k, _)| k.clone())
        .collect()
}

#[test]
fn an_older_snapshot_restores_forks_and_diffs_and_is_never_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let snapshots = dir.path().join("snapshots");
    let world = World::create(
        &dir.path().join("world"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .unwrap();
    let (alice, _) = world.create_actor("alice", ActorKind::User).unwrap();
    let old = orrery_snapshot::create(&world, &snapshots, "old").unwrap();
    world.advance_clock(60, by(alice.id)).unwrap();
    world.create_actor("bob", ActorKind::User).unwrap();
    let pristine = files(&snapshots.join("old"));
    let forked = dir.path().join("fork");

    with_appended_migrations(NEXT, || {
        assert!(matches!(
            WorldReader::open(&snapshots.join("old")),
            Err(WorldError::NeedsMigration { .. })
        ));

        // Reads go through a migrated scratch copy.
        assert_eq!(
            orrery_snapshot::info(&snapshots, "old").unwrap(),
            Some(old.clone())
        );
        assert_eq!(
            orrery_snapshot::list(&snapshots).unwrap(),
            vec![old.clone()]
        );
        let from = orrery_snapshot::open(&snapshots, "old").unwrap();
        let d = diff::diff(
            from.side(),
            DiffSide {
                name: "live",
                view: &world,
                repos_dir: &world.repos_dir(),
            },
        )
        .unwrap();
        let added: Vec<&str> = d.actors_added.iter().map(|a| a.login.as_str()).collect();
        assert_eq!(added, ["bob"]);
        drop(from);
        assert_eq!(entries(&snapshots), ["old"], "the scratch copy is gone");

        // A fork copies, and the copy migrates when it opens.
        orrery_snapshot::fork(&snapshots, "old", &forked, &FORK_SECRET).unwrap();
        let fork = World::open(&forked.join("world")).unwrap();
        assert!(fork.actor_by_login("alice").unwrap().is_some());
        drop(fork);

        // So does a restore.
        orrery_snapshot::restore(&world, &snapshots, "old").unwrap();
        assert_eq!(world.clock().unwrap(), DEFAULT_T0_UNIX);
        assert!(world.actor_by_login("bob").unwrap().is_none());
        world.create_actor("carol", ActorKind::User).unwrap();
        orrery_snapshot::create(&world, &snapshots, "new").unwrap();
    });

    assert_unchanged(&snapshots.join("old"), &pristine, "the snapshot");
    assert!(WorldReader::open(&snapshots.join("old")).is_ok());
    // The fork migrated: it is now newer than the binary.
    assert!(matches!(
        World::open(&forked.join("world")),
        Err(WorldError::SchemaVersion { found, .. }) if found == SCHEMA_VERSION + 1
    ));

    // A snapshot newer than the binary is refused before the live world is
    // touched.
    let other = World::create(
        &dir.path().join("other"),
        DEFAULT_T0_UNIX,
        &WorldSecret::LOCAL,
    )
    .unwrap();
    other.create_actor("dave", ActorKind::User).unwrap();
    assert!(matches!(
        orrery_snapshot::restore(&other, &snapshots, "new"),
        Err(SnapshotError::World(WorldError::SchemaVersion { .. }))
    ));
    assert!(other.actor_by_login("dave").unwrap().is_some());
    assert!(matches!(
        orrery_snapshot::open(&snapshots, "new"),
        Err(SnapshotError::World(WorldError::SchemaVersion { .. }))
    ));
}

/// The fixtures, by schema version.
fn fixtures() -> BTreeMap<i64, PathBuf> {
    std::fs::read_dir(FIXTURES)
        .unwrap()
        .filter_map(|e| {
            let path = e.unwrap().path();
            let name = path.file_name()?.to_str()?;
            let version = name.strip_prefix('v')?.strip_suffix(".db")?.parse().ok()?;
            Some((version, path))
        })
        .collect()
}

fn assert_every_table_has_rows(view: &dyn WorldView, version: i64) {
    let listings = [
        ("actors", view.list_actors().unwrap().len()),
        (
            "actor_addresses",
            view.list_actor_addresses().unwrap().len(),
        ),
        ("repos", view.list_repos().unwrap().len()),
        ("pulls", view.list_pulls().unwrap().len()),
        ("timeline", view.list_timeline().unwrap().len()),
        (
            "review_requests",
            view.list_review_requests().unwrap().len(),
        ),
        ("reviews", view.list_reviews().unwrap().len()),
        ("issue_comments", view.list_issue_comments().unwrap().len()),
        ("check_runs", view.list_check_runs().unwrap().len()),
        ("org_members", view.list_org_members().unwrap().len()),
        ("teams", view.list_teams().unwrap().len()),
        ("team_members", view.list_team_members().unwrap().len()),
        ("oauth_apps", view.list_oauth_apps().unwrap().len()),
        (
            "oauth_authorizations",
            view.list_oauth_authorizations().unwrap().len(),
        ),
    ];
    for (listing, rows) in listings {
        assert!(
            rows > 0,
            "fixture v{version} came through with no {listing}"
        );
    }
    let meta = view.view_meta().unwrap();
    assert!(meta.clock_unix > DEFAULT_T0_UNIX && meta.last_seq > 0);
}

#[test]
fn every_fixture_migrates_opens_and_diffs() {
    let fixtures = fixtures();
    // The baseline's, then one for the version each migration starts from.
    let expected: Vec<i64> = (BASELINE_VERSION..SCHEMA_VERSION.max(BASELINE_VERSION + 1)).collect();
    assert_eq!(
        fixtures.keys().copied().collect::<Vec<_>>(),
        expected,
        "fixtures in {FIXTURES} (see its README.md)"
    );

    for (version, path) in &fixtures {
        let dir = tempfile::tempdir().unwrap();
        let snapshots = dir.path().join("snapshots");
        let name = format!("v{version}");
        std::fs::create_dir_all(snapshots.join(&name)).unwrap();
        std::fs::copy(path, snapshots.join(&name).join("world.db")).unwrap();
        let pristine = files(&snapshots.join(&name));

        // As a world: a copy that migrates when it opens.
        let forked = dir.path().join("fork");
        orrery_snapshot::fork(&snapshots, &name, &forked, &FORK_SECRET).unwrap();
        let world = World::open(&forked.join("world")).unwrap();
        assert_every_table_has_rows(&world, *version);
        // A world that stored no `test_merge_stale` takes each pull's from
        // its `base_moved`.
        if *version < 14 {
            for pull in world.list_pulls().unwrap() {
                assert_eq!(pull.test_merge_stale, pull.base_moved, "fixture v{version}");
            }
        }

        // As a snapshot, read through its own migrated copy: nothing happened
        // between it and the world but the fork's re-key, until the world
        // takes a write.
        let snapshot = orrery_snapshot::open(&snapshots, &name).unwrap();
        assert_every_table_has_rows(snapshot.view(), *version);
        let repos_dir = world.repos_dir();
        let to = || DiffSide {
            name: "live",
            view: &world,
            repos_dir: &repos_dir,
        };
        let unchanged = diff::diff(snapshot.side(), to()).unwrap();
        assert_eq!(
            changed(&unchanged),
            Vec::<String>::new(),
            "fixture v{version}"
        );
        let since: Vec<String> = world
            .log_after(unchanged.log.shared_seq, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.body.type_name().to_string())
            .collect();
        assert_eq!(since, ["world:rekeyed"], "fixture v{version}");
        world
            .create_actor("after-migration", ActorKind::User)
            .unwrap();
        let after = diff::diff(snapshot.side(), to()).unwrap();
        assert_eq!(changed(&after), ["actors_added"], "fixture v{version}");
        assert_eq!(after.log.appended, 2);
        drop(snapshot);

        assert_unchanged(&snapshots.join(&name), &pristine, &name);
    }
}
