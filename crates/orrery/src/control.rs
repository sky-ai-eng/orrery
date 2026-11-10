//! The CLI's side of a world's control plane: how a command addresses one
//! world, and the calls it issues there.

use std::io::{self, BufReader, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::Context as _;
use ureq::http::Uri;

/// Which world a command talks to, named by its control listener's origin.
#[derive(clap::Args)]
pub struct Address {
    /// The world's control listener, as an http or https origin without
    /// `/_orrery`. The default is `serve`'s default listen.
    #[arg(
        long = "control-url",
        env = "ORRERY_CONTROL_URL",
        default_value = "http://127.0.0.1:4180",
        value_parser = parse_origin,
    )]
    origin: String,
}

/// Seed scripts reach a world over TCP, because `git` cannot push to a Unix
/// socket, so only http and https are accepted. A path is refused rather than
/// joined to `/_orrery`, so a URL that already ends in `/_orrery` fails here
/// with a reason instead of at the world with a 501.
fn parse_origin(raw: &str) -> Result<String, String> {
    let fault = |what: &str| {
        format!("{what}; expected an http or https origin, like http://127.0.0.1:4180")
    };
    let uri: Uri = raw.parse().map_err(|_| fault("not a URL"))?;
    let scheme = uri.scheme_str().unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(fault("the scheme is not http or https"));
    }
    let Some(authority) = uri.authority() else {
        return Err(fault("no host"));
    };
    if uri.path() != "/" || uri.query().is_some() {
        return Err(fault("a path or query after the host (leave out /_orrery)"));
    }
    Ok(format!("{scheme}://{authority}"))
}

#[derive(serde::Deserialize)]
struct Clock {
    clock_unix: i64,
}

/// The data of the `restored` event that ends a log stream.
#[derive(serde::Deserialize)]
pub struct Restored {
    /// The newest `seq` in the log the restore left.
    pub newest_seq: i64,
}

/// How long a log stream may send nothing at all before its connection is
/// taken to be gone. The stream sends a keepalive every 15 seconds, so this
/// is three missed in a row.
const SILENCE: Duration = Duration::from_secs(45);

impl Address {
    /// `GET /_orrery/clock`: what time the world believes it is.
    pub fn clock(&self) -> anyhow::Result<i64> {
        let body = self.get("/_orrery/clock")?;
        let clock: Clock = serde_json::from_str(&body)
            .with_context(|| format!("{}/_orrery/clock answered {body}", self.origin))?;
        Ok(clock.clock_unix)
    }

    /// `GET /_orrery/log/stream?after_seq=<n>`: write each entry's data to
    /// `out` as one line, flushed, until the stream ends. It returns only when
    /// a restore ends it. Every other end is an error: one for an error answer
    /// carries the answer, and any other names the position to resume from.
    pub fn follow_log(&self, after_seq: u64, out: &mut impl Write) -> anyhow::Result<Restored> {
        let url = format!("{}/_orrery/log/stream?after_seq={after_seq}", self.origin);
        let mut last = after_seq.to_string();
        let resume = |last: &str| {
            format!("following {url} stopped after seq {last}; resume with --after-seq {last}")
        };
        // ureq's other timeouts bound a whole call or body, and a stream can
        // rightly last for hours, so `Watched` times the body's silences.
        // Waiting to connect, or for the answer's head, is silence too.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(SILENCE))
            .timeout_recv_response(Some(SILENCE))
            .build()
            .into();
        let mut response = agent.get(&url).call().with_context(|| resume(&last))?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .body_mut()
                .read_to_string()
                .with_context(|| format!("reading the answer from {url}"))?;
            anyhow::bail!("GET {url} answered {status}: {body}");
        }
        let body = Watched::new(response.into_body().into_reader());
        read_stream(BufReader::new(body), out, &mut last).with_context(|| resume(&last))
    }

    /// The body of a 2xx answer; any other answer is an error carrying it.
    fn get(&self, path: &str) -> anyhow::Result<String> {
        let url = format!("{}{path}", self.origin);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .get(&url)
            .call()
            .with_context(|| format!("reaching the world at {}", self.origin))?;
        let status = response.status();
        let body = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("reading the answer from {url}"))?;
        anyhow::ensure!(status.is_success(), "GET {url} answered {status}: {body}");
        Ok(body)
    }
}

/// The events of a stream, read until one ends it. Each entry's `id` is its
/// `seq`, and becomes `last` once the entry is written.
fn read_stream(
    lines: impl io::BufRead,
    out: &mut impl Write,
    last: &mut String,
) -> anyhow::Result<Restored> {
    let (mut id, mut event, mut data) = (None, String::new(), String::new());
    for line in lines.lines() {
        let line = line?;
        if line.is_empty() {
            let (id, event, mut data) = (
                id.take(),
                std::mem::take(&mut event),
                std::mem::take(&mut data),
            );
            // Every data line ends in a newline, the last one dropped here;
            // an event with no data line is not dispatched.
            if data.pop().is_none() {
                continue;
            }
            match event.as_str() {
                "entry" => {
                    writeln!(out, "{data}")
                        .and_then(|()| out.flush())
                        .context("writing to stdout")?;
                    if let Some(id) = id {
                        *last = id;
                    }
                }
                "restored" => {
                    return serde_json::from_str(&data)
                        .with_context(|| format!("the restored event's data is {data}"));
                }
                // Skipped, as an `EventSource` skips an event nobody listens
                // for, so a world that sends a kind this build does not know
                // can still be followed.
                _ => {}
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "id" => id = Some(value.to_string()),
                "event" => event = value.to_string(),
                "data" => {
                    data.push_str(value);
                    data.push('\n');
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("the world ended the stream")
}

/// A response body read on a thread of its own, so that a read waiting longer
/// than [`SILENCE`] for a byte can fail while the thread is still blocked.
struct Watched {
    chunks: mpsc::Receiver<io::Result<Vec<u8>>>,
    chunk: io::Cursor<Vec<u8>>,
}

impl Watched {
    fn new(mut body: impl Read + Send + 'static) -> Watched {
        // One chunk in flight, so a caller that reads slowly holds the stream
        // back rather than letting it pile up here.
        let (tx, chunks) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            loop {
                let mut chunk = vec![0; 8192];
                let read = body.read(&mut chunk).map(|n| {
                    chunk.truncate(n);
                    chunk
                });
                let more = matches!(&read, Ok(chunk) if !chunk.is_empty());
                if tx.send(read).is_err() || !more {
                    return;
                }
            }
        });
        Watched {
            chunks,
            chunk: io::Cursor::default(),
        }
    }
}

impl Read for Watched {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.chunk.position() == self.chunk.get_ref().len() as u64 {
            self.chunk = match self.chunks.recv_timeout(SILENCE) {
                Ok(chunk) => io::Cursor::new(chunk?),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("the world sent nothing for {} seconds", SILENCE.as_secs()),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
            };
        }
        self.chunk.read(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_origin;

    #[test]
    fn an_origin_is_accepted_with_or_without_a_trailing_slash() {
        for raw in ["http://127.0.0.1:4180", "http://127.0.0.1:4180/"] {
            assert_eq!(parse_origin(raw).unwrap(), "http://127.0.0.1:4180");
        }
        assert_eq!(
            parse_origin("https://world.example").unwrap(),
            "https://world.example"
        );
    }

    #[test]
    fn anything_but_an_http_origin_is_refused() {
        for raw in [
            "unix:/run/orrery/control.sock",
            "ftp://127.0.0.1",
            "127.0.0.1:4180",
            "http://127.0.0.1:4180/_orrery",
            "http://127.0.0.1:4180/?x=1",
            "",
        ] {
            assert!(parse_origin(raw).is_err(), "{raw:?} was accepted");
        }
    }
}
