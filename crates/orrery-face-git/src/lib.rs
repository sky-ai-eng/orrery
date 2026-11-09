//! The git face: serves clone/fetch/push for the world's bare repos over git
//! smart-HTTP, and observes every push into the kernel's action log.
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
use orrery_world::{Actor, RefMove, Repo, World};

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
    /// never lands partway through a merge that read the pull's title.
    repo_locks: std::sync::Mutex<BTreeMap<i64, Arc<tokio::sync::RwLock<()>>>>,
    /// World-wide write gate: every git-side operation holds it for read;
    /// snapshot create/restore hold it for write, so a copy of the world
    /// directory never races an in-flight push or merge.
    gate: Arc<tokio::sync::RwLock<()>>,
}

impl GitFace {
    pub fn new(world: Arc<World>) -> Arc<GitFace> {
        Arc::new(GitFace {
            world,
            repo_locks: std::sync::Mutex::new(BTreeMap::new()),
            gate: Arc::new(tokio::sync::RwLock::new(())),
        })
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
    /// has no log entry of its own: it follows from the pull's `pull.open` or
    /// `pull.reopen` and from the `ref.update` that moved its head branch, so
    /// replaying those entries rebuilds it. The caller holds the repository's
    /// lock for write.
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

    /// Record a completed receive-pack: one `ref.update` log entry per changed
    /// ref (sorted, so replay order is stable), GitHub's "synchronize" for the
    /// pull requests on each pushed branch, a `pushed_at` bump, and GitHub's
    /// first-push rule — the first branch pushed into an empty repo becomes
    /// the default branch. The caller holds the repository's lock for write.
    fn observe_push(
        &self,
        repo: &Repo,
        actor: &Actor,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Result<(), GitFaceError> {
        let names: std::collections::BTreeSet<&String> =
            before.keys().chain(after.keys()).collect();
        let mut moves = Vec::new();
        for name in names {
            let old = before
                .get(name)
                .map(String::as_str)
                .unwrap_or(store::ZERO_OID);
            let new = after
                .get(name)
                .map(String::as_str)
                .unwrap_or(store::ZERO_OID);
            if old == new {
                continue;
            }
            // A deleted branch leaves its pull requests pinned to the last
            // sha, as GitHub does.
            let repins = match name.strip_prefix("refs/heads/") {
                Some(branch) if new != store::ZERO_OID => {
                    self.world.pulls_to_sync(repo.id, branch, new)?
                }
                _ => Vec::new(),
            };
            moves.push(RefMove {
                name,
                old,
                new,
                repins,
            });
        }
        if moves.is_empty() {
            return Ok(());
        }

        let had_branches = before.keys().any(|r| r.starts_with("refs/heads/"));
        let adopted = if had_branches {
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
        // Every ref the push moves is in place before the record commits, as
        // receive-pack moved the pushed branches before this ran, so a reader
        // that can see the push's entries also sees its refs.
        let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
        if let Some(branch) = adopted {
            store::set_head_symref(&bare, branch)?;
        }
        for m in &moves {
            for &number in &m.repins {
                self.pin_pull_ref(repo, number, m.new)?;
            }
        }
        self.world.record_push(repo.id, actor.id, &moves, adopted)?;
        Ok(())
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
    actor: Option<Extension<Actor>>,
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
        actor.as_deref(),
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
    actor: Option<Extension<Actor>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    serve(
        face,
        &owner,
        &repo_name,
        actor.as_deref(),
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
    actor: Option<Extension<Actor>>,
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
            actor.as_deref(),
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
    actor: Option<&Actor>,
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
    if actor.is_none() && (repo.private || is_push) {
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
    let before = if is_push {
        match store::list_refs(&bare) {
            Ok(refs) => Some(refs),
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
        remote_user: actor.map(|a| a.login.as_str()),
        body,
    };
    let cgi_resp = match cgi::run_http_backend(&face.world.repos_dir(), req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "git http-backend failed");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
    };

    if let Some(before) = before
        && cgi_resp.status == 200
    {
        let pusher = actor.expect("pushes are refused without an actor above");
        let observed = store::list_refs(&bare)
            .and_then(|after| face.observe_push(&repo, pusher, &before, &after));
        if let Err(e) = observed {
            // TODO(ORR-70): the refs receive-pack moved stay moved, ahead of the
            // log, and a retry of the push finds nothing to move; move them back,
            // as a merge whose record fails does.
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

    use orrery_world::{ActorKind, DEFAULT_T0_UNIX, WorldSecret};

    use super::*;

    type Request = Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

    /// A world with one public repository, `alice/webapp`, whose `main` holds
    /// one commit.
    struct Fixture {
        face: Arc<GitFace>,
        repo: Repo,
        alice: Actor,
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
            .create_repo(alice.id, "webapp", None, false, "main", alice.id)
            .unwrap();
        let face = GitFace::new(world);
        face.init_repo_dir(&repo).unwrap();
        let bare = store::bare_dir(&face.world.repos_dir(), "alice", "webapp");
        let main = commit(&bare, &[], "first");
        store::update_ref(&bare, "refs/heads/main", &main).unwrap();
        Fixture {
            face,
            repo,
            alice,
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
            log.iter().any(|e| e.action == "ref.update"
                && e.details["ref"] == "refs/heads/topic"
                && e.details["new"] == topic.as_str()),
            "{log:?}"
        );
    }
}
