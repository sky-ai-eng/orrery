//! The world schema, applied whole when a world is created. Orrery has no
//! deployed worlds to carry forward, so the schema changes in place and its
//! version is bumped with it; a world written under another version is refused
//! at open rather than migrated.

use rusqlite::Connection;

use crate::{Result, WorldError};

/// Bumped on every schema change.
pub(crate) const SCHEMA_VERSION: i64 = 2;

// State vocabulary is neutral (rule 9: vendor-neutral state, vendor faces) —
// faces render casing like "APPROVED" or "CHANGES_REQUESTED" at the wire,
// never the store. `pulls.base_sha` is pinned at PR creation (and on base
// retarget), matching GitHub's static `base.sha`; `head_sha` tracks the branch
// via push observation, matching GitHub's synchronize behavior.
const SCHEMA: &str = r#"
    CREATE TABLE world (
        id         INTEGER PRIMARY KEY CHECK (id = 1),
        clock_unix INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE actors (
        id                 INTEGER PRIMARY KEY,
        login              TEXT NOT NULL COLLATE NOCASE UNIQUE,
        kind               TEXT NOT NULL CHECK (kind IN ('user', 'bot', 'org', 'system')),
        created_clock_unix INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE tokens (
        token              TEXT PRIMARY KEY,
        actor_id           INTEGER NOT NULL REFERENCES actors(id),
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
    -- timeline items. `label` is set exactly for the label kinds.
    CREATE TABLE pull_timeline (
        id         INTEGER PRIMARY KEY,
        pull_id    INTEGER NOT NULL REFERENCES pulls(id),
        kind       TEXT NOT NULL CHECK (kind IN ('labeled', 'unlabeled', 'ready_for_review')),
        label      TEXT,
        actor_id   INTEGER NOT NULL REFERENCES actors(id),
        clock_unix INTEGER NOT NULL,
        CHECK ((kind IN ('labeled', 'unlabeled')) = (label IS NOT NULL))
    ) STRICT;

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

/// Create the schema on an empty database, or check an existing one's
/// version.
pub(crate) fn apply(conn: &Connection) -> Result<()> {
    match schema_version(conn)? {
        0 => {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        found => Err(WorldError::SchemaVersion {
            found,
            expected: SCHEMA_VERSION,
        }),
    }
}

pub(crate) fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}
