//! The world's clock (spec/usage.md, "Time"): what time the world holds it
//! is, and how that moves. A `manual` world's clock moves only by an
//! advance. A `real` world's follows the host's time: a read answers the
//! later of the host's time and the stored clock and writes nothing, and a
//! write first records an advance to the host's time, so every time a write
//! used is in the log.

use rusqlite::{Connection, params};

use crate::events::{self, Attribution, Door, EventBody};
use crate::log::{Write, append_write};
use crate::{
    ClockMode, ClockReading, Result, SYSTEM_ACTOR_ID, Signal, World, WorldError, objects, queries,
};

/// The latest second a world's clock holds: the latest `jiff` represents,
/// so that every time the world stamps can be rendered.
fn latest() -> i64 {
    jiff::Timestamp::MAX.as_second()
}

/// The host's time, in Unix milliseconds.
pub(crate) fn host_unix_ms() -> i64 {
    // The world's one wall-clock read. Each event records it as the time
    // its write was appended, which nothing in the world reads. A `real`
    // world's clock follows it, and the log records the advance to it
    // before any write uses it. So neither is a hidden input to replay
    // (rule 5).
    #[allow(clippy::disallowed_methods)]
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    }
}

/// Where a world reads the host's time for its clock. A served world reads
/// the host's clock. A test sets the time instead
/// ([`World::set_host_time`]), so it sees time pass without waiting for it.
#[derive(Default)]
pub(crate) struct HostTime {
    #[cfg(any(test, feature = "test-support"))]
    set: std::sync::Mutex<Option<i64>>,
}

impl HostTime {
    /// In Unix seconds, and no later than the latest second a clock holds.
    pub(crate) fn now_unix(&self) -> i64 {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(set) = *self.set.lock().expect("host time poisoned") {
            return set.min(latest());
        }
        host_unix_ms().div_euclid(1000).min(latest())
    }
}

/// When a write reads the host's time, before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeepTime {
    /// In a `real` world.
    IfReal,
    /// Always: the write makes the world `real`.
    Always,
    /// Never: the write creates the world, which has no clock yet.
    Never,
}

/// What a write found when it read the host's time before it ran.
#[derive(Default)]
pub(crate) struct Kept {
    /// The host's time, when the write read it.
    pub(crate) host_unix: Option<i64>,
    /// The advance to the host's time, made in the write's transaction when
    /// the host's time was ahead of the stored clock, for the write to
    /// append before its own events.
    pub(crate) advance: Option<EventBody>,
}

/// Who makes the advance a `real` world records before a write `by` makes:
/// the system actor, through the control plane, whose
/// `POST /_orrery/clock/advance` makes the same advance, or through the CLI
/// when the write is a CLI command's.
pub(crate) fn keeper(by: Attribution) -> Attribution {
    let door = match by.door {
        Door::Cli => Door::Cli,
        _ => Door::Control,
    };
    Attribution::new(SYSTEM_ACTOR_ID, door)
}

impl World {
    /// What time the world holds it is, as a read answers it.
    pub fn clock(&self) -> Result<i64> {
        Ok(self.read_clock()?.clock_unix)
    }

    /// The clock and its mode. In a `real` world the clock is the later of
    /// the host's time and the stored clock, and the read writes nothing.
    pub fn read_clock(&self) -> Result<ClockReading> {
        self.with_conn(|conn| self.reading(conn))
    }

    pub(crate) fn reading(&self, conn: &Connection) -> Result<ClockReading> {
        let stored = queries::clock(conn)?;
        let mode = queries::clock_mode(conn)?;
        let clock_unix = match mode {
            ClockMode::Manual => stored,
            ClockMode::Real => stored.max(self.host.now_unix()),
        };
        Ok(ClockReading { clock_unix, mode })
    }

    /// Advance a `manual` world's clock. Forward-only: the clock never
    /// rewinds in a live world (spec/usage.md — rollback is a snapshot
    /// operation). It stops at the latest second `jiff` represents, so that
    /// every time the world stamps can be rendered. A `real` world's clock
    /// follows the host's time, and is never advanced by hand.
    pub fn advance_clock(&self, seconds: i64, by: Attribution) -> Result<i64> {
        if seconds <= 0 {
            return Err(WorldError::ClockRegression(seconds));
        }
        self.write(by, |w| {
            if queries::clock_mode(w.tx)? == ClockMode::Real {
                return Err(WorldError::ClockIsReal);
            }
            let from = queries::clock(w.tx)?;
            let to = from
                .checked_add(seconds)
                .filter(|to| *to <= latest())
                .ok_or_else(|| WorldError::ClockOutOfRange {
                    seconds,
                    remaining: latest().saturating_sub(from).max(0),
                    latest: jiff::Timestamp::MAX,
                })?;
            let (to, advance) = advance_to(w.tx, to)?;
            w.operation(advance);
            Ok(to)
        })
    }

    /// Change how the clock moves, and answer the clock as a read then
    /// would. The clock is first brought to the host's time, as a write of
    /// its own: in a `real` world because every write is, and for a world
    /// becoming `real` because that is where its clock starts following the
    /// host's. A clock ahead of the host's time never goes back, so a
    /// `manual` world whose clock is ahead cannot become `real`.
    pub fn set_clock_mode(&self, mode: ClockMode, by: Attribution) -> Result<ClockReading> {
        let keep = match mode {
            ClockMode::Real => KeepTime::Always,
            ClockMode::Manual => KeepTime::IfReal,
        };
        self.write_keeping(
            keep,
            |_| Ok((by, ())),
            |w: &mut Write<'_>, ()| {
                let from = objects::world_object(w.tx)?;
                if mode == ClockMode::Real && from.clock_mode == ClockMode::Manual {
                    let host = w
                        .host_unix
                        .expect("a write that makes a world real reads the host's time");
                    if from.clock_unix > host {
                        return Err(WorldError::ClockAhead {
                            clock: from.clock_unix,
                            host,
                        });
                    }
                }
                let reading = w.tx.query_row(
                "UPDATE world SET clock_mode = ?1 WHERE id = 1 RETURNING clock_unix, clock_mode",
                params![mode.as_str()],
                |r| {
                    Ok(ClockReading {
                        clock_unix: r.get(0)?,
                        mode: ClockMode::from_db(&r.get::<_, String>(1)?),
                    })
                },
            )?;
                let changes = events::WorldChanges::between(&from, &objects::world_object(w.tx)?);
                if !changes.is_empty() {
                    w.operation(EventBody::WorldClockModeSet(events::WorldChanged {
                        subject: events::WorldSubject::default(),
                        changes,
                    }));
                }
                Ok(reading)
            },
        )
    }

    /// Read `unix` as the host's time from now on, in place of the host's
    /// clock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_host_time(&self, unix: i64) {
        *self.host.set.lock().expect("host time poisoned") = Some(unix);
    }

    /// Read the host's time inside `tx` when `keep` says to, and move the
    /// stored clock to it when it is later.
    pub(crate) fn keep_time(&self, tx: &Connection, keep: KeepTime) -> Result<Kept> {
        let reads = match keep {
            KeepTime::Never => false,
            KeepTime::Always => true,
            KeepTime::IfReal => queries::clock_mode(tx)? == ClockMode::Real,
        };
        if !reads {
            return Ok(Kept::default());
        }
        let host = self.host.now_unix();
        if host <= queries::clock(tx)? {
            return Ok(Kept {
                host_unix: Some(host),
                advance: None,
            });
        }
        #[cfg(any(test, feature = "test-support"))]
        let audit = crate::audit::Audit::begin(tx)?;
        let (_, advance) = advance_to(tx, host)?;
        #[cfg(any(test, feature = "test-support"))]
        audit.end(tx, std::slice::from_ref(&advance))?;
        Ok(Kept {
            host_unix: Some(host),
            advance: Some(advance),
        })
    }

    /// In a `real` world, record an advance to the host's time as a write
    /// of its own through `door`, so the stored clock is the present.
    pub(crate) fn catch_up(&self, conn: &Connection, door: Door) -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        if let Some(advance) = self.keep_time(&tx, KeepTime::IfReal)?.advance {
            append_write(&tx, Attribution::new(SYSTEM_ACTOR_ID, door), &[advance])?;
            self.signal(Signal::LogMayHaveGrown);
            tx.commit()?;
        }
        Ok(())
    }
}

/// Move the stored clock forward to `to`, and the event that records it.
fn advance_to(tx: &Connection, to: i64) -> Result<(i64, EventBody)> {
    let from = objects::world_object(tx)?;
    let to: i64 = tx.query_row(
        "UPDATE world SET clock_unix = ?1 WHERE id = 1 RETURNING clock_unix",
        params![to],
        |r| r.get(0),
    )?;
    let advance = EventBody::WorldClockAdvanced(events::WorldChanged {
        subject: events::WorldSubject::default(),
        changes: events::WorldChanges::between(&from, &objects::world_object(tx)?),
    });
    Ok((to, advance))
}
