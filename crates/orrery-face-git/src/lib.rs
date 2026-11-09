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
use orrery_world::{Actor, Repo, World};

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
    /// Serializes git-side writes per repo — receive-pack's before/after ref
    /// observation and the merge endpoint's check-then-move — so neither can
    /// interleave with a concurrent write to the same repo.
    repo_locks: std::sync::Mutex<BTreeMap<i64, Arc<tokio::sync::Mutex<()>>>>,
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
    /// has no log entry of its own: it follows from the pull's `pull.open` and
    /// from the `ref.update` that moved its head branch, so replaying those
    /// entries rebuilds it. The caller holds the repository's lock.
    pub fn pin_pull_ref(
        &self,
        repo: &Repo,
        number: i64,
        head_sha: &str,
    ) -> Result<(), GitFaceError> {
        let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
        store::update_ref(&bare, &store::pull_head_ref(number), head_sha)
    }

    /// GitHub's "synchronize": every open pull request whose head is `branch`
    /// takes `new_sha` as its head, and its `refs/pull/{n}/head` moves with
    /// it. A closed or merged pull request keeps both where they are.
    pub fn sync_pull_heads(
        &self,
        repo: &Repo,
        branch: &str,
        new_sha: &str,
    ) -> Result<(), GitFaceError> {
        for number in self.world.sync_pull_heads(repo.id, branch, new_sha)? {
            self.pin_pull_ref(repo, number, new_sha)?;
        }
        Ok(())
    }

    pub fn repo_lock(&self, repo_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.repo_locks.lock().expect("repo_locks mutex poisoned");
        locks.entry(repo_id).or_default().clone()
    }

    pub fn gate(&self) -> Arc<tokio::sync::RwLock<()>> {
        self.gate.clone()
    }

    /// Record a completed receive-pack: one `ref.update` log entry per changed
    /// ref (sorted, so replay order is stable), a `pushed_at` bump, and
    /// GitHub's first-push rule — the first branch pushed into an empty repo
    /// becomes the default branch.
    fn observe_push(
        &self,
        repo: &Repo,
        actor: &Actor,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Result<(), GitFaceError> {
        let mut changed = false;
        let names: std::collections::BTreeSet<&String> =
            before.keys().chain(after.keys()).collect();
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
            changed = true;
            self.world.log_action(
                actor.id,
                "ref.update",
                &serde_json::json!({
                    "repo": format!("{}/{}", repo.owner_login, repo.name),
                    "ref": name,
                    "old": old,
                    "new": new,
                }),
            )?;
            // A deleted branch leaves its pull requests pinned to the last
            // sha, as GitHub does.
            if let Some(branch) = name.strip_prefix("refs/heads/")
                && new != store::ZERO_OID
            {
                self.sync_pull_heads(repo, branch, new)?;
            }
        }
        if !changed {
            return Ok(());
        }
        self.world.touch_pushed(repo.id)?;

        let had_branches = before.keys().any(|r| r.starts_with("refs/heads/"));
        if !had_branches {
            let new_branches: Vec<&str> = after
                .keys()
                .filter_map(|r| r.strip_prefix("refs/heads/"))
                .collect();
            let adopted = ["main", "master"]
                .into_iter()
                .find(|b| new_branches.contains(b))
                .or(new_branches.first().copied());
            if let Some(branch) = adopted
                && branch != repo.default_branch
            {
                self.world.set_default_branch(repo.id, branch)?;
                let bare = store::bare_dir(&self.world.repos_dir(), &repo.owner_login, &repo.name);
                store::set_head_symref(&bare, branch)?;
            }
        }
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
    let lock = is_push.then(|| face.repo_lock(repo.id));
    let _guard = match &lock {
        Some(l) => Some(l.lock().await),
        None => None,
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
