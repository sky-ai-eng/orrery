//! Check runs — fabricated CI state. The kernel stores what it is told
//! (timestamps included: they are caller inputs, like commit dates, so replay
//! reproduces them); the status/conclusion coupling GitHub enforces at the
//! wire lives in the face, not here.

use rusqlite::{OptionalExtension, params};

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, objects, queries};

pub struct NewCheckRun<'a> {
    pub repo_id: i64,
    pub head_sha: &'a str,
    pub name: &'a str,
    pub status: CheckStatus,
    pub conclusion: Option<CheckConclusion>,
    pub details_url: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub output: Option<&'a serde_json::Value>,
    pub started_clock_unix: Option<i64>,
    pub completed_clock_unix: Option<i64>,
}

#[derive(Default)]
pub struct CheckRunPatch<'a> {
    pub name: Option<&'a str>,
    pub status: Option<CheckStatus>,
    pub conclusion: Option<CheckConclusion>,
    pub details_url: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub output: Option<&'a serde_json::Value>,
    pub started_clock_unix: Option<i64>,
    pub completed_clock_unix: Option<i64>,
}

impl World {
    /// Create a check run. `at` is the time the caller read from
    /// [`World::clock`] to fill in the timestamps it was not given, which a
    /// `real` world's clock is brought to for the write.
    pub fn create_check_run(
        &self,
        by: Attribution,
        new: NewCheckRun<'_>,
        at: i64,
    ) -> Result<CheckRun> {
        self.write_at(at, by, |w| {
            let tx = w.tx;
            let clock = queries::clock(tx)?;
            let run = tx.query_row(
                &format!(
                    "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion,
                                             details_url, external_id, output, creator_id,
                                             started_clock_unix, completed_clock_unix,
                                             created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     RETURNING {}",
                    queries::CHECK_RUN_COLUMNS
                ),
                params![
                    new.repo_id,
                    new.head_sha,
                    new.name,
                    new.status.as_str(),
                    new.conclusion.map(CheckConclusion::as_str),
                    new.details_url,
                    new.external_id,
                    new.output.map(|v| v.to_string()),
                    by.actor_id,
                    new.started_clock_unix,
                    new.completed_clock_unix,
                    clock
                ],
                queries::row_to_check_run,
            )?;
            w.operation(EventBody::CheckRunCreated(events::CheckRunCreated {
                subject: objects::check_run_subject(tx, run.id)?,
                object: objects::check_run_object(tx, run.id)?,
            }));
            Ok(run)
        })
    }

    /// Write the fields `patch` names. A patch that changes nothing writes
    /// nothing. `at` is as [`World::create_check_run`] takes it.
    pub fn update_check_run(
        &self,
        id: i64,
        by: Attribution,
        patch: CheckRunPatch<'_>,
        at: i64,
    ) -> Result<CheckRun> {
        self.write_at(at, by, |w| {
            let tx = w.tx;
            let before = objects::check_run_object(tx, id)?;
            let run = tx
                .query_row(
                    &format!(
                        "UPDATE check_runs SET
                             name = COALESCE(?1, name),
                             status = COALESCE(?2, status),
                             conclusion = COALESCE(?3, conclusion),
                             details_url = COALESCE(?4, details_url),
                             external_id = COALESCE(?5, external_id),
                             output = COALESCE(?6, output),
                             started_clock_unix = COALESCE(?7, started_clock_unix),
                             completed_clock_unix = COALESCE(?8, completed_clock_unix)
                         WHERE id = ?9
                         RETURNING {}",
                        queries::CHECK_RUN_COLUMNS
                    ),
                    params![
                        patch.name,
                        patch.status.map(CheckStatus::as_str),
                        patch.conclusion.map(CheckConclusion::as_str),
                        patch.details_url,
                        patch.external_id,
                        patch.output.map(|v| v.to_string()),
                        patch.started_clock_unix,
                        patch.completed_clock_unix,
                        id
                    ],
                    queries::row_to_check_run,
                )
                .optional()?
                .ok_or(WorldError::NoSuchCheckRun(id))?;
            let changes =
                events::CheckRunChanges::between(&before, &objects::check_run_object(tx, id)?);
            if !changes.is_empty() {
                w.operation(EventBody::CheckRunUpdated(events::CheckRunChanged {
                    subject: objects::check_run_subject(tx, id)?,
                    changes,
                }));
            }
            Ok(run)
        })
    }

    /// Check runs attached to one commit, oldest first.
    pub fn list_check_runs_for_sha(&self, repo_id: i64, head_sha: &str) -> Result<Vec<CheckRun>> {
        self.with_conn(|conn| queries::list_check_runs_for_sha(conn, repo_id, head_sha))
    }

    pub fn check_run_by_id(&self, id: i64) -> Result<Option<CheckRun>> {
        self.with_conn(|conn| queries::check_run_by_id(conn, id))
    }
}
