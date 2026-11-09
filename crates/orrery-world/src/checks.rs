//! Check runs — fabricated CI state. The kernel stores what it is told
//! (timestamps included: they are caller inputs, like commit dates, so replay
//! reproduces them); the status/conclusion coupling GitHub enforces at the
//! wire lives in the face, not here.

use rusqlite::{OptionalExtension, params};

use crate::queries;
use crate::types::*;
use crate::{Result, World, WorldError};

pub struct NewCheckRun<'a> {
    pub repo_id: i64,
    pub creator_id: i64,
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
    pub fn create_check_run(&self, new: NewCheckRun<'_>) -> Result<CheckRun> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let clock = queries::clock(&tx)?;
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
                    new.creator_id,
                    new.started_clock_unix,
                    new.completed_clock_unix,
                    clock
                ],
                queries::row_to_check_run,
            )?;
            self.log_locked(
                &tx,
                new.creator_id,
                "check_run.create",
                &serde_json::json!({
                    "repo": crate::pulls::repo_full_name(&tx, new.repo_id)?,
                    "name": run.name,
                    "head_sha": run.head_sha,
                    "status": run.status.as_str(),
                    "conclusion": run.conclusion.map(CheckConclusion::as_str),
                }),
            )?;
            tx.commit()?;
            Ok(run)
        })
    }

    pub fn update_check_run(
        &self,
        id: i64,
        actor_id: i64,
        patch: CheckRunPatch<'_>,
    ) -> Result<CheckRun> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
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
            self.log_locked(
                &tx,
                actor_id,
                "check_run.update",
                &serde_json::json!({
                    "repo": crate::pulls::repo_full_name(&tx, run.repo_id)?,
                    "name": run.name,
                    "head_sha": run.head_sha,
                    "status": run.status.as_str(),
                    "conclusion": run.conclusion.map(CheckConclusion::as_str),
                }),
            )?;
            tx.commit()?;
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
