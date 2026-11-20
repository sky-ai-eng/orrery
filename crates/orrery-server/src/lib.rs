//! The HTTP host. Mounts the faces (git smart-HTTP at the root, the GitHub
//! REST face under `/api/v3` — the GHES mount shape — and GitHub's OAuth web
//! flow under `/login/oauth`), the `/_orrery/` control plane, face auth, and the
//! NOT-MOCKED fallback. Under github.com's layout, a layer in front of them
//! serves each request by the host it names. The faces and the control plane are separate routers so
//! a world can serve the control plane on a listener that no public
//! request reaches.
//!
//! Route classes and their failure behavior (spec/README.md rule 1):
//! - matched face route, missing data → vendor-faithful 404
//! - unmatched face path, or a face path called with a method it doesn't
//!   serve → loud 501 NOT-MOCKED envelope naming the path
//! - unmatched control path → 404, and a control path called with a method it
//!   doesn't take → 405, both in the control plane's error shape
//!
//! The first two must never blur: a plausible 404 for unimplemented surface
//! is how consumers silently break. The control plane is Orrery's own API, so
//! it has no vendor surface to leave unmocked.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::uri::PathAndQuery;
use axum::http::{HeaderMap, Method, Response, StatusCode, Uri};
use axum::middleware::Next;
use axum::routing::any;
use axum::{Router, middleware};
use base64::Engine as _;
use orrery_face_git::GitFace;
use orrery_face_github::GithubFace;
pub use orrery_face_github::layout::Layout;
use orrery_face_github::layout::{GRAPHQL_PATH, Landing, REST_PATH, under};
use orrery_world::World;

mod alerts;
mod apps;
mod control;
mod list;
mod log_stream;
mod sender;
mod snapshots;
pub mod tls;

/// Where the control plane is mounted, on its own listener or the faces'.
const CONTROL_PREFIX: &str = "/_orrery";

/// The full SHA of the commit this binary was built from, or `unknown` when
/// the build named none and was not run in a git checkout. See `build.rs`.
pub const BUILD_COMMIT: &str = env!("ORRERY_BUILD_COMMIT");

#[derive(Clone)]
pub struct AppState {
    pub world: Arc<World>,
    /// The git face's handle, here for its world write gate: snapshot
    /// create/restore hold it exclusively against in-flight git traffic.
    pub git: Arc<GitFace>,
    /// `<instance>/snapshots` — a sibling of the world root.
    pub snapshots_dir: PathBuf,
    /// The GitHub GraphQL executor, built once per instance.
    pub schema: orrery_face_github::graphql::GithubSchema,
    /// See [`Options::anonymous_git_reads`].
    pub anonymous_git_reads: bool,
    /// See [`Options::config_sha256`].
    pub config_sha256: Option<String>,
    /// What the log's streams wait on.
    log_changes: Arc<tokio::sync::watch::Sender<log_stream::LogChanges>>,
    /// True once [`Stop::stop`] has been called.
    stopping: Arc<tokio::sync::watch::Sender<bool>>,
}

pub use orrery_face_github::oauth::SignIn;

/// How a world process exposes its faces. The default is local `orrery serve`.
#[derive(Clone, Debug)]
pub struct Options {
    /// Let fetch-side git traffic through without a token, as GitHub does for
    /// public repos. A world whose faces are on the public internet turns this
    /// off, since its host and repository names can be guessed.
    pub anonymous_git_reads: bool,
    /// How the pages that stand in for GitHub's sign a person in: the
    /// authorize page, and the pages an App is registered and installed on.
    /// By login alone, whoever can reach a page can act as any user, and be
    /// issued a token for them. A world whose faces are on the public
    /// internet asks for a token too.
    pub page_sign_in: SignIn,
    /// The SHA-256 of the config file this world was started from, as 64
    /// lowercase hex characters, reported by `GET /_orrery/healthz`. A
    /// supervisor that rewrites a world's config compares it with the digest
    /// of the file it holds to learn whether the running process has read
    /// that file. `None` for a world started from no file.
    pub config_sha256: Option<String>,
    /// What sends the world's webhook deliveries, or `None` for a world that
    /// makes its deliveries and sends none of them, as one whose log is
    /// being replayed does: each attempt's result is in the log, and the
    /// replay writes it back.
    pub outbound: Option<orrery_outbound::Client>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            anonymous_git_reads: true,
            page_sign_in: SignIn::Login,
            config_sha256: None,
            outbound: None,
        }
    }
}

/// One world's faces and control plane, built over shared state (the git
/// face's write gate in particular) and mounted on one listener or two.
pub struct Routers {
    faces: Router,
    control: Router,
    layout: Layout,
    stop: Stop,
}

impl Routers {
    /// What stops the server these routers make up. Taken before
    /// [`Routers::combined`] or [`Routers::split`] consumes them.
    pub fn stop(&self) -> Stop {
        self.stop.clone()
    }

    /// The faces alone, for a listener beside the one [`Routers::combined`]
    /// or [`Routers::split`] serves them on, such as a TLS listener or the
    /// HTTPS proxy ([`tls::TlsListener`]). Taken before either consumes them.
    pub fn faces(&self) -> Router {
        by_host(self.faces.clone().fallback(not_mocked), self.layout.clone())
    }

    /// Both on one listener, the control plane at `/_orrery`.
    pub fn combined(self) -> Router {
        by_host(
            self.control.merge(self.faces).fallback(not_mocked),
            self.layout,
        )
    }

    /// `(faces, control)` for two listeners. The faces router has no
    /// `/_orrery`, so a control path sent to it is unknown surface like any
    /// other. The control router keeps the `/_orrery` prefix, so a forwarder
    /// passes paths through unchanged.
    pub fn split(self) -> (Router, Router) {
        (
            by_host(self.faces.fallback(not_mocked), self.layout),
            self.control.fallback(not_mocked),
        )
    }
}

/// Stops one world's server: its listeners, its log streams, and its git
/// processes.
#[derive(Clone)]
pub struct Stop {
    stopping: Arc<tokio::sync::watch::Sender<bool>>,
    git: Arc<GitFace>,
}

impl Stop {
    /// Resolves once [`Stop::stop`] has been called. Passed to [`run`], it
    /// stops the listener accepting connections then.
    pub fn requested(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut stopping = self.stopping.subscribe();
        async move {
            let _ = stopping.wait_for(|stopping| *stopping).await;
        }
    }

    /// Stop the world's server: [`Stop::requested`] resolves, every log
    /// stream ends, and the git face stops ([`GitFace::stop`]), which kills
    /// git still running after `grace`. Returns once no git request is
    /// running, and none can start again. Requests the listeners are still
    /// answering are the caller's to wait for, by awaiting [`run`].
    pub async fn stop(&self, grace: std::time::Duration) {
        self.stopping.send_replace(true);
        self.git.stop(grace).await;
    }
}

/// Build the routers for one world instance.
pub fn routers(
    world: Arc<World>,
    layout: Layout,
    snapshots_dir: PathBuf,
    options: Options,
) -> Routers {
    let git = GitFace::new(world.clone());
    let github = GithubFace {
        world: world.clone(),
        git: git.clone(),
        layout: layout.clone(),
    };
    // The GitHub face makes the deliveries of every write this world takes
    // from here on, and of its newest write, if a process stopped between
    // that write and them.
    world.set_notifier(orrery_face_github::webhooks::notifier(&github));
    if let Err(e) = world.finish_last_write() {
        tracing::error!(error = %e, "making the newest write's deliveries failed");
    }
    let stopping = Arc::new(tokio::sync::watch::Sender::new(false));
    if let Some(client) = options.outbound {
        sender::start(world.clone(), client, stopping.subscribe());
    }
    let stop = Stop {
        stopping: stopping.clone(),
        git: git.clone(),
    };
    let state = AppState {
        world: world.clone(),
        git: git.clone(),
        snapshots_dir,
        schema: orrery_face_github::graphql::schema(github.clone()),
        anonymous_git_reads: options.anonymous_git_reads,
        config_sha256: options.config_sha256,
        log_changes: log_stream::follow(&world),
        stopping,
    };

    // GraphQL mounts at the GHES address, a sibling of the REST root, never
    // nested under it. Consumers derive it exactly there.
    let graphql = Router::new()
        .route(GRAPHQL_PATH, axum::routing::post(graphql_handler))
        .with_state(state.clone());

    // The OAuth web flow lives on the web host and authenticates with client
    // credentials or a signed-in actor, never an API token.
    let oauth = orrery_face_github::oauth::web_router(github.clone(), options.page_sign_in)
        .method_not_allowed_fallback(not_mocked);

    // The pages a person registers and installs a GitHub App on are on the
    // web host too, and the call that trades a manifest's code carries no
    // credential but the code.
    let app_flows = orrery_face_github::app_flows::web_router(github.clone(), options.page_sign_in)
        .method_not_allowed_fallback(not_mocked);

    // A GitHub App's own routes authenticate with a JWT the App signs, and
    // the client-credentials check with a client id and secret, never an
    // API token.
    let apps = orrery_face_github::apps::app_router(github.clone());

    // The method fallback is set before the auth layer so that it is behind
    // face auth, as the unmatched-path fallback under `/api/v3` is.
    let faces = Router::new()
        .merge(orrery_face_git::router(git))
        .nest(
            REST_PATH,
            orrery_face_github::router(github).fallback(not_mocked),
        )
        .merge(graphql)
        .method_not_allowed_fallback(not_mocked)
        .layer(middleware::from_fn_with_state(state.clone(), face_auth));

    Routers {
        faces: Router::new()
            .merge(oauth)
            .merge(app_flows)
            .merge(apps)
            .merge(faces),
        // Every path under the prefix is the control plane's. One that no
        // control route names answers 404 in the control plane's error shape,
        // and on a listener the faces share, a face route whose first segment
        // is a parameter (git's `/{owner}/{repo}/info/refs`) can't take it.
        control: Router::new()
            .nest(CONTROL_PREFIX, control::router(state))
            .route(CONTROL_PREFIX, any(control::no_such_route))
            .route(&format!("{CONTROL_PREFIX}/"), any(control::no_such_route))
            .route(
                &format!("{CONTROL_PREFIX}/{{*rest}}"),
                any(control::no_such_route),
            ),
        layout,
        stop,
    }
}

/// Under github.com's layout, `app` behind a layer that serves each request
/// by the host it names (spec/grammar.md, "Where the face is"). It runs
/// before `app` routes, so it can move an `api.github.com` request to the
/// path that serves it; the request's original URI, which every not-mocked
/// answer names, is recorded before it does.
fn by_host(app: Router, layout: Layout) -> Router {
    if !layout.is_github_com() {
        return app;
    }
    Router::new()
        .fallback_service(app)
        .layer(middleware::from_fn_with_state(Arc::new(layout), land))
}

async fn land(State(layout): State<Arc<Layout>>, mut req: Request, next: Next) -> Response<Body> {
    let host = request_host(&req);
    let landing = {
        let path_and_query = req.uri().path_and_query().map_or("/", PathAndQuery::as_str);
        layout.landing(host.as_deref(), path_and_query)
    };
    match landing {
        Landing::Own => next.run(req).await,
        // The control plane is the world's own host's, never github.com's.
        Landing::Web if under(req.uri().path(), CONTROL_PREFIX) => not_served(&req, host),
        Landing::Web => next.run(req).await,
        Landing::Api(served) => {
            let mut parts = req.uri().clone().into_parts();
            match served.parse::<PathAndQuery>() {
                Ok(served) => parts.path_and_query = Some(served),
                Err(_) => return not_served(&req, host),
            }
            match Uri::from_parts(parts) {
                Ok(uri) => *req.uri_mut() = uri,
                Err(_) => return not_served(&req, host),
            }
            next.run(req).await
        }
        Landing::NotServed => not_served(&req, host),
    }
}

fn not_served(req: &Request, host: Option<String>) -> Response<Body> {
    orrery_face_github::not_mocked_on(
        req.method().as_str(),
        host.as_deref().unwrap_or(""),
        req.uri().path(),
    )
}

/// The name of the host a request names, in lower case, without a port, the
/// trailing dot of a fully qualified name, or any user information: from its
/// URI's authority when it carries one, as an HTTP/2 request and an
/// absolute-form request do, and from its `Host` header otherwise.
fn request_host(req: &Request) -> Option<String> {
    let authority = match req.uri().authority() {
        Some(authority) => authority.clone(),
        None => req
            .headers()
            .get(axum::http::header::HOST)?
            .to_str()
            .ok()?
            .parse::<axum::http::uri::Authority>()
            .ok()?,
    };
    let host = authority.host();
    Some(host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase())
}

/// Serve until `stop` resolves ([`Stop::requested`]), then stop accepting
/// connections, and return once the ones being served have closed, each after
/// the request it is answering. The caller owns binding — a TCP port (tests
/// bind an ephemeral one and read it back before starting) or a Unix socket.
pub async fn run<L>(
    listener: L,
    app: Router,
    stop: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: std::fmt::Debug,
{
    axum::serve(listener, app)
        .with_graceful_shutdown(stop)
        .await
}

/// Reads and writes alike hold the world write gate for read (the git face's
/// discipline): resolution reads the bare repos, so a snapshot copy must wait
/// for an in-flight query and block new ones for its duration.
async fn graphql_handler(
    State(state): State<AppState>,
    axum::extract::Extension(caller): axum::extract::Extension<orrery_world::Caller>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let gate = Arc::new(state.git.gate().read_owned().await);
    orrery_face_github::graphql::handle(&state.schema, gate, caller, &body).await
}

/// Face auth: resolve the caller's token to its actor and the OAuth app it
/// was issued to, and attach both to the request: the actor alone, which
/// reads take, and the [`orrery_world::Caller`], which writes record. Accepted spellings mirror what real clients send (spec/grammar.md):
/// `Basic` with the token as the password (git's `x-access-token:<tok>`
/// convention — username ignored), `Bearer <tok>`, and `token <tok>`.
///
/// Git *read* traffic may pass anonymously — real GitHub serves public-repo
/// clones without credentials, and a consumer may clone a public repo with no
/// token on purpose. The request proceeds with no actor
/// attached; the git face itself refuses anonymous access to private repos.
/// Pushes and the REST face always require a token, and so do git reads when
/// [`Options::anonymous_git_reads`] is off.
async fn face_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response<Body> {
    let Some(token) = extract_token(req.headers()) else {
        if state.anonymous_git_reads && is_anonymous_git_read(&req) {
            return next.run(req).await;
        }
        return bad_credentials();
    };
    match state.world.caller_by_token(&token) {
        Ok(Some(caller)) => {
            // GitHub knows a suspended installation's token and says why it
            // refuses it, where an unknown token is bad credentials.
            if caller.installation.as_ref().is_some_and(|i| i.suspended) {
                // Git is told the token is no good, in git's words.
                if is_git(&req) {
                    return Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .header("www-authenticate", "Basic realm=\"orrery\"")
                        .header("content-type", "text/plain; charset=UTF-8")
                        .body(Body::from(
                            "Invalid username or token. Password authentication is not \
                             supported for Git operations.",
                        ))
                        .expect("static response parts are valid");
                }
                return json(
                    StatusCode::FORBIDDEN,
                    serde_json::json!({
                        "message": "This GitHub App installation is currently suspended.",
                    }),
                );
            }
            req.extensions_mut().insert(caller.actor.clone());
            req.extensions_mut().insert(caller);
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

/// Git smart-HTTP, either direction: a negotiation or one of the two
/// services.
fn is_git(req: &Request) -> bool {
    let path = req.uri().path();
    ["/info/refs", "/git-upload-pack", "/git-receive-pack"]
        .iter()
        .any(|end| path.ends_with(end))
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
/// so it can never be mistaken for "the thing doesn't exist". The path is the
/// one the request arrived with, never a nested router's remainder, so a
/// caller finds it in its own logs.
async fn not_mocked(method: Method, OriginalUri(uri): OriginalUri) -> Response<Body> {
    orrery_face_github::not_mocked(method.as_str(), uri.path())
}

pub(crate) fn json(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}
