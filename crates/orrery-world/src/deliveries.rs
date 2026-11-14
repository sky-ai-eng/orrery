//! Deliveries: the POSTs a world owes an app's webhook.
//!
//! A delivery is made for a write, by a face, in that face's own words: the
//! world holds no vendor's event names or payloads. When a write lands, the
//! world hands its events to the [`Notifier`] a face registered and stores
//! what it answers as a write of its own, directly after. The write-order
//! lock is held from before the first write until the second commits, so no
//! other write lands between them: a delivery is rendered from the world as
//! its write left it, and re-executing the write makes the same deliveries
//! again.
//!
//! The engine makes each delivery's one attempt and records what came of it
//! as a logged write ([`World::record_delivery_attempt`]). Nothing here
//! sends anything.

use std::cell::Cell;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::clock::KeepTime;
use crate::events::{self, Attribution, Door, EventBody};
use crate::log::{Landed, Write};
use crate::{
    Delivery, DeliveryAttempt, DeliveryFailure, DeliveryOutcome, DeliveryState, NewDelivery,
    Result, SYSTEM_ACTOR_ID, World, WorldError, credentials, objects, queries,
};

/// What a face registers to make deliveries for the writes a world takes.
pub trait Notifier: Send + Sync {
    /// The deliveries the write that has just landed makes: `events` are
    /// what it appended, and `by` who made it. It is called on the thread
    /// that made the write, which waits for it, with every other write held
    /// off. It reads the world freely and must not write to it: a write
    /// from here is refused.
    fn deliveries(&self, world: &World, by: Attribution, events: &[EventBody]) -> Vec<NewDelivery>;
}

/// Which attempted deliveries a list holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFilter {
    All,
    /// The receiver answered with a status below 400.
    Succeeded,
    /// The receiver answered with a status of 400 or more, or did not
    /// answer.
    Failed,
}

impl DeliveryFilter {
    fn sql(self) -> &'static str {
        match self {
            DeliveryFilter::All => "",
            DeliveryFilter::Succeeded => "AND state = 'answered' AND status_code < 400",
            DeliveryFilter::Failed => "AND NOT (state = 'answered' AND status_code < 400)",
        }
    }
}

thread_local! {
    /// Whether this thread is inside a [`Notifier`] call.
    static NOTIFYING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn notifying() -> bool {
    NOTIFYING.with(Cell::get)
}

struct Notifying;

impl Notifying {
    fn begin() -> Notifying {
        NOTIFYING.with(|n| n.set(true));
        Notifying
    }
}

impl Drop for Notifying {
    fn drop(&mut self) {
        NOTIFYING.with(|n| n.set(false));
    }
}

const DELIVERY_COLUMNS: &str =
    "id, app_id, guid, event, action, installation_id, repo_id, url, body,
     redelivery_of, created_clock_unix, state, attempted_clock_unix, status_code, failure,
     failure_detail, duration_ms, response_headers, response_body, response_truncated";

impl World {
    /// Register the face that makes deliveries for this world's writes.
    /// One face does: a second call replaces the first.
    pub fn set_notifier(&self, notifier: Arc<dyn Notifier>) {
        *self.notifier.write().expect("notifier lock poisoned") = Some(notifier);
    }

    /// Make the deliveries of a write that has landed. The caller holds the
    /// write-order lock and not the connection.
    pub(crate) fn notify(&self, landed: &Landed) -> Result<()> {
        let notifier = self
            .notifier
            .read()
            .expect("notifier lock poisoned")
            .clone();
        let Some(notifier) = notifier else {
            return Ok(());
        };
        let new = {
            let _notifying = Notifying::begin();
            notifier.deliveries(self, landed.by, &landed.events)
        };
        if new.is_empty() {
            return Ok(());
        }
        // The deliveries belong to the write before them, so they are
        // stamped with its clock: a `real` world's time is not read again.
        self.write_locked(
            &self.lock(),
            KeepTime::Never,
            |_| Ok((landed.by, ())),
            |w, ()| {
                for delivery in &new {
                    let hook: Option<(Option<String>, bool)> =
                        w.tx.query_row(
                            "SELECT hook_url, hook_active FROM apps WHERE id = ?1",
                            params![delivery.app_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    // Only an active hook with a URL is owed anything.
                    let Some((Some(url), true)) = hook else {
                        continue;
                    };
                    let id = insert(w.tx, delivery, &url, None)?;
                    created(w, id)?;
                }
                Ok(())
            },
        )
    }

    /// Make the deliveries of the newest write in the log, when a process
    /// stopped between that write and them. A write's deliveries are always
    /// the write directly after it, so only the newest can be without its
    /// own, and making them again for a write that made none makes none.
    pub fn finish_last_write(&self) -> Result<()> {
        let _order = self.order();
        let landed = {
            let conn = self.lock();
            let events = conn
                .prepare(&format!(
                    "SELECT {} FROM action_log
                     WHERE write_seq = (SELECT write_seq FROM action_log ORDER BY seq DESC LIMIT 1)
                     ORDER BY seq",
                    crate::log::EVENT_COLUMNS
                ))?
                .query_map([], crate::log::row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let Some(first) = events.first() else {
                return Ok(());
            };
            Landed {
                by: Attribution::new(first.actor.id, first.door)
                    .with_app(first.app.as_ref().map(|app| app.id)),
                events: events.into_iter().map(|event| event.body).collect(),
            }
        };
        self.notify(&landed)
    }

    /// The deliveries whose attempt has not been made, oldest first, at most
    /// `limit`.
    pub fn pending_deliveries(&self, limit: usize) -> Result<Vec<Delivery>> {
        let conn = self.lock();
        let rows = conn
            .prepare(&format!(
                "SELECT {DELIVERY_COLUMNS} FROM deliveries
                 WHERE state = 'pending' ORDER BY id LIMIT ?1"
            ))?
            .query_map(params![limit as i64], row_to_delivery)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delivery_by_id(&self, id: i64) -> Result<Option<Delivery>> {
        delivery(&self.lock(), id)
    }

    /// Record what came of a delivery's one attempt, which took
    /// `duration_ms` of real time. The engine made the attempt, so the write
    /// is the system actor's, and the result is the one part of a delivery
    /// that comes from outside the world: the log holds all of it, so
    /// replaying the log writes it back rather than sending again. A
    /// delivery that is not pending is refused.
    pub fn record_delivery_attempt(
        &self,
        id: i64,
        duration_ms: i64,
        outcome: &DeliveryOutcome,
    ) -> Result<Delivery> {
        let by = Attribution::new(SYSTEM_ACTOR_ID, Door::Engine);
        self.write(by, |w| {
            let tx = w.tx;
            let pending = delivery(tx, id)?.ok_or(WorldError::NoSuchDelivery(id))?;
            if pending.state != DeliveryState::Pending {
                return Err(WorldError::DeliveryNotPending(id));
            }
            let before = objects::delivery_object(tx, id)?;
            let clock = queries::clock(tx)?;
            match outcome {
                DeliveryOutcome::Answered {
                    status_code,
                    headers,
                    body,
                    truncated,
                } => tx.execute(
                    "UPDATE deliveries
                     SET state = 'answered', attempted_clock_unix = ?1, duration_ms = ?2,
                         status_code = ?3, response_headers = ?4, response_body = ?5,
                         response_truncated = ?6
                     WHERE id = ?7",
                    params![
                        clock,
                        duration_ms,
                        status_code,
                        serde_json::to_string(headers).expect("header pairs serialize"),
                        body,
                        truncated,
                        id
                    ],
                )?,
                DeliveryOutcome::Failed { step, detail } => tx.execute(
                    "UPDATE deliveries
                     SET state = 'failed', attempted_clock_unix = ?1, duration_ms = ?2,
                         failure = ?3, failure_detail = ?4
                     WHERE id = ?5",
                    params![clock, duration_ms, step.as_str(), detail, id],
                )?,
            };
            w.operation(EventBody::DeliveryAttempted(events::DeliveryChanged {
                subject: objects::delivery_subject(tx, id)?,
                changes: events::DeliveryChanges::between(
                    &before,
                    &objects::delivery_object(tx, id)?,
                ),
            }));
            delivery(tx, id)?.ok_or(WorldError::NoSuchDelivery(id))
        })
    }

    /// Owe the receiver one of an app's deliveries again: a new delivery
    /// with the same GUID, event, and body, to the URL the hook has now.
    /// The app asks with its own signature, so the write is its bot's,
    /// through the app. An inactive hook sends nothing, this included.
    pub fn redeliver(&self, app_id: i64, delivery_id: i64) -> Result<Delivery> {
        let by = |tx: &Connection| -> Result<(Attribution, Delivery)> {
            let original = delivery(tx, delivery_id)?
                .filter(|d| d.app_id == app_id)
                .ok_or(WorldError::NoSuchDelivery(delivery_id))?;
            let bot_id: i64 = tx.query_row(
                "SELECT bot_id FROM apps WHERE id = ?1",
                params![app_id],
                |r| r.get(0),
            )?;
            let by = Attribution::new(bot_id, Door::Rest).with_app(Some(app_id));
            Ok((by, original))
        };
        self.write_by(by, |w, original| {
            let tx = w.tx;
            let hook: (Option<String>, bool) = tx.query_row(
                "SELECT hook_url, hook_active FROM apps WHERE id = ?1",
                params![app_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let (Some(url), true) = hook else {
                return Err(WorldError::HookInactive);
            };
            let again = NewDelivery {
                app_id,
                installation_id: original.installation_id,
                repo_id: original.repo_id,
                event: original.event.clone(),
                action: original.action.clone(),
                body: original.body.clone(),
            };
            let first = original.redelivery_of.unwrap_or(original.id);
            let id = insert(tx, &again, &url, Some((first, &original.guid)))?;
            created(w, id)?;
            delivery(tx, id)?.ok_or(WorldError::NoSuchDelivery(id))
        })
    }

    /// One page of an app's attempted deliveries under `filter`, newest
    /// first: at most `limit`, each older than `before` when it is given.
    /// A delivery that is pending, or was dropped, was never sent, so no
    /// page holds it.
    pub fn deliveries_page(
        &self,
        app_id: i64,
        before: Option<i64>,
        limit: usize,
        filter: DeliveryFilter,
    ) -> Result<Vec<Delivery>> {
        let outcome = filter.sql();
        let conn = self.lock();
        let rows = conn
            .prepare(&format!(
                "SELECT {DELIVERY_COLUMNS} FROM deliveries
                 WHERE app_id = ?1 AND state IN ('answered', 'failed') AND id < ?2 {outcome}
                 ORDER BY id DESC LIMIT ?3"
            ))?
            .query_map(
                params![app_id, before.unwrap_or(i64::MAX), limit as i64],
                row_to_delivery,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The attempted deliveries of an app under `filter` that are newer than
    /// the delivery `id`, nearest first, at most `limit`: what a list paged
    /// backwards from `id` reads.
    pub fn deliveries_newer(
        &self,
        app_id: i64,
        id: i64,
        limit: usize,
        filter: DeliveryFilter,
    ) -> Result<Vec<Delivery>> {
        let outcome = filter.sql();
        let conn = self.lock();
        let rows = conn
            .prepare(&format!(
                "SELECT {DELIVERY_COLUMNS} FROM deliveries
                 WHERE app_id = ?1 AND state IN ('answered', 'failed') AND id > ?2 {outcome}
                 ORDER BY id LIMIT ?3"
            ))?
            .query_map(params![app_id, id, limit as i64], row_to_delivery)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop every pending delivery, so none is attempted: what a restore
    /// does with the ones its snapshot held, which were owed by a world
    /// that has since moved on. Answers how many it dropped.
    pub fn drop_pending_deliveries(&self, by: Attribution) -> Result<usize> {
        self.write(by, |w| drop_pending(w, None))
    }
}

/// Drop the pending deliveries, of one app or of every app, each as an
/// event of the write: its operation when the write has none yet.
pub(crate) fn drop_pending(w: &mut Write<'_>, app_id: Option<i64>) -> Result<usize> {
    let tx = w.tx;
    let ids: Vec<i64> = tx
        .prepare(
            "SELECT id FROM deliveries
             WHERE state = 'pending' AND (?1 IS NULL OR app_id = ?1) ORDER BY id",
        )?
        .query_map(params![app_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &ids {
        let before = objects::delivery_object(tx, *id)?;
        tx.execute(
            "UPDATE deliveries SET state = 'dropped' WHERE id = ?1",
            params![id],
        )?;
        let event = EventBody::DeliveryDropped(events::DeliveryChanged {
            subject: objects::delivery_subject(tx, *id)?,
            changes: events::DeliveryChanges::between(&before, &objects::delivery_object(tx, *id)?),
        });
        match w.has_operation() {
            true => w.emit(event),
            false => w.operation(event),
        }
    }
    Ok(ids.len())
}

/// Store a pending delivery to `url`. A redelivery carries the delivery it
/// sends again and that one's GUID; any other is given a GUID of its own,
/// derived from its id.
fn insert(
    tx: &Connection,
    new: &NewDelivery,
    url: &str,
    again: Option<(i64, &str)>,
) -> Result<i64> {
    let id: i64 = tx.query_row(
        "INSERT INTO deliveries (app_id, guid, event, action, installation_id, repo_id, url,
                                 body, redelivery_of, created_clock_unix, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')
         RETURNING id",
        params![
            new.app_id,
            again.map_or("", |(_, guid)| guid),
            new.event,
            new.action,
            new.installation_id,
            new.repo_id,
            url,
            new.body,
            again.map(|(first, _)| first),
            queries::clock(tx)?
        ],
        |r| r.get(0),
    )?;
    if again.is_none() {
        tx.execute(
            "UPDATE deliveries SET guid = ?1 WHERE id = ?2",
            params![
                credentials::delivery_guid(&credentials::world_secret(tx)?, id),
                id
            ],
        )?;
    }
    Ok(id)
}

/// The event for a delivery the write has just stored: its operation when
/// the write has none yet.
fn created(w: &mut Write<'_>, id: i64) -> Result<()> {
    let event = EventBody::DeliveryCreated(events::DeliveryCreated {
        subject: objects::delivery_subject(w.tx, id)?,
        object: objects::delivery_object(w.tx, id)?,
    });
    match w.has_operation() {
        true => w.emit(event),
        false => w.operation(event),
    }
    Ok(())
}

/// Every delivery, in id order.
pub(crate) fn list_deliveries(conn: &Connection) -> Result<Vec<Delivery>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {DELIVERY_COLUMNS} FROM deliveries ORDER BY id"
        ))?
        .query_map([], row_to_delivery)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn delivery(conn: &Connection, id: i64) -> Result<Option<Delivery>> {
    Ok(conn
        .query_row(
            &format!("SELECT {DELIVERY_COLUMNS} FROM deliveries WHERE id = ?1"),
            params![id],
            row_to_delivery,
        )
        .optional()?)
}

fn row_to_delivery(r: &Row<'_>) -> rusqlite::Result<Delivery> {
    let state = DeliveryState::from_db(&r.get::<_, String>(11)?);
    let attempt = match (state, r.get::<_, Option<i64>>(12)?) {
        (DeliveryState::Answered | DeliveryState::Failed, Some(clock_unix)) => {
            let outcome = match r.get::<_, Option<i64>>(13)? {
                Some(status_code) => {
                    let headers: Option<String> = r.get(17)?;
                    DeliveryOutcome::Answered {
                        status_code,
                        headers: headers
                            .and_then(|text| serde_json::from_str(&text).ok())
                            .unwrap_or_default(),
                        body: r.get::<_, Option<String>>(18)?.unwrap_or_default(),
                        truncated: r.get::<_, Option<bool>>(19)?.unwrap_or(false),
                    }
                }
                None => DeliveryOutcome::Failed {
                    step: r
                        .get::<_, Option<String>>(14)?
                        .and_then(|step| DeliveryFailure::from_db(&step))
                        .unwrap_or(DeliveryFailure::Exchange),
                    detail: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                },
            };
            Some(DeliveryAttempt {
                clock_unix,
                duration_ms: r.get::<_, Option<i64>>(16)?.unwrap_or(0),
                outcome,
            })
        }
        _ => None,
    };
    Ok(Delivery {
        id: r.get(0)?,
        app_id: r.get(1)?,
        guid: r.get(2)?,
        event: r.get(3)?,
        action: r.get(4)?,
        installation_id: r.get(5)?,
        repo_id: r.get(6)?,
        url: r.get(7)?,
        body: r.get(8)?,
        redelivery_of: r.get(9)?,
        created_clock_unix: r.get(10)?,
        state,
        attempt,
    })
}
