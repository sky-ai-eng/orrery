//! One attempt against a receiver on the loopback interface, served over TLS
//! with a certificate from a throwaway authority, directly and through a
//! `CONNECT` proxy listening on TCP or a Unix socket. Nothing here connects
//! past the loopback interface.
//!
//! The proxy answers for one name, `receiver.test`, which no resolver knows
//! (RFC 6761 reserves `.test`), so an attempt through it can only work if
//! the client handed the proxy the name rather than looking it up.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use orrery_outbound::http::{HeaderMap, HeaderValue, StatusCode};
use orrery_outbound::{
    ATTEMPT_LIMIT, Attempt, BODY_LIMIT, Client, Egress, Failure, HEAD_LIMIT, ProxyAddress, Request,
    TrustError,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// A certificate authority, the file the client is told to trust it from,
/// and a receiver certificate it issued for `localhost`, `127.0.0.1`, and
/// `receiver.test`.
struct Authority {
    pem: PathBuf,
    server: Arc<rustls::ServerConfig>,
    _dir: tempfile::TempDir,
}

impl Authority {
    fn new() -> Authority {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "orrery test authority");
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let ca = rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap())
            .unwrap();

        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "receiver.test".to_string(),
        ])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("ca.pem");
        std::fs::write(&pem, ca.as_ref().pem()).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
        Authority {
            pem,
            server: Arc::new(server),
            _dir: dir,
        }
    }

    fn client(&self, egress: Egress) -> Client {
        Client::new(egress, Some(&self.pem)).unwrap()
    }
}

/// What a receiver does once it has read a request.
#[derive(Clone)]
enum Answer {
    /// Writes these bytes, then closes the connection.
    Bytes(Vec<u8>),
    /// Says nothing, and holds the connection open past the attempt's limit.
    Silence,
    /// Writes these bytes one at a time, 300 milliseconds apart.
    Trickle(Vec<u8>),
}

fn answer(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
    let mut bytes = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        bytes.push_str(&format!("{name}: {value}\r\n"));
    }
    bytes.push_str("\r\n");
    let mut bytes = bytes.into_bytes();
    bytes.extend_from_slice(body);
    Answer::Bytes(bytes)
}

/// A request as the receiver read it.
#[derive(Debug)]
struct Received {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// A receiver on the loopback interface: over TLS with the authority's
/// certificate, or in plain HTTP without one. It reports each request it
/// reads before it answers it.
struct Receiver {
    port: u16,
    requests: mpsc::Receiver<Received>,
}

impl Receiver {
    fn start(tls: Option<Arc<rustls::ServerConfig>>, answer: Answer) -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, requests) = mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let (conn, tls, answer, tx) =
                    (conn.unwrap(), tls.clone(), answer.clone(), tx.clone());
                thread::spawn(move || {
                    let _ = match tls {
                        Some(tls) => {
                            let session = rustls::ServerConnection::new(tls).unwrap();
                            serve(rustls::StreamOwned::new(session, conn), &answer, &tx)
                        }
                        None => serve(conn, &answer, &tx),
                    };
                });
            }
        });
        Receiver { port, requests }
    }

    /// The request the receiver read: it has always read it by the time the
    /// attempt that sent it has its answer.
    fn received(&self) -> Received {
        self.requests
            .try_recv()
            .expect("the receiver read a request")
    }

    fn nothing_more(&self) {
        if let Ok(more) = self.requests.try_recv() {
            panic!("the receiver read another request: {more:?}");
        }
    }
}

fn serve(
    mut conn: impl Read + Write,
    answer: &Answer,
    tx: &mpsc::Sender<Received>,
) -> io::Result<()> {
    let mut read = Vec::new();
    let (head_len, received) = loop {
        let mut chunk = [0; 4096];
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        read.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        if let httparse::Status::Complete(len) = request.parse(&read).unwrap() {
            let received = Received {
                method: request.method.unwrap().to_string(),
                path: request.path.unwrap().to_string(),
                headers: request
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_string(),
                            String::from_utf8(h.value.to_vec()).unwrap(),
                        )
                    })
                    .collect(),
                body: Vec::new(),
            };
            break (len, received);
        }
    };
    let length: usize = received
        .header("content-length")
        .first()
        .map_or(0, |v| v.parse().unwrap());
    let mut body = read.split_off(head_len);
    while body.len() < length {
        let mut chunk = [0; 4096];
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let _ = tx.send(Received { body, ..received });
    match answer {
        Answer::Bytes(bytes) => {
            conn.write_all(bytes)?;
            conn.flush()?;
        }
        Answer::Silence => thread::sleep(ATTEMPT_LIMIT + Duration::from_secs(5)),
        Answer::Trickle(bytes) => {
            for byte in bytes {
                conn.write_all(&[*byte])?;
                conn.flush()?;
                thread::sleep(Duration::from_millis(300));
            }
        }
    }
    Ok(())
}

/// What a proxy does when asked for a tunnel.
#[derive(Clone, Copy)]
enum Tunnel {
    /// Opens it to `receiver.test`, which it resolves to the loopback
    /// address, and refuses every other name with a 403.
    Open,
    /// Answers 403 whatever the name.
    Refuse,
    /// Says nothing.
    Silence,
}

/// An HTTP `CONNECT` proxy. It reports the authority each `CONNECT` names.
struct Proxy {
    address: ProxyAddress,
    asked: mpsc::Receiver<String>,
    _dir: Option<tempfile::TempDir>,
}

trait Duplex: Read + Write + Send + Sized + 'static {
    fn duplicate(&self) -> Self;
    fn close_write(&self);
}

impl Duplex for TcpStream {
    fn duplicate(&self) -> Self {
        self.try_clone().unwrap()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Duplex for UnixStream {
    fn duplicate(&self) -> Self {
        self.try_clone().unwrap()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Proxy {
    fn tcp(tunnel: Tunnel) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = ProxyAddress::Tcp(listener.local_addr().unwrap());
        let (tx, asked) = mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                relay_on_thread(conn.unwrap(), tunnel, tx.clone());
            }
        });
        Proxy {
            address,
            asked,
            _dir: None,
        }
    }

    fn unix(tunnel: Tunnel) -> Proxy {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("egress.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tx, asked) = mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                relay_on_thread(conn.unwrap(), tunnel, tx.clone());
            }
        });
        Proxy {
            address: ProxyAddress::Unix(path),
            asked,
            _dir: Some(dir),
        }
    }

    fn egress(&self) -> Egress {
        Egress::Proxy(self.address.clone())
    }

    fn asked(&self) -> String {
        self.asked
            .try_recv()
            .expect("the proxy was asked for a tunnel")
    }
}

fn relay_on_thread<S: Duplex>(conn: S, tunnel: Tunnel, tx: mpsc::Sender<String>) {
    thread::spawn(move || {
        let _ = relay(conn, tunnel, &tx);
    });
}

fn relay<S: Duplex>(mut client: S, tunnel: Tunnel, tx: &mpsc::Sender<String>) -> io::Result<()> {
    // Byte by byte, so nothing after the request's head is read here.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if client.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let line = head.lines().next().unwrap();
    let mut words = line.split(' ');
    assert_eq!(words.next(), Some("CONNECT"), "{line}");
    let authority = words.next().unwrap().to_string();
    let _ = tx.send(authority.clone());
    let (host, port) = authority.rsplit_once(':').unwrap();
    match tunnel {
        Tunnel::Silence => {
            thread::sleep(ATTEMPT_LIMIT + Duration::from_secs(5));
            return Ok(());
        }
        Tunnel::Open if host == "receiver.test" => {}
        Tunnel::Open | Tunnel::Refuse => {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    }
    let upstream = TcpStream::connect(("127.0.0.1", port.parse::<u16>().unwrap()))?;
    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    let (mut up_read, up_write) = (upstream.duplicate(), upstream);
    let (mut down_read, down_write) = (client.duplicate(), client);
    let up = thread::spawn(move || {
        let mut up_write = up_write;
        let _ = io::copy(&mut down_read, &mut up_write);
        up_write.close_write();
    });
    let mut down_write = down_write;
    let _ = io::copy(&mut up_read, &mut down_write);
    down_write.close_write();
    let _ = up.join();
    Ok(())
}

fn hook(url: String) -> Request {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("user-agent", HeaderValue::from_static("GitHub-Hookshot/0"));
    headers.insert("x-github-event", HeaderValue::from_static("ping"));
    Request {
        url,
        headers,
        body: br#"{"zen":"Keep it logically awesome."}"#.to_vec(),
    }
}

/// The receiver read exactly the request [`hook`] makes, with `host`.
fn assert_hook(received: &Received, host: &str) {
    assert_eq!(received.method, "POST");
    assert_eq!(received.path, "/hook");
    assert_eq!(received.body, hook(String::new()).body);
    assert_eq!(received.header("host"), [host]);
    assert_eq!(received.header("x-github-event"), ["ping"]);
    assert_eq!(received.header("content-type"), ["application/json"]);
    assert_eq!(received.header("user-agent"), ["GitHub-Hookshot/0"]);
    let length = hook(String::new()).body.len().to_string();
    assert_eq!(received.header("content-length"), [length.as_str()]);
    // Nothing the caller did not give, beyond what the URL and body set.
    let mut names: Vec<String> = received
        .headers
        .iter()
        .map(|(n, _)| n.to_ascii_lowercase())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "content-length",
            "content-type",
            "host",
            "user-agent",
            "x-github-event"
        ]
    );
}

fn answered(attempt: Attempt) -> orrery_outbound::Response {
    assert!(attempt.elapsed < ATTEMPT_LIMIT, "{:?}", attempt.elapsed);
    attempt.outcome.expect("the receiver answered")
}

fn failed(attempt: Attempt) -> Failure {
    match attempt.outcome {
        Ok(response) => panic!("the attempt was answered {}", response.status),
        Err(failure) => failure,
    }
}

/// An attempt that ran out of time took the whole limit, and not much more.
fn timed_out(attempt: Attempt) {
    let elapsed = attempt.elapsed;
    assert_eq!(failed(attempt), Failure::Timeout);
    assert!(
        elapsed >= ATTEMPT_LIMIT && elapsed < ATTEMPT_LIMIT + Duration::from_secs(1),
        "{elapsed:?}"
    );
}

#[tokio::test]
async fn a_post_reaches_a_receiver_directly() {
    let ca = Authority::new();
    let receiver = Receiver::start(
        Some(ca.server.clone()),
        answer("202 Accepted", &[("X-Answer", "yes")], b"thanks"),
    );
    let client = ca.client(Egress::Direct);
    let response = answered(
        client
            .post(hook(format!("https://localhost:{}/hook", receiver.port)))
            .await,
    );
    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(response.headers["x-answer"], "yes");
    assert_eq!(response.body, b"thanks");
    assert!(!response.body_truncated);
    assert_hook(
        &receiver.received(),
        &format!("localhost:{}", receiver.port),
    );
}

#[tokio::test]
async fn a_post_goes_through_a_connect_proxy_over_tcp() {
    let ca = Authority::new();
    let receiver = Receiver::start(Some(ca.server.clone()), answer("200 OK", &[], b"ok"));
    let proxy = Proxy::tcp(Tunnel::Open);
    let url = format!("https://receiver.test:{}/hook", receiver.port);
    let response = answered(ca.client(proxy.egress()).post(hook(url)).await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, b"ok");
    // The name, for the proxy to resolve.
    assert_eq!(proxy.asked(), format!("receiver.test:{}", receiver.port));
    assert_hook(
        &receiver.received(),
        &format!("receiver.test:{}", receiver.port),
    );
}

#[tokio::test]
async fn a_post_goes_through_a_connect_proxy_over_a_unix_socket() {
    let ca = Authority::new();
    let receiver = Receiver::start(Some(ca.server.clone()), answer("200 OK", &[], b"ok"));
    let proxy = Proxy::unix(Tunnel::Open);
    let url = format!("https://receiver.test:{}/hook", receiver.port);
    let response = answered(ca.client(proxy.egress()).post(hook(url)).await);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(proxy.asked(), format!("receiver.test:{}", receiver.port));
    assert_hook(
        &receiver.received(),
        &format!("receiver.test:{}", receiver.port),
    );
}

/// An http URL goes through the tunnel too, with no TLS inside it.
#[tokio::test]
async fn a_plain_http_post_goes_through_the_proxy() {
    let receiver = Receiver::start(None, answer("204 No Content", &[], b""));
    let proxy = Proxy::unix(Tunnel::Open);
    let client = Client::new(proxy.egress(), None).unwrap();
    let url = format!("http://receiver.test:{}/hook", receiver.port);
    let response = answered(client.post(hook(url)).await);
    assert_eq!(response.status, StatusCode::NO_CONTENT);
    assert_eq!(proxy.asked(), format!("receiver.test:{}", receiver.port));
    assert_hook(
        &receiver.received(),
        &format!("receiver.test:{}", receiver.port),
    );
}

/// Without the proxy, the same name is one nothing resolves.
#[tokio::test]
async fn a_name_only_the_proxy_knows_does_not_resolve_directly() {
    let ca = Authority::new();
    let url = "https://receiver.test:4443/hook".to_string();
    match failed(ca.client(Egress::Direct).post(hook(url)).await) {
        Failure::Connect(message) => {
            assert!(
                message.starts_with("looking up receiver.test:"),
                "{message}"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_tunnel_the_proxy_refuses_is_its_own_failure() {
    let ca = Authority::new();
    let receiver = Receiver::start(Some(ca.server.clone()), answer("200 OK", &[], b""));
    for proxy in [Proxy::tcp(Tunnel::Refuse), Proxy::unix(Tunnel::Refuse)] {
        let url = format!("https://receiver.test:{}/hook", receiver.port);
        match failed(ca.client(proxy.egress()).post(hook(url)).await) {
            Failure::Proxy(message) => assert!(message.contains("403"), "{message}"),
            other => panic!("{other:?}"),
        }
    }
    receiver.nothing_more();
}

#[tokio::test]
async fn a_refused_connection_is_a_connect_failure() {
    let ca = Authority::new();
    // Bound and never listening, so a connection to it is refused, and the
    // port stays this test's: no other test can take it while it runs.
    let closed = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    closed
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    let gone = closed.local_addr().unwrap().as_socket().unwrap();
    let url = format!("https://{gone}/hook");
    match failed(ca.client(Egress::Direct).post(hook(url)).await) {
        Failure::Connect(message) => assert!(message.contains("refused"), "{message}"),
        other => panic!("{other:?}"),
    }

    let dir = tempfile::tempdir().unwrap();
    for proxy in [
        ProxyAddress::Tcp(gone),
        ProxyAddress::Unix(dir.path().join("no-proxy.sock")),
    ] {
        let url = "https://receiver.test/hook".to_string();
        match failed(
            ca.client(Egress::Proxy(proxy.clone()))
                .post(hook(url))
                .await,
        ) {
            Failure::Connect(message) => {
                assert!(message.contains(&proxy.to_string()), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_certificate_no_trusted_authority_issued_is_a_tls_failure() {
    let ca = Authority::new();
    let receiver = Receiver::start(Some(ca.server.clone()), answer("200 OK", &[], b""));
    let untrusting = Client::new(Egress::Direct, None).unwrap();
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    match failed(untrusting.post(hook(url)).await) {
        Failure::Tls(message) => assert!(message.contains("UnknownIssuer"), "{message}"),
        other => panic!("{other:?}"),
    }
    receiver.nothing_more();
}

#[tokio::test]
async fn a_redirect_is_answered_and_not_followed() {
    let ca = Authority::new();
    let receiver = Receiver::start(
        Some(ca.server.clone()),
        answer("302 Found", &[("Location", "/elsewhere")], b""),
    );
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    let response = answered(ca.client(Egress::Direct).post(hook(url)).await);
    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(response.headers["location"], "/elsewhere");
    assert_eq!(receiver.received().path, "/hook");
    receiver.nothing_more();
}

#[tokio::test]
async fn a_body_past_the_bound_is_cut_at_it() {
    let ca = Authority::new();
    let client = ca.client(Egress::Direct);
    let long: Vec<u8> = (0..4 * BODY_LIMIT).map(|i| (i % 251) as u8).collect();
    for (len, truncated) in [
        (BODY_LIMIT, false),
        (BODY_LIMIT + 1, true),
        (long.len(), true),
    ] {
        let receiver =
            Receiver::start(Some(ca.server.clone()), answer("200 OK", &[], &long[..len]));
        let url = format!("https://127.0.0.1:{}/hook", receiver.port);
        let response = answered(client.post(hook(url)).await);
        assert_eq!(response.body, &long[..BODY_LIMIT], "a {len}-byte body");
        assert_eq!(response.body_truncated, truncated, "a {len}-byte body");
    }
}

#[tokio::test]
async fn a_head_past_its_bound_fails_the_exchange() {
    let ca = Authority::new();
    let padding = "x".repeat(HEAD_LIMIT);
    let receiver = Receiver::start(
        Some(ca.server.clone()),
        answer("200 OK", &[("X-Padding", &padding)], b""),
    );
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    assert!(matches!(
        failed(ca.client(Egress::Direct).post(hook(url)).await),
        Failure::Exchange(_)
    ));
}

/// The body is the receiver's bytes, whatever encoding it says they are in.
#[tokio::test]
async fn a_body_is_kept_as_the_receiver_sent_it() {
    let ca = Authority::new();
    let receiver = Receiver::start(
        Some(ca.server.clone()),
        answer("200 OK", &[("Content-Encoding", "gzip")], b"not gzip"),
    );
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    let response = answered(ca.client(Egress::Direct).post(hook(url)).await);
    assert_eq!(response.body, b"not gzip");
}

#[tokio::test]
async fn a_receiver_that_never_answers_times_out() {
    let ca = Authority::new();
    let receiver = Receiver::start(Some(ca.server.clone()), Answer::Silence);
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    timed_out(ca.client(Egress::Direct).post(hook(url)).await);
}

/// The limit is on the whole attempt: an answer still arriving when it runs
/// out times out, however recently its last byte came.
#[tokio::test]
async fn a_slow_answer_times_out_at_the_limit() {
    let ca = Authority::new();
    let Answer::Bytes(bytes) = answer("200 OK", &[("X-Padding", "slow")], b"too late") else {
        unreachable!()
    };
    assert!(bytes.len() as u64 * 300 > ATTEMPT_LIMIT.as_millis() as u64);
    let receiver = Receiver::start(Some(ca.server.clone()), Answer::Trickle(bytes));
    let url = format!("https://127.0.0.1:{}/hook", receiver.port);
    timed_out(ca.client(Egress::Direct).post(hook(url)).await);
}

#[tokio::test]
async fn a_proxy_that_never_answers_times_out() {
    let ca = Authority::new();
    let proxy = Proxy::unix(Tunnel::Silence);
    let url = "https://receiver.test/hook".to_string();
    timed_out(ca.client(proxy.egress()).post(hook(url)).await);
}

#[test]
fn extra_certificates_must_be_a_readable_pem_file_of_some() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.pem");
    assert!(matches!(
        Client::new(Egress::Direct, Some(&missing)),
        Err(TrustError::Read { path, .. }) if path == missing
    ));
    let empty = write(dir.path(), "empty.pem", "no certificates here\n");
    assert!(matches!(
        Client::new(Egress::Direct, Some(&empty)),
        Err(TrustError::Empty { path }) if path == empty
    ));
    let broken = write(
        dir.path(),
        "broken.pem",
        "-----BEGIN CERTIFICATE-----\nnot base64 at all!\n-----END CERTIFICATE-----\n",
    );
    assert!(Client::new(Egress::Direct, Some(&broken)).is_err());
}

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}
