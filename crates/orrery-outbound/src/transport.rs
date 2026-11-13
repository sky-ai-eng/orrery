//! The connection under ureq's HTTP/1.1: how it leaves the machine, its TLS,
//! and the one deadline every wait in it is bounded by.
//!
//! ureq's own proxy support reaches a `CONNECT` proxy only over TCP, and its
//! TLS trusts either the built-in roots or a list of its own, never both, so
//! both steps are connectors here. ureq still writes the request and reads
//! the answer.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::{ClientConnection, StreamOwned};
use rustls_pki_types::ServerName;
use ureq::Timeout;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, TcpConnector, Transport,
    TransportAdapter,
};

use crate::{ATTEMPT_LIMIT, Egress, Failure, HEAD_LIMIT, ProxyAddress};

/// The real time an attempt started at. It bounds every wait the attempt
/// makes, and says how long the attempt took.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Deadline {
    start: Instant,
}

impl Deadline {
    pub(crate) fn start() -> Deadline {
        // Real time, not the world's clock: a receiver and a proxy answer in
        // real time, and how long an attempt took is a fact about the
        // network. It reaches a world only as a result a logged write
        // records, so it is not an input to replay.
        #[allow(clippy::disallowed_methods)]
        let start = Instant::now();
        Deadline { start }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// The time left, which is a timeout once there is none.
    fn left(&self) -> Result<Duration, ureq::Error> {
        match ATTEMPT_LIMIT.checked_sub(self.elapsed()) {
            Some(left) if !left.is_zero() => Ok(left),
            _ => Err(ureq::Error::Timeout(Timeout::Global)),
        }
    }

    fn next(&self) -> Result<NextTimeout, ureq::Error> {
        Ok(NextTimeout {
            after: self.left()?.into(),
            reason: Timeout::Global,
        })
    }
}

/// A failure at a step before the exchange, carried through ureq as
/// `ureq::Error::Other` so the attempt can say which step it was.
fn failed(failure: Failure) -> ureq::Error {
    ureq::Error::Other(Box::new(failure))
}

/// `e` reported at `stage`, unless it is the deadline running out, which is
/// a timeout whichever step it ran out in.
fn at(stage: impl FnOnce(String) -> Failure, e: ureq::Error) -> ureq::Error {
    match e {
        ureq::Error::Timeout(_) => e,
        e => failed(stage(describe(&e))),
    }
}

/// An error's own words: an I/O error without ureq's `io:` prefix.
pub(crate) fn describe(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Io(e) => e.to_string(),
        e => e.to_string(),
    }
}

/// Looks up the receiver's name when it connects directly, and never through
/// a proxy, which does its own lookup.
#[derive(Debug)]
pub(crate) enum Lookup {
    System(DefaultResolver),
    Proxy,
}

impl Resolver for Lookup {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        match self {
            Lookup::System(resolver) => resolver.resolve(uri, config, timeout).map_err(|e| {
                let host = uri.host().expect("a target has a host");
                at(
                    |message| Failure::Connect(format!("looking up {host}: {message}")),
                    e,
                )
            }),
            Lookup::Proxy => Ok(self.empty()),
        }
    }
}

/// Opens the connection the request goes over: to the receiver, or a tunnel
/// to it through the egress proxy.
#[derive(Debug)]
pub(crate) struct EgressConnector {
    pub(crate) egress: Egress,
    pub(crate) deadline: Deadline,
}

impl Connector<()> for EgressConnector {
    type Out = Bounded;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> Result<Option<Bounded>, ureq::Error> {
        let bounded = |inner| Bounded {
            inner,
            deadline: self.deadline,
        };
        let address = match &self.egress {
            Egress::Direct => {
                let Some(tcp) = TcpConnector::default()
                    .connect(details, None::<()>)
                    .map_err(|e| {
                        let receiver = details.uri.authority().expect("a target has a host");
                        at(
                            |message| {
                                Failure::Connect(format!("connecting to {receiver}: {message}"))
                            },
                            e,
                        )
                    })?
                else {
                    return Err(ureq::Error::ConnectionFailed);
                };
                return Ok(Some(bounded(tcp.boxed())));
            }
            Egress::Proxy(address) => address,
        };
        let socket = open(address, self.deadline).map_err(|e| {
            at(
                |message| {
                    Failure::Connect(format!("reaching the egress proxy at {address}: {message}"))
                },
                e,
            )
        })?;
        let mut proxy = bounded(
            SocketTransport {
                socket,
                buffers: LazyBuffers::new(
                    details.config.input_buffer_size(),
                    details.config.output_buffer_size(),
                ),
            }
            .boxed(),
        );
        tunnel(&mut proxy, details.uri)?;
        Ok(Some(proxy))
    }
}

/// A connection to the proxy at `address`, made before the deadline.
fn open(address: &ProxyAddress, deadline: Deadline) -> Result<Socket, ureq::Error> {
    let left = deadline.left()?;
    let opened = match address {
        ProxyAddress::Tcp(addr) => TcpStream::connect_timeout(addr, left).map(Socket::Tcp),
        ProxyAddress::Unix(path) => {
            use socket2::{Domain, SockAddr, Type};
            socket2::Socket::new(Domain::UNIX, Type::STREAM, None).and_then(|socket| {
                // A connect waiting for room in a full listen queue gives
                // up after the send timeout, so this bounds it.
                socket.set_write_timeout(Some(left))?;
                socket.connect(&SockAddr::unix(path)?)?;
                Ok(Socket::Unix(socket.into()))
            })
        }
    };
    opened.map_err(|e| timed_out(e, Timeout::Connect))
}

/// `e`, or a timeout when it is a socket's timeout running out.
fn timed_out(e: io::Error, reason: Timeout) -> ureq::Error {
    match e.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ureq::Error::Timeout(reason),
        _ => e.into(),
    }
}

/// Asks the proxy for a tunnel to `uri`'s host and port, named as the URL
/// names them, and reads the proxy's answer. Bytes after the answer are left
/// in the buffers, for the TLS handshake or the request to read first.
fn tunnel(proxy: &mut Bounded, uri: &Uri) -> Result<(), ureq::Error> {
    let host = uri.host().expect("a target has a host");
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });
    // `host()` keeps an IPv6 address's brackets, as the authority form needs.
    let authority = format!("{host}:{port}");
    let no_tunnel = |message: String| failed(Failure::Proxy(message));
    proxy
        .send(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .map_err(|e| at(Failure::Proxy, e))?;
    loop {
        let more = proxy.fill().map_err(|e| at(Failure::Proxy, e))?;
        let input = proxy.buffers().input();
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut answer = httparse::Response::new(&mut headers);
        let head = match answer.parse(input) {
            Ok(httparse::Status::Complete(len)) => Ok((
                len,
                answer.code.expect("a complete answer has a status"),
                answer.reason.unwrap_or_default().to_string(),
            )),
            Ok(httparse::Status::Partial) => Err(input.len()),
            Err(e) => {
                return Err(no_tunnel(format!(
                    "the proxy answered CONNECT {authority} with something that is not HTTP: {e}"
                )));
            }
        };
        match head {
            Ok((len, code, reason)) => {
                proxy.buffers().input_consume(len);
                return if (200..300).contains(&code) {
                    Ok(())
                } else {
                    Err(no_tunnel(format!(
                        "the proxy answered CONNECT {authority} with {code} {reason}"
                    )))
                };
            }
            Err(_) if !more => {
                return Err(no_tunnel(format!(
                    "the proxy closed the connection before it answered CONNECT {authority}"
                )));
            }
            Err(read) if read > HEAD_LIMIT => {
                return Err(no_tunnel(format!(
                    "the proxy's answer to CONNECT {authority} is longer than {HEAD_LIMIT} bytes"
                )));
            }
            // Consuming nothing marks the input as no progress, so the next
            // fill waits for more instead of returning what is there.
            Err(_) => proxy.buffers().input_consume(0),
        }
    }
}

/// Wraps the connection in TLS for an https URL, and hands it on as it is
/// for an http one.
#[derive(Debug)]
pub(crate) struct TlsConnector {
    pub(crate) config: Arc<rustls::ClientConfig>,
}

impl Connector<Bounded> for TlsConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<Bounded>,
    ) -> Result<Option<Box<dyn Transport>>, ureq::Error> {
        let Some(connection) = chained else {
            return Ok(None);
        };
        if !details.needs_tls() {
            return Ok(Some(connection.boxed()));
        }
        let host = details.uri.host().expect("a target has a host");
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let name = ServerName::try_from(bare.to_string()).map_err(|e| {
            failed(Failure::Tls(format!(
                "{host:?} can't be a TLS server name: {e}"
            )))
        })?;
        let conn = ClientConnection::new(self.config.clone(), name)
            .map_err(|e| failed(Failure::Tls(e.to_string())))?;
        let mut stream = StreamOwned::new(conn, TransportAdapter::new(connection));
        // Done here rather than on the first write, so that a failure in it
        // is the TLS step's and not the exchange's.
        while stream.conn.is_handshaking() {
            stream
                .conn
                .complete_io(&mut stream.sock)
                .map_err(|e| at(Failure::Tls, e.into()))?;
        }
        Ok(Some(
            TlsTransport {
                buffers: LazyBuffers::new(
                    details.config.input_buffer_size(),
                    details.config.output_buffer_size(),
                ),
                stream,
            }
            .boxed(),
        ))
    }
}

/// A connection whose every wait ends by the attempt's deadline, whatever
/// timeout ureq asks for.
pub(crate) struct Bounded {
    inner: Box<dyn Transport>,
    deadline: Deadline,
}

impl Bounded {
    fn send(&mut self, mut bytes: &[u8]) -> Result<(), ureq::Error> {
        while !bytes.is_empty() {
            let output = self.inner.buffers().output();
            let n = output.len().min(bytes.len());
            output[..n].copy_from_slice(&bytes[..n]);
            self.inner.transmit_output(n, self.deadline.next()?)?;
            bytes = &bytes[n..];
        }
        Ok(())
    }

    /// Whether there is input to read, waiting for some if there is none:
    /// false when the peer has closed the connection.
    fn fill(&mut self) -> Result<bool, ureq::Error> {
        let timeout = self.deadline.next()?;
        self.inner.maybe_await_input(timeout)
    }
}

impl Transport for Bounded {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, _: NextTimeout) -> Result<(), ureq::Error> {
        let timeout = self.deadline.next()?;
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
        let timeout = self.deadline.next()?;
        self.inner.await_input(timeout)
    }

    /// Never, so that ureq never keeps a connection for another request: an
    /// attempt is one request on a connection of its own.
    fn is_open(&mut self) -> bool {
        false
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

impl fmt::Debug for Bounded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bounded")
            .field("inner", &self.inner)
            .finish()
    }
}

/// A connection to the egress proxy.
enum Socket {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Socket {
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Socket::Tcp(s) => s
                .set_read_timeout(timeout)
                .and_then(|()| s.set_write_timeout(timeout)),
            Socket::Unix(s) => s
                .set_read_timeout(timeout)
                .and_then(|()| s.set_write_timeout(timeout)),
        }
    }
}

impl Read for Socket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Socket::Tcp(s) => s.read(buf),
            Socket::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Socket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Socket::Tcp(s) => s.write(buf),
            Socket::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Socket::Tcp(s) => s.flush(),
            Socket::Unix(s) => s.flush(),
        }
    }
}

struct SocketTransport {
    socket: Socket,
    buffers: LazyBuffers,
}

impl Transport for SocketTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.socket.set_timeouts(timeout.not_zero().map(|t| *t))?;
        let output = &self.buffers.output()[..amount];
        self.socket
            .write_all(output)
            .map_err(|e| timed_out(e, timeout.reason))
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.socket.set_timeouts(timeout.not_zero().map(|t| *t))?;
        let input = self.buffers.input_append_buf();
        let read = self
            .socket
            .read(input)
            .map_err(|e| timed_out(e, timeout.reason))?;
        self.buffers.input_appended(read);
        Ok(read > 0)
    }

    fn is_open(&mut self) -> bool {
        false
    }
}

impl fmt::Debug for SocketTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.socket {
            Socket::Tcp(_) => "tcp",
            Socket::Unix(_) => "unix",
        };
        f.debug_struct("SocketTransport")
            .field("socket", &kind)
            .finish()
    }
}

/// TLS over a [`Bounded`] connection, whose deadline its reads and writes
/// keep.
struct TlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter<Bounded>>,
}

impl Transport for TlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, _: NextTimeout) -> Result<(), ureq::Error> {
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        self.stream.flush()?;
        Ok(())
    }

    fn await_input(&mut self, _: NextTimeout) -> Result<bool, ureq::Error> {
        let input = self.buffers.input_append_buf();
        let read = self.stream.read(input)?;
        self.buffers.input_appended(read);
        Ok(read > 0)
    }

    fn is_open(&mut self) -> bool {
        false
    }

    fn is_tls(&self) -> bool {
        true
    }
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsTransport")
            .field("connection", self.stream.sock.get_ref())
            .finish()
    }
}
