//! The one outbound connection a world makes: a webhook delivery's POST to
//! the receiver's URL (spec/README.md rule 6, spec/usage.md "What a world
//! sends").
//!
//! A [`Client`] makes one attempt per call and reports what happened. It
//! never retries and never follows a redirect, because GitHub does neither
//! for a delivery, and it gives the whole attempt [`ATTEMPT_LIMIT`], the 10
//! seconds GitHub gives a receiver to answer. How the connection leaves the
//! machine is the [`Egress`]: straight to the receiver, or through an HTTP
//! `CONNECT` proxy, as a supervisor that gives a world no network of its own
//! provides.

use std::fmt;
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject as _;
use ureq::config::AutoHeaderValue;
use ureq::unversioned::transport::Connector as _;

pub use ureq::http;

mod transport;

use transport::{Deadline, EgressConnector, Lookup, TlsConnector};

/// How long an attempt may take, from when it starts to look up or connect
/// to when it has read the last byte of the answer it keeps.
pub const ATTEMPT_LIMIT: Duration = Duration::from_secs(10);

/// How much of a response body an attempt keeps. Nothing past it is read.
pub const BODY_LIMIT: usize = 64 * 1024;

/// The largest response head, its status line and headers, an attempt reads.
/// A longer one fails the attempt as [`Failure::Exchange`].
pub const HEAD_LIMIT: usize = 64 * 1024;

/// How an attempt's connection leaves the machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Egress {
    /// Look up the receiver's host name and connect to it.
    #[default]
    Direct,
    /// Connect to an HTTP `CONNECT` proxy and ask it for a tunnel to the
    /// receiver's host name and port. The name is the proxy's to resolve, so
    /// the proxy can refuse an address, and the engine looks up no name.
    Proxy(ProxyAddress),
}

/// Where an egress proxy listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyAddress {
    /// An IP address and port. A host name is refused, so that an engine
    /// with a proxy looks up no name at all.
    Tcp(SocketAddr),
    /// A Unix socket, by its absolute path.
    Unix(PathBuf),
}

/// `direct`, an IP address and port (`127.0.0.1:3128`), or `unix:` and an
/// absolute path (`unix:/run/egress.sock`).
impl FromStr for Egress {
    type Err = EgressError;

    fn from_str(raw: &str) -> Result<Egress, EgressError> {
        if raw == "direct" {
            return Ok(Egress::Direct);
        }
        if let Some(path) = raw.strip_prefix("unix:") {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(EgressError::RelativeSocket(raw.to_string()));
            }
            return Ok(Egress::Proxy(ProxyAddress::Unix(path)));
        }
        raw.parse()
            .map(|addr| Egress::Proxy(ProxyAddress::Tcp(addr)))
            .map_err(|_| EgressError::Unrecognized(raw.to_string()))
    }
}

impl fmt::Display for Egress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Egress::Direct => f.write_str("direct"),
            Egress::Proxy(address) => address.fmt(f),
        }
    }
}

impl fmt::Display for ProxyAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProxyAddress::Tcp(addr) => addr.fmt(f),
            ProxyAddress::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

/// An egress setting that names no egress.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("{0:?}: a proxy's socket path must be absolute, as in unix:/run/egress.sock")]
    RelativeSocket(String),
    #[error(
        "{0:?}: expected \"direct\", or a CONNECT proxy's address as an IP address and port (127.0.0.1:3128) or unix:/absolute/path"
    )]
    Unrecognized(String),
}

/// A file of extra certificate authorities that can't be trusted as one.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("reading certificates from {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: rustls_pki_types::pem::Error,
    },
    #[error("{} holds no PEM certificate", path.display())]
    Empty { path: PathBuf },
    #[error("certificate {index} in {} can't be trusted as an authority: {source}", path.display())]
    Unusable {
        path: PathBuf,
        /// Its place among the file's certificates, from 1.
        index: usize,
        source: rustls::Error,
    },
}

/// What an attempt sends: a POST of `body` to `url`, with `headers`. It also
/// carries `Host` and `Content-Length`, from the URL and the body, and no
/// other header the caller did not give.
#[derive(Clone, Debug)]
pub struct Request {
    pub url: String,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

/// What one attempt came to, and how long it took.
#[derive(Debug)]
pub struct Attempt {
    pub elapsed: Duration,
    pub outcome: Result<Response, Failure>,
}

/// The receiver's answer, whatever its status: a redirect is answered here,
/// never followed.
#[derive(Debug)]
pub struct Response {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    /// The body's first [`BODY_LIMIT`] bytes, as the receiver sent them.
    pub body: Vec<u8>,
    /// Whether the body went on past [`BODY_LIMIT`].
    pub body_truncated: bool,
}

/// Why an attempt got no answer. Each says what it is about in its own
/// words; the type says which step failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// The URL is not one an attempt can POST to.
    #[error("not a URL a delivery can be sent to: {0}")]
    Url(String),
    /// No connection was made to the receiver, or to the egress proxy: it
    /// refused, it was unreachable, or the receiver's name did not resolve.
    #[error("no connection: {0}")]
    Connect(String),
    /// The egress proxy opened no tunnel to the receiver: it refused the
    /// address, could not reach it, or did not answer as a `CONNECT` proxy.
    #[error("no tunnel through the egress proxy: {0}")]
    Proxy(String),
    /// The TLS handshake with the receiver failed, as it does for a
    /// certificate no trusted authority issued.
    #[error("TLS handshake failed: {0}")]
    Tls(String),
    /// The attempt had not read its answer when [`ATTEMPT_LIMIT`] ran out.
    #[error("no complete answer within {} seconds", ATTEMPT_LIMIT.as_secs())]
    Timeout,
    /// The connection failed after it was made, or the answer was not HTTP.
    #[error("the exchange failed: {0}")]
    Exchange(String),
}

/// Makes attempts. Cloning one is cheap, and every clone sends the same way.
#[derive(Clone, Debug)]
pub struct Client {
    egress: Egress,
    tls: Arc<rustls::ClientConfig>,
}

impl Client {
    /// A client that sends through `egress` and trusts the public root
    /// certificates built into the binary, and, when `extra_ca_certs` names a
    /// PEM file, every certificate in it as an authority too.
    pub fn new(egress: Egress, extra_ca_certs: Option<&Path>) -> Result<Client, TrustError> {
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        if let Some(path) = extra_ca_certs {
            let read = |source| TrustError::Read {
                path: path.to_path_buf(),
                source,
            };
            let mut added = 0;
            for cert in CertificateDer::pem_file_iter(path).map_err(read)? {
                added += 1;
                roots
                    .add(cert.map_err(read)?)
                    .map_err(|source| TrustError::Unusable {
                        path: path.to_path_buf(),
                        index: added,
                        source,
                    })?;
            }
            if added == 0 {
                return Err(TrustError::Empty {
                    path: path.to_path_buf(),
                });
            }
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("ring offers cipher suites for every default protocol version")
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Client {
            egress,
            tls: Arc::new(tls),
        })
    }

    pub fn egress(&self) -> &Egress {
        &self.egress
    }

    /// One attempt to POST `request`. Its I/O blocks, for up to
    /// [`ATTEMPT_LIMIT`], so it runs on the blocking pool and never holds one
    /// of the runtime's workers.
    pub async fn post(&self, request: Request) -> Attempt {
        let client = self.clone();
        tokio::task::spawn_blocking(move || client.attempt(request))
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }

    fn attempt(&self, request: Request) -> Attempt {
        let deadline = Deadline::start();
        let outcome = self.exchange(request, deadline);
        Attempt {
            elapsed: deadline.elapsed(),
            outcome,
        }
    }

    fn exchange(&self, request: Request, deadline: Deadline) -> Result<Response, Failure> {
        let uri = target(&request.url)?;
        // An agent per attempt, so no connection outlives the attempt that
        // made it and the connectors bound their waits by its deadline.
        let config = ureq::Agent::config_builder()
            // The egress is the client's alone: proxy variables in the
            // process's environment must not reroute a world's deliveries.
            .proxy(None)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(ATTEMPT_LIMIT))
            .max_response_header_size(HEAD_LIMIT)
            .max_idle_connections(0)
            .max_idle_connections_per_host(0)
            .user_agent(AutoHeaderValue::None)
            .accept(AutoHeaderValue::None)
            .accept_encoding(AutoHeaderValue::None)
            .build();
        let connector = EgressConnector {
            egress: self.egress.clone(),
            deadline,
        }
        .chain(TlsConnector {
            config: self.tls.clone(),
        });
        let lookup = match self.egress {
            Egress::Direct => Lookup::System(Default::default()),
            Egress::Proxy(_) => Lookup::Proxy,
        };
        let agent = ureq::Agent::with_parts(config, connector, lookup);

        let mut post = http::Request::new(request.body);
        *post.method_mut() = http::Method::POST;
        *post.uri_mut() = uri;
        *post.headers_mut() = request.headers;
        let (head, body) = agent.run(post).map_err(failure)?.into_parts();

        let mut kept = Vec::new();
        body.into_reader()
            .take(BODY_LIMIT as u64 + 1)
            .read_to_end(&mut kept)
            .map_err(|e| failure(e.into()))?;
        let body_truncated = kept.len() > BODY_LIMIT;
        kept.truncate(BODY_LIMIT);
        Ok(Response {
            status: head.status,
            headers: head.headers,
            body: kept,
            body_truncated,
        })
    }
}

/// `url`, when it is an http or https URL with a host.
fn target(url: &str) -> Result<http::Uri, Failure> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| Failure::Url(format!("{url:?}: {e}")))?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(Failure::Url(format!("{url:?} is not an http or https URL")));
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err(Failure::Url(format!("{url:?} names no host")));
    }
    Ok(uri)
}

/// Which step `e` failed at. The lookup and the connectors report their own
/// step as a [`Failure`]; what fails after them is the exchange.
fn failure(e: ureq::Error) -> Failure {
    match e {
        ureq::Error::Timeout(_) => Failure::Timeout,
        ureq::Error::Other(other) => match other.downcast::<Failure>() {
            Ok(failure) => *failure,
            Err(other) => Failure::Exchange(other.to_string()),
        },
        e => Failure::Exchange(transport::describe(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_egress_is_direct_an_address_or_a_socket_path() {
        assert_eq!("direct".parse::<Egress>().unwrap(), Egress::Direct);
        assert_eq!(
            "127.0.0.1:3128".parse::<Egress>().unwrap(),
            Egress::Proxy(ProxyAddress::Tcp("127.0.0.1:3128".parse().unwrap()))
        );
        assert_eq!(
            "[::1]:3128".parse::<Egress>().unwrap(),
            Egress::Proxy(ProxyAddress::Tcp("[::1]:3128".parse().unwrap()))
        );
        assert_eq!(
            "unix:/run/egress.sock".parse::<Egress>().unwrap(),
            Egress::Proxy(ProxyAddress::Unix("/run/egress.sock".into()))
        );
        for raw in ["direct", "127.0.0.1:3128", "unix:/run/egress.sock"] {
            assert_eq!(raw.parse::<Egress>().unwrap().to_string(), raw);
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for raw in [
            "",
            "Direct",
            "proxy",
            "localhost:3128",
            "127.0.0.1",
            "http://127.0.0.1:3128",
            "unix:run/egress.sock",
            "unix:",
        ] {
            assert!(raw.parse::<Egress>().is_err(), "{raw:?} was accepted");
        }
    }

    #[test]
    fn only_an_http_url_with_a_host_is_a_target() {
        for url in [
            "https://receiver.test/hook",
            "http://127.0.0.1:8080",
            "https://[::1]:8443/x?y=1",
        ] {
            assert!(target(url).is_ok(), "{url:?} was refused");
        }
        for url in [
            "ftp://receiver.test/",
            "/hook",
            "receiver.test",
            "",
            "https://",
        ] {
            assert!(
                matches!(target(url), Err(Failure::Url(_))),
                "{url:?} was accepted"
            );
        }
    }
}
