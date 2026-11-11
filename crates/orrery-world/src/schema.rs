//! The world schema: a baseline, applied whole when a world is created, and
//! the forward migrations that carry an existing world from the version it
//! was written at to this binary's.
//!
//! `user_version` counts them: the baseline stamps `BASELINE_VERSION`, and
//! each migration adds one. World databases outlive the binary that wrote
//! them, so every schema change is a new forward migration with a fixture
//! (`tests/fixtures/README.md`), and the baseline never changes.

use rusqlite::{Connection, TransactionBehavior};

use crate::{Result, WorldError};

/// The version a database is at once the baseline is applied, and the oldest
/// one this binary migrates from.
pub const BASELINE_VERSION: i64 = 6;

/// Forward migrations, oldest first, each one SQL batch. `MIGRATIONS[i]`
/// takes a database from version `BASELINE_VERSION + i` to the next.
const MIGRATIONS: &[&str] = &[
    // 7: the real time each action-log entry was appended at, in Unix
    // milliseconds. Entries appended before this version never recorded one,
    // so they hold NULL.
    "ALTER TABLE action_log ADD COLUMN wall_unix_ms INTEGER;",
    // 8: at most one open pull per head and base in a repository. Every write
    // that opens or retargets a pull checks this first and answers its own
    // error; the index makes a write that skips the check fail instead of
    // leaving two open.
    "CREATE UNIQUE INDEX pulls_one_open_per_pair ON pulls (repo_id, head_ref, base_ref)
     WHERE state = 'open';",
    // 9: the timeline items a push records when it deletes or restores a
    // pull's branch. `ref_name` is the branch a deletion item names, set
    // exactly for those kinds. SQLite cannot alter a CHECK, so the table is
    // rebuilt with every row and id it held; nothing references it. And
    // `pulls.base_moved`: whether the base branch has moved since the pull's
    // head or base was last pinned, which decides whether a closed pull whose
    // head branch is gone still has a test merge to report.
    "ALTER TABLE pulls ADD COLUMN base_moved INTEGER NOT NULL DEFAULT 0
         CHECK (base_moved IN (0, 1));
     CREATE TABLE pull_timeline_next (
        id          INTEGER PRIMARY KEY,
        pull_id     INTEGER NOT NULL REFERENCES pulls(id),
        kind        TEXT NOT NULL CHECK (kind IN ('labeled', 'unlabeled', 'ready_for_review',
                                                  'review_requested', 'review_request_removed',
                                                  'closed', 'head_ref_deleted',
                                                  'base_ref_deleted', 'head_ref_restored')),
        label       TEXT,
        reviewer_id INTEGER REFERENCES actors(id),
        team_id     INTEGER,
        team_slug   TEXT,
        actor_id    INTEGER NOT NULL REFERENCES actors(id),
        clock_unix  INTEGER NOT NULL,
        ref_name    TEXT,
        CHECK ((kind IN ('labeled', 'unlabeled')) = (label IS NOT NULL)),
        CHECK ((kind IN ('head_ref_deleted', 'base_ref_deleted')) = (ref_name IS NOT NULL)),
        CHECK ((team_id IS NULL) = (team_slug IS NULL)),
        CHECK ((kind IN ('review_requested', 'review_request_removed'))
               = ((reviewer_id IS NULL) <> (team_id IS NULL))),
        CHECK (kind IN ('review_requested', 'review_request_removed')
               OR (reviewer_id IS NULL AND team_id IS NULL))
     ) STRICT;
     INSERT INTO pull_timeline_next (id, pull_id, kind, label, reviewer_id, team_id, team_slug,
                                     actor_id, clock_unix)
         SELECT id, pull_id, kind, label, reviewer_id, team_id, team_slug, actor_id, clock_unix
         FROM pull_timeline;
     DROP TABLE pull_timeline;
     ALTER TABLE pull_timeline_next RENAME TO pull_timeline;",
    // 10: the log holds events (spec/events.md). `type` and `body` are an
    // event's type and the rest of its type's fields; `write_seq` and
    // `write_events` place it in its write; `door` and `app_id` say how the
    // write reached the world.
    "ALTER TABLE action_log RENAME COLUMN action TO type;
     ALTER TABLE action_log RENAME COLUMN details TO body;
     ALTER TABLE action_log ADD COLUMN write_seq INTEGER;
     ALTER TABLE action_log ADD COLUMN write_events INTEGER CHECK (write_events >= 1);
     ALTER TABLE action_log ADD COLUMN door TEXT
         CHECK (door IN ('rest', 'graphql', 'git', 'web', 'control', 'cli'));
     ALTER TABLE action_log ADD COLUMN app_id INTEGER REFERENCES oauth_apps(id);",
    // 11: how many entries the action log holds, so the whole log's count is
    // read without reading the log. The append adds to it in the transaction
    // that adds the entries; a world migrated here starts from one count of
    // the log it holds.
    "ALTER TABLE world ADD COLUMN log_entries INTEGER NOT NULL DEFAULT 0
         CHECK (log_entries >= 0);
     UPDATE world SET log_entries = (SELECT COUNT(*) FROM action_log);",
    // 12: the SSH keys accounts sign commits with, and the verdicts a push
    // stored for the commits it brought in. A key is held by one account,
    // stored as its type and base64 blob with no comment. AUTOINCREMENT so a
    // deleted key's id never comes back as another key's. A verification
    // names the key that made it by id, not by reference, since the key can
    // be deleted after it.
    "CREATE TABLE ssh_signing_keys (
        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
        actor_id           INTEGER NOT NULL REFERENCES actors(id),
        public_key         TEXT NOT NULL UNIQUE,
        title              TEXT,
        created_clock_unix INTEGER NOT NULL
     ) STRICT;
     CREATE INDEX ssh_signing_keys_by_actor ON ssh_signing_keys (actor_id, id);
     CREATE TABLE commit_verifications (
        repo_id             INTEGER NOT NULL REFERENCES repos(id),
        sha                 TEXT NOT NULL,
        signer_id           INTEGER NOT NULL REFERENCES actors(id),
        key_id              INTEGER NOT NULL,
        verified_clock_unix INTEGER NOT NULL,
        PRIMARY KEY (repo_id, sha)
     ) STRICT;",
    // 13: indexes for the reads made per pull and per commit: a commit's
    // check runs, and a pull's reviews, comments, and timeline. Without them
    // each read scans its whole table. Every index entry ends in its row's
    // id, so these reads, which all order by `id`, need no sort.
    "CREATE INDEX check_runs_by_commit ON check_runs (repo_id, head_sha);
     CREATE INDEX reviews_by_pull ON reviews (pull_id);
     CREATE INDEX issue_comments_by_pull ON issue_comments (pull_id);
     CREATE INDEX pull_timeline_by_pull ON pull_timeline (pull_id);",
];

/// The version this binary reads and writes.
pub const SCHEMA_VERSION: i64 = BASELINE_VERSION + MIGRATIONS.len() as i64;

// State vocabulary is neutral (rule 9: vendor-neutral state, vendor faces) —
// faces render casing like "APPROVED" or "CHANGES_REQUESTED" at the wire,
// never the store. `pulls.base_sha` is pinned at PR creation (and on base
// retarget), matching GitHub's static `base.sha`; `head_sha` tracks the branch
// via push observation, matching GitHub's synchronize behavior.
const BASELINE: &str = r#"
    -- `secret` keys every credential the world derives (credentials.rs): 32
    -- bytes as lowercase hex. It is never logged and no read returns it.
    CREATE TABLE world (
        id         INTEGER PRIMARY KEY CHECK (id = 1),
        clock_unix INTEGER NOT NULL,
        secret     TEXT NOT NULL CHECK (length(secret) = 64 AND secret NOT GLOB '*[^0-9a-f]*')
    ) STRICT;

    CREATE TABLE actors (
        id                 INTEGER PRIMARY KEY,
        login              TEXT NOT NULL COLLATE NOCASE UNIQUE,
        kind               TEXT NOT NULL CHECK (kind IN ('user', 'bot', 'org', 'system')),
        created_clock_unix INTEGER NOT NULL
    ) STRICT;

    -- `origin` is what a token was derived from, which its value cannot say:
    -- `actor` for the token minted with its actor, `actor:{n}` for the nth
    -- one issued for it since, `oauth:{code_id}` for one an authorization
    -- code was exchanged for. Re-keying re-derives from it.
    CREATE TABLE tokens (
        token              TEXT PRIMARY KEY,
        actor_id           INTEGER NOT NULL REFERENCES actors(id),
        origin             TEXT NOT NULL CHECK (origin = 'actor'
                                                OR origin GLOB 'actor:[1-9]*'
                                                OR origin GLOB 'oauth:[1-9]*'),
        created_clock_unix INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE repos (
        id                   INTEGER PRIMARY KEY,
        owner_id             INTEGER NOT NULL REFERENCES actors(id),
        name                 TEXT NOT NULL COLLATE NOCASE,
        description          TEXT,
        private              INTEGER NOT NULL DEFAULT 0 CHECK (private IN (0, 1)),
        default_branch       TEXT NOT NULL,
        created_by           INTEGER NOT NULL REFERENCES actors(id),
        created_clock_unix   INTEGER NOT NULL,
        pushed_at_clock_unix INTEGER,
        UNIQUE (owner_id, name)
    ) STRICT;

    CREATE TABLE action_log (
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        clock_unix INTEGER NOT NULL,
        actor_id   INTEGER NOT NULL REFERENCES actors(id),
        action     TEXT NOT NULL,
        details    TEXT NOT NULL DEFAULT '{}'
    ) STRICT;

    CREATE TABLE actor_emails (
        actor_id INTEGER NOT NULL REFERENCES actors(id),
        email    TEXT NOT NULL COLLATE NOCASE,
        is_primary INTEGER NOT NULL CHECK (is_primary IN (0, 1)),
        verified INTEGER NOT NULL CHECK (verified IN (0, 1)),
        PRIMARY KEY (actor_id, email)
    ) STRICT;
    CREATE UNIQUE INDEX actor_emails_one_primary ON actor_emails (actor_id) WHERE is_primary = 1;

    CREATE TABLE oauth_apps (
        id                 INTEGER PRIMARY KEY,
        name               TEXT NOT NULL,
        client_id          TEXT NOT NULL UNIQUE,
        client_secret      TEXT NOT NULL,
        callback_url       TEXT NOT NULL,
        created_clock_unix INTEGER NOT NULL
    ) STRICT;

    -- Client secrets issued for an app after it was registered, `n` counting
    -- from 1. The secret issued at registration stays in `oauth_apps`, and
    -- every one of them is accepted.
    CREATE TABLE oauth_app_secrets (
        app_id             INTEGER NOT NULL REFERENCES oauth_apps(id),
        n                  INTEGER NOT NULL CHECK (n >= 1),
        secret             TEXT NOT NULL,
        created_clock_unix INTEGER NOT NULL,
        PRIMARY KEY (app_id, n)
    ) STRICT;

    -- An authorization grant: issued when an actor authorizes an app, spent
    -- (exchanged_clock_unix set) when the app trades it for a token.
    CREATE TABLE oauth_codes (
        id                   INTEGER PRIMARY KEY,
        code                 TEXT NOT NULL UNIQUE,
        app_id               INTEGER NOT NULL REFERENCES oauth_apps(id),
        actor_id             INTEGER NOT NULL REFERENCES actors(id),
        redirect_uri         TEXT NOT NULL,
        scope                TEXT NOT NULL,
        created_clock_unix   INTEGER NOT NULL,
        exchanged_clock_unix INTEGER
    ) STRICT;

    CREATE TABLE pulls (
        id                 INTEGER PRIMARY KEY,
        repo_id            INTEGER NOT NULL REFERENCES repos(id),
        number             INTEGER NOT NULL,
        title              TEXT NOT NULL,
        body               TEXT,
        author_id          INTEGER NOT NULL REFERENCES actors(id),
        state              TEXT NOT NULL CHECK (state IN ('open', 'closed')),
        draft              INTEGER NOT NULL DEFAULT 0 CHECK (draft IN (0, 1)),
        head_ref           TEXT NOT NULL,
        head_sha           TEXT NOT NULL,
        base_ref           TEXT NOT NULL,
        base_sha           TEXT NOT NULL,
        merged             INTEGER NOT NULL DEFAULT 0 CHECK (merged IN (0, 1)),
        merge_commit_sha   TEXT,
        merged_by_id       INTEGER REFERENCES actors(id),
        created_clock_unix INTEGER NOT NULL,
        updated_clock_unix INTEGER NOT NULL,
        closed_clock_unix  INTEGER,
        UNIQUE (repo_id, number)
    ) STRICT;

    -- A pull's current labels. GitHub keeps labels per repository and creates
    -- one when it is first added to an issue; Orrery keeps only the names on
    -- each pull until a consumer reads a repository's labels.
    CREATE TABLE pull_labels (
        pull_id INTEGER NOT NULL REFERENCES pulls(id),
        name    TEXT NOT NULL COLLATE NOCASE,
        PRIMARY KEY (pull_id, name)
    ) STRICT;

    -- What happened to a pull that its current state no longer shows: GitHub's
    -- timeline items. `label` is set exactly for the label kinds, and one of
    -- `reviewer_id` / `team_id` exactly for the review-request kinds. A
    -- deleted team's requests stay on the timeline, so `team_id` is not a
    -- foreign key; team ids are never reused, so it never names a later team.
    -- `team_slug` keeps the slug the team had.
    CREATE TABLE pull_timeline (
        id          INTEGER PRIMARY KEY,
        pull_id     INTEGER NOT NULL REFERENCES pulls(id),
        kind        TEXT NOT NULL CHECK (kind IN ('labeled', 'unlabeled', 'ready_for_review',
                                                  'review_requested', 'review_request_removed',
                                                  'closed')),
        label       TEXT,
        reviewer_id INTEGER REFERENCES actors(id),
        team_id     INTEGER,
        team_slug   TEXT,
        actor_id    INTEGER NOT NULL REFERENCES actors(id),
        clock_unix  INTEGER NOT NULL,
        CHECK ((kind IN ('labeled', 'unlabeled')) = (label IS NOT NULL)),
        CHECK ((team_id IS NULL) = (team_slug IS NULL)),
        CHECK ((kind IN ('review_requested', 'review_request_removed'))
               = ((reviewer_id IS NULL) <> (team_id IS NULL))),
        CHECK (kind IN ('review_requested', 'review_request_removed')
               OR (reviewer_id IS NULL AND team_id IS NULL))
    ) STRICT;

    -- An organization's members. GitHub Enterprise Server adds a member
    -- directly rather than inviting them, so every membership is active.
    CREATE TABLE org_members (
        org_id             INTEGER NOT NULL REFERENCES actors(id),
        member_id          INTEGER NOT NULL REFERENCES actors(id),
        role               TEXT NOT NULL CHECK (role IN ('admin', 'member')),
        created_clock_unix INTEGER NOT NULL,
        PRIMARY KEY (org_id, member_id)
    ) STRICT;

    -- AUTOINCREMENT so a deleted team's id, and the node id derived from it,
    -- never comes back as another team's.
    CREATE TABLE teams (
        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
        org_id             INTEGER NOT NULL REFERENCES actors(id),
        slug               TEXT NOT NULL COLLATE NOCASE,
        name               TEXT NOT NULL COLLATE NOCASE,
        description        TEXT,
        created_clock_unix INTEGER NOT NULL,
        UNIQUE (org_id, slug),
        UNIQUE (org_id, name)
    ) STRICT;

    CREATE TABLE team_members (
        team_id            INTEGER NOT NULL REFERENCES teams(id),
        member_id          INTEGER NOT NULL REFERENCES actors(id),
        role               TEXT NOT NULL CHECK (role IN ('member', 'maintainer')),
        created_clock_unix INTEGER NOT NULL,
        PRIMARY KEY (team_id, member_id)
    ) STRICT;

    -- A pull's pending review requests, each naming one user or one team.
    CREATE TABLE review_requests (
        id          INTEGER PRIMARY KEY,
        pull_id     INTEGER NOT NULL REFERENCES pulls(id),
        reviewer_id INTEGER REFERENCES actors(id),
        team_id     INTEGER REFERENCES teams(id),
        CHECK ((reviewer_id IS NULL) <> (team_id IS NULL))
    ) STRICT;
    CREATE UNIQUE INDEX review_requests_user ON review_requests (pull_id, reviewer_id)
        WHERE reviewer_id IS NOT NULL;
    CREATE UNIQUE INDEX review_requests_team ON review_requests (pull_id, team_id)
        WHERE team_id IS NOT NULL;

    CREATE TABLE reviews (
        id                   INTEGER PRIMARY KEY,
        pull_id              INTEGER NOT NULL REFERENCES pulls(id),
        author_id            INTEGER NOT NULL REFERENCES actors(id),
        state                TEXT NOT NULL CHECK (state IN ('approved', 'changes_requested', 'commented')),
        body                 TEXT,
        commit_sha           TEXT NOT NULL,
        submitted_clock_unix INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE issue_comments (
        id                 INTEGER PRIMARY KEY,
        pull_id            INTEGER NOT NULL REFERENCES pulls(id),
        author_id          INTEGER NOT NULL REFERENCES actors(id),
        body               TEXT NOT NULL,
        created_clock_unix INTEGER NOT NULL,
        updated_clock_unix INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE check_runs (
        id                     INTEGER PRIMARY KEY,
        repo_id                INTEGER NOT NULL REFERENCES repos(id),
        head_sha               TEXT NOT NULL,
        name                   TEXT NOT NULL,
        status                 TEXT NOT NULL CHECK (status IN ('queued', 'in_progress', 'completed')),
        conclusion             TEXT CHECK (conclusion IN ('success', 'failure', 'neutral', 'cancelled',
                                                          'skipped', 'stale', 'timed_out', 'action_required')),
        details_url            TEXT,
        external_id            TEXT,
        output                 TEXT,
        creator_id             INTEGER NOT NULL REFERENCES actors(id),
        started_clock_unix     INTEGER,
        completed_clock_unix   INTEGER,
        created_clock_unix     INTEGER NOT NULL
    ) STRICT;
"#;

/// Bring a database to this binary's version: create the baseline on an
/// empty one, then run every pending migration. A database newer than the
/// binary, or older than the baseline, is refused.
pub(crate) fn apply(conn: &mut Connection) -> Result<()> {
    let migrations = migrations();
    let target = BASELINE_VERSION + migrations.len() as i64;
    // IMMEDIATE takes the write lock before the version is read, so a second
    // opener waits and then finds the work done, rather than migrating from a
    // version that is already gone.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = schema_version(&tx)?;
    let pending = match found {
        0 => {
            tx.execute_batch(BASELINE)?;
            &migrations[..]
        }
        v if v == target => return Ok(()),
        v if (BASELINE_VERSION..target).contains(&v) => {
            &migrations[(v - BASELINE_VERSION) as usize..]
        }
        _ => {
            return Err(WorldError::SchemaVersion {
                found,
                expected: target,
            });
        }
    };
    for sql in pending {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", target)?;
    tx.commit()?;
    Ok(())
}

/// Check that a database can be read as is. One that `apply` would migrate
/// is `NeedsMigration`, so a reader can migrate a copy instead.
pub(crate) fn check_current(conn: &Connection) -> Result<()> {
    let found = schema_version(conn)?;
    let expected = BASELINE_VERSION + migrations().len() as i64;
    match found {
        v if v == expected => Ok(()),
        v if (BASELINE_VERSION..expected).contains(&v) => {
            Err(WorldError::NeedsMigration { found, expected })
        }
        _ => Err(WorldError::SchemaVersion { found, expected }),
    }
}

fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

#[cfg(not(any(test, feature = "test-support")))]
fn migrations() -> Vec<&'static str> {
    MIGRATIONS.to_vec()
}

#[cfg(any(test, feature = "test-support"))]
fn migrations() -> Vec<&'static str> {
    let mut all = MIGRATIONS.to_vec();
    all.extend(test_support::APPENDED.get());
    all
}

/// Migrations appended for a test, so a world at this binary's version can
/// be made older than the binary. Worlds opened on the calling thread while
/// `with_appended_migrations` runs see a binary at `SCHEMA_VERSION` plus the
/// number appended.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;

    thread_local! {
        pub(super) static APPENDED: Cell<&'static [&'static str]> = const { Cell::new(&[]) };
    }

    /// Whether a test has appended migrations, so the schema holds tables
    /// and columns this binary does not know.
    pub(crate) fn extended() -> bool {
        !APPENDED.get().is_empty()
    }

    pub fn with_appended_migrations<T>(
        migrations: &'static [&'static str],
        f: impl FnOnce() -> T,
    ) -> T {
        struct Reset(&'static [&'static str]);
        impl Drop for Reset {
            fn drop(&mut self) {
                APPENDED.set(self.0);
            }
        }
        let _reset = Reset(APPENDED.replace(migrations));
        f()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::test_support::with_appended_migrations;
    use super::{BASELINE, BASELINE_VERSION, SCHEMA_VERSION, schema_version};
    use crate::{
        ActorKind, Attribution, DEFAULT_T0_UNIX, Door, Event, SYSTEM_ACTOR_ID, World, WorldError,
        WorldReader, WorldSecret, WorldView,
    };

    // The shapes a real migration takes: a column with a default, and a new
    // table backfilled from an old one.
    const TOPICS: &str = "
        ALTER TABLE repos ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));
        CREATE TABLE repo_topics (
            repo_id INTEGER NOT NULL REFERENCES repos(id),
            topic   TEXT NOT NULL,
            PRIMARY KEY (repo_id, topic)
        ) STRICT;
        INSERT INTO repo_topics (repo_id, topic) SELECT id, 'migrated' FROM repos;
    ";
    const BROKEN: &str = "INSERT INTO no_such_table VALUES (1);";

    /// A world at this binary's version with a little of everything the
    /// tests compare: actors, a repo, and events.
    fn world(dir: &Path) -> PathBuf {
        let root = dir.join("world");
        let w = World::create(&root, DEFAULT_T0_UNIX, &WorldSecret::LOCAL).unwrap();
        let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
        let by = Attribution::new(alice.id, Door::Rest);
        w.create_repo(alice.id, "webapp", None, false, "main", by)
            .unwrap();
        let system = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);
        w.advance_clock(60, system).unwrap();
        root
    }

    fn version(w: &World) -> i64 {
        w.with_conn(schema_version).unwrap()
    }

    fn has_table(w: &World, name: &str) -> bool {
        w.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [name],
                |r| r.get::<_, i64>(0),
            )? == 1)
        })
        .unwrap()
    }

    fn whole_log(w: &World) -> Vec<Event> {
        w.log_after(0, 10_000).unwrap()
    }

    // The whole text is hashed, comments and whitespace included, so the test
    // never has to decide which edits would change a world the baseline
    // creates.
    #[test]
    fn the_baseline_never_changes() {
        use sha2::{Digest, Sha256};

        let found = (BASELINE_VERSION, hex::encode(Sha256::digest(BASELINE)));
        let frozen = (
            6,
            "e0b8db1239647acec5bee52f89035aca4fb0a1d730d3c82b8b16b161110ffeba".to_owned(),
        );
        assert_eq!(
            found, frozen,
            "the baseline schema changed. World databases outlive the binary that \
             wrote them, so the baseline and BASELINE_VERSION never change: revert \
             the edit and add a forward migration to MIGRATIONS instead, with a \
             fixture (tests/fixtures/README.md)"
        );
    }

    #[test]
    fn a_world_opens_at_the_next_version_with_its_data_intact() {
        let dir = tempfile::tempdir().unwrap();
        let root = world(dir.path());
        let (actors, repos, log) = {
            let w = World::open(&root).unwrap();
            assert_eq!(version(&w), SCHEMA_VERSION);
            (
                w.list_actors().unwrap(),
                w.list_repos().unwrap(),
                whole_log(&w),
            )
        };

        with_appended_migrations(&[TOPICS], || {
            let w = World::open(&root).unwrap();
            assert_eq!(version(&w), SCHEMA_VERSION + 1);
            let topics: i64 = w
                .with_conn(|conn| {
                    Ok(conn.query_row(
                        "SELECT count(*) FROM repo_topics WHERE topic = 'migrated'",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(topics, 1, "the backfill ran");
            assert_eq!(w.list_actors().unwrap(), actors);
            assert_eq!(w.list_repos().unwrap(), repos);
            assert_eq!(whole_log(&w), log, "the log reads as before");

            // The migrated world takes writes, and its log carries on.
            w.create_actor("bob", ActorKind::User).unwrap();
            let last = log.last().unwrap().seq;
            let appended = w.log_after(last, 10).unwrap();
            assert_eq!(appended.len(), 1);
            assert_eq!(appended[0].seq, last + 1);
            drop(w);

            // Opening again finds nothing pending.
            let w = World::open(&root).unwrap();
            assert_eq!(version(&w), SCHEMA_VERSION + 1);
        });
    }

    #[test]
    fn pending_migrations_run_in_one_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let root = world(dir.path());

        with_appended_migrations(&[TOPICS, BROKEN], || {
            assert!(matches!(World::open(&root), Err(WorldError::Sqlite(_))));
        });

        let w = World::open(&root).unwrap();
        assert_eq!(version(&w), SCHEMA_VERSION);
        assert!(
            !has_table(&w, "repo_topics"),
            "the migration before the failing one rolled back with it"
        );
    }

    #[test]
    fn a_world_newer_than_the_binary_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = world(dir.path());
        with_appended_migrations(&[TOPICS], || World::open(&root).map(drop)).unwrap();

        assert!(matches!(
            World::open(&root),
            Err(WorldError::SchemaVersion { found, expected })
                if found == SCHEMA_VERSION + 1 && expected == SCHEMA_VERSION
        ));
        assert!(matches!(
            WorldReader::open(&root),
            Err(WorldError::SchemaVersion { found, .. }) if found == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn a_world_below_the_baseline_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = world(dir.path());
        World::open(&root)
            .unwrap()
            .with_conn(|conn| Ok(conn.pragma_update(None, "user_version", BASELINE_VERSION - 1)?))
            .unwrap();

        assert!(matches!(
            World::open(&root),
            Err(WorldError::SchemaVersion { found, .. }) if found == BASELINE_VERSION - 1
        ));
        assert!(matches!(
            WorldReader::open(&root),
            Err(WorldError::SchemaVersion { found, .. }) if found == BASELINE_VERSION - 1
        ));
    }

    #[test]
    fn at_most_one_pull_is_open_per_head_and_base() {
        let dir = tempfile::tempdir().unwrap();
        let w = World::open(&world(dir.path())).unwrap();
        let open_into = |base: &str| {
            w.with_conn(|conn| {
                Ok(conn.execute(
                    "INSERT INTO pulls (repo_id, number, title, author_id, state, draft,
                                        head_ref, head_sha, base_ref, base_sha,
                                        created_clock_unix, updated_clock_unix)
                     SELECT id, (SELECT COUNT(*) + 1 FROM pulls), 't', owner_id, 'open', 0,
                            'feature', ?2, ?1, ?2, 0, 0
                     FROM repos",
                    rusqlite::params![base, "a".repeat(40)],
                )?)
            })
        };
        open_into("main").unwrap();
        open_into("dev").unwrap();

        // Writes that skip the kernel's own check still cannot open a second
        // pull on a held pair.
        let violation = |r: crate::Result<usize>| {
            matches!(r, Err(WorldError::Sqlite(rusqlite::Error::SqliteFailure(e, _)))
                if e.code == rusqlite::ErrorCode::ConstraintViolation)
        };
        assert!(violation(open_into("main")));
        assert!(violation(w.with_conn(|conn| Ok(conn.execute(
            "UPDATE pulls SET base_ref = 'main' WHERE base_ref = 'dev'",
            []
        )?))));

        // Closed pulls are outside it.
        w.with_conn(|conn| Ok(conn.execute("UPDATE pulls SET state = 'closed'", [])?))
            .unwrap();
        open_into("main").unwrap();
    }

    #[test]
    fn a_reader_never_migrates() {
        let dir = tempfile::tempdir().unwrap();
        let root = world(dir.path());

        with_appended_migrations(&[TOPICS], || {
            assert!(matches!(
                WorldReader::open(&root),
                Err(WorldError::NeedsMigration { found, expected })
                    if found == SCHEMA_VERSION && expected == SCHEMA_VERSION + 1
            ));
        });
        let w = World::open(&root).unwrap();
        assert_eq!(version(&w), SCHEMA_VERSION);
        assert!(!has_table(&w, "repo_topics"));
    }
}
