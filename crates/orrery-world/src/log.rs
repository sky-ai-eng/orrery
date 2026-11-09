//! The log: the one place a world's events are appended, and where they are
//! read back. Every kernel write runs through [`World::write`], which commits
//! the write's state and its events in one transaction.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::events::*;
use crate::{LogOrder, Result, Signal, World, WorldError, objects, queries};

/// One write in progress: its transaction, and the events it has made.
pub(crate) struct Write<'a> {
    pub(crate) tx: &'a Connection,
    pub(crate) by: Attribution,
    operation: Option<EventBody>,
    consequences: Vec<EventBody>,
}

impl<'a> Write<'a> {
    fn new(tx: &'a Connection, by: Attribution) -> Write<'a> {
        Write {
            tx,
            by,
            operation: None,
            consequences: Vec::new(),
        }
    }

    /// The write's operation, which is its first event however late the
    /// write learns what it changed.
    pub(crate) fn operation(&mut self, body: EventBody) {
        assert!(
            self.operation.is_none(),
            "a write has one operation, and {} came after {}",
            body.type_name(),
            self.operation.as_ref().map_or("", EventBody::type_name),
        );
        self.operation = Some(body);
    }

    pub(crate) fn has_operation(&self) -> bool {
        self.operation.is_some()
    }

    /// Drop the events made so far: the write found nothing to change after
    /// it had started recording.
    pub(crate) fn discard(&mut self) {
        self.operation = None;
        self.consequences.clear();
    }

    /// A consequence of the operation, after the ones before it.
    pub(crate) fn emit(&mut self, body: EventBody) {
        self.consequences.push(body);
    }

    /// Run `change` on the pull and emit `kind` with whatever it changed.
    pub(crate) fn pull<T>(
        &mut self,
        pull_id: i64,
        kind: fn(PullChanged) -> EventBody,
        change: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let (out, body) = self.pull_change(pull_id, kind, change)?;
        if let Some(body) = body {
            self.emit(body);
        }
        Ok(out)
    }

    /// Run `change` on the pull and make it the write's operation, `kind`,
    /// with whatever it changed.
    pub(crate) fn pull_operation<T>(
        &mut self,
        pull_id: i64,
        kind: fn(PullChanged) -> EventBody,
        change: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let before = objects::pull_object(self.tx, pull_id)?;
        let out = change(self.tx)?;
        let after = objects::pull_object(self.tx, pull_id)?;
        self.operation(kind(PullChanged {
            subject: objects::pull_subject(self.tx, pull_id)?,
            changes: PullChanges::between(&before, &after),
        }));
        Ok(out)
    }

    pub(crate) fn pull_change<T>(
        &mut self,
        pull_id: i64,
        kind: fn(PullChanged) -> EventBody,
        change: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<(T, Option<EventBody>)> {
        let before = objects::pull_object(self.tx, pull_id)?;
        let out = change(self.tx)?;
        let after = objects::pull_object(self.tx, pull_id)?;
        let changes = PullChanges::between(&before, &after);
        let body = match changes.is_empty() {
            true => None,
            false => Some(kind(PullChanged {
                subject: objects::pull_subject(self.tx, pull_id)?,
                changes,
            })),
        };
        Ok((out, body))
    }

    fn into_events(self) -> Vec<EventBody> {
        assert!(
            self.operation.is_some() || self.consequences.is_empty(),
            "a write emitted {} with no operation",
            self.consequences[0].type_name()
        );
        self.operation
            .into_iter()
            .chain(self.consequences)
            .collect()
    }
}

impl World {
    /// Run one write: `f` changes the world through `Write::tx` and makes
    /// the write's events, which are appended by `by` and committed with it.
    /// A write whose `f` fails commits nothing, and one that makes no event
    /// appends nothing.
    pub(crate) fn write<T>(
        &self,
        by: Attribution,
        f: impl FnOnce(&mut Write<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_by(|_| Ok((by, ())), |w, ()| f(w))
    }

    /// [`World::write`] for a write that learns who makes it from the world:
    /// `by` reads that inside the write's transaction, before `f` runs, and
    /// hands `f` what else it read.
    pub(crate) fn write_by<X, T>(
        &self,
        by: impl FnOnce(&Connection) -> Result<(Attribution, X)>,
        f: impl FnOnce(&mut Write<'_>, X) -> Result<T>,
    ) -> Result<T> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        #[cfg(any(test, feature = "test-support"))]
        let audit = crate::audit::Audit::begin(&tx)?;
        let (by, read) = by(&tx)?;
        let mut w = Write::new(&tx, by);
        let out = f(&mut w, read)?;
        let events = w.into_events();
        if !events.is_empty() {
            append(&tx, by, &events)?;
            self.signal(Signal::LogMayHaveGrown);
        }
        #[cfg(any(test, feature = "test-support"))]
        audit.end(&tx, &events)?;
        tx.commit()?;
        Ok(out)
    }

    /// Events with `seq > after_seq`, oldest first, at most `limit`. `seq` is
    /// a total order by construction, so pagination is stable. It is not
    /// contiguous: a restore leaves a gap (see [`World::swap_state`]).
    pub fn log_after(&self, after_seq: i64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM action_log WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![after_seq, limit as i64], row_to_event)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Whether `seq` is a position in the log: 0, or the `seq` of an event
    /// it holds. Any other number is [`WorldError::NoSuchLogPosition`],
    /// carrying the log's newest `seq`.
    pub fn check_log_position(&self, seq: i64) -> Result<()> {
        check_log_position(&self.lock(), seq)
    }

    /// One page of the events with `seq > after_seq`, in `order`, and how
    /// many such events there are. With `resume`, the `seq` of the previous
    /// page's last event, the page holds only events past it in `order`, so a
    /// walk neither repeats nor skips an event while the log grows. The count
    /// is read under the same lock as the page. Only `seq`'s order is relied
    /// on, never that its values are contiguous.
    ///
    /// An `after_seq` that is not a position in the log is
    /// [`WorldError::NoSuchLogPosition`]. A `resume` the log no longer holds
    /// is [`WorldError::ResumeNotInLog`]:
    /// the walk's earlier pages came from a log a restore replaced. A `seq`
    /// is issued once, so while the log holds `resume` it holds the same
    /// events up to it, and a walk in either order goes on exactly. Both are
    /// checked under the lock the page is read under, so no restore lands
    /// between a check and the read it gates.
    pub fn log_page(
        &self,
        after_seq: i64,
        order: LogOrder,
        resume: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Event>, i64)> {
        let conn = self.lock();
        check_log_position(&conn, after_seq)?;
        if let Some(resume) = resume
            && !queries::log_holds(&conn, resume)?
        {
            return Err(WorldError::ResumeNotInLog(resume));
        }
        let limit = limit as i64;
        // Each bound is a plain comparison on `seq`, the rowid, so a page deep
        // in the log reads only its own rows.
        let events = match order {
            LogOrder::OldestFirst => conn
                .prepare(&format!(
                    "SELECT {EVENT_COLUMNS} FROM action_log
                     WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2"
                ))?
                .query_map(
                    params![resume.map_or(after_seq, |r| r.max(after_seq)), limit],
                    row_to_event,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?,
            LogOrder::NewestFirst => conn
                .prepare(&format!(
                    "SELECT {EVENT_COLUMNS} FROM action_log
                     WHERE seq > ?1 AND seq <= ?2
                     ORDER BY seq DESC LIMIT ?3"
                ))?
                .query_map(
                    params![
                        after_seq,
                        resume.map_or(i64::MAX, |r| r.saturating_sub(1)),
                        limit
                    ],
                    row_to_event,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        // TODO(ORR-59): this count reads every entry above `after_seq` while
        // holding the world's connection, about 60 ms at a million entries;
        // the whole-log total should come from a count kept with the log.
        let total = conn.query_row(
            "SELECT COUNT(*) FROM action_log WHERE seq > ?1",
            params![after_seq],
            |r| r.get(0),
        )?;
        Ok((events, total))
    }
}

/// Append a write's events, numbered from the next `seq` this database
/// issues, each stamped with the current clock and the one real time.
fn append(tx: &Connection, by: Attribution, events: &[EventBody]) -> Result<()> {
    let first = issued_seq(tx)? + 1;
    let wall = wall_unix_ms();
    let mut insert = tx.prepare_cached(
        "INSERT INTO action_log (seq, write_seq, write_events, clock_unix, wall_unix_ms,
                                 actor_id, door, app_id, type, body)
         VALUES (?1, ?2, ?3, (SELECT clock_unix FROM world WHERE id = 1), ?4, ?5, ?6, ?7, ?8,
                 ?9)",
    )?;
    for (i, event) in events.iter().enumerate() {
        let mut body = serde_json::to_value(event).expect("an event serializes");
        body.as_object_mut()
            .expect("an event serializes as an object")
            .remove("type");
        insert.execute(params![
            first + i as i64,
            first,
            events.len() as i64,
            wall,
            by.actor_id,
            by.door.as_str(),
            by.app_id,
            event.type_name(),
            body.to_string(),
        ])?;
    }
    Ok(())
}

pub(crate) fn check_log_position(conn: &Connection, seq: i64) -> Result<()> {
    if seq == 0 || queries::log_holds(conn, seq)? {
        return Ok(());
    }
    Err(WorldError::NoSuchLogPosition {
        seq,
        newest_seq: queries::last_seq(conn)?,
    })
}

/// What every read of the log projects, in the order `row_to_event` reads it.
const EVENT_COLUMNS: &str = "seq, write_seq, write_events, clock_unix, wall_unix_ms, actor_id,
    (SELECT login FROM actors WHERE actors.id = action_log.actor_id), door, app_id,
    (SELECT client_id FROM oauth_apps WHERE oauth_apps.id = action_log.app_id), type, body";

fn row_to_event(r: &Row<'_>) -> rusqlite::Result<Event> {
    let door: String = r.get(7)?;
    let app_id: Option<i64> = r.get(8)?;
    let mut body: serde_json::Value =
        serde_json::from_str(&r.get::<_, String>(11)?).map_err(|e| conversion_failure(11, e))?;
    body.as_object_mut()
        .ok_or_else(|| conversion_failure(11, "a body is an object"))?
        .insert("type".to_string(), r.get::<_, String>(10)?.into());
    Ok(Event {
        seq: r.get(0)?,
        write: WriteSpan {
            seq: r.get(1)?,
            events: r.get(2)?,
        },
        clock_unix: r.get(3)?,
        wall_unix_ms: r.get(4)?,
        actor: ActorRef {
            id: r.get(5)?,
            login: r.get(6)?,
        },
        door: Door::from_db(&door).ok_or_else(|| conversion_failure(7, "not a door"))?,
        app: match app_id {
            Some(id) => Some(AppRef {
                id,
                client_id: r.get(9)?,
            }),
            None => None,
        },
        body: serde_json::from_value(body).map_err(|e| conversion_failure(11, e))?,
    })
}

fn conversion_failure(
    column: usize,
    e: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, e.into())
}

/// The largest `seq` this database has issued, kept by SQLite for an
/// `AUTOINCREMENT` key. It outlives the events it numbered, which `MAX(seq)`
/// does not.
pub(crate) fn issued_seq(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'action_log'",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// The real time, in Unix milliseconds, a write's events record as the
/// moment they were appended.
pub(crate) fn wall_unix_ms() -> i64 {
    // The world's one wall-clock read. It is recorded for callers showing
    // when something happened, and nothing in the world reads it, so it is
    // not an input to replay (rule 5).
    #[allow(clippy::disallowed_methods)]
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}
