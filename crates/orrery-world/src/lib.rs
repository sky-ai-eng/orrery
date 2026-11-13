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

#[cfg(any(test, feature = "test-support"))]
mod audit;
mod checks;
mod clock;
mod credentials;
mod error;
pub mod events;
mod identity;
mod log;
mod objects;
mod orgs;
mod pulls;
mod queries;
mod reader;
mod schema;
mod signing;
mod types;

pub use checks::{CheckRunPatch, NewCheckRun};
pub use credentials::{OAUTH_CLIENT_ID_PREFIX, WorldSecret};
pub use error::WorldError;
pub use events::{Attribution, Door, Event, EventBody, WriteSpan};
pub use identity::{
    OAUTH_CODE_TTL_SECONDS, validate_oauth_app_callback_url, validate_oauth_app_name,
};
pub use pulls::NewPull;
pub use reader::{ViewMeta, WorldReader, WorldView};
#[cfg(feature = "test-support")]
pub use schema::test_support;
pub use schema::{BASELINE_VERSION, SCHEMA_VERSION};
pub use signing::{ssh_public_key, ssh_public_key_from_blob};
pub use types::*;

pub type Result<T> = std::result::Result<T, WorldError>;

/// Fallback T0 when a new world's config doesn't name one: 2020-01-01T00:00:00Z.
/// A fixed constant rather than "now" so that two worlds created from the same
/// config are the same world (rule 5), apart from the real time each of their
/// events records.
pub const DEFAULT_T0_UNIX: i64 = 1_577_836_800;

/// The reserved actor that makes control-plane and CLI writes. Seeded at
/// world creation; holds no token.
pub const SYSTEM_ACTOR_LOGIN: &str = "orrery";

/// The system actor's id: it is the first actor every world seeds.
pub const SYSTEM_ACTOR_ID: i64 = 1;

/// The most commits a ref event lists ([`RefMove::commits`]), GitHub's
/// limit for a push.
pub const REF_EVENT_COMMITS: usize = 2048;

const DB_FILE: &str = "world.db";
const REPOS_DIR: &str = "repos";

/// A ref a push or a merge moved, as the git side moved it, for
/// [`World::record_push`] and [`World::record_merge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMove<'a> {
    /// The ref's full name, such as `refs/heads/main`.
    pub name: &'a str,
    /// The object id before the move, in the caller's spelling for a ref
    /// that did not exist.
    pub old: &'a str,
    /// The object id after the move, in the caller's spelling for a ref the
    /// push deleted.
    pub new: &'a str,
    /// The pulls, by number, that take `new` as their head: for a branch,
    /// what [`World::pulls_to_sync`] named for this move.
    pub repins: Vec<i64>,
    /// Whether the move created the ref, moved it, or deleted it, which the
    /// kernel cannot read from `old` and `new` in the caller's spelling.
    pub change: RefChange,
    /// For a move, whether `old` is not an ancestor of `new`.
    pub forced: bool,
    /// The commits the move brought in, oldest first: for a move, those
    /// reachable from `new` and not from `old`; for a created ref, those
    /// reachable from it that no ref reached before the write. At most
    /// [`REF_EVENT_COMMITS`], the newest, and none for a deleted ref.
    pub commits: Vec<String>,
    /// How many commits the move brought in, all of them.
    pub commit_count: i64,
}

/// What a [`RefMove`] did to its ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefChange {
    Created,
    Moved,
    Deleted,
}

pub struct World {
    root: PathBuf,
    conn: Mutex<Connection>,
    listeners: RwLock<Vec<Listener>>,
    host: clock::HostTime,
}

type Listener = Box<dyn Fn(Signal) + Send + Sync>;

/// What a [`World`] tells the listeners registered with [`World::listen`].
/// A signal says only that something may have changed: the change itself is
/// read through the `World`. Each is raised with the connection lock held,
/// so any read that sees the change it announces begins after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A write appended its events to the log. It may be inside a
    /// transaction that has not committed yet, and that may roll back, so a
    /// read that follows sees the events once they commit, or nothing.
    LogMayHaveGrown,
    /// The world's state was replaced by a snapshot's, and its log
    /// with it. `newest_seq` is the newest `seq` in the log the world holds
    /// now, or 0 for an empty one.
    StateReplaced { newest_seq: i64 },
}

impl World {
    /// Create a new world at `root` (which must not already contain one) with
    /// a `manual` clock at `t0_unix` and every credential keyed by `secret`,
    /// and seed the system actor.
    pub fn create(root: &Path, t0_unix: i64, secret: &WorldSecret) -> Result<World> {
        World::create_with_clock(root, NewClock::Manual { t0_unix }, secret)
    }

    /// [`World::create`], with the clock `clock` says. The `orrery` command
    /// is the one caller that creates a world, so the write is the system
    /// actor's, through the CLI.
    pub fn create_with_clock(root: &Path, clock: NewClock, secret: &WorldSecret) -> Result<World> {
        if root.join(DB_FILE).exists() {
            return Err(WorldError::AlreadyExists(root.to_path_buf()));
        }
        std::fs::create_dir_all(root.join(REPOS_DIR))?;
        let world = World::connect(root)?;
        let (t0_unix, mode) = match clock {
            NewClock::Manual { t0_unix } => (t0_unix, ClockMode::Manual),
            NewClock::Real => (world.host.now_unix(), ClockMode::Real),
        };
        let by = Attribution::new(SYSTEM_ACTOR_ID, Door::Cli);
        world.write_keeping(
            clock::KeepTime::Never,
            |_| Ok((by, ())),
            |w, ()| {
                w.tx.execute(
                    "INSERT INTO world (id, clock_unix, clock_mode, secret) VALUES (1, ?1, ?2, ?3)",
                    params![t0_unix, mode.as_str(), secret],
                )?;
                w.tx.execute(
                    "INSERT INTO actors (id, login, kind, created_clock_unix)
                     VALUES (?1, ?2, 'system', ?3)",
                    params![SYSTEM_ACTOR_ID, SYSTEM_ACTOR_LOGIN, t0_unix],
                )?;
                w.operation(EventBody::WorldCreated(events::WorldCreated {
                    subject: events::WorldSubject::default(),
                    object: objects::world_object(w.tx)?,
                }));
                w.emit(EventBody::ActorCreated(events::ActorCreated {
                    subject: objects::actor_subject(w.tx, SYSTEM_ACTOR_ID)?,
                    object: objects::actor_object(w.tx, SYSTEM_ACTOR_ID)?,
                }));
                Ok(())
            },
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

    /// Open the world at `root`, creating it with `clock` and `secret` if
    /// absent. An existing world keeps the clock and the secret it holds.
    pub fn open_or_create(root: &Path, clock: NewClock, secret: &WorldSecret) -> Result<World> {
        if root.join(DB_FILE).exists() {
            World::open(root)
        } else {
            World::create_with_clock(root, clock, secret)
        }
    }

    fn connect(root: &Path) -> Result<World> {
        let conn = open_conn(root)?;
        Ok(World {
            root: root.to_path_buf(),
            conn: Mutex::new(conn),
            listeners: RwLock::new(Vec::new()),
            host: clock::HostTime::default(),
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

    // ---- actors & tokens ----

    /// Create an actor and mint its token. The token is derived, not random
    /// (rule 5: replay must reproduce it; rule 12: it is fake by construction),
    /// and keyed by the world secret, so it cannot be guessed. No public API
    /// creates an actor, so the control plane is the one caller, and the
    /// write is the system actor's.
    pub fn create_actor(&self, login: &str, kind: ActorKind) -> Result<(Actor, String)> {
        validate_login(login)?;
        validate_actor_kind(kind)?;
        let by = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);
        self.write(by, |w| {
            let conn = w.tx;
            let clock = queries::clock(conn)?;
            let actor = conn
                .query_row(
                    &format!(
                        "INSERT INTO actors (login, kind, created_clock_unix) VALUES (?1, ?2, ?3)
                     RETURNING {}",
                        queries::ACTOR_COLUMNS
                    ),
                    params![login, kind.as_str(), clock],
                    queries::row_to_actor,
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::ActorExists(login.to_string()),
                    false => WorldError::from(e),
                })?;
            let token = credentials::actor_token(
                &credentials::world_secret(conn)?,
                &actor.login,
                actor.id,
                0,
            );
            conn.execute(
                "INSERT INTO tokens (token, actor_id, origin, created_clock_unix)
                 VALUES (?1, ?2, ?3, ?4)",
                params![token, actor.id, credentials::TokenOrigin::Actor(0), clock],
            )?;
            identity::insert_email(conn, actor.id, &identity::default_email(&actor.login))?;
            w.operation(EventBody::ActorCreated(events::ActorCreated {
                subject: objects::actor_subject(conn, actor.id)?,
                object: objects::actor_object(conn, actor.id)?,
            }));
            Ok((actor, token))
        })
    }

    /// Issue another token for an existing actor. A stored token is never
    /// read back, so this is how anyone gets one for an actor they did not
    /// create, as in a world re-keyed at its creation. Every token issued
    /// before stays valid.
    pub fn issue_actor_token(&self, login: &str, by: Attribution) -> Result<(Actor, String)> {
        self.write(by, |w| {
            let tx = w.tx;
            let actor = tx
                .query_row(
                    &format!(
                        "SELECT {} FROM actors WHERE login = ?1",
                        queries::ACTOR_COLUMNS
                    ),
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
                        &credentials::world_secret(tx)?,
                        &actor.login,
                        actor.id,
                        n
                    ),
                    actor.id,
                    credentials::TokenOrigin::Actor(n),
                    queries::clock(tx)?
                ],
                |r| r.get(0),
            )?;
            w.operation(EventBody::ActorTokenIssued(events::ActorTokenIssued {
                subject: objects::actor_subject(tx, actor.id)?,
                n,
            }));
            Ok((actor, token))
        })
    }

    pub fn actor_by_login(&self, login: &str) -> Result<Option<Actor>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {} FROM actors WHERE login = ?1",
                    queries::ACTOR_COLUMNS
                ),
                params![login],
                queries::row_to_actor,
            )
            .optional()?)
    }

    pub fn actor_by_id(&self, id: i64) -> Result<Option<Actor>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {} FROM actors WHERE id = ?1",
                    queries::ACTOR_COLUMNS
                ),
                params![id],
                queries::row_to_actor,
            )
            .optional()?)
    }

    /// The caller `token` authenticates: the actor it acts as, and the
    /// OAuth app whose authorization code was traded for it.
    pub fn caller_by_token(&self, token: &str) -> Result<Option<Caller>> {
        let conn = self.lock();
        // The actor columns in `queries::ACTOR_COLUMNS`'s order, qualified,
        // since `tokens` has a `created_clock_unix` too. The code's id is
        // read out of `origin` so the lookup uses the primary key.
        let found = conn
            .query_row(
                "SELECT a.id, a.login, a.kind, a.created_clock_unix,
                        CASE WHEN t.origin GLOB 'oauth:*' THEN
                            (SELECT app_id FROM oauth_codes
                             WHERE id = CAST(substr(t.origin, 7) AS INTEGER))
                        END
                 FROM tokens t JOIN actors a ON a.id = t.actor_id
                 WHERE t.token = ?1",
                params![token],
                |r| Ok((queries::row_to_actor(r)?, r.get::<_, Option<i64>>(4)?)),
            )
            .optional()?;
        Ok(found.map(|(actor, app_id)| Caller { actor, app_id }))
    }

    pub fn actor_by_token(&self, token: &str) -> Result<Option<Actor>> {
        Ok(self.caller_by_token(token)?.map(|caller| caller.actor))
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
        by: Attribution,
    ) -> Result<Repo> {
        validate_repo_name(name)?;
        self.write(by, |w| {
            let tx = w.tx;
            // The insert's foreign keys refuse an unknown actor too, but as an
            // untyped SQLite error that does not say which id it was.
            for id in [owner_id, by.actor_id] {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM actors WHERE id = ?1)",
                    params![id],
                    |r| r.get(0),
                )?;
                if !exists {
                    return Err(WorldError::NoSuchActorId(id));
                }
            }
            let clock = queries::clock(tx)?;
            let repo = tx
                .query_row(
                    &format!(
                        "INSERT INTO repos (owner_id, name, description, private, default_branch,
                                        created_by, created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     RETURNING {}",
                        queries::REPO_COLUMNS
                    ),
                    params![
                        owner_id,
                        name,
                        description,
                        private,
                        default_branch,
                        by.actor_id,
                        clock
                    ],
                    queries::row_to_repo,
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::RepoExists(name.to_string()),
                    false => WorldError::from(e),
                })?;
            w.operation(EventBody::RepoCreated(events::RepoCreated {
                subject: objects::repo_subject(tx, repo.id)?,
                object: objects::repo_object(tx, repo.id)?,
            }));
            Ok(repo)
        })
    }

    pub fn repo_by_owner_name(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {} FROM repos
                     WHERE owner_id = (SELECT id FROM actors WHERE login = ?1) AND name = ?2",
                    queries::REPO_COLUMNS
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
                &format!("SELECT {} FROM repos WHERE id = ?1", queries::REPO_COLUMNS),
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

    /// Record a push the git side has taken. Its operation is `repo:pushed`:
    /// `pushed_at` moves to the current clock, and `default_branch` (when
    /// given) becomes the repository's default branch. A ref event follows
    /// for each move in `moves`, in order. Then every unmerged pull whose
    /// base a move creates or moves is marked [`Pull::base_moved`], and each
    /// move re-pins the pulls it names, so a pull the same push re-pins ends
    /// unmarked. Either leaves the pull's test merge stale
    /// ([`Pull::test_merge_stale`]). Then each branch a move deleted or created changes the pulls
    /// on it, in the same order, as GitHub changes them:
    ///
    /// - a deleted branch closes every open pull whose head it is, then
    ///   records its deletion on every pull whose head it is, closed and
    ///   merged ones too; and it records its deletion on every open pull whose
    ///   base it is, then closes that pull, one pull at a time;
    /// - a created branch records its restoration on every closed pull whose
    ///   head it is, which stays closed.
    ///
    /// A close leaves the pull's head, base, and draft flag as they were. It
    /// is one write, so a reader that sees any of the push's events sees the
    /// repository and its pulls as the push left them. A push that moved no
    /// ref records nothing.
    ///
    /// Last, it stores the verdict of each commit in `signed` that passes,
    /// in order, unless one is stored for it already: `signed` is what the
    /// commits the moves list say about themselves, and the verdict is
    /// reached against the world as the write finds it.
    ///
    /// A named pull that is not open refuses the whole record: the caller
    /// moved that pull's ref on the strength of it being open, and the world
    /// no longer agrees.
    pub fn record_push(
        &self,
        repo_id: i64,
        by: Attribution,
        moves: &[RefMove<'_>],
        default_branch: Option<&str>,
        signed: &[CommitFacts],
    ) -> Result<()> {
        if moves.is_empty() {
            return Ok(());
        }
        self.write(by, |w| {
            self.record_moves(w, repo_id, moves, default_branch, true)?;
            signing::record_verifications(w, repo_id, signed)
        })
    }

    /// The writes a push and a merge share, inside the caller's write: the
    /// repository's `repo:pushed`, as the operation when `operation`, then
    /// everything else [`World::record_push`] describes.
    pub(crate) fn record_moves(
        &self,
        w: &mut log::Write<'_>,
        repo_id: i64,
        moves: &[RefMove<'_>],
        default_branch: Option<&str>,
        operation: bool,
    ) -> Result<()> {
        let before = objects::repo_object(w.tx, repo_id)?;
        w.tx.execute(
            "UPDATE repos
             SET pushed_at_clock_unix = (SELECT clock_unix FROM world WHERE id = 1),
                 default_branch = COALESCE(?2, default_branch)
             WHERE id = ?1",
            params![repo_id, default_branch],
        )?;
        let changes = events::RepoChanges::between(&before, &objects::repo_object(w.tx, repo_id)?);
        // A push is its operation however little it changed in the
        // repository's row; a merge's move is a consequence only if it did.
        let changed = !changes.is_empty();
        let pushed = EventBody::RepoPushed(events::RepoChanged {
            subject: objects::repo_subject(w.tx, repo_id)?,
            changes,
        });
        if operation {
            w.operation(pushed);
        } else if changed {
            w.emit(pushed);
        }
        for m in moves {
            let event = ref_event(w.tx, repo_id, m)?;
            w.emit(event);
        }
        // Before any re-pin, so a pull whose head and base one push both move
        // ends with its head pinned against the moved base. A deleted base
        // needs no mark: a pull whose base is gone has no test merge at all.
        for m in moves {
            if let Some(branch) = m.name.strip_prefix("refs/heads/")
                && m.change != RefChange::Deleted
            {
                pulls::mark_base_moved(w, repo_id, branch)?;
            }
        }
        for m in moves {
            for &number in &m.repins {
                pulls::repin_open_pull(w, repo_id, number, m.new)?;
            }
        }
        // After every re-pin, so a pull a move re-pins is still open when it
        // is re-pinned, though a later move's deletion closes it.
        for m in moves {
            if let Some(branch) = m.name.strip_prefix("refs/heads/") {
                self.record_branch_change(w, repo_id, branch, m.change)?;
            }
        }
        Ok(())
    }

    // ---- snapshot support ----

    /// Run `f` with the database quiesced: the WAL checkpointed into the main
    /// file and the connection lock held, so no write can land while `f` runs.
    /// This is the moment a snapshot copy of the world directory is exact.
    ///
    /// A `real` world's clock is first brought to the host's time, as a
    /// write of the system actor's through the control plane, which takes
    /// snapshots, so a copy's clock is the moment it was made.
    pub fn quiesce<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        let conn = self.lock();
        self.catch_up(&conn, Door::Control)?;
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
    /// before. The log then has a gap where the discarded events were.
    ///
    /// [`Signal::StateReplaced`] is raised once the reopen succeeds, before
    /// the raise and whatever `f` returned: the state may have changed even
    /// when a later step fails, and listeners must not go on reading it as
    /// the log they had.
    pub fn swap_state(&self, f: impl FnOnce() -> std::io::Result<()>) -> Result<()> {
        let mut guard = self.lock();
        checkpoint(&guard)?;
        let issued = log::issued_seq(&guard)?;
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

/// The ref event for one move a push or a merge made.
fn ref_event(conn: &Connection, repo_id: i64, m: &RefMove<'_>) -> Result<EventBody> {
    let subject = objects::ref_subject(conn, repo_id, m.name)?;
    let object = |sha: &str| events::RefObject {
        name: m.name.to_string(),
        sha: sha.to_string(),
    };
    Ok(match m.change {
        RefChange::Created => EventBody::RefCreated(events::RefCreated {
            subject,
            object: object(m.new),
            commits: m.commits.clone(),
            commit_count: m.commit_count,
        }),
        RefChange::Moved => EventBody::RefUpdated(events::RefUpdated {
            subject,
            changes: events::RefChanges {
                sha: Some(events::Change {
                    from: m.old.to_string(),
                    to: m.new.to_string(),
                }),
                ..Default::default()
            },
            forced: m.forced,
            commits: m.commits.clone(),
            commit_count: m.commit_count,
        }),
        RefChange::Deleted => EventBody::RefDeleted(events::RefDeleted {
            subject,
            object: object(m.old),
        }),
    })
}

fn open_conn(root: &Path) -> Result<Connection> {
    let mut conn = Connection::open(root.join(DB_FILE))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    schema::apply(&mut conn)?;
    Ok(conn)
}

fn checkpoint(conn: &Connection) -> Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(())
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

/// Whether a write failed because a row with the same key already exists: a
/// UNIQUE constraint or index, or a PRIMARY KEY. A foreign-key, CHECK, or NOT
/// NULL failure is a different fault, and its caller reports it as itself.
pub(crate) fn uniqueness_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
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
    use super::clock::host_unix_ms;
    use super::{ActorKind, DEFAULT_T0_UNIX, World, WorldSecret, uniqueness_violation};

    // No kernel write reaches a foreign-key, CHECK, or NOT NULL failure, so
    // the predicate is checked against each kind of failure directly.
    #[test]
    fn only_a_repeated_key_is_a_uniqueness_violation() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE t (
                 a         INTEGER NOT NULL,
                 b         INTEGER NOT NULL,
                 u         INTEGER UNIQUE,
                 c         INTEGER CHECK (c > 0),
                 parent_id INTEGER REFERENCES parent(id),
                 PRIMARY KEY (a, b)
             );
             INSERT INTO t (a, b, u) VALUES (1, 1, 1);",
        )
        .unwrap();
        let fails = |sql: &str| {
            let e = conn.execute(sql, []).unwrap_err();
            assert!(
                matches!(&e, rusqlite::Error::SqliteFailure(f, _)
                    if f.code == rusqlite::ErrorCode::ConstraintViolation),
                "{sql}: {e}"
            );
            e
        };
        assert!(uniqueness_violation(&fails(
            "INSERT INTO t (a, b) VALUES (1, 1)"
        )));
        assert!(uniqueness_violation(&fails(
            "INSERT INTO t (a, b, u) VALUES (2, 2, 1)"
        )));
        assert!(!uniqueness_violation(&fails(
            "INSERT INTO t (a, b, parent_id) VALUES (3, 3, 9)"
        )));
        assert!(!uniqueness_violation(&fails(
            "INSERT INTO t (a, b, c) VALUES (4, 4, 0)"
        )));
        assert!(!uniqueness_violation(&fails(
            "INSERT INTO t (a) VALUES (5)"
        )));
    }

    // The bounds are read through the function the append reads, so the
    // world's one wall-clock read stays its only one.
    #[test]
    fn an_event_records_the_real_time_it_was_appended_at() {
        let dir = tempfile::tempdir().unwrap();
        let w = World::create(
            &dir.path().join("world"),
            DEFAULT_T0_UNIX,
            &WorldSecret::LOCAL,
        )
        .unwrap();

        let before = host_unix_ms();
        w.create_actor("alice", ActorKind::User).unwrap();
        let after = host_unix_ms();

        let log = w.log_after(0, 100).unwrap();
        let created = log.last().unwrap();
        assert_eq!(created.body.type_name(), "actor:created");
        let at = created.wall_unix_ms;
        assert!(
            (before..=after).contains(&at),
            "appended at {at}, outside {before}..={after}"
        );
    }
}
