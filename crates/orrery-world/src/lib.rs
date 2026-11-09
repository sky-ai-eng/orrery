//! The world kernel: one directory holding everything a world is.
//!
//! Layout (spec/usage.md — rule 10: delete the directory and the world is gone):
//!
//! ```text
//! <root>/            # the world root, owned exclusively by this kernel
//! ├── world.db       # SQLite: clock, actors, repos, pulls, reviews, log, ...
//! └── repos/         # bare git repositories, managed by the git face
//! ```
//!
//! This crate has no HTTP dependencies and no vendor vocabulary. Faces adapt
//! vendors onto it; nothing else opens `world.db`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use rusqlite::{Connection, OptionalExtension, params};

mod checks;
mod credentials;
mod error;
mod identity;
mod orgs;
mod pulls;
mod queries;
mod reader;
mod schema;
mod types;

pub use checks::{CheckRunPatch, NewCheckRun};
pub use credentials::{OAUTH_CLIENT_ID_PREFIX, WorldSecret};
pub use error::WorldError;
pub use identity::{
    OAUTH_CODE_TTL_SECONDS, validate_oauth_app_callback_url, validate_oauth_app_name,
};
pub use pulls::NewPull;
pub use reader::{ViewMeta, WorldReader, WorldView};
#[cfg(feature = "test-support")]
pub use schema::test_support;
pub use schema::{BASELINE_VERSION, SCHEMA_VERSION};
pub use types::*;

pub type Result<T> = std::result::Result<T, WorldError>;

/// Fallback T0 when a new world's config doesn't name one: 2020-01-01T00:00:00Z.
/// A fixed constant rather than "now" so that two worlds created from the same
/// config are the same world (rule 5), apart from the real time each of their
/// log entries records.
pub const DEFAULT_T0_UNIX: i64 = 1_577_836_800;

/// The reserved actor that attributes control-plane and engine-internal
/// mutations in the action log. Seeded at world creation; holds no token.
pub const SYSTEM_ACTOR_LOGIN: &str = "orrery";

const DB_FILE: &str = "world.db";
const REPOS_DIR: &str = "repos";

pub struct World {
    root: PathBuf,
    conn: Mutex<Connection>,
    listeners: RwLock<Vec<Listener>>,
}

type Listener = Box<dyn Fn(Signal) + Send + Sync>;

/// What a [`World`] tells the listeners registered with [`World::listen`].
/// A signal says only that something may have changed: the change itself is
/// read through the `World`. Each is raised with the connection lock held,
/// so any read that sees the change it announces begins after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// An entry was appended to the action log. It may be inside a
    /// transaction that has not committed yet, and that may roll back, so a
    /// read that follows sees the entry once it commits, or nothing.
    LogMayHaveGrown,
    /// The world's state was replaced by a snapshot's, and its action log
    /// with it. `newest_seq` is the newest `seq` in the log the world holds
    /// now, or 0 for an empty one.
    StateReplaced { newest_seq: i64 },
}

impl World {
    /// Create a new world at `root` (which must not already contain one) with
    /// the clock at `t0_unix` and every credential keyed by `secret`, and seed
    /// the system actor.
    pub fn create(root: &Path, t0_unix: i64, secret: &WorldSecret) -> Result<World> {
        if root.join(DB_FILE).exists() {
            return Err(WorldError::AlreadyExists(root.to_path_buf()));
        }
        std::fs::create_dir_all(root.join(REPOS_DIR))?;
        let world = World::connect(root)?;
        {
            let conn = world.lock();
            conn.execute(
                "INSERT INTO world (id, clock_unix, secret) VALUES (1, ?1, ?2)",
                params![t0_unix, secret],
            )?;
            conn.execute(
                "INSERT INTO actors (login, kind, created_clock_unix) VALUES (?1, 'system', ?2)",
                params![SYSTEM_ACTOR_LOGIN, t0_unix],
            )?;
        }
        let system = world
            .actor_by_login(SYSTEM_ACTOR_LOGIN)?
            .expect("system actor was just seeded");
        world.log_action(
            system.id,
            "world.init",
            &serde_json::json!({ "t0_unix": t0_unix }),
        )?;
        Ok(world)
    }

    /// Open an existing world at `root`, migrating it to this binary's schema
    /// version first.
    pub fn open(root: &Path) -> Result<World> {
        if !root.join(DB_FILE).exists() {
            return Err(WorldError::NotFound(root.to_path_buf()));
        }
        World::connect(root)
    }

    /// Open the world at `root`, creating it with `t0_unix` and `secret` if
    /// absent. An existing world keeps the secret it was created with.
    pub fn open_or_create(root: &Path, t0_unix: i64, secret: &WorldSecret) -> Result<World> {
        if root.join(DB_FILE).exists() {
            World::open(root)
        } else {
            World::create(root, t0_unix, secret)
        }
    }

    fn connect(root: &Path) -> Result<World> {
        let conn = open_conn(root)?;
        Ok(World {
            root: root.to_path_buf(),
            conn: Mutex::new(conn),
            listeners: RwLock::new(Vec::new()),
        })
    }

    /// Call `listener` with every [`Signal`] this world raises from now on.
    ///
    /// A listener is called on the thread making the change, with the
    /// connection lock held, so every query waits for it to return. It must
    /// not block, and it must not call this `World` or register another
    /// listener: either would deadlock.
    pub fn listen(&self, listener: impl Fn(Signal) + Send + Sync + 'static) {
        self.listeners
            .write()
            .expect("listener list poisoned")
            .push(Box::new(listener));
    }

    fn signal(&self, signal: Signal) {
        for listener in self
            .listeners
            .read()
            .expect("listener list poisoned")
            .iter()
        {
            listener(signal);
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory the git face keeps bare repositories in.
    pub fn repos_dir(&self) -> PathBuf {
        self.root.join(REPOS_DIR)
    }

    // ---- clock ----

    pub fn clock(&self) -> Result<i64> {
        queries::clock(&self.lock())
    }

    /// Advance the virtual clock. Forward-only: the clock never rewinds in a
    /// live world (spec/usage.md — rollback is a snapshot operation). It
    /// stops at the latest second `jiff` represents, so that every time the
    /// world stamps can be rendered.
    pub fn advance_clock(&self, seconds: i64, actor_id: i64) -> Result<i64> {
        if seconds <= 0 {
            return Err(WorldError::ClockRegression(seconds));
        }
        let latest = jiff::Timestamp::MAX.as_second();
        let (from, to) = {
            let conn = self.lock();
            let from = queries::clock(&conn)?;
            let to = from
                .checked_add(seconds)
                .filter(|to| *to <= latest)
                .ok_or_else(|| WorldError::ClockOutOfRange {
                    seconds,
                    remaining: latest.saturating_sub(from).max(0),
                    latest: jiff::Timestamp::MAX,
                })?;
            let to: i64 = conn.query_row(
                "UPDATE world SET clock_unix = ?1 WHERE id = 1 RETURNING clock_unix",
                params![to],
                |r| r.get(0),
            )?;
            (from, to)
        };
        self.log_action(
            actor_id,
            "clock.advance",
            &serde_json::json!({ "seconds": seconds, "from_unix": from, "to_unix": to }),
        )?;
        Ok(to)
    }

    // ---- actors & tokens ----

    /// Create an actor and mint its token. The token is derived, not random
    /// (rule 5: replay must reproduce it; rule 12: it is fake by construction),
    /// and keyed by the world secret, so it cannot be guessed.
    pub fn create_actor(&self, login: &str, kind: ActorKind) -> Result<(Actor, String)> {
        validate_login(login)?;
        validate_actor_kind(kind)?;
        let conn = self.lock();
        let clock = queries::clock(&conn)?;
        let actor = conn
            .query_row(
                "INSERT INTO actors (login, kind, created_clock_unix) VALUES (?1, ?2, ?3)
                 RETURNING id, login, kind, created_clock_unix",
                params![login, kind.as_str(), clock],
                queries::row_to_actor,
            )
            .map_err(|e| match constraint_violation(&e) {
                true => WorldError::ActorExists(login.to_string()),
                false => WorldError::from(e),
            })?;
        let token = credentials::actor_token(
            &credentials::world_secret(&conn)?,
            &actor.login,
            actor.id,
            0,
        );
        conn.execute(
            "INSERT INTO tokens (token, actor_id, origin, created_clock_unix)
             VALUES (?1, ?2, ?3, ?4)",
            params![token, actor.id, credentials::TokenOrigin::Actor(0), clock],
        )?;
        identity::insert_email(&conn, actor.id, &identity::default_email(&actor.login))?;
        self.log_locked(
            &conn,
            actor.id,
            "actor.create",
            &serde_json::json!({ "login": actor.login, "kind": kind.as_str() }),
        )?;
        Ok((actor, token))
    }

    /// Issue another token for an existing actor. A stored token is never
    /// read back, so this is how anyone gets one for an actor they did not
    /// create, as in a world re-keyed at its creation. Every token issued
    /// before stays valid. `by` is the actor the action log attributes the
    /// issue to.
    pub fn issue_actor_token(&self, login: &str, by: i64) -> Result<(Actor, String)> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let actor = tx
            .query_row(
                &format!("{} WHERE login = ?1", queries::ACTOR_SELECT),
                params![login],
                queries::row_to_actor,
            )
            .optional()?
            .ok_or_else(|| WorldError::NoSuchActor(login.to_string()))?;
        if actor.kind == ActorKind::System {
            return Err(WorldError::SystemActorToken);
        }
        let n: i64 = tx.query_row(
            "SELECT COALESCE(MAX(CAST(substr(origin, 7) AS INTEGER)), 0) + 1
             FROM tokens WHERE actor_id = ?1 AND origin GLOB 'actor:*'",
            params![actor.id],
            |r| r.get(0),
        )?;
        let token: String = tx.query_row(
            "INSERT INTO tokens (token, actor_id, origin, created_clock_unix)
             VALUES (?1, ?2, ?3, ?4)
             RETURNING token",
            params![
                credentials::actor_token(
                    &credentials::world_secret(&tx)?,
                    &actor.login,
                    actor.id,
                    n
                ),
                actor.id,
                credentials::TokenOrigin::Actor(n),
                queries::clock(&tx)?
            ],
            |r| r.get(0),
        )?;
        self.log_locked(
            &tx,
            by,
            "actor.token.issue",
            &serde_json::json!({ "login": actor.login, "n": n }),
        )?;
        tx.commit()?;
        Ok((actor, token))
    }

    pub fn actor_by_login(&self, login: &str) -> Result<Option<Actor>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("{} WHERE login = ?1", queries::ACTOR_SELECT),
                params![login],
                queries::row_to_actor,
            )
            .optional()?)
    }

    pub fn actor_by_token(&self, token: &str) -> Result<Option<Actor>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT a.id, a.login, a.kind, a.created_clock_unix
                 FROM tokens t JOIN actors a ON a.id = t.actor_id
                 WHERE t.token = ?1",
                params![token],
                queries::row_to_actor,
            )
            .optional()?)
    }

    // ---- repos ----

    /// Insert the repo row. Disk-side initialization of the bare repository is
    /// the git face's job; the kernel records only state.
    pub fn create_repo(
        &self,
        owner_id: i64,
        name: &str,
        description: Option<&str>,
        private: bool,
        default_branch: &str,
        created_by: i64,
    ) -> Result<Repo> {
        validate_repo_name(name)?;
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let clock = queries::clock(&tx)?;
        let id: i64 = tx
            .query_row(
                "INSERT INTO repos (owner_id, name, description, private, default_branch,
                                    created_by, created_clock_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 RETURNING id",
                params![
                    owner_id,
                    name,
                    description,
                    private,
                    default_branch,
                    created_by,
                    clock
                ],
                |r| r.get(0),
            )
            .map_err(|e| match constraint_violation(&e) {
                true => WorldError::RepoExists(name.to_string()),
                false => WorldError::from(e),
            })?;
        let repo = tx
            .query_row(
                &format!("{} WHERE r.id = ?1", queries::REPO_SELECT),
                params![id],
                queries::row_to_repo,
            )
            .map_err(WorldError::from)?;
        self.log_locked(
            &tx,
            created_by,
            "repo.create",
            &serde_json::json!({
                "owner": repo.owner_login,
                "name": repo.name,
                "private": repo.private,
            }),
        )?;
        tx.commit()?;
        Ok(repo)
    }

    pub fn repo_by_owner_name(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!(
                    "{} WHERE a.login = ?1 AND r.name = ?2",
                    queries::REPO_SELECT
                ),
                params![owner, name],
                queries::row_to_repo,
            )
            .optional()?)
    }

    pub fn repo_by_id(&self, id: i64) -> Result<Option<Repo>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!("{} WHERE r.id = ?1", queries::REPO_SELECT),
                params![id],
                queries::row_to_repo,
            )
            .optional()?)
    }

    /// An owner's repos in `sort=pushed&direction=desc` order. Ownership is
    /// the only repository affiliation the world models — there are no
    /// collaborators, and org membership grants no repository access — so
    /// "repos visible to this actor" and "repos this actor owns" are the
    /// same set.
    pub fn list_repos_owned_by(&self, owner_id: i64) -> Result<Vec<Repo>> {
        self.with_conn(|conn| queries::list_repos_for_owner(conn, owner_id))
    }

    /// Record that a push landed: bump `pushed_at` to the current clock.
    pub fn touch_pushed(&self, repo_id: i64) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE repos
             SET pushed_at_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
             WHERE id = ?1",
            params![repo_id],
        )?;
        if n == 0 {
            return Err(WorldError::NoSuchRepo(repo_id));
        }
        Ok(())
    }

    pub fn set_default_branch(&self, repo_id: i64, branch: &str) -> Result<()> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE repos SET default_branch = ?1 WHERE id = ?2",
            params![branch, repo_id],
        )?;
        if n == 0 {
            return Err(WorldError::NoSuchRepo(repo_id));
        }
        Ok(())
    }

    // ---- action log ----

    /// Append to the action log, stamped with the current clock. The log is
    /// append-only; nothing in the kernel updates or deletes its rows.
    pub fn log_action(
        &self,
        actor_id: i64,
        action: &str,
        details: &serde_json::Value,
    ) -> Result<LogEntry> {
        let conn = self.lock();
        self.log_locked(&conn, actor_id, action, details)
    }

    /// Append to the action log through an already-held connection (or an
    /// open transaction — the clock subselect and the insert see the same
    /// state), and signal [`Signal::LogMayHaveGrown`].
    pub(crate) fn log_locked(
        &self,
        conn: &Connection,
        actor_id: i64,
        action: &str,
        details: &serde_json::Value,
    ) -> Result<LogEntry> {
        let entry = conn.query_row(
            "INSERT INTO action_log (clock_unix, wall_unix_ms, actor_id, action, details)
             VALUES ((SELECT clock_unix FROM world WHERE id = 1), ?1, ?2, ?3, ?4)
             RETURNING seq, clock_unix, wall_unix_ms, actor_id, action, details",
            params![wall_unix_ms(), actor_id, action, details.to_string()],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            },
        )?;
        self.signal(Signal::LogMayHaveGrown);
        let login: String = conn.query_row(
            "SELECT login FROM actors WHERE id = ?1",
            params![entry.3],
            |r| r.get(0),
        )?;
        Ok(LogEntry {
            seq: entry.0,
            clock_unix: entry.1,
            wall_unix_ms: entry.2,
            actor_login: login,
            action: entry.4,
            details: serde_json::from_str(&entry.5).unwrap_or(serde_json::Value::Null),
        })
    }

    /// Log entries with `seq > after_seq`, oldest first, at most `limit`.
    /// `seq` is a total order by construction, so pagination is stable. It is
    /// not contiguous: a restore leaves a gap (see [`World::swap_state`]).
    pub fn log_after(&self, after_seq: i64, limit: usize) -> Result<Vec<LogEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "{LOG_ENTRY_SELECT} WHERE l.seq > ?1 ORDER BY l.seq ASC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![after_seq, limit as i64], log_entry)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Whether `seq` is a position in the action log: 0, or the `seq` of an
    /// entry it holds. Any other number is [`WorldError::NoSuchLogPosition`],
    /// carrying the log's newest `seq`.
    pub fn check_log_position(&self, seq: i64) -> Result<()> {
        check_log_position(&self.lock(), seq)
    }

    /// One page of the entries with `seq > after_seq`, in `order`, and how
    /// many such entries there are. With `resume`, the `seq` of the previous
    /// page's last entry, the page holds only entries past it in `order`, so a
    /// walk neither repeats nor skips an entry while the log grows. The count
    /// is read under the same lock as the page. Only `seq`'s order is relied
    /// on, never that its values are contiguous.
    ///
    /// `after_seq` is checked as [`World::check_log_position`] checks it. A
    /// `resume` the log no longer holds is [`WorldError::ResumeNotInLog`]:
    /// the walk's earlier pages came from a log a restore replaced. A `seq`
    /// is issued once, so while the log holds `resume` it holds the same
    /// entries up to it, and a walk in either order goes on exactly. Both are
    /// checked under the lock the page is read under, so no restore lands
    /// between a check and the read it gates.
    pub fn log_page(
        &self,
        after_seq: i64,
        order: LogOrder,
        resume: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<LogEntry>, i64)> {
        let conn = self.lock();
        check_log_position(&conn, after_seq)?;
        if let Some(resume) = resume
            && !log_holds(&conn, resume)?
        {
            return Err(WorldError::ResumeNotInLog(resume));
        }
        let limit = limit as i64;
        // Each bound is a plain comparison on `seq`, the rowid, so a page deep
        // in the log reads only its own rows.
        let entries = match order {
            LogOrder::OldestFirst => conn
                .prepare(&format!(
                    "{LOG_ENTRY_SELECT} WHERE l.seq > ?1 ORDER BY l.seq ASC LIMIT ?2"
                ))?
                .query_map(
                    params![resume.map_or(after_seq, |r| r.max(after_seq)), limit],
                    log_entry,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            LogOrder::NewestFirst => conn
                .prepare(&format!(
                    "{LOG_ENTRY_SELECT} WHERE l.seq > ?1 AND l.seq <= ?2
                     ORDER BY l.seq DESC LIMIT ?3"
                ))?
                .query_map(
                    params![
                        after_seq,
                        resume.map_or(i64::MAX, |r| r.saturating_sub(1)),
                        limit
                    ],
                    log_entry,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        // TODO(ORR-59): this count reads every entry above `after_seq` while
        // holding the world's connection, about 60 ms at a million entries;
        // the whole-log total should come from a count kept with the log.
        let total = conn.query_row(
            "SELECT COUNT(*) FROM action_log WHERE seq > ?1",
            params![after_seq],
            |r| r.get(0),
        )?;
        Ok((entries, total))
    }

    // ---- snapshot support ----

    /// Run `f` with the database quiesced: the WAL checkpointed into the main
    /// file and the connection lock held, so no write can land while `f` runs.
    /// This is the moment a snapshot copy of the world directory is exact.
    pub fn quiesce<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        let conn = self.lock();
        checkpoint(&conn)?;
        Ok(f())
    }

    /// Close the database, run `f` (which replaces the world directory's
    /// contents — a snapshot restore), then reopen. The connection lock is
    /// held throughout, so no query can observe the closed state. If `f`
    /// fails, the reopen still runs against whatever `f` left behind; the
    /// reopen error (world unusable) outranks `f`'s.
    ///
    /// A `seq` is issued once per world, so a position a caller holds from
    /// before the swap never names a different entry after it: the reopened
    /// database's next `seq` is raised past every one this world issued
    /// before. The log then has a gap where the discarded entries were.
    ///
    /// [`Signal::StateReplaced`] is raised once the reopen succeeds, before
    /// the raise and whatever `f` returned: the state may have changed even
    /// when a later step fails, and listeners must not go on reading it as
    /// the log they had.
    pub fn swap_state(&self, f: impl FnOnce() -> std::io::Result<()>) -> Result<()> {
        let mut guard = self.lock();
        checkpoint(&guard)?;
        let issued = issued_seq(&guard)?;
        let old = std::mem::replace(&mut *guard, Connection::open_in_memory()?);
        drop(old);
        let swap_result = f();
        *guard = open_conn(&self.root)?;
        self.signal(Signal::StateReplaced {
            newest_seq: queries::last_seq(&guard)?,
        });
        raise_issued_seq(&guard, issued)?;
        swap_result?;
        Ok(())
    }

    // TODO(ORR-60): reads take this lock too, so a slow read delays every
    // write to the world; WAL would let reads run on a connection of their own.
    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("world.db mutex poisoned")
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&self.lock())
    }
}

fn open_conn(root: &Path) -> Result<Connection> {
    let mut conn = Connection::open(root.join(DB_FILE))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::apply(&mut conn)?;
    Ok(conn)
}

fn check_log_position(conn: &Connection, seq: i64) -> Result<()> {
    if seq == 0 || log_holds(conn, seq)? {
        return Ok(());
    }
    Err(WorldError::NoSuchLogPosition {
        seq,
        newest_seq: queries::last_seq(conn)?,
    })
}

fn log_holds(conn: &Connection, seq: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM action_log WHERE seq = ?1)",
        params![seq],
        |r| r.get(0),
    )?)
}

fn checkpoint(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
}

/// What every read of the action log selects, in the order `log_entry`
/// reads it.
const LOG_ENTRY_SELECT: &str =
    "SELECT l.seq, l.clock_unix, l.wall_unix_ms, a.login, l.action, l.details
     FROM action_log l JOIN actors a ON a.id = l.actor_id";

fn log_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<LogEntry> {
    Ok(LogEntry {
        seq: r.get(0)?,
        clock_unix: r.get(1)?,
        wall_unix_ms: r.get(2)?,
        actor_login: r.get(3)?,
        action: r.get(4)?,
        details: serde_json::from_str::<serde_json::Value>(&r.get::<_, String>(5)?)
            .unwrap_or(serde_json::Value::Null),
    })
}

/// The largest `seq` this database has issued, kept by SQLite for an
/// `AUTOINCREMENT` key. It outlives the entries it numbered, which `MAX(seq)`
/// does not.
fn issued_seq(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'action_log'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Make the next `seq` this database issues greater than `issued`.
fn raise_issued_seq(conn: &Connection, issued: i64) -> Result<()> {
    let raised = conn.execute(
        "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'action_log'",
        params![issued],
    )?;
    if raised == 0 {
        conn.execute(
            "INSERT INTO sqlite_sequence (name, seq) VALUES ('action_log', ?1)",
            params![issued],
        )?;
    }
    Ok(())
}

/// The real time, in Unix milliseconds, an action-log entry records as the
/// moment it was appended.
fn wall_unix_ms() -> i64 {
    // The world's one wall-clock read. It is recorded for callers showing
    // when something happened, and nothing in the world reads it, so it is
    // not an input to replay (rule 5).
    #[allow(clippy::disallowed_methods)]
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}

pub(crate) fn constraint_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// GitHub login rules: alphanumeric and hyphens, no leading/trailing hyphen,
/// at most 39 chars. The system actor's login is reserved.
pub fn validate_login(login: &str) -> Result<()> {
    let valid = !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.eq_ignore_ascii_case(SYSTEM_ACTOR_LOGIN);
    if valid {
        Ok(())
    } else {
        Err(WorldError::InvalidLogin(login.to_string()))
    }
}

/// The kinds an actor can be created as: the engine seeds the one system
/// actor itself.
pub fn validate_actor_kind(kind: ActorKind) -> Result<()> {
    if kind == ActorKind::System {
        Err(WorldError::InvalidActorKind)
    } else {
        Ok(())
    }
}

/// GitHub repo-name rules, plus the shapes the git face's path gate refuses.
fn validate_repo_name(name: &str) -> Result<()> {
    let reserved: BTreeSet<&str> = [".", ".."].into();
    let valid = !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !reserved.contains(name)
        && !name.to_ascii_lowercase().ends_with(".git");
    if valid {
        Ok(())
    } else {
        Err(WorldError::InvalidRepoName(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{ActorKind, DEFAULT_T0_UNIX, World, WorldSecret, wall_unix_ms};

    // The bounds are read through the function the append reads, so the
    // world's one wall-clock read stays its only one.
    #[test]
    fn an_entry_records_the_real_time_it_was_appended_at() {
        let dir = tempfile::tempdir().unwrap();
        let w = World::create(
            &dir.path().join("world"),
            DEFAULT_T0_UNIX,
            &WorldSecret::LOCAL,
        )
        .unwrap();

        let before = wall_unix_ms();
        w.create_actor("alice", ActorKind::User).unwrap();
        let after = wall_unix_ms();

        let log = w.log_after(0, 100).unwrap();
        let created = log.last().unwrap();
        assert_eq!(created.action, "actor.create");
        let at = created.wall_unix_ms.expect("the entry records a real time");
        assert!(
            (before..=after).contains(&at),
            "appended at {at}, outside {before}..={after}"
        );
    }
}
