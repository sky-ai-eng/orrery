//! Forward-only migrations, tracked by `PRAGMA user_version`. New migrations
//! append to the list; existing entries never change (the TF schema rule,
//! minus goose: a world created by an older binary migrates forward on open).

use rusqlite::Connection;

use crate::Result;

pub(crate) const MIGRATIONS: &[&str] = &[
    // 1: the M0 schema.
    r#"
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
    "#,
    // 2: the M3 collaboration objects. State vocabulary is neutral (rule 9:
    // vendor-neutral state, vendor faces) — faces render casing like
    // "APPROVED" or "CHANGES_REQUESTED" at the wire, never the store.
    // `base_sha` is pinned at PR creation (and on base retarget), matching
    // GitHub's static `base.sha`; `head_sha` tracks the branch via push
    // observation, matching GitHub's synchronize behavior.
    r#"
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
    "#,
];

pub(crate) const CURRENT_VERSION: i64 = MIGRATIONS.len() as i64;

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version = schema_version(conn)?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version < target {
            conn.execute_batch(sql)?;
            conn.pragma_update(None, "user_version", target)?;
        }
    }
    Ok(())
}

pub(crate) fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}
