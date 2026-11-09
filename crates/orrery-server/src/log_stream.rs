//! `GET /_orrery/log/stream`: the log's events as server-sent events, from a
//! position the caller holds, then each one as it is appended.
//!
//! The kernel's [`Signal`]s only wake a stream; what a stream sends is always
//! read through the `World`, so a signal for a write that never commits
//! sends nothing. Between reads a stream holds neither the connection nor the
//! world write gate, so it holds up no write, snapshot, or restore.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use orrery_world::{Signal, World, WorldError};
use tokio::sync::watch;

use crate::AppState;
use crate::control::{Fault, errors, faults, internal};

/// How many events a stream reads at a time. A stream holds at most one
/// page, so a slow caller costs this much memory and no more.
const PAGE: usize = 100;

const KEEPALIVE: Duration = Duration::from_secs(15);

const LAST_EVENT_ID: &str = "Last-Event-ID";

const NOT_DECIMAL: &str = "is not a decimal integer";

/// What the streams of one world wait on. The kernel's signals bump it, and
/// a watch channel keeps only its latest value, so a burst of appends wakes a
/// waiting stream once.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LogChanges {
    /// How many times the world's state has been replaced.
    restores: u64,
    /// The newest `seq` in the log the latest restore left.
    newest_seq: i64,
}

/// The channel the world's streams wait on, fed by a listener on `world`.
pub(crate) fn follow(world: &World) -> Arc<watch::Sender<LogChanges>> {
    let changes = Arc::new(watch::Sender::new(LogChanges::default()));
    let sender = changes.clone();
    world.listen(move |signal| match signal {
        Signal::LogMayHaveGrown => sender.send_modify(|_| {}),
        Signal::StateReplaced { newest_seq } => sender.send_modify(|c| {
            c.restores += 1;
            c.newest_seq = newest_seq;
        }),
    });
    changes
}

pub(crate) async fn stream(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    let position = match Position::parse(uri.query(), &headers) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    // Subscribed before the position is checked, so a restore that lands
    // after the check is seen by the stream's first read.
    let mut changes = state.log_changes.subscribe();
    let restores = changes.borrow_and_update().restores;

    let world = state.world.clone();
    let seq = position.seq;
    match tokio::task::spawn_blocking(move || world.check_log_position(seq)).await {
        Ok(Ok(())) => {}
        Ok(Err(WorldError::NoSuchLogPosition { newest_seq, .. })) => {
            return position.not_found(newest_seq);
        }
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(format!("blocking task failed: {e}")),
    }

    let follow = Follow {
        world: state.world.clone(),
        changes,
        restores,
        after: position.seq,
        page: Vec::new().into_iter(),
        read_now: true,
        ended: false,
    };
    Sse::new(futures_util::stream::unfold(follow, Follow::next))
        .keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive"))
        .into_response()
}

/// Where a stream starts, and which input named it.
struct Position {
    seq: i64,
    from_header: bool,
}

impl Position {
    /// The query is read strictly, and the header too. When both name a
    /// position the header wins: an `EventSource` reconnects to the URL it
    /// was opened with, whose `after_seq` is stale by then.
    fn parse(query: Option<&str>, headers: &HeaderMap) -> Result<Position, Box<Response<Body>>> {
        let mut found = Vec::new();
        let mut after_seq = None;
        let mut repeated = false;
        let mut unknown = BTreeSet::new();
        for (name, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
            if name != "after_seq" {
                unknown.insert(name.into_owned());
            } else if after_seq.is_some() {
                repeated = true;
            } else {
                after_seq = Some(match parse_seq(&value) {
                    Ok(seq) => Some(seq),
                    Err((reason, message)) => {
                        found.push(Fault::new(
                            reason,
                            format!("after_seq {message}"),
                            Some("after_seq"),
                        ));
                        None
                    }
                });
            }
        }
        if repeated {
            found.push(Fault::invalid(
                "after_seq appears more than once".to_string(),
                "after_seq",
            ));
        }
        found.extend(unknown.into_iter().map(|name| {
            Fault::invalid(
                format!("unknown query parameter {name:?}; this route takes after_seq"),
                name,
            )
        }));

        let mut header = headers.get_all(LAST_EVENT_ID).iter();
        let last_event_id = match (header.next(), header.next()) {
            (None, _) => None,
            (Some(_), Some(_)) => {
                found.push(Fault::new(
                    "INVALID_ARGUMENT",
                    format!("the {LAST_EVENT_ID} header appears more than once"),
                    None,
                ));
                None
            }
            (Some(value), None) => {
                let parsed = match value.to_str() {
                    Ok(text) => parse_seq(text),
                    Err(_) => Err(("INVALID_ARGUMENT", NOT_DECIMAL.to_string())),
                };
                match parsed {
                    Ok(seq) => Some(seq),
                    Err((reason, message)) => {
                        found.push(Fault::new(
                            reason,
                            format!("the {LAST_EVENT_ID} header {message}"),
                            None,
                        ));
                        None
                    }
                }
            }
        };
        if !found.is_empty() {
            return Err(Box::new(faults(StatusCode::BAD_REQUEST, found)));
        }
        match (last_event_id, after_seq.flatten()) {
            (Some(seq), _) => Ok(Position {
                seq,
                from_header: true,
            }),
            (None, Some(seq)) => Ok(Position {
                seq,
                from_header: false,
            }),
            (None, None) => Err(Box::new(errors(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                &format!(
                    "after_seq is required unless the {LAST_EVENT_ID} header names the position"
                ),
                Some("after_seq"),
            ))),
        }
    }

    fn not_found(&self, newest: i64) -> Response<Body> {
        match self.from_header {
            true => not_in_log(
                &format!("the {LAST_EVENT_ID} header"),
                None,
                self.seq,
                newest,
            ),
            false => not_in_log("after_seq", Some("after_seq"), self.seq, newest),
        }
    }
}

/// A position, `seq`, that names no entry: it was never issued, or a restore
/// discarded it. `named` is what held it, and `field` is its name when that
/// was a field.
pub(crate) fn not_in_log(
    named: &str,
    field: Option<&str>,
    seq: i64,
    newest: i64,
) -> Response<Body> {
    errors(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &format!(
            "{named} names seq {seq}, which is not in the log; \
             a position is 0 or the seq of an event, and the newest is {newest}"
        ),
        field,
    )
}

/// A position: a decimal integer, 0 or more. The error is the reason and a
/// message that follows the name of whatever held the text.
fn parse_seq(text: &str) -> Result<i64, (&'static str, String)> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(("INVALID_ARGUMENT", format!("{NOT_DECIMAL}: {text:?}")));
    }
    match text.parse::<i64>() {
        Ok(seq) if seq >= 0 => Ok(seq),
        _ if text.starts_with('-') => Err(("OUT_OF_RANGE", "must be 0 or more".to_string())),
        _ => Err((
            "OUT_OF_RANGE",
            format!("is {text}, which is larger than any seq"),
        )),
    }
}

/// One open stream: where it is, and the page it is sending.
struct Follow {
    world: Arc<World>,
    changes: watch::Receiver<LogChanges>,
    /// The restore count when the stream opened. Any other count means the
    /// log this stream was following no longer exists.
    restores: u64,
    /// The `seq` of the last entry sent, or the position it opened at.
    after: i64,
    page: std::vec::IntoIter<orrery_world::Event>,
    /// Read again without waiting for a signal: the stream has just opened,
    /// or the last read filled a page.
    read_now: bool,
    ended: bool,
}

impl Follow {
    /// The next event, read when the caller asks for it.
    async fn next(mut self) -> Option<(Result<Event, Infallible>, Follow)> {
        loop {
            if self.ended {
                return None;
            }
            if let Some(entry) = self.page.next() {
                self.after = entry.seq;
                let event = Event::default()
                    .id(entry.seq.to_string())
                    .event("entry")
                    .data(serde_json::json!(entry).to_string());
                return Some((Ok(event), self));
            }
            if !self.read_now && self.changes.changed().await.is_err() {
                return None;
            }
            // Marked seen before the read, so an append that lands during
            // it wakes the stream again.
            self.changes.borrow_and_update();
            let world = self.world.clone();
            let after = self.after;
            let page = match tokio::task::spawn_blocking(move || world.log_after(after, PAGE)).await
            {
                Ok(Ok(page)) => page,
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "log stream read failed");
                    return None;
                }
                Err(e) => {
                    tracing::error!(error = %e, "log stream read task failed");
                    return None;
                }
            };
            // Checked after the read, so a page read from a replaced log is
            // dropped unsent (the ordering is `Signal`'s).
            let changes = *self.changes.borrow();
            if changes.restores != self.restores {
                self.ended = true;
                let event = Event::default()
                    .event("restored")
                    .data(serde_json::json!({ "newest_seq": changes.newest_seq }).to_string());
                return Some((Ok(event), self));
            }
            self.read_now = page.len() == PAGE;
            self.page = page.into_iter();
        }
    }
}
