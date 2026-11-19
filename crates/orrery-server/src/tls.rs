//! TLS for a world served under github.com's layout, which a client reaches
//! as `https://api.github.com` and `https://github.com` (spec/usage.md,
//! "Reaching github.com's hosts"). A [`TlsListener`] wraps a listener and
//! hands `axum::serve` the TLS streams it accepts, so their connections are
//! served, and stopped, as a plain listener's are. It speaks TLS from the
//! first byte, or after a `CONNECT`, as an HTTPS proxy for GitHub's hosts.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::serve::Listener;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// The certificate a world serves GitHub's hosts with, and its key. The
/// authority behind it is the operator's: nothing in a world holds it.
#[derive(Clone)]
pub struct Certificate {
    acceptor: TlsAcceptor,
}

/// A certificate and key that can't be served.
#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error("reading {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: rustls_pki_types::pem::Error,
    },
    #[error("{} holds no PEM certificate", path.display())]
    NoCertificate { path: PathBuf },
    #[error("the certificate in {} and the key in {} can't be served: {source}", cert.display(), key.display())]
    Unusable {
        cert: PathBuf,
        key: PathBuf,
        source: rustls::Error,
    },
}

impl Certificate {
    /// The certificate chain in `cert`, the served certificate first, and
    /// the private key in `key`, both PEM.
    pub fn from_pem_files(cert: &Path, key: &Path) -> Result<Certificate, CertificateError> {
        let read = |path: &Path| {
            let path = path.to_path_buf();
            move |source| CertificateError::Read { path, source }
        };
        let chain = CertificateDer::pem_file_iter(cert)
            .map_err(read(cert))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(read(cert))?;
        if chain.is_empty() {
            return Err(CertificateError::NoCertificate {
                path: cert.to_path_buf(),
            });
        }
        let private = PrivateKeyDer::from_pem_file(key).map_err(read(key))?;
        let unusable = |source| CertificateError::Unusable {
            cert: cert.to_path_buf(),
            key: key.to_path_buf(),
            source,
        };
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(unusable)?
        .with_no_client_auth()
        .with_single_cert(chain, private)
        .map_err(unusable)?;
        // The listeners serve HTTP/1.1, so they offer nothing else.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Certificate {
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }
}

/// How long a connection has to finish its TLS handshake, and its `CONNECT`
/// first when it comes through the proxy, before it is dropped. It bounds
/// what a connection that never speaks holds; it reaches no world.
const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);

/// The most a `CONNECT` request's head may hold.
const MAX_CONNECT_HEAD: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Direct,
    Proxy,
}

/// A listener whose connections speak TLS, and through the proxy, open with
/// a `CONNECT`. Each connection's handshake runs on a task of its own, so one
/// slow client holds back no other.
pub struct TlsListener<L: Listener> {
    accepted: mpsc::Receiver<(TlsStream<L::Io>, L::Addr)>,
    local_addr: Option<L::Addr>,
}

impl<L> TlsListener<L>
where
    L: Listener,
    L::Addr: Clone + Sync,
{
    /// Connections that speak TLS from their first byte, as a client that
    /// resolves GitHub's hosts to the world sends.
    pub fn direct(inner: L, certificate: &Certificate) -> TlsListener<L> {
        TlsListener::start(inner, certificate, Mode::Direct)
    }

    /// An HTTPS proxy for GitHub's hosts: a connection opens with `CONNECT`
    /// to one of them on port 443, then speaks TLS inside the tunnel. A
    /// `CONNECT` to any other host, or any other request, is refused, and
    /// Orrery opens no connection of its own.
    pub fn proxy(inner: L, certificate: &Certificate) -> TlsListener<L> {
        TlsListener::start(inner, certificate, Mode::Proxy)
    }

    fn start(mut inner: L, certificate: &Certificate, mode: Mode) -> TlsListener<L> {
        let local_addr = inner.local_addr().ok();
        let (tx, accepted) = mpsc::channel(64);
        let acceptor = certificate.acceptor.clone();
        // Ends when `axum::serve` drops this listener, as it does once it
        // stops accepting, and drops `inner` with it.
        tokio::spawn(async move {
            loop {
                let (io, addr) = tokio::select! {
                    () = tx.closed() => return,
                    accepted = inner.accept() => accepted,
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    let started = tokio::time::timeout(HANDSHAKE_LIMIT, async {
                        let mut io = io;
                        if mode == Mode::Proxy {
                            open_tunnel(&mut io).await?;
                        }
                        acceptor.accept(io).await
                    });
                    match started.await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, addr)).await;
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(error = %e, "a TLS connection ended before its first request")
                        }
                        Err(_) => tracing::debug!("a TLS connection did not start in time"),
                    }
                });
            }
        });
        TlsListener {
            accepted,
            local_addr,
        }
    }
}

impl<L> Listener for TlsListener<L>
where
    L: Listener,
    L::Addr: Clone + Sync,
{
    type Io = TlsStream<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.accepted.recv().await {
            Some(accepted) => accepted,
            // The accept task ends only once this listener is dropped.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.local_addr
            .clone()
            .ok_or_else(|| io::Error::other("the listener has no local address"))
    }
}

/// Read the request that opens a tunnel and answer it: `200` for a `CONNECT`
/// to one of GitHub's hosts on port 443, after which the client speaks TLS,
/// and a refusal for anything else, after which the connection closes.
async fn open_tunnel<Io>(io: &mut Io) -> io::Result<()>
where
    Io: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    let (len, method, target) = loop {
        let n = io.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&head) {
            Ok(httparse::Status::Complete(len)) => {
                let method = request.method.unwrap_or_default().to_string();
                let target = request.path.unwrap_or_default().to_string();
                break (len, method, target);
            }
            Ok(httparse::Status::Partial) if head.len() < MAX_CONNECT_HEAD => {}
            Ok(httparse::Status::Partial) => {
                return refuse(
                    io,
                    431,
                    "Request Header Fields Too Large",
                    "orrery: the proxy request's head is too large".to_string(),
                    None,
                )
                .await;
            }
            Err(e) => {
                return refuse(
                    io,
                    400,
                    "Bad Request",
                    format!("orrery: the proxy request is not HTTP: {e}"),
                    None,
                )
                .await;
            }
        }
    };
    if method != "CONNECT" {
        return refuse(
            io,
            405,
            "Method Not Allowed",
            format!(
                "orrery: the proxy takes CONNECT to GitHub's hosts only, not {method} {target}"
            ),
            None,
        )
        .await;
    }
    let github = target.rsplit_once(':').is_some_and(|(host, port)| {
        port == "443" && orrery_face_github::layout::is_github_host(host)
    });
    if !github {
        tracing::warn!(%target, "the proxy refused a CONNECT to a host other than GitHub's; a client that reaches other hosts lists them in NO_PROXY");
        return refuse(
            io,
            403,
            "Forbidden",
            format!("orrery: the proxy reaches only GitHub's hosts, on port 443, not {target}. A client that reaches other hosts lists them in NO_PROXY."),
            Some(&target),
        )
        .await;
    }
    // A client waits for the answer before it starts TLS, so nothing may
    // follow the head yet.
    if len != head.len() {
        return refuse(
            io,
            400,
            "Bad Request",
            "orrery: the proxy request was followed by data before the tunnel opened".to_string(),
            None,
        )
        .await;
    }
    io.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    io.flush().await
}

/// Answer a proxy request with `status` and an Orrery error body, and fail,
/// so the connection closes.
async fn refuse<Io>(
    io: &mut Io,
    status: u16,
    reason: &str,
    message: String,
    target: Option<&str>,
) -> io::Result<()>
where
    Io: AsyncWrite + Unpin,
{
    let mut orrery = serde_json::json!({ "proxy_refused": true });
    if let Some(target) = target {
        orrery["target"] = serde_json::json!(target);
    }
    let body = serde_json::json!({ "message": message, "orrery": orrery }).to_string();
    let answer = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    io.write_all(answer.as_bytes()).await?;
    io.flush().await?;
    Err(io::Error::other(message))
}
