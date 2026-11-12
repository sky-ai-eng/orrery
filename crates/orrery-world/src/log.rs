//! The log: the one place a world's events are appended, and where they are
//! read back. Every kernel write runs through [`World::write`], which commits
//! the write's state and its events in one transaction.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::clock::{KeepTime, host_unix_ms};
use crate::events::*;
use crate::{LogOrder, Result, Signal, World, WorldError, objects, queries};

/// One write in progress: its transaction, and the events it has made.
pub(crate) struct Write<'a> {
    pub(crate) tx: &'a Connection,
    pub(crate) by: Attribution,
    /// The host's time, when the write read it before it ran
    /// ([`KeepTime`]).
    pub(crate) host_unix: Option<i64>,
    operation: Option<EventBody>,
    consequences: Vec<EventBody>,
}

impl<'a> Write<'a> {
    fn new(tx: &'a Connection, by: Attribution, host_unix: Option<i64>) -> Write<'a> {
        Write {
            tx,
            by,
            host_unix,
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
    /// with whatever it changed. A change that changed nothing makes no
    /// event.
    pub(crate) fn pull_operation<T>(
        &mut self,
        pull_id: i64,
        kind: fn(PullChanged) -> EventBody,
        change: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let (out, body) = self.pull_change(pull_id, kind, change)?;
        if let Some(body) = body {
            self.operation(body);
        }
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
        self.write_keeping(KeepTime::IfReal, by, f)
    }

    /// [`World::write_by`], reading the host's time first as `keep` says.
    /// When that moves the clock, the advance is appended as a write of its
    /// own, before this one, and in its transaction. A write that appends
    /// nothing used no time the log has to hold, so the advance is rolled
    /// back with it.
    pub(crate) fn write_keeping<X, T>(
        &self,
        keep: KeepTime,
        by: impl FnOnce(&Connection) -> Result<(Attribution, X)>,
        f: impl FnOnce(&mut Write<'_>, X) -> Result<T>,
    ) -> Result<T> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let kept = self.keep_time(&tx, keep)?;
        #[cfg(any(test, feature = "test-support"))]
        let audit = crate::audit::Audit::begin(&tx)?;
        let (by, read) = by(&tx)?;
        let mut w = Write::new(&tx, by, kept.host_unix);
        let out = f(&mut w, read)?;
        let events = w.into_events();
        let advanced = kept.advance.is_some();
        if !events.is_empty() {
            if let Some(advance) = kept.advance {
                append_write(&tx, crate::clock::keeper(by), &[advance])?;
            }
            append_write(&tx, by, &events)?;
            self.signal(Signal::LogMayHaveGrown);
        }
        #[cfg(any(test, feature = "test-support"))]
        audit.end(&tx, &events)?;
        if events.is_empty() && advanced {
            // Dropped uncommitted, the transaction rolls the advance back.
            return Ok(out);
        }
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
        // Counting reads every entry it counts, with the world's connection
        // held, so the whole log's total is the number the append keeps. A
        // total above `after_seq` reads only the span the caller asked about.
        let total = match after_seq {
            0 => queries::log_entries(&conn)?,
            _ => queries::log_count_after(&conn, after_seq)?,
        };
        Ok((events, total))
    }

    /// The branches and tags each repository holds according to the log, by
    /// repository id, as `ref -> sha`: every ref at the sha of the last ref
    /// event about it, and absent when that event deletes it or there is
    /// none. A repository whose log moved no ref has no entry. A pull
    /// request's `refs/pull/{n}/head` has no events of its own and is not
    /// here: it belongs at the pull request's head.
    ///
    /// A world migrated from a schema version before its log held events
    /// keeps the entries it logged then. Each push and merge then logged one
    /// `ref.update` per ref it moved, naming the repository as `owner/name`
    /// and the ref's new sha as `new` (all zeros for a delete), and those
    /// count as the ref events they were.
    pub fn logged_refs(&self) -> Result<BTreeMap<i64, BTreeMap<String, String>>> {
        let conn = self.lock();
        let by_name: BTreeMap<String, i64> = queries::list_repos(&conn)?
            .into_iter()
            .map(|r| (r.full_name(), r.id))
            .collect();
        let mut stmt = conn.prepare(
            "SELECT seq, type, body FROM action_log
             WHERE type IN ('ref:created', 'ref:updated', 'ref:deleted', 'ref.update')
             ORDER BY seq",
        )?;
        let mut rows = stmt.query([])?;
        let mut refs: BTreeMap<i64, BTreeMap<String, String>> = BTreeMap::new();
        while let Some(row) = rows.next()? {
            let seq: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let body: String = row.get(2)?;
            let malformed = |e: &dyn std::fmt::Display| {
                conversion_failure(2, format!("log entry {seq} ({kind}): {e}"))
            };
            let (repo_id, name, sha) = if kind == "ref.update" {
                let entry: LegacyRefUpdate =
                    serde_json::from_str(&body).map_err(|e| malformed(&e))?;
                let repo_id = *by_name
                    .get(&entry.repo)
                    .ok_or_else(|| malformed(&format!("no repository {}", entry.repo)))?;
                let deleted = entry.new.bytes().all(|b| b == b'0');
                (repo_id, entry.name, (!deleted).then_some(entry.new))
            } else {
                let entry: RefEntry = serde_json::from_str(&body).map_err(|e| malformed(&e))?;
                let sha = match (kind.as_str(), entry.object, entry.changes) {
                    ("ref:created", Some(object), _) => Some(object.sha),
                    ("ref:updated", _, Some(RefChanges { sha: Some(sha), .. })) => Some(sha.to),
                    ("ref:deleted", _, _) => None,
                    _ => return Err(malformed(&"it names no sha").into()),
                };
                (entry.subject.repo.id, entry.subject.name, sha)
            };
            let repo = refs.entry(repo_id).or_default();
            match sha {
                Some(sha) => repo.insert(name, sha),
                None => repo.remove(&name),
            };
        }
        Ok(refs)
    }
}

/// The fields of a ref event that say where it left the ref. The commits it
/// lists are skipped, not read.
#[derive(serde::Deserialize)]
struct RefEntry {
    subject: RefSubject,
    object: Option<RefObject>,
    changes: Option<RefChanges>,
}

/// A ref move as a world logged it before its log held events.
#[derive(serde::Deserialize)]
struct LegacyRefUpdate {
    repo: String,
    #[serde(rename = "ref")]
    name: String,
    new: String,
}

/// Append one write's events, and in a test build check them against the
/// schema.
pub(crate) fn append_write(tx: &Connection, by: Attribution, events: &[EventBody]) -> Result<()> {
    let _first = append(tx, by, events)?;
    #[cfg(any(test, feature = "test-support"))]
    crate::audit::check_schema(&read_write(tx, _first)?);
    Ok(())
}

/// Append a write's events, numbered from the next `seq` this database
/// issues, each stamped with the current clock and the host's time, add
/// them to the log's count, and return the first event's `seq`.
fn append(tx: &Connection, by: Attribution, events: &[EventBody]) -> Result<i64> {
    let first = issued_seq(tx)? + 1;
    let wall = host_unix_ms();
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
    tx.execute(
        "UPDATE world SET log_entries = log_entries + ?1 WHERE id = 1",
        params![events.len() as i64],
    )?;
    Ok(first)
}

/// The events of the write whose first event is `seq`, as the log serves
/// them.
#[cfg(any(test, feature = "test-support"))]
fn read_write(conn: &Connection, seq: i64) -> Result<Vec<Event>> {
    Ok(conn
        .prepare(&format!(
            "SELECT {EVENT_COLUMNS} FROM action_log WHERE write_seq = ?1 ORDER BY seq"
        ))?
        .query_map(params![seq], row_to_event)?
        .collect::<std::result::Result<Vec<_>, _>>()?)
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

#[cfg(test)]
mod tests {
    use crate::{ActorKind, DEFAULT_T0_UNIX, LogOrder, World, WorldSecret};

    // A total counted from the log is the log's size, and one read from the
    // kept number is whatever that number says, so a world whose number is
    // set wrong shows which one each total reads.
    #[test]
    fn the_whole_logs_total_reads_the_kept_number_and_a_narrowed_one_counts() {
        let dir = tempfile::tempdir().unwrap();
        let w = World::create(
            &dir.path().join("world"),
            DEFAULT_T0_UNIX,
            &WorldSecret::LOCAL,
        )
        .unwrap();
        w.create_actor("alice", ActorKind::User).unwrap();
        w.create_actor("bob", ActorKind::User).unwrap();
        let log = w.log_after(0, 100).unwrap();
        let held = log.len() as i64;

        w.with_conn(|conn| Ok(conn.execute("UPDATE world SET log_entries = 1000", [])?))
            .unwrap();
        for order in [LogOrder::OldestFirst, LogOrder::NewestFirst] {
            assert_eq!(w.log_page(0, order, None, 1).unwrap().1, 1000);
            assert_eq!(w.log_page(log[0].seq, order, None, 1).unwrap().1, held - 1);
        }
    }
}
