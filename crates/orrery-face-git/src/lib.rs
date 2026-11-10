//! The git face: serves clone/fetch/push for the world's bare repos over git
//! smart-HTTP, and records every push in the kernel's log.
//!
//! URL shape is GitHub's, deliberately hardcoded: exactly two path segments,
//! `/{owner}/{repo}[.git]/...` (spec/surface-git.md).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::routing::{get, post};
use orrery_world::{Caller, Door, REF_EVENT_COMMITS, RefChange, RefMove, Repo, World};

pub mod cgi;
pub mod store;

/// Upper bound on a single git request body (a push's pack). Requests and CGI
/// responses are buffered whole — simple and adequate at test-world sizes;
/// streaming is a deliberate non-feature until a real consumer needs it.
pub const MAX_GIT_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum GitFaceError {
    #[error("git {args}: {stderr}")]
    Git { args: String, stderr: String },
    #[error("cgi: {0}")]
    Cgi(String),
    #[error(transparent)]
    World(#[from] orrery_world::WorldError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Refs that disagree with the world and could not be put back, one
    /// line each naming the repository and the ref.
    #[error("refs disagree with the world and could not be moved back: {}", .0.join("; "))]
    RefsNotRestored(Vec<String>),
}

pub struct GitFace {
    world: Arc<World>,
    /// One read-write lock per repo. A write holds it for write, from the
    /// refs it reads before moving any until its record commits: a push
    /// (receive-pack's before/after ref observation), a merge (its
    /// check-then-move), opening a pull request, and an edit of one that sets
    /// its state or base (closing, reopening, or retargeting it), since which
    /// pulls are open on a branch decides which pull refs a push moves before
    /// it is recorded. Every other git request, a fetch's in particular,
    /// holds it for read, so it sees the repository before a write or after its
    /// record, never partway, and so does an edit that sets neither, so it
    /// never lands between a write's read of the pull and that write's record.
    repo_locks: std::sync::Mutex<BTreeMap<i64, Arc<tokio::sync::RwLock<()>>>>,
    /// World-wide write gate: every git-side operation holds it for read;
    /// snapshot create/restore hold it for write, so a copy of the world
    /// directory never races an in-flight push or merge.
    gate: Arc<tokio::sync::RwLock<()>>,
    /// Set by [`GitFace::stop`] when its grace period runs out: every
    /// `git http-backend` still running, and any started after, is killed
    /// together with the processes it started ([`cgi::run_http_backend`]).
    kill: tokio::sync::watch::Sender<bool>,
    /// The gate, held for write from the moment [`GitFace::stop`] returns, so
    /// no git work starts again.
    stopped: std::sync::Mutex<Option<tokio::sync::OwnedRwLockWriteGuard<()>>>,
}

impl GitFace {
    pub fn new(world: Arc<World>) -> Arc<GitFace> {
        Arc::new(GitFace {
            world,
            repo_locks: std::sync::Mutex::new(BTreeMap::new()),
            gate: Arc::new(tokio::sync::RwLock::new(())),
            kill: tokio::sync::watch::Sender::new(false),
            stopped: std::sync::Mutex::new(None),
        })
    }

    /// Stop the face's git work for good, as the process serving it stops.
    /// It waits for every git operation already running, a push whose client
    /// has gone away included, since its task outlives the request, and
    /// keeps new ones from starting. One still running after `grace` has its
    /// `git http-backend` killed, with the receive-pack or upload-pack and
    /// hooks it started, and fails as any git request whose `git
    /// http-backend` fails does: a push moves back what it moved. Returns once
    /// none is running, so no git process the face started outlives the
    /// caller's exit.
    pub async fn stop(&self, grace: std::time::Duration) {
        let mut gate = std::pin::pin!(self.gate.clone().write_owned());
        let held = match tokio::time::timeout(grace, &mut gate).await {
            Ok(held) => held,
            Err(_) => {
                tracing::warn!(
                    grace_secs = grace.as_secs_f64(),
                    "git requests still running after the grace period; killing them"
                );
                self.kill.send_replace(true);
                gate.await
            }
        };
        *self.stopped.lock().expect("stopped mutex poisoned") = Some(held);
    }

    pub fn world(&self) -> &Arc<World> {
        &self.world
    }

    /// Initialize the bare repository for a kernel repo row (idempotent).
    pub fn init_repo_dir(&self, repo: &Repo) -> Result<(), GitFaceError> {
        store::init_bare(
            &self.world.repos_dir(),
            &repo.owner_login,
            &repo.name,
            &repo.default_branch,
        )?;
        Ok(())
    }

    /// Point a pull request's `refs/pull/{n}/head` at `head_sha`. The move
    /// has no event of its own: the ref is the pull's head, which every
    /// event that moves it carries. The caller holds the repository's lock
    /// for write.
    pub fn pin_pull_ref(
        &self,
        repo: &Repo,
        number: i64,
        head_sha: &str,
    ) -> Result<(), GitFaceError> {
        let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
        store::update_ref(&bare, &store::pull_head_ref(number), head_sha)
    }

    pub fn repo_lock(&self, repo_id: i64) -> Arc<tokio::sync::RwLock<()>> {
        let mut locks = self.repo_locks.lock().expect("repo_locks mutex poisoned");
        locks.entry(repo_id).or_default().clone()
    }

    /// The repository's lock, held for read, for a read of its refs outside
    /// git traffic. The caller already holds the world write gate for read,
    /// as git traffic does. It holds this lock only while it reads, never
    /// across an await for another repository's lock: a waiting write lets no
    /// new read in, so two reads each holding one repository and waiting on
    /// the other could each wait on the other's writer forever.
    pub async fn read_lock(&self, repo_id: i64) -> tokio::sync::OwnedRwLockReadGuard<()> {
        self.repo_lock(repo_id).read_owned().await
    }

    pub fn gate(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.gate.clone()
    }

    /// Put back the refs a write moved when recording it failed, so the
    /// repository agrees with the world, which holds no trace of the write.
    /// Each ref in `refs`, as `(name, old, new)`, returns to `old` by
    /// compare-and-swap, which deletes a ref the write created and recreates
    /// one it deleted. The ref of each pull request in `pinned`, the ones the
    /// write re-pinned, returns to the head the world still pins it at. A
    /// retry of the write then starts from the refs the failed one found. It
    /// is best effort: a ref that cannot be put back is logged, and the rest
    /// are still put back. The caller holds the repository's lock for write.
    pub fn move_back(&self, repo: &Repo, refs: &[(&str, &str, &str)], pinned: &[i64]) {
        let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
        // TODO(ORR-79): refs go back in name order, so a push that replaced
        // `a` with `a/b` leaves `a` deleted until the world is next served.
        for &(name, old, new) in refs {
            if let Err(e) = store::update_ref_cas(&bare, name, old, new) {
                tracing::error!(error = %e, git_ref = name, "unwind left a ref moved");
            }
        }
        for &number in pinned {
            let restored = match self.world.pull_by_number(repo.id, number) {
                Ok(Some(p)) => self.pin_pull_ref(repo, number, &p.head_sha),
                Ok(None) => Ok(()),
                Err(e) => Err(e.into()),
            };
            if let Err(e) = restored {
                tracing::error!(
                    error = %e,
                    git_ref = store::pull_head_ref(number),
                    "unwind left a ref moved"
                );
            }
        }
    }

    /// Record a completed receive-pack ([`World::record_push`]): one ref
    /// event per changed ref (sorted, so replay order is stable), with the
    /// commits each brought in, GitHub's "synchronize" for the pull requests
    /// on each pushed branch, the closes and timeline items a deleted or
    /// restored branch brings its pull requests, a `pushed_at` bump, and
    /// GitHub's first-push rule — the first branch pushed into an empty repo
    /// becomes the default branch. Receive-pack has already moved the pushed
    /// refs, so a step that fails here moves them back
    /// ([`GitFace::move_back`]), and `HEAD` if the push set it, before the
    /// error is returned. The caller holds the repository's lock for write.
    fn observe_push(
        &self,
        repo: &Repo,
        pusher: &Caller,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Result<(), GitFaceError> {
        let changed = changed_refs(before, after);
        if changed.is_empty() {
            return Ok(());
        }

        let adopted = if has_branches(before) {
            None
        } else {
            let new_branches: Vec<&str> = after
                .keys()
                .filter_map(|r| r.strip_prefix("refs/heads/"))
                .collect();
            ["main", "master"]
                .into_iter()
                .find(|b| new_branches.contains(b))
                .or(new_branches.first().copied())
                .filter(|b| *b != repo.default_branch)
        };
        let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
        let mut head_set = false;
        let mut pinned = Vec::new();
        let mut record = || -> Result<(), GitFaceError> {
            let mut moves = Vec::with_capacity(changed.len());
            for &(name, old, new) in &changed {
                let change = if old == store::ZERO_OID {
                    RefChange::Created
                } else if new == store::ZERO_OID {
                    RefChange::Deleted
                } else {
                    RefChange::Moved
                };
                // A deleted branch leaves its pull requests pinned to the last
                // sha, as GitHub does.
                let repins = match name.strip_prefix("refs/heads/") {
                    Some(branch) if change != RefChange::Deleted => {
                        self.world.pulls_to_sync(repo.id, branch, new)?
                    }
                    _ => Vec::new(),
                };
                // A new ref's commits are the ones no ref reached before.
                let (forced, (commits, commit_count)) = match change {
                    RefChange::Created => {
                        let reached: Vec<&str> = before.values().map(String::as_str).collect();
                        let added = store::added_commits(&bare, new, &reached, REF_EVENT_COMMITS)?;
                        (false, added)
                    }
                    RefChange::Moved => (
                        !store::is_ancestor(&bare, old, new)?,
                        store::added_commits(&bare, new, &[old], REF_EVENT_COMMITS)?,
                    ),
                    RefChange::Deleted => (false, (Vec::new(), 0)),
                };
                moves.push(RefMove {
                    name,
                    old,
                    new,
                    repins,
                    change,
                    forced,
                    commits,
                    commit_count,
                });
            }
            // Every ref the push moves is in place before the record commits,
            // as receive-pack moved the pushed branches before this ran, so a
            // reader that can see the push's events also sees its refs.
            if let Some(branch) = adopted {
                store::set_head_symref(&bare, branch)?;
                head_set = true;
            }
            for m in &moves {
                for &number in &m.repins {
                    self.pin_pull_ref(repo, number, m.new)?;
                    pinned.push(number);
                }
            }
            self.world
                .record_push(repo.id, pusher.via(Door::Git), &moves, adopted)?;
            Ok(())
        };
        if let Err(e) = record() {
            self.move_back(repo, &changed, &pinned);
            // Only the first-push rule moves `HEAD`, and its record moves the
            // default branch with it, so `HEAD` was at the default branch the
            // world still has.
            if head_set && let Err(head) = store::set_head_symref(&bare, &repo.default_branch) {
                tracing::error!(error = %head, git_ref = "HEAD", "unwind left a ref moved");
            }
            return Err(e);
        }
        Ok(())
    }
}

/// The refs that differ between `before` and `after`, as `(name, old, new)`
/// in ref-name order, [`store::ZERO_OID`] spelling a ref absent on one side.
fn changed_refs<'a>(
    before: &'a BTreeMap<String, String>,
    after: &'a BTreeMap<String, String>,
) -> Vec<(&'a str, &'a str, &'a str)> {
    let names: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let old = before.get(name).map_or(store::ZERO_OID, String::as_str);
            let new = after.get(name).map_or(store::ZERO_OID, String::as_str);
            (old != new).then_some((name.as_str(), old, new))
        })
        .collect()
}

fn has_branches(refs: &BTreeMap<String, String>) -> bool {
    refs.keys().any(|r| r.starts_with("refs/heads/"))
}

/// Put every repository's refs where the world puts them, before the world
/// serves anything (spec/surface-git.md, "Starting a world"). A push is
/// recorded only by the process that serves it, so one cut off by that
/// process dying can leave refs no event describes; this moves them back, as
/// a push that fails moves back what it moved.
///
/// Per repository: a missing directory is created with the recipe; every
/// ref lock file is removed, since only a git that was killed partway leaves
/// one; each branch and tag goes to the sha of its last ref event in the log
/// ([`World::logged_refs`]), and is deleted when that event deletes it or
/// there is none; each pull request's `refs/pull/{n}/head` goes to its head;
/// and `HEAD` names the default branch. Each change is logged. A ref that
/// can't be moved back is reported in [`GitFaceError::RefsNotRestored`],
/// after every other repository has been put right, and the world must not
/// be served: it would serve refs its log does not describe. It takes no
/// repository lock, so nothing may serve the world while it runs.
pub fn restore_refs(world: &World) -> Result<(), GitFaceError> {
    use orrery_world::WorldView as _;

    let mut expected_by_repo = world.logged_refs()?;
    for pull in world.list_pulls()? {
        expected_by_repo
            .entry(pull.repo_id)
            .or_default()
            .insert(store::pull_head_ref(pull.number), pull.head_sha);
    }
    let repos_dir = world.repos_dir();
    let mut stuck = Vec::new();
    for repo in world.list_repos()? {
        let expected = expected_by_repo.remove(&repo.id).unwrap_or_default();
        if let Err(e) = restore_repo(&repos_dir, &repo, &expected, &mut stuck) {
            stuck.push(format!("{}: {e}", repo.full_name()));
        }
    }
    if stuck.is_empty() {
        Ok(())
    } else {
        Err(GitFaceError::RefsNotRestored(stuck))
    }
}

/// [`restore_refs`] for one repository, whose refs belong at `expected`. A
/// ref or `HEAD` that won't move is added to `stuck`; an error is one that
/// stopped the repository from being checked at all.
fn restore_repo(
    repos_dir: &std::path::Path,
    repo: &Repo,
    expected: &BTreeMap<String, String>,
    stuck: &mut Vec<String>,
) -> Result<(), GitFaceError> {
    let full_name = repo.full_name();
    let bare = store::bare_dir(repos_dir, &repo.owner_login, &repo.name);
    if !bare.join("HEAD").exists() {
        tracing::warn!(repo = %full_name, "the repository had no directory; creating it");
        store::init_bare(
            repos_dir,
            &repo.owner_login,
            &repo.name,
            &repo.default_branch,
        )?;
    }
    for lock in store::ref_lock_files(&bare)? {
        std::fs::remove_file(&lock)?;
        tracing::warn!(repo = %full_name, lock = %lock.display(), "removed a stale ref lock file");
    }
    let actual = store::list_refs(&bare)?;
    let mut changes = changed_refs(expected, &actual);
    // Deletions first: a ref put back can need a path a deleted one holds, as
    // `refs/heads/a` does while `refs/heads/a/b` exists.
    changes.sort_by_key(|&(_, to, _)| to != store::ZERO_OID);
    for (name, to, from) in changes {
        match store::update_ref_cas(&bare, name, to, from) {
            Ok(()) => tracing::warn!(
                repo = %full_name,
                git_ref = name,
                from,
                to,
                "moved a ref the log does not describe back to where the world puts it"
            ),
            Err(e) => stuck.push(format!("{full_name} {name}: {e}")),
        }
    }
    let head = format!("refs/heads/{}", repo.default_branch);
    let named = store::head_symref(&bare)?;
    if named.as_deref() != Some(head.as_str()) {
        match store::set_head_symref(&bare, &repo.default_branch) {
            Ok(()) => tracing::warn!(
                repo = %full_name,
                from = named.as_deref().unwrap_or("no ref"),
                to = %head,
                "pointed HEAD back at the default branch"
            ),
            Err(e) => stuck.push(format!("{full_name} HEAD: {e}")),
        }
    }
    Ok(())
}

/// A push from the moment it reads the repository's refs, before
/// receive-pack runs, until it is observed ([`UnobservedPush::observe`]).
/// Dropped before then, by a return that answers the push with an error or
/// by a panic unwinding the push's task, it moves back every ref that differs
/// from the ones it read ([`GitFace::move_back`]), pull request refs an
/// observation re-pinned included, and `HEAD` to the default branch if the
/// repository had no branches, since only the first-push rule moves it. The
/// repository then agrees with the world, which holds nothing of the push,
/// and a retry starts from the refs the failed push found. The caller holds
/// the repository's lock for write until this is dropped.
struct UnobservedPush<'a> {
    face: &'a GitFace,
    repo: &'a Repo,
    bare: &'a std::path::Path,
    before: BTreeMap<String, String>,
    armed: bool,
}

impl UnobservedPush<'_> {
    /// Record what receive-pack moved ([`GitFace::observe_push`]), which
    /// moves back what the push moved when the record fails. Refs that can't
    /// be listed after receive-pack can't be moved back either: which ones it
    /// moved is unknown.
    fn observe(mut self, pusher: &Caller) -> Result<(), GitFaceError> {
        let observed = store::list_refs(self.bare).and_then(|after| {
            self.face
                .observe_push(self.repo, pusher, &self.before, &after)
        });
        self.armed = false;
        observed
    }
}

impl Drop for UnobservedPush<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Nothing here reads the world, so a pull request ref goes back to the
        // value read before the push rather than to the head the world pins:
        // the panic being unwound may have poisoned the world's lock, and a
        // second panic while unwinding aborts the process.
        match store::list_refs(self.bare) {
            Ok(now) => self
                .face
                .move_back(self.repo, &changed_refs(&self.before, &now), &[]),
            Err(e) => {
                tracing::error!(error = %e, "listing refs after a failed push failed; none moved back");
            }
        }
        if !has_branches(&self.before)
            && let Err(e) = store::set_head_symref(self.bare, &self.repo.default_branch)
        {
            tracing::error!(error = %e, git_ref = "HEAD", "unwind left a ref moved");
        }
    }
}

pub fn router(face: Arc<GitFace>) -> Router {
    Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
        .layer(DefaultBodyLimit::max(MAX_GIT_BODY_BYTES))
        .with_state(face)
}

/// Whether a request path is git smart-HTTP shaped (used by the server's
/// router to keep face fallbacks apart from git traffic).
pub fn is_smart_http_path(path: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
}

async fn info_refs(
    State(face): State<Arc<GitFace>>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    caller: Option<Extension<Caller>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    let service = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("service="))
        .unwrap_or("");
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return plain(
            StatusCode::BAD_REQUEST,
            "smart HTTP only: missing service parameter",
        );
    }
    serve(
        face,
        &owner,
        &repo_name,
        caller.as_deref(),
        "GET",
        "info/refs",
        &query,
        &headers,
        &[],
    )
    .await
}

async fn upload_pack(
    State(face): State<Arc<GitFace>>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    caller: Option<Extension<Caller>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    serve(
        face,
        &owner,
        &repo_name,
        caller.as_deref(),
        "POST",
        "git-upload-pack",
        &query,
        &headers,
        &body,
    )
    .await
}

async fn receive_pack(
    State(face): State<Arc<GitFace>>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    caller: Option<Extension<Caller>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    // The push runs in a task of its own, which a dropped handler doesn't
    // cancel. The server drops a handler whose client has gone away, and the
    // receive-pack it started moves refs whether anyone waits for it or not,
    // so the push must still hold the lock and record what it moved.
    let push = tokio::spawn(async move {
        serve(
            face,
            &owner,
            &repo_name,
            caller.as_deref(),
            "POST",
            "git-receive-pack",
            &query,
            &headers,
            &body,
        )
        .await
    });
    match push.await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, "push task failed");
            plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    face: Arc<GitFace>,
    owner: &str,
    repo_name: &str,
    caller: Option<&Caller>,
    method: &str,
    tail: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response<Body> {
    // Reads and writes alike hold the world write gate for read: a snapshot
    // copy (write side) waits for in-flight git traffic and blocks new
    // traffic for its duration.
    let _gate = face.gate.clone().read_owned().await;
    let name = repo_name.strip_suffix(".git").unwrap_or(repo_name);
    let repo = match face.world.repo_by_owner_name(owner, name) {
        Ok(Some(r)) => r,
        Ok(None) => return plain(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => {
            tracing::error!(error = %e, "repo lookup failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);

    let is_push = tail == "git-receive-pack";
    // GitHub's anonymous-access rule: public repos serve fetch traffic with
    // no credentials; pushes and private repos never do. 401 (not 404) so a
    // real git client knows credentials would change the answer. The auth
    // middleware already refuses anonymous non-read traffic; this re-check
    // makes the face safe on its own terms.
    if caller.is_none() && (repo.private || is_push) {
        return plain_unauthorized();
    }
    // Fetch requests share the lock, so they never wait on each other. A
    // write waits for the ones already running, and new ones queue behind a
    // waiting write, so a stream of fetches cannot hold a push off forever.
    let lock = face.repo_lock(repo.id);
    let (_write, _read) = if is_push {
        (Some(lock.write().await), None)
    } else {
        (None, Some(lock.read().await))
    };
    // Declared after the lock, so it is dropped while the lock is held.
    let unobserved = if is_push {
        match store::list_refs(&bare) {
            Ok(before) => Some(UnobservedPush {
                face: &face,
                repo: &repo,
                bare: &bare,
                before,
                armed: true,
            }),
            Err(e) => {
                tracing::error!(error = %e, "listing refs before push failed");
                return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
            }
        }
    } else {
        None
    };

    let req = cgi::CgiRequest {
        method,
        // The CGI path always uses the row's canonical casing and the `.git`
        // form, whatever the URL said.
        path_info: format!("/{}/{}.git/{}", repo.owner_login, repo.name, tail),
        query,
        content_type: header(headers, "content-type"),
        content_encoding: header(headers, "content-encoding"),
        git_protocol: header(headers, "git-protocol"),
        remote_user: caller.map(|c| c.actor.login.as_str()),
        body,
    };
    let cgi_resp =
        match cgi::run_http_backend(&face.world.repos_dir(), req, face.kill.subscribe()).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "git http-backend failed");
                return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
            }
        };

    // http-backend writes its whole header block, with no `Status` line,
    // before it starts receive-pack, and closes its output once it has, so a
    // push answered with any other status ended before receive-pack ran and
    // is not observed. `unobserved` is still dropped armed on that path, so
    // it would move back any ref that had moved; it finds none.
    if let Some(unobserved) = unobserved
        && cgi_resp.status == 200
    {
        let pusher = caller.expect("pushes are refused without a caller above");
        if let Err(e) = unobserved.observe(pusher) {
            tracing::error!(error = %e, "push observation failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    }

    let mut builder = Response::builder()
        .status(StatusCode::from_u16(cgi_resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in &cgi_resp.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(cgi_resp.body))
        .expect("static response parts are valid")
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The git-flavored 401: WWW-Authenticate so a real client retries with
/// credentials rather than reporting a missing repo.
fn plain_unauthorized() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", "Basic realm=\"orrery\"")
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from("Authentication required"))
        .expect("static response parts are valid")
}

fn plain(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_string()))
        .expect("static response parts are valid")
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::{Pin, pin};
    use std::task::{Context, Poll, Waker};

    use orrery_world::{ActorKind, Attribution, DEFAULT_T0_UNIX, Signal, WorldSecret};

    use super::*;

    type Request = Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

    /// A world with one public repository, `alice/webapp`, whose `main` holds
    /// one commit.
    struct Fixture {
        face: Arc<GitFace>,
        repo: Repo,
        alice: Caller,
        bare: PathBuf,
        main: String,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let world = Arc::new(
            World::create(
                &dir.path().join("world"),
                DEFAULT_T0_UNIX,
                &WorldSecret::LOCAL,
            )
            .unwrap(),
        );
        let (alice, _) = world.create_actor("alice", ActorKind::User).unwrap();
        let repo = world
            .create_repo(
                alice.id,
                "webapp",
                None,
                false,
                "main",
                Attribution::new(alice.id, Door::Rest),
            )
            .unwrap();
        let face = GitFace::new(world);
        face.init_repo_dir(&repo).unwrap();
        let bare = store::bare_dir(&face.world.repos_dir(), "alice", "webapp");
        let main = commit(&bare, &[], "first");
        store::update_ref(&bare, "refs/heads/main", &main).unwrap();
        Fixture {
            face,
            repo,
            alice: Caller {
                actor: alice,
                app_id: None,
            },
            bare,
            main,
            _dir: dir,
        }
    }

    /// A commit of the empty tree on `parents`.
    fn commit(bare: &std::path::Path, parents: &[&str], message: &str) -> String {
        let tree = store::hermetic_git(bare).arg("mktree").output().unwrap();
        let tree = String::from_utf8(tree.stdout).unwrap();
        store::commit_tree(
            bare,
            tree.trim(),
            parents,
            message,
            "alice",
            DEFAULT_T0_UNIX,
        )
        .unwrap()
    }

    /// The two requests of a fetch that read the repository's refs: the ref
    /// advertisement, and protocol v2's `ls-refs`, a POST to upload-pack.
    /// Each is polled outside tokio's cooperative budget, so a lock that is
    /// free is taken on the first poll however much the test polled before.
    fn ref_reads(face: &Arc<GitFace>) -> [(&'static str, Request); 2] {
        let path = || UrlPath(("alice".to_string(), "webapp".to_string()));
        let mut v2 = HeaderMap::new();
        v2.insert("git-protocol", "version=2".parse().unwrap());
        v2.insert(
            "content-type",
            "application/x-git-upload-pack-request".parse().unwrap(),
        );
        [
            (
                "ref advertisement",
                Box::pin(tokio::task::unconstrained(info_refs(
                    State(face.clone()),
                    path(),
                    None,
                    RawQuery(Some("service=git-upload-pack".to_string())),
                    HeaderMap::new(),
                ))),
            ),
            (
                "ls-refs",
                Box::pin(tokio::task::unconstrained(upload_pack(
                    State(face.clone()),
                    path(),
                    None,
                    RawQuery(None),
                    v2,
                    Bytes::from_static(b"0014command=ls-refs\n00010000"),
                ))),
            ),
        ]
    }

    /// Poll once, with a waker that does nothing: a request that is
    /// waiting stays where it is until it is polled again.
    fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
        fut.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// A push of `body` to `alice/webapp`, as alice.
    fn push(f: &Fixture, body: Bytes) -> Request {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/x-git-receive-pack-request".parse().unwrap(),
        );
        Box::pin(tokio::task::unconstrained(receive_pack(
            State(f.face.clone()),
            UrlPath(("alice".to_string(), "webapp".to_string())),
            Some(Extension(f.alice.clone())),
            RawQuery(None),
            headers,
            body,
        )))
    }

    /// A push's body creating `refs/heads/topic` at a new root commit, as a
    /// client sends it: the command, a flush, and a pack of the commit and its
    /// tree. Returns the commit and the body.
    fn push_topic() -> (String, Bytes) {
        let src = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| store::hermetic_git(src.path()).args(args).output().unwrap();
        assert!(git(&["init", "-q", "--bare", "."]).status.success());
        let topic = commit(src.path(), &[], "topic");
        let mut pack = store::hermetic_git(src.path())
            .args(["pack-objects", "-q", "--stdout", "--revs"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            &mut pack.stdin.take().unwrap(),
            format!("{topic}\n").as_bytes(),
        )
        .unwrap();
        let pack = pack.wait_with_output().unwrap();
        assert!(pack.status.success());
        let command = format!("{} {topic} refs/heads/topic\n", store::ZERO_OID);
        let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
        body.extend_from_slice(&pack.stdout);
        (topic, Bytes::from(body))
    }

    /// Poll `push` until it waits for the repository's lock for write, which
    /// it does in the task it runs in, so possibly after its first poll. A
    /// waiting write lets no new read in. A push that answers before it
    /// waits ran during the read.
    async fn until_waiting<F: Future + ?Sized>(
        lock: &tokio::sync::RwLock<()>,
        mut push: Pin<&mut F>,
    ) {
        while lock.try_read().is_ok() {
            assert!(
                poll_once(push.as_mut()).is_pending(),
                "the push ran during a read"
            );
            tokio::task::yield_now().await;
        }
    }

    async fn body_text(resp: Response<Body>) -> String {
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).into_owned()
    }

    /// The test holds the repository's lock for write, as a merge does, and
    /// under it moves `main` to a merge commit and back, as a merge that fails
    /// to record does. A fetch request sent meanwhile is still waiting for the
    /// lock when the write ends, since it is handed the lock at that moment
    /// and no other write can start; it then sees `main` where it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fetch_request_waits_out_a_write() {
        let f = fixture();
        let lock = f.face.repo_lock(f.repo.id);
        let merge = commit(&f.bare, &[&f.main], "merge");
        for (what, request) in ref_reads(&f.face) {
            let write = lock.clone().write_owned().await;
            let mut request = pin!(request);
            assert!(
                poll_once(request.as_mut()).is_pending(),
                "{what}: answered during the write"
            );
            store::update_ref_cas(&f.bare, "refs/heads/main", &merge, &f.main).unwrap();
            store::update_ref_cas(&f.bare, "refs/heads/main", &f.main, &merge).unwrap();
            drop(write);
            assert!(
                lock.try_write().is_err(),
                "{what}: was not waiting for the repository's lock"
            );

            let body = body_text(request.await).await;
            assert!(
                body.contains(&format!("{} refs/heads/main", f.main)),
                "{what}: {body}"
            );
            assert!(!body.contains(&merge), "{what}: {body}");
        }
    }

    /// The test holds the repository's lock for read, as a fetch request in
    /// progress does. Another fetch request takes it too and is answered
    /// meanwhile. One waiting for write would have stopped any new read from
    /// starting, as a waiting write does.
    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_requests_do_not_wait_on_each_other() {
        let f = fixture();
        let lock = f.face.repo_lock(f.repo.id);
        for (what, request) in ref_reads(&f.face) {
            let read = lock.clone().read_owned().await;
            let mut request = pin!(request);
            let _ = poll_once(request.as_mut());
            assert!(
                lock.try_read().is_ok(),
                "{what}: waited for the read in progress"
            );
            let body = body_text(request.await).await;
            assert!(
                body.contains(&format!("{} refs/heads/main", f.main)),
                "{what}: {body}"
            );
            drop(read);
        }
    }

    /// The test holds the repository's lock for read, as a fetch request in
    /// progress does. A push waits for it, and a fetch request sent after the
    /// push waits for the push, so fetches arriving one after another cannot
    /// hold a push off.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_waits_for_fetch_requests_in_progress_and_not_after() {
        let f = fixture();
        let lock = f.face.repo_lock(f.repo.id);
        let read = lock.clone().read_owned().await;

        // A push that moves nothing: a flush, with no commands before it.
        let mut push = push(&f, Bytes::from_static(b"0000"));
        until_waiting(&lock, push.as_mut()).await;
        let [(_, later), _] = ref_reads(&f.face);
        let mut later = pin!(later);
        assert!(
            poll_once(later.as_mut()).is_pending(),
            "a later fetch request was answered ahead of the push"
        );

        drop(read);
        assert!(
            lock.try_read().is_err(),
            "the push was not waiting for the repository's lock"
        );
        assert_eq!(push.await.status(), StatusCode::OK);
        assert!(
            lock.try_write().is_err(),
            "the later fetch request was not waiting for the push"
        );
        body_text(later.await).await;
    }

    /// The test holds the repository's lock for read, as a fetch request in
    /// progress does, and a push creating `topic` waits for it. The push's
    /// client then goes away and the server drops its handler. The push keeps
    /// its place for the lock, and lands and is recorded once the read ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_whose_client_goes_away_still_lands_and_is_recorded() {
        let f = fixture();
        let lock = f.face.repo_lock(f.repo.id);
        let read = lock.clone().read_owned().await;
        let (topic, body) = push_topic();

        let mut push = push(&f, body);
        until_waiting(&lock, push.as_mut()).await;
        drop(push);
        assert!(
            lock.try_read().is_err(),
            "the push stopped waiting when its handler was dropped"
        );

        drop(read);
        let _after = lock.clone().write_owned().await;
        assert_eq!(
            store::rev_parse(&f.bare, "refs/heads/topic").unwrap(),
            Some(topic.clone())
        );
        let log = f.face.world.log_after(0, 100).unwrap();
        assert!(
            log.iter().any(|e| matches!(&e.body,
                orrery_world::EventBody::RefCreated(c)
                    if c.subject.name == "refs/heads/topic" && c.object.sha == topic)),
            "{log:?}"
        );
    }

    /// The branch `HEAD` names.
    fn head(bare: &std::path::Path) -> String {
        let out = store::hermetic_git(bare)
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Record the repository's refs as one push by alice that created them
    /// all, as if receive-pack had moved them.
    fn record_refs(f: &Fixture) {
        f.face
            .observe_push(
                &f.repo,
                &f.alice,
                &BTreeMap::new(),
                &store::list_refs(&f.bare).unwrap(),
            )
            .unwrap();
    }

    /// A world whose log records `main`, `feature`, and `a` in
    /// `alice/webapp`, and which has a pull request from `feature` into
    /// `main`, has every kind of damage a killed push or a hand-run git can
    /// leave: a branch moved, one deleted, one created, `refs/heads/a`
    /// replaced by `refs/heads/a/b`, the pull request's ref moved, a symbolic
    /// ref to `main`, `HEAD` naming another branch, and lock files left on
    /// refs. Start-up puts
    /// every ref back where the world puts it, removes the lock files, and
    /// appends nothing to the log.
    #[test]
    fn start_up_puts_back_every_ref_the_world_does_not_describe() {
        let f = fixture();
        let feature = commit(&f.bare, &[&f.main], "feature");
        store::update_ref(&f.bare, "refs/heads/feature", &feature).unwrap();
        store::update_ref(&f.bare, "refs/heads/a", &f.main).unwrap();
        record_refs(&f);
        f.face
            .world
            .create_pull(
                Attribution::new(f.alice.actor.id, Door::Rest),
                orrery_world::NewPull {
                    repo_id: f.repo.id,
                    title: "Add the feature",
                    body: None,
                    head_ref: "feature",
                    head_sha: &feature,
                    base_ref: "main",
                    base_sha: &f.main,
                    draft: false,
                },
            )
            .unwrap();
        f.face.pin_pull_ref(&f.repo, 1, &feature).unwrap();
        let recorded = store::list_refs(&f.bare).unwrap();
        let mark = f.face.world.log_after(0, 100).unwrap().last().unwrap().seq;

        let stray = commit(&f.bare, &[&feature], "stray");
        store::update_ref(&f.bare, "refs/heads/main", &stray).unwrap();
        store::update_ref_cas(&f.bare, "refs/heads/feature", store::ZERO_OID, &feature).unwrap();
        store::update_ref(&f.bare, "refs/heads/topic", &stray).unwrap();
        store::update_ref_cas(&f.bare, "refs/heads/a", store::ZERO_OID, &f.main).unwrap();
        store::update_ref(&f.bare, "refs/heads/a/b", &stray).unwrap();
        store::update_ref(&f.bare, &store::pull_head_ref(1), &stray).unwrap();
        let alias = store::hermetic_git(&f.bare)
            .args(["symbolic-ref", "refs/heads/alias", "refs/heads/main"])
            .status()
            .unwrap();
        assert!(alias.success());
        store::set_head_symref(&f.bare, "topic").unwrap();
        for lock in [
            "refs/heads/main.lock",
            "refs/heads/a/b.lock",
            "packed-refs.lock",
        ] {
            std::fs::write(f.bare.join(lock), "").unwrap();
        }

        restore_refs(&f.face.world).unwrap();
        assert_eq!(store::list_refs(&f.bare).unwrap(), recorded);
        assert_eq!(head(&f.bare), "refs/heads/main");
        assert_eq!(
            store::ref_lock_files(&f.bare).unwrap(),
            Vec::<PathBuf>::new()
        );
        assert!(f.face.world.log_after(mark, 100).unwrap().is_empty());

        // A repository that agrees with the world is left as it is.
        restore_refs(&f.face.world).unwrap();
        assert_eq!(store::list_refs(&f.bare).unwrap(), recorded);
    }

    /// The log puts `ghost` at a commit the repository does not hold, so
    /// start-up cannot put it back. It still deletes the branch no event
    /// describes, and then names the repository and the ref it could not
    /// move.
    #[test]
    fn start_up_names_a_ref_it_cannot_put_back() {
        let f = fixture();
        record_refs(&f);
        let missing = "1111111111111111111111111111111111111111";
        f.face
            .world
            .record_push(
                f.repo.id,
                f.alice.via(Door::Git),
                &[RefMove {
                    name: "refs/heads/ghost",
                    old: store::ZERO_OID,
                    new: missing,
                    repins: Vec::new(),
                    change: RefChange::Created,
                    forced: false,
                    commits: vec![missing.to_string()],
                    commit_count: 1,
                }],
                None,
            )
            .unwrap();
        store::update_ref(&f.bare, "refs/heads/topic", &f.main).unwrap();

        let err = restore_refs(&f.face.world).unwrap_err();
        let GitFaceError::RefsNotRestored(stuck) = &err else {
            panic!("{err}");
        };
        assert_eq!(stuck.len(), 1, "{stuck:?}");
        assert!(
            stuck[0].starts_with("alice/webapp refs/heads/ghost: "),
            "{stuck:?}"
        );
        assert_eq!(
            store::list_refs(&f.bare).unwrap(),
            [("refs/heads/main".to_string(), f.main.clone())].into()
        );
    }

    /// A repository the world holds a row for and no directory, as a
    /// process that died between the two leaves it, is created at start-up
    /// with the recipe.
    #[test]
    fn start_up_creates_a_missing_repository() {
        let f = fixture();
        std::fs::remove_dir_all(&f.bare).unwrap();
        restore_refs(&f.face.world).unwrap();
        assert_eq!(head(&f.bare), "refs/heads/main");
        assert!(store::list_refs(&f.bare).unwrap().is_empty());
    }

    /// The first push into a repository with no branches sets `HEAD`, and a
    /// record that fails then puts `HEAD` back with the branch. git refuses
    /// to delete a repository's current branch, so a repository with no
    /// branches never had any and has no pull request whose ref could fail
    /// to move; the record fails here because the world holds no row for the
    /// repository the push names. The test moves the refs itself, as
    /// receive-pack would have.
    #[test]
    fn a_first_push_that_fails_puts_head_back() {
        let f = fixture();
        store::update_ref_cas(&f.bare, "refs/heads/main", store::ZERO_OID, &f.main).unwrap();
        let before = store::list_refs(&f.bare).unwrap();
        let mark = f.face.world.log_after(0, 100).unwrap().last().unwrap().seq;

        store::update_ref(&f.bare, "refs/heads/topic", &f.main).unwrap();
        let after = store::list_refs(&f.bare).unwrap();
        let unrecorded = Repo {
            id: f.repo.id + 1,
            ..f.repo.clone()
        };
        assert!(
            f.face
                .observe_push(&unrecorded, &f.alice, &before, &after)
                .is_err()
        );
        assert_eq!(store::list_refs(&f.bare).unwrap(), before);
        assert_eq!(head(&f.bare), "refs/heads/main");
        assert!(f.face.world.log_after(mark, 100).unwrap().is_empty());

        // The same push, recorded, sets `HEAD` and the default branch.
        store::update_ref(&f.bare, "refs/heads/topic", &f.main).unwrap();
        f.face
            .observe_push(&f.repo, &f.alice, &before, &after)
            .unwrap();
        assert_eq!(head(&f.bare), "refs/heads/topic");
        let repo = f.face.world.repo_by_owner_name("alice", "webapp").unwrap();
        assert_eq!(repo.unwrap().default_branch, "topic");
    }

    /// The first push into a repository with no branches, creating `topic`,
    /// sets `HEAD` to it before its record, and a listener panics the push's
    /// task as the record appends its first entry. The push is answered
    /// `500`, `topic` is gone, and `HEAD` names `main` again. The panic
    /// poisons the world's lock, so the log is read through a connection of
    /// its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_push_that_panics_puts_head_back() {
        let f = fixture();
        store::update_ref_cas(&f.bare, "refs/heads/main", store::ZERO_OID, &f.main).unwrap();
        let before = store::list_refs(&f.bare).unwrap();
        let mark = f.face.world.log_after(0, 100).unwrap().last().unwrap().seq;
        let (_, body) = push_topic();

        f.face.world.listen(|signal| {
            if signal == Signal::LogMayHaveGrown {
                panic!("the push's record panicked");
            }
        });
        assert_eq!(
            push(&f, body).await.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(store::list_refs(&f.bare).unwrap(), before);
        assert_eq!(head(&f.bare), "refs/heads/main");
        let world = World::open(f.face.world.root()).unwrap();
        assert!(world.log_after(mark, 100).unwrap().is_empty());
    }

    /// Install `hooks/{name}` in `bare`: a hook that creates `started`, then
    /// waits for `release` to exist, for at most 30 seconds, so a test that
    /// fails partway leaves nothing running for long.
    fn hold_in_hook(
        bare: &std::path::Path,
        name: &str,
        started: &std::path::Path,
        release: &std::path::Path,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let hook = bare.join("hooks").join(name);
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho $$ > '{0}.new' && mv '{0}.new' '{0}'\ni=0\n\
                 while [ ! -e '{1}' ] && [ $i -lt 600 ]; do sleep 0.05; i=$((i+1)); done\n",
                started.display(),
                release.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Wait, for up to 10 seconds, for the hook [`hold_in_hook`] installed to
    /// start; returns its process id.
    async fn hook_started(started: &std::path::Path) -> u32 {
        for _ in 0..200 {
            if let Ok(pid) = std::fs::read_to_string(started) {
                return pid.trim().parse().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the hook did not start");
    }

    /// A request dropped before its `git http-backend` exits, as the server
    /// drops one whose client has gone away, kills the processes it started:
    /// here a pre-receive hook that would otherwise wait on. A process killed
    /// with SIGKILL stays listed until it has finished exiting, and a zombie
    /// waits for whoever reaps it, so both count as gone.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_request_kills_the_git_it_started() {
        let f = fixture();
        let started = f._dir.path().join("started");
        hold_in_hook(
            &f.bare,
            "pre-receive",
            &started,
            &f._dir.path().join("never"),
        );
        let (_, body) = push_topic();
        let repos = f.face.world.repos_dir();
        let (_kill, kill) = tokio::sync::watch::channel(false);
        let request = tokio::spawn(async move {
            let req = cgi::CgiRequest {
                method: "POST",
                path_info: "/alice/webapp.git/git-receive-pack".to_string(),
                query: "",
                content_type: Some("application/x-git-receive-pack-request"),
                content_encoding: None,
                git_protocol: None,
                remote_user: Some("alice"),
                body: &body,
            };
            cgi::run_http_backend(&repos, req, kill).await
        });
        let hook = hook_started(&started).await;

        request.abort();
        match request.await {
            Err(e) => assert!(e.is_cancelled(), "{e}"),
            Ok(_) => panic!("the request finished while its hook was waiting"),
        }
        let alive = || {
            std::fs::read_to_string(format!("/proc/{hook}/stat")).is_ok_and(|stat| {
                let state = stat.rsplit_once(") ").map(|(_, rest)| rest.as_bytes()[0]);
                !matches!(state, Some(b'Z' | b'X'))
            })
        };
        for _ in 0..40 {
            if !alive() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the hook outlived the request that started it");
    }

    /// A push whose handler is dropped runs on in its own task, holding no
    /// connection, and [`GitFace::stop`] waits for it rather than returning
    /// while its receive-pack runs. The push is then recorded.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_waits_for_a_push_whose_handler_was_dropped() {
        let f = fixture();
        let started = f._dir.path().join("started");
        let release = f._dir.path().join("release");
        hold_in_hook(&f.bare, "post-receive", &started, &release);
        let (topic, body) = push_topic();
        let mut push = push(&f, body);
        // The first poll spawns the push's task.
        assert!(poll_once(push.as_mut()).is_pending());
        drop(push);
        hook_started(&started).await;

        let face = f.face.clone();
        let stopped = tokio::spawn(async move {
            face.stop(std::time::Duration::from_secs(30)).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!stopped.is_finished(), "stop returned with a push running");

        std::fs::write(&release, "").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), stopped)
            .await
            .expect("stop did not return once the push finished")
            .unwrap();
        let log = f.face.world.log_after(0, 100).unwrap();
        assert!(
            log.iter().any(|e| matches!(&e.body,
                orrery_world::EventBody::RefCreated(c)
                    if c.subject.name == "refs/heads/topic" && c.object.sha == topic)),
            "{log:?}"
        );
    }
}
