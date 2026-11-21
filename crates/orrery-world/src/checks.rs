//! Check runs — fabricated CI state. The kernel stores what it is told
//! (timestamps included: they are caller inputs, like commit dates, so replay
//! reproduces them); the status/conclusion coupling GitHub enforces at the
//! wire lives in the face, not here.

use rusqlite::{OptionalExtension, params};

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, objects, queries, suites};

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
    /// What the run says of lines of files, to start with.
    pub annotations: &'a [Annotation],
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
    /// More of what the run says of lines of files, kept after what it has.
    pub annotations: &'a [Annotation],
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
            let suite = suites::suite_for(w, new.repo_id, new.head_sha, by.actor_id)?;
            let tx = w.tx;
            let clock = queries::clock(tx)?;
            let run = tx.query_row(
                &format!(
                    "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion,
                                             details_url, external_id, output, creator_id,
                                             started_clock_unix, completed_clock_unix,
                                             created_clock_unix, suite_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
                    clock,
                    suite
                ],
                queries::row_to_check_run,
            )?;
            w.operation(EventBody::CheckRunCreated(events::CheckRunCreated {
                subject: objects::check_run_subject(tx, run.id)?,
                object: objects::check_run_object(tx, run.id)?,
            }));
            let run = annotate(w, run, new.annotations)?;
            suites::settle(w, suite)?;
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
            // A job moves as its runner reports, which lists the jobs that
            // were waiting for it.
            if before.workflow_run_id.is_some() {
                return Err(WorldError::CheckRunIsJob(id));
            }
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
            let changed = !changes.is_empty();
            if changed {
                w.operation(EventBody::CheckRunUpdated(events::CheckRunChanged {
                    subject: objects::check_run_subject(tx, id)?,
                    changes,
                }));
            }
            let run = annotate(w, run, patch.annotations)?;
            if changed {
                suites::settle(w, run.suite_id)?;
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

    /// A check run's annotations, in the order it was given them.
    pub fn list_check_run_annotations(&self, check_run_id: i64) -> Result<Vec<Annotation>> {
        self.with_conn(|conn| queries::list_check_run_annotations(conn, check_run_id, false, None))
    }

    /// The `limit` annotations of a check run that come after its first
    /// `offset`, in the order it was given them or with the newest first.
    pub fn page_check_run_annotations(
        &self,
        check_run_id: i64,
        newest_first: bool,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Annotation>> {
        self.with_conn(|conn| {
            queries::list_check_run_annotations(
                conn,
                check_run_id,
                newest_first,
                Some((limit, offset)),
            )
        })
    }

    /// Give a job the annotations its runner makes of it, after those it
    /// has. Only a job a runner is on takes them.
    pub fn annotate_workflow_job(
        &self,
        job_id: i64,
        annotations: &[Annotation],
        by: Attribution,
    ) -> Result<()> {
        self.write(by, |w| {
            let job = queries::check_run_by_id(w.tx, job_id)?
                .filter(|run| run.job.is_some())
                .ok_or(WorldError::NoSuchWorkflowJob(job_id))?;
            if job.status != CheckStatus::InProgress {
                return Err(WorldError::JobNotInProgress(job_id));
            }
            annotate(w, job, annotations)?;
            Ok(())
        })
    }
}

/// Store `annotations` after those the check run has, and say so: as the
/// write's operation when nothing came before it, and otherwise as a
/// consequence. `run` is the row as it was read before this, and the answer
/// is the row with these counted.
pub(crate) fn annotate(
    w: &mut crate::log::Write<'_>,
    run: CheckRun,
    annotations: &[Annotation],
) -> Result<CheckRun> {
    if annotations.is_empty() {
        return Ok(run);
    }
    let tx = w.tx;
    let check_run_id = run.id;
    for a in annotations {
        tx.execute(
            "INSERT INTO check_run_annotations (check_run_id, path, start_line, end_line,
                                                start_column, end_column, level, message,
                                                title, raw_details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                check_run_id,
                a.path,
                a.start_line,
                a.end_line,
                a.start_column,
                a.end_column,
                a.level,
                a.message,
                a.title,
                a.raw_details
            ],
        )?;
    }
    let event = EventBody::CheckRunAnnotated(events::CheckRunAnnotated {
        subject: objects::check_run_subject(tx, check_run_id)?,
        annotations: annotations.to_vec(),
    });
    match w.has_operation() {
        true => w.emit(event),
        false => w.operation(event),
    }
    Ok(CheckRun {
        annotations_count: run.annotations_count + annotations.len() as i64,
        ..run
    })
}
