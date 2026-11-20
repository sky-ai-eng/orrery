//! The relay: an HTTPS proxy for GitHub's hosts that runs beside a client and
//! forwards each request to a world another process serves under github.com's
//! layout (spec/usage.md, "Reaching a world served elsewhere").
//!
//! It is a client of the world, not a world. It holds no state, and the one
//! connection it opens is to the world it was given. A request for
//! `api.github.com` goes to the world's REST or GraphQL path, and one for
//! `github.com` to the same path on the world, each with the world's own host
//! in `Host`, so a world behind a router that finds it by that name is reached
//! too. The world names GitHub's hosts in every URL it returns, so the relay
//! changes no body, and streams each one through as it arrives.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::uri::Scheme;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, Uri, Version, header};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use orrery_face_github::layout::{Landing, Layout, RootUrlError, root_url, under};
use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// How long a connection to the world has to open, its TLS handshake
/// included. Nothing after that is timed: a push or a fetch takes as long as
/// it takes.
const CONNECT_LIMIT: Duration = Duration::from_secs(10);

/// The world a relay forwards to: the root of its own host, as the world's
/// `server.base_url` names it.
#[derive(Clone, Debug)]
pub struct WorldUrl {
    /// The scheme, the authority, and the path, with no trailing slash.
    root: String,
    /// The authority, which every forwarded request names as its `Host`.
    host: HeaderValue,
}

impl WorldUrl {
    /// `url`, which must be an `http` or `https` URL with a host and no user
    /// information, query, or fragment. A path is kept, for a world served
    /// below one.
    pub fn parse(url: &str) -> Result<WorldUrl, RootUrlError> {
        let uri = root_url(url)?;
        let authority = uri.authority().ok_or(RootUrlError::NoHost)?;
        let host = HeaderValue::from_str(authority.as_str()).map_err(|_| RootUrlError::NotAUrl)?;
        let scheme = uri.scheme_str().unwrap_or_default();
        let root = format!("{scheme}://{authority}{}", uri.path().trim_end_matches('/'));
        Ok(WorldUrl { root, host })
    }

    pub fn as_str(&self) -> &str {
        &self.root
    }
}

impl std::fmt::Display for WorldUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.root)
    }
}

/// Forwards each request for GitHub's hosts to the world.
pub struct Relay {
    world: WorldUrl,
    layout: Layout,
    client: Client<Connector, Body>,
}

impl Relay {
    /// A relay to `world`, whose certificate, when it is served over TLS, is
    /// checked against the public roots the binary ships and, when
    /// `extra_ca_certs` names a PEM file, the authorities in it too.
    pub fn new(
        world: WorldUrl,
        extra_ca_certs: Option<&Path>,
    ) -> Result<Relay, orrery_outbound::TrustError> {
        let mut tls = orrery_outbound::trust(extra_ca_certs)?;
        // The relay forwards HTTP/1.1, so it offers nothing else.
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_connect_timeout(Some(CONNECT_LIMIT));
        let connector = Connector {
            http,
            tls: TlsConnector::from(Arc::new(tls)),
        };
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .build(connector);
        Ok(Relay {
            world,
            layout: Layout::github_com(),
            client,
        })
    }

    /// The router a listener serves the relay with.
    pub fn router(self) -> axum::Router {
        axum::Router::new()
            .fallback(relay)
            .with_state(Arc::new(self))
    }

    /// Send `req` to the world at `at`, a path and query, and answer what
    /// the world answers.
    async fn forward(&self, req: Request, at: &str) -> Response<Body> {
        let method = req.method().clone();
        let Ok(uri) = format!("{}{at}", self.world.root).parse::<Uri>() else {
            return not_served(&req, orrery_server::request_host(&req));
        };
        let (mut parts, body) = req.into_parts();
        parts.uri = uri;
        parts.version = Version::HTTP_11;
        drop_hop_by_hop(&mut parts.headers);
        // The relay's own server answered a client's `Expect` already, and
        // the world reads the body as it arrives.
        parts.headers.remove(header::EXPECT);
        parts.headers.insert(header::HOST, self.world.host.clone());
        match self.client.request(Request::from_parts(parts, body)).await {
            Ok(answer) => {
                let (mut parts, body) = answer.into_parts();
                drop_hop_by_hop(&mut parts.headers);
                Response::from_parts(parts, Body::new(body))
            }
            Err(e) => {
                let why = causes(&e);
                tracing::warn!(world = %self.world, %method, path = at, error = %why, "the relay could not reach the world");
                orrery_error(
                    StatusCode::BAD_GATEWAY,
                    format!("orrery relay: {method} {}{at} failed: {why}", self.world),
                    serde_json::json!({ "relay_failed": true, "world": self.world.as_str() }),
                )
            }
        }
    }
}

/// Where a request is served: at the world, for one of the GitHub hosts the
/// world serves, and otherwise the answer the world would give, or a refusal
/// for a host that is not GitHub's.
async fn relay(State(relay): State<Arc<Relay>>, req: Request) -> Response<Body> {
    let host = orrery_server::request_host(&req);
    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or("/", |pq| pq.as_str())
        .to_string();
    let at = match relay.layout.landing(host.as_deref(), &path_and_query) {
        Landing::Api(served) => served,
        // The control plane is the world's own host's, never github.com's.
        Landing::Web if under(req.uri().path(), orrery_server::CONTROL_PREFIX) => {
            return not_served(&req, host);
        }
        Landing::Web => path_and_query,
        Landing::NotServed => return not_served(&req, host),
        Landing::Own => return misdirected(host.as_deref(), &relay.world),
    };
    relay.forward(req, &at).await
}

/// The `501` the world answers for a GitHub host or a path it does not
/// serve, answered here, so it reaches no world.
fn not_served(req: &Request, host: Option<String>) -> Response<Body> {
    orrery_face_github::not_mocked_on(
        req.method().as_str(),
        host.as_deref().unwrap_or(""),
        req.uri().path(),
    )
}

/// A request for a host that is not GitHub's, which the relay does not
/// forward: the world is reached at its own host directly.
fn misdirected(host: Option<&str>, world: &WorldUrl) -> Response<Body> {
    let named = host.map_or_else(
        || "a request that names no host".to_string(),
        str::to_string,
    );
    orrery_error(
        StatusCode::MISDIRECTED_REQUEST,
        format!(
            "orrery relay: the relay serves github.com and api.github.com, not {named}. The world's own host is reached at {world}, not through the relay."
        ),
        serde_json::json!({ "relay_refused": true, "host": host }),
    )
}

/// An answer of the relay's own, in the shape of the world's proxy refusals.
fn orrery_error(status: StatusCode, message: String, orrery: serde_json::Value) -> Response<Body> {
    let body = serde_json::json!({ "message": message, "orrery": orrery }).to_string();
    let mut answer = Response::new(Body::from(body));
    *answer.status_mut() = status;
    answer.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    answer
}

/// An error and each error it was caused by, as one line.
fn causes(e: &(dyn std::error::Error + 'static)) -> String {
    let mut line = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        source = cause.source();
    }
    line
}

/// The headers that describe one connection rather than the message (RFC
/// 9110 §7.6.1), which the relay's two connections each set for themselves.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn drop_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Opens the relay's connections to the world: TCP, and TLS over it for an
/// `https` world.
#[derive(Clone)]
struct Connector {
    http: HttpConnector,
    tls: TlsConnector,
}

impl tower_service::Service<Uri> for Connector {
    type Response = Stream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Stream, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let https = uri.scheme() == Some(&Scheme::HTTPS);
        let name = uri
            .host()
            .unwrap_or_default()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let tcp = self.http.call(uri);
        let tls = self.tls.clone();
        Box::pin(async move {
            let tcp = tcp.await?;
            if !https {
                return Ok(Stream::Plain(tcp));
            }
            let name = ServerName::try_from(name)?;
            let tls = tokio::time::timeout(CONNECT_LIMIT, tls.connect(name, tcp.into_inner()))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the TLS handshake with the world did not finish in time",
                    )
                })??;
            Ok(Stream::Tls(Box::new(TokioIo::new(tls))))
        })
    }
}

/// A connection to the world.
enum Stream {
    Plain(TokioIo<TcpStream>),
    Tls(Box<TokioIo<TlsStream<TcpStream>>>),
}

impl Connection for Stream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl hyper::rt::Read for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Stream::Tls(io) => Pin::new(io.as_mut()).poll_read(cx, buf),
        }
    }
}

impl hyper::rt::Write for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(io) => Pin::new(io).poll_write(cx, buf),
            Stream::Tls(io) => Pin::new(io.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(io) => Pin::new(io).poll_flush(cx),
            Stream::Tls(io) => Pin::new(io.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(io) => Pin::new(io).poll_shutdown(cx),
            Stream::Tls(io) => Pin::new(io.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_world_url_keeps_its_path_without_a_trailing_slash() {
        for (url, root, host) in [
            (
                "http://127.0.0.1:4180",
                "http://127.0.0.1:4180",
                "127.0.0.1:4180",
            ),
            (
                "https://world.example/",
                "https://world.example",
                "world.example",
            ),
            (
                "https://world.example/prefix/",
                "https://world.example/prefix",
                "world.example",
            ),
            ("http://[::1]:4180", "http://[::1]:4180", "[::1]:4180"),
        ] {
            let world = WorldUrl::parse(url).unwrap();
            assert_eq!(world.as_str(), root, "{url}");
            assert_eq!(world.host, host, "{url}");
        }
    }

    #[test]
    fn a_world_url_a_path_cannot_follow_is_refused() {
        for (url, why) in [
            ("world.example", RootUrlError::NotHttp),
            ("ftp://world.example", RootUrlError::NotHttp),
            ("https://user@world.example", RootUrlError::UserInfo),
            ("https://world.example/?x=1", RootUrlError::Query),
            ("https://world.example/#top", RootUrlError::Fragment),
            ("not a url", RootUrlError::NotAUrl),
        ] {
            assert_eq!(WorldUrl::parse(url).unwrap_err(), why, "{url}");
        }
    }

    #[test]
    fn hop_by_hop_headers_and_those_connection_names_are_dropped() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("connection", "keep-alive, x-hop"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("x-hop", "1"),
            ("content-length", "12"),
            ("authorization", "token t"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        drop_hop_by_hop(&mut headers);
        let mut left: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
        left.sort_unstable();
        assert_eq!(left, ["authorization", "content-length"]);
    }
}
