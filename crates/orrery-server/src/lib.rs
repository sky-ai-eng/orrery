//! The HTTP host. Mounts the faces (git smart-HTTP at the root, the GitHub
//! REST face under `/api/v3` — the GHES mount shape), the `/_orrery/` control
//! plane, face auth, and the NOT-MOCKED fallback.
//!
//! Route classes and their failure behavior (spec/README.md rule 1):
//! - matched face route, missing data → vendor-faithful 404
//! - unmatched path anywhere → loud 501 NOT-MOCKED envelope naming the path
//!
//! Those two must never blur: a plausible 404 for unimplemented surface is
//! how consumers silently break.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::middleware::Next;
use axum::{Router, middleware};
use base64::Engine as _;
use orrery_face_git::GitFace;
use orrery_face_github::GithubFace;
use orrery_world::World;

mod control;
mod snapshots;

#[derive(Clone)]
pub struct AppState {
    pub world: Arc<World>,
    pub base_url: String,
    /// The git face's handle, here for its world write gate: snapshot
    /// create/restore hold it exclusively against in-flight git traffic.
    pub git: Arc<GitFace>,
    /// `<instance>/snapshots` — a sibling of the world root.
    pub snapshots_dir: PathBuf,
    /// The GitHub GraphQL executor, built once per instance.
    pub schema: orrery_face_github::graphql::GithubSchema,
}

/// Build the full application router for one world instance.
pub fn app(world: Arc<World>, base_url: String, snapshots_dir: PathBuf) -> Router {
    let git = GitFace::new(world.clone());
    let github = GithubFace {
        world: world.clone(),
        git: git.clone(),
        base_url: base_url.clone(),
    };
    let state = AppState {
        world: world.clone(),
        base_url,
        git: git.clone(),
        snapshots_dir,
        schema: orrery_face_github::graphql::schema(github.clone()),
    };

    // GraphQL mounts at the GHES address — `/api/graphql`, a sibling of
    // `/api/v3`, never nested under it. Consumers derive it exactly there.
    let graphql = Router::new()
        .route("/api/graphql", axum::routing::post(graphql_handler))
        .with_state(state.clone());

    let faces = Router::new()
        .merge(orrery_face_git::router(git))
        .nest(
            "/api/v3",
            orrery_face_github::router(github).fallback(not_mocked),
        )
        .merge(graphql)
        .layer(middleware::from_fn_with_state(state.clone(), face_auth));

    Router::new()
        .nest("/_orrery", control::router(state))
        .merge(faces)
        .fallback(not_mocked)
}

/// Serve until the listener is closed. The caller owns binding, so tests can
/// use an ephemeral port and read it back before starting.
pub async fn run(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
    axum::serve(listener, app).await
}

/// Reads and writes alike hold the world write gate for read (the git face's
/// discipline): resolution reads the bare repos, so a snapshot copy must wait
/// for an in-flight query and block new ones for its duration.
async fn graphql_handler(
    State(state): State<AppState>,
    axum::extract::Extension(actor): axum::extract::Extension<orrery_world::Actor>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let _gate = state.git.gate().read_owned().await;
    orrery_face_github::graphql::handle(&state.schema, actor, &body).await
}

/// Face auth: resolve the caller's token to an actor and attach it to the
/// request. Accepted spellings mirror what real clients send (spec/grammar.md):
/// `Basic` with the token as the password (git's `x-access-token:<tok>`
/// convention — username ignored), `Bearer <tok>`, and `token <tok>`.
///
/// Git *read* traffic may pass anonymously — real GitHub serves public-repo
/// clones without credentials, and consumers rely on it (TF deliberately
/// clones public repos with no token). The request proceeds with no actor
/// attached; the git face itself refuses anonymous access to private repos.
/// Pushes and the REST face always require a token.
async fn face_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response<Body> {
    let Some(token) = extract_token(req.headers()) else {
        if is_anonymous_git_read(&req) {
            return next.run(req).await;
        }
        return bad_credentials();
    };
    match state.world.actor_by_token(&token) {
        Ok(Some(actor)) => {
            req.extensions_mut().insert(actor);
            next.run(req).await
        }
        Ok(None) => bad_credentials(),
        Err(e) => {
            tracing::error!(error = %e, "token lookup failed");
            json(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "message": "Internal Server Error" }),
            )
        }
    }
}

/// Fetch-side git smart-HTTP only: the upload-pack POST, or an info/refs
/// negotiation *for* upload-pack. Receive-pack traffic (either leg) never
/// qualifies.
fn is_anonymous_git_read(req: &Request) -> bool {
    let path = req.uri().path();
    if path.ends_with("/git-upload-pack") {
        return true;
    }
    path.ends_with("/info/refs")
        && req
            .uri()
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "service=git-upload-pack"))
}

fn extract_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get("authorization")?.to_str().ok()?;
    let (scheme, value) = auth.split_once(' ')?;
    let value = value.trim();
    if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token") {
        return Some(value.to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value)
            .ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (_user, pass) = decoded.split_once(':')?;
        return Some(pass.to_string());
    }
    None
}

fn bad_credentials() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("www-authenticate", "Basic realm=\"orrery\"")
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(
            serde_json::json!({ "message": "Bad credentials" }).to_string(),
        ))
        .expect("static response parts are valid")
}

/// The loud refusal for surface Orrery does not implement. 501 with a
/// self-describing envelope — deliberately unlike any real GitHub response,
/// so it can never be mistaken for "the thing doesn't exist".
async fn not_mocked(req: Request) -> Response<Body> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    tracing::warn!(%method, %path, "NOT MOCKED: unimplemented surface was called");
    json(
        StatusCode::NOT_IMPLEMENTED,
        serde_json::json!({
            "message": format!("orrery: {method} {path} is not mocked"),
            "orrery": {
                "not_mocked": true,
                "method": method,
                "path": path,
                "hint": "If a consumer needs this endpoint, add it to the surface inventory (spec/surface-github.md) and implement it.",
            },
        }),
    )
}

pub(crate) fn json(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}
