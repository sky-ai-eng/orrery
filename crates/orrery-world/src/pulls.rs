//! Pull requests, reviews, and issue comments. Numbers share one per-repo
//! sequence (GitHub numbers issues and PRs together; when issues land they
//! join this sequence). Every state transition is an action-log entry —
//! the log is the benchmark substrate, so nothing changes state silently.

use rusqlite::{Connection, OptionalExtension, params};

use crate::queries;
use crate::types::*;
use crate::{Result, World, WorldError, log_locked};

pub struct NewPull<'a> {
    pub repo_id: i64,
    pub author_id: i64,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub head_ref: &'a str,
    pub head_sha: &'a str,
    pub base_ref: &'a str,
    pub base_sha: &'a str,
    pub draft: bool,
}

impl World {
    /// Open a pull request. At most one *open* pull per (repo, head, base) —
    /// GitHub's rule; a closed one does not block reopening the conversation
    /// as a fresh PR.
    pub fn create_pull(&self, new: NewPull<'_>) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let clock = queries::clock(&tx)?;
            let dup: Option<i64> = tx
                .query_row(
                    "SELECT number FROM pulls
                     WHERE repo_id = ?1 AND head_ref = ?2 AND base_ref = ?3 AND state = 'open'",
                    params![new.repo_id, new.head_ref, new.base_ref],
                    |r| r.get(0),
                )
                .optional()?;
            if dup.is_some() {
                return Err(WorldError::PullExists(new.head_ref.to_string()));
            }
            let number: i64 = tx.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM pulls WHERE repo_id = ?1",
                params![new.repo_id],
                |r| r.get(0),
            )?;
            let id: i64 = tx.query_row(
                "INSERT INTO pulls (repo_id, number, title, body, author_id, state, draft,
                                    head_ref, head_sha, base_ref, base_sha,
                                    created_clock_unix, updated_clock_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                 RETURNING id",
                params![
                    new.repo_id,
                    number,
                    new.title,
                    new.body,
                    new.author_id,
                    new.draft,
                    new.head_ref,
                    new.head_sha,
                    new.base_ref,
                    new.base_sha,
                    clock
                ],
                |r| r.get(0),
            )?;
            let pull = queries::require_pull(&tx, id)?;
            log_locked(
                &tx,
                new.author_id,
                "pull.open",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, new.repo_id)?,
                    "number": pull.number,
                    "title": pull.title,
                    "head": pull.head_ref,
                    "base": pull.base_ref,
                    "draft": pull.draft,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    pub fn pull_by_number(&self, repo_id: i64, number: i64) -> Result<Option<Pull>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "{} WHERE p.repo_id = ?1 AND p.number = ?2",
                        queries::PULL_SELECT
                    ),
                    params![repo_id, number],
                    queries::row_to_pull,
                )
                .optional()?)
        })
    }

    /// Edit title/body/base. `body` distinguishes absent (keep) from explicit
    /// clear; `base` carries the retargeted ref together with its freshly
    /// pinned sha, resolved by the caller who owns the git side.
    pub fn edit_pull(
        &self,
        pull_id: i64,
        actor_id: i64,
        title: Option<&str>,
        body: Option<Option<&str>>,
        base: Option<(&str, &str)>,
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let mut fields = Vec::new();
            if let Some(t) = title
                && t != before.title
            {
                tx.execute(
                    "UPDATE pulls SET title = ?1 WHERE id = ?2",
                    params![t, pull_id],
                )?;
                fields.push("title");
            }
            if let Some(b) = body
                && b != before.body.as_deref()
            {
                tx.execute(
                    "UPDATE pulls SET body = ?1 WHERE id = ?2",
                    params![b, pull_id],
                )?;
                fields.push("body");
            }
            if let Some((base_ref, base_sha)) = base
                && base_ref != before.base_ref
            {
                tx.execute(
                    "UPDATE pulls SET base_ref = ?1, base_sha = ?2 WHERE id = ?3",
                    params![base_ref, base_sha, pull_id],
                )?;
                fields.push("base");
            }
            if fields.is_empty() {
                return Ok(before);
            }
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            log_locked(
                &tx,
                actor_id,
                "pull.edit",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "fields": fields,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    pub fn close_pull(&self, pull_id: i64, actor_id: i64) -> Result<Pull> {
        self.transition(pull_id, actor_id, "pull.close", |p| {
            if p.state != PullState::Open {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is not open",
                    p.number
                )));
            }
            Ok("UPDATE pulls SET state = 'closed',
                 closed_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
                 WHERE id = ?1")
        })
    }

    pub fn reopen_pull(&self, pull_id: i64, actor_id: i64) -> Result<Pull> {
        self.transition(pull_id, actor_id, "pull.reopen", |p| {
            if p.merged {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} has been merged",
                    p.number
                )));
            }
            if p.state != PullState::Closed {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is not closed",
                    p.number
                )));
            }
            Ok("UPDATE pulls SET state = 'open', closed_clock_unix = NULL WHERE id = ?1")
        })
    }

    /// Record a merge the git side already performed: state bookkeeping only —
    /// the ref move and its `ref.update` entry are the caller's, since the
    /// kernel never touches git.
    pub fn record_merge(
        &self,
        pull_id: i64,
        actor_id: i64,
        merge_commit_sha: &str,
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            if before.state != PullState::Open || before.merged {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is not open",
                    before.number
                )));
            }
            tx.execute(
                "UPDATE pulls SET state = 'closed', merged = 1, merge_commit_sha = ?1,
                 merged_by_id = ?2,
                 closed_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
                 WHERE id = ?3",
                params![merge_commit_sha, actor_id, pull_id],
            )?;
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            log_locked(
                &tx,
                actor_id,
                "pull.merge",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "merge_commit_sha": merge_commit_sha,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// Re-pin the head sha of every open pull whose head branch just moved —
    /// GitHub's "synchronize". Returns the affected pull numbers.
    pub fn sync_pull_heads(&self, repo_id: i64, branch: &str, new_sha: &str) -> Result<Vec<i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "UPDATE pulls SET head_sha = ?1,
                 updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
                 WHERE repo_id = ?2 AND head_ref = ?3 AND state = 'open' AND head_sha <> ?1
                 RETURNING number",
            )?;
            let numbers = stmt
                .query_map(params![new_sha, repo_id, branch], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(numbers)
        })
    }

    /// Submit a review, pinned to the pull's current head. Bumps the pull's
    /// `updated` clock, as activity does on GitHub.
    pub fn create_review(
        &self,
        pull_id: i64,
        author_id: i64,
        state: ReviewState,
        body: Option<&str>,
        commit_sha: Option<&str>,
    ) -> Result<Review> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let pull = queries::require_pull(&tx, pull_id)?;
            let clock = queries::clock(&tx)?;
            let sha = commit_sha.unwrap_or(&pull.head_sha);
            let id: i64 = tx.query_row(
                "INSERT INTO reviews (pull_id, author_id, state, body, commit_sha, submitted_clock_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 RETURNING id",
                params![pull_id, author_id, state.as_str(), body, sha, clock],
                |r| r.get(0),
            )?;
            let review = tx.query_row(
                &format!("{} WHERE v.id = ?1", queries::REVIEW_SELECT),
                params![id],
                queries::row_to_review,
            )?;
            touch_pull(&tx, pull_id)?;
            log_locked(
                &tx,
                author_id,
                "review.submit",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "state": state.as_str(),
                }),
            )?;
            tx.commit()?;
            Ok(review)
        })
    }

    pub fn create_issue_comment(
        &self,
        pull_id: i64,
        author_id: i64,
        body: &str,
    ) -> Result<IssueComment> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let pull = queries::require_pull(&tx, pull_id)?;
            let clock = queries::clock(&tx)?;
            let id: i64 = tx.query_row(
                "INSERT INTO issue_comments (pull_id, author_id, body,
                                             created_clock_unix, updated_clock_unix)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 RETURNING id",
                params![pull_id, author_id, body, clock],
                |r| r.get(0),
            )?;
            let comment = tx.query_row(
                &format!("{} WHERE c.id = ?1", queries::COMMENT_SELECT),
                params![id],
                queries::row_to_comment,
            )?;
            touch_pull(&tx, pull_id)?;
            log_locked(
                &tx,
                author_id,
                "comment.create",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                }),
            )?;
            tx.commit()?;
            Ok(comment)
        })
    }

    /// Shared shape of close/reopen: guard the current state, run one UPDATE,
    /// bump `updated`, log, return the stored row.
    fn transition(
        &self,
        pull_id: i64,
        actor_id: i64,
        action: &str,
        guard: impl FnOnce(&Pull) -> Result<&'static str>,
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let sql = guard(&before)?;
            tx.execute(sql, params![pull_id])?;
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            log_locked(
                &tx,
                actor_id,
                action,
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }
}

fn touch_pull(conn: &Connection, pull_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE pulls SET updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
         WHERE id = ?1",
        params![pull_id],
    )?;
    Ok(())
}

pub(crate) fn repo_full_name(conn: &Connection, repo_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT a.login || '/' || r.name FROM repos r JOIN actors a ON a.id = r.owner_id
         WHERE r.id = ?1",
        params![repo_id],
        |r| r.get(0),
    )?)
}
