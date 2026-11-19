//! Check suites: the check runs one creator made on one commit, and what
//! they come to.

use rusqlite::{OptionalExtension, params};

use crate::events::{self, EventBody};
use crate::types::*;
use crate::{RefChange, RefMove, Result, World, log, objects, queries};

/// The id a ref that does not exist has, as a suite records it.
const NO_COMMIT: &str = "0000000000000000000000000000000000000000";

/// What a suite's check runs come to. Only the newest run of each name
/// counts. A suite is in progress while any run is, queued while any run is
/// queued or it has none, and complete otherwise, with the first of these
/// conclusions any run holds: action required, timed out, failure,
/// cancelled, success, neutral, stale, skipped.
pub(crate) fn rollup(runs: &[CheckRun]) -> (CheckStatus, Option<CheckConclusion>) {
    let newest: Vec<&CheckRun> = runs
        .iter()
        .filter(|run| {
            !runs
                .iter()
                .any(|other| other.name == run.name && other.id > run.id)
        })
        .collect();
    let any = |status| newest.iter().any(|run| run.status == status);
    if any(CheckStatus::InProgress) {
        return (CheckStatus::InProgress, None);
    }
    if newest.is_empty() || any(CheckStatus::Queued) {
        return (CheckStatus::Queued, None);
    }
    const ORDER: [CheckConclusion; 8] = [
        CheckConclusion::ActionRequired,
        CheckConclusion::TimedOut,
        CheckConclusion::Failure,
        CheckConclusion::Cancelled,
        CheckConclusion::Success,
        CheckConclusion::Neutral,
        CheckConclusion::Stale,
        CheckConclusion::Skipped,
    ];
    let conclusion = ORDER
        .into_iter()
        .find(|c| newest.iter().any(|run| run.conclusion == Some(*c)));
    (CheckStatus::Completed, conclusion)
}

/// The creator's suite on a commit, made if there is none.
pub(crate) fn suite_for(
    w: &mut log::Write<'_>,
    repo_id: i64,
    head_sha: &str,
    creator_id: i64,
) -> Result<i64> {
    let found =
        w.tx.query_row(
            "SELECT id FROM check_suites
             WHERE repo_id = ?1 AND head_sha = ?2 AND creator_id = ?3 ORDER BY id LIMIT 1",
            params![repo_id, head_sha, creator_id],
            |r| r.get(0),
        )
        .optional()?;
    match found {
        Some(id) => Ok(id),
        None => create(w, repo_id, head_sha, creator_id, None),
    }
}

/// The branch, what it pointed at, and who pushed, for a suite a push made.
struct Pushed<'a> {
    branch: &'a str,
    before: &'a str,
    pusher_id: i64,
}

fn create(
    w: &mut log::Write<'_>,
    repo_id: i64,
    head_sha: &str,
    creator_id: i64,
    pushed: Option<Pushed<'_>>,
) -> Result<i64> {
    let clock = queries::clock(w.tx)?;
    let id: i64 = w.tx.query_row(
        "INSERT INTO check_suites (repo_id, head_sha, head_branch, before_sha, creator_id,
                                   pusher_id, status, created_clock_unix, updated_clock_unix)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?7)
         RETURNING id",
        params![
            repo_id,
            head_sha,
            pushed.as_ref().map(|p| p.branch),
            pushed.as_ref().map(|p| p.before),
            creator_id,
            pushed.as_ref().map(|p| p.pusher_id),
            clock
        ],
        |r| r.get(0),
    )?;
    w.emit(EventBody::CheckSuiteCreated(events::CheckSuiteCreated {
        subject: objects::check_suite_subject(w.tx, id)?,
        object: objects::check_suite_object(w.tx, id)?,
    }));
    Ok(id)
}

/// Bring a suite's status and conclusion to what its check runs come to,
/// after a write to one of them.
pub(crate) fn settle(w: &mut log::Write<'_>, suite_id: i64) -> Result<()> {
    let before = objects::check_suite_object(w.tx, suite_id)?;
    let (status, conclusion) = rollup(&queries::list_check_runs_for_suite(w.tx, suite_id)?);
    if (before.status, before.conclusion) == (status, conclusion) {
        return Ok(());
    }
    w.tx.execute(
        "UPDATE check_suites
         SET status = ?2, conclusion = ?3,
             updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
         WHERE id = ?1",
        params![
            suite_id,
            status.as_str(),
            conclusion.map(CheckConclusion::as_str)
        ],
    )?;
    let after = objects::check_suite_object(w.tx, suite_id)?;
    w.emit(EventBody::CheckSuiteUpdated(events::CheckSuiteChanged {
        subject: objects::check_suite_subject(w.tx, suite_id)?,
        changes: events::CheckSuiteChanges::between(&before, &after),
    }));
    Ok(())
}

/// Make a suite on each commit a push brought to a branch's tip, for each
/// app installed on the repository that may write checks and has none
/// there.
pub(crate) fn open_at_push(
    w: &mut log::Write<'_>,
    repo_id: i64,
    moves: &[RefMove<'_>],
) -> Result<()> {
    let tips: Vec<(&str, &RefMove<'_>)> = moves
        .iter()
        .filter(|m| m.change != RefChange::Deleted)
        .filter_map(|m| Some((m.name.strip_prefix("refs/heads/")?, m)))
        .collect();
    if tips.is_empty() {
        return Ok(());
    }
    let installed: Vec<(i64, String)> = objects::list_by(
        w.tx,
        "SELECT a.bot_id, i.permissions FROM app_installations i JOIN apps a ON a.id = i.app_id
         WHERE i.suspended_clock_unix IS NULL
           AND i.account_id = (SELECT owner_id FROM repos WHERE id = ?1)
           AND (i.repository_selection = 'all' OR EXISTS
                (SELECT 1 FROM app_installation_repos r
                 WHERE r.installation_id = i.id AND r.repo_id = ?1))
         ORDER BY i.id",
        repo_id,
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let pusher_id = w.by.actor_id;
    for (bot_id, permissions) in installed {
        let permissions: Permissions = serde_json::from_str(&permissions).unwrap_or_default();
        if !matches!(
            permissions.get("checks"),
            Some(AccessLevel::Write | AccessLevel::Admin)
        ) {
            continue;
        }
        for (branch, m) in &tips {
            let has: bool = w.tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM check_suites
                                WHERE repo_id = ?1 AND head_sha = ?2 AND creator_id = ?3)",
                params![repo_id, m.new, bot_id],
                |r| r.get(0),
            )?;
            if has {
                continue;
            }
            let before = match m.change {
                RefChange::Created => NO_COMMIT,
                _ => m.old,
            };
            let pushed = Pushed {
                branch,
                before,
                pusher_id,
            };
            create(w, repo_id, m.new, bot_id, Some(pushed))?;
        }
    }
    Ok(())
}

impl World {
    pub fn check_suite_by_id(&self, id: i64) -> Result<Option<CheckSuite>> {
        self.with_conn(|conn| queries::check_suite_by_id(conn, id))
    }

    /// The suites on one commit, oldest first.
    pub fn list_check_suites_for_sha(
        &self,
        repo_id: i64,
        head_sha: &str,
    ) -> Result<Vec<CheckSuite>> {
        self.with_conn(|conn| queries::list_check_suites_for_sha(conn, repo_id, head_sha))
    }

    /// A suite's check runs, oldest first.
    pub fn list_check_runs_for_suite(&self, suite_id: i64) -> Result<Vec<CheckRun>> {
        self.with_conn(|conn| queries::list_check_runs_for_suite(conn, suite_id))
    }
}
