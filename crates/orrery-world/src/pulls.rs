//! Pull requests, reviews, and issue comments. Numbers share one per-repo
//! sequence (GitHub numbers issues and PRs together; when issues land they
//! join this sequence). Every state transition is an action-log entry —
//! the log is the benchmark substrate, so nothing changes state silently.

use rusqlite::{Connection, OptionalExtension, params};

use crate::queries;
use crate::types::*;
use crate::{Result, World, WorldError};

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
            self.log_locked(
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

    pub fn pull_by_id(&self, id: i64) -> Result<Option<Pull>> {
        self.with_conn(|conn| queries::pull_by_id(conn, id))
    }

    pub fn review_by_id(&self, id: i64) -> Result<Option<Review>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("{} WHERE v.id = ?1", queries::REVIEW_SELECT),
                    params![id],
                    queries::row_to_review,
                )
                .optional()?)
        })
    }

    /// Flip the draft flag — GitHub's ready-for-review / convert-to-draft
    /// pair. Refused on a non-open pull, and refused when the pull is already
    /// in the target state: GitHub errors there rather than succeeding as a
    /// no-op, and a silent no-op would also leave a phantom log entry.
    /// Ready-for-review also lands on the pull's timeline, where GitHub reports
    /// when it happened.
    pub fn set_pull_draft(&self, pull_id: i64, actor_id: i64, draft: bool) -> Result<Pull> {
        let (action, timeline) = if draft {
            ("pull.convert_to_draft", None)
        } else {
            ("pull.ready_for_review", Some(TimelineKind::ReadyForReview))
        };
        self.transition(pull_id, actor_id, action, timeline, |p| {
            if p.state != PullState::Open {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is not open",
                    p.number
                )));
            }
            if p.draft == draft {
                return Err(WorldError::InvalidTransition(if draft {
                    format!("pull #{} is already a draft", p.number)
                } else {
                    format!("pull #{} is not a draft", p.number)
                }));
            }
            Ok(if draft {
                "UPDATE pulls SET draft = 1 WHERE id = ?1"
            } else {
                "UPDATE pulls SET draft = 0 WHERE id = ?1"
            })
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
            self.log_locked(
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

    /// Add labels to a pull, GitHub's `POST …/issues/{n}/labels`: names it
    /// already carries (compared without case) are skipped, so only a label
    /// that is new to the pull is logged and lands on its timeline. Every name
    /// is validated before anything is written.
    pub fn add_pull_labels(&self, pull_id: i64, actor_id: i64, names: &[&str]) -> Result<Pull> {
        if let Some(bad) = names.iter().find(|n| !valid_label(n)) {
            return Err(WorldError::InvalidLabel(bad.to_string()));
        }
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let mut added: Vec<&str> = Vec::new();
            for name in names {
                let inserted = tx.execute(
                    "INSERT INTO pull_labels (pull_id, name) VALUES (?1, ?2)
                     ON CONFLICT DO NOTHING",
                    params![pull_id, name],
                )?;
                if inserted == 1 {
                    record_timeline(
                        &tx,
                        pull_id,
                        actor_id,
                        &TimelineKind::Labeled(name.to_string()),
                    )?;
                    added.push(name);
                }
            }
            if added.is_empty() {
                return Ok(before);
            }
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
                &tx,
                actor_id,
                "pull.label",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "added": added,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// Remove one label, GitHub's `DELETE …/issues/{n}/labels/{name}`; the
    /// name matches without case. A label the pull does not carry is refused.
    pub fn remove_pull_label(&self, pull_id: i64, actor_id: i64, name: &str) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            queries::require_pull(&tx, pull_id)?;
            let removed: Option<String> = tx
                .query_row(
                    "DELETE FROM pull_labels WHERE pull_id = ?1 AND name = ?2 RETURNING name",
                    params![pull_id, name],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(removed) = removed else {
                return Err(WorldError::NoSuchLabel(name.to_string()));
            };
            record_timeline(
                &tx,
                pull_id,
                actor_id,
                &TimelineKind::Unlabeled(removed.clone()),
            )?;
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
                &tx,
                actor_id,
                "pull.unlabel",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "removed": removed,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// Request reviews, GitHub's `POST …/pulls/{n}/requested_reviewers`.
    /// Every name is checked before anything is written: a user must be a
    /// person other than the pull's author, and a team must belong to the
    /// organization that owns the repository. A reviewer already pending is
    /// skipped, so only a new request is logged and lands on the timeline.
    ///
    /// The returned pull is read back inside the write's transaction, as every
    /// pull write's is: its labels and requests are aggregates over other
    /// tables, which a RETURNING clause on `pulls` cannot project.
    pub fn request_reviewers(
        &self,
        pull_id: i64,
        actor_id: i64,
        users: &[&str],
        teams: &[&str],
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let user_ids = users
                .iter()
                .map(|login| {
                    let id = reviewer_user_id(&tx, login)?;
                    if login.eq_ignore_ascii_case(&before.author_login) {
                        return Err(WorldError::ReviewerIsAuthor);
                    }
                    Ok(id)
                })
                .collect::<Result<Vec<i64>>>()?;
            let team_ids = teams
                .iter()
                .map(|slug| reviewer_team_id(&tx, before.repo_id, slug))
                .collect::<Result<Vec<i64>>>()?;
            let mut added: Vec<RequestedReviewer> = Vec::new();
            for id in &user_ids {
                if insert_request(&tx, pull_id, Some(*id), None)? {
                    added.push(RequestedReviewer::User(login_of(&tx, *id)?));
                }
            }
            for id in &team_ids {
                if insert_request(&tx, pull_id, None, Some(*id))? {
                    added.push(RequestedReviewer::Team {
                        id: *id,
                        slug: slug_of(&tx, *id)?,
                    });
                }
            }
            if added.is_empty() {
                return Ok(before);
            }
            for rv in &added {
                record_timeline(
                    &tx,
                    pull_id,
                    actor_id,
                    &TimelineKind::ReviewRequested(rv.clone()),
                )?;
            }
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
                &tx,
                actor_id,
                "pull.request_reviewers",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "requested": added,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// Withdraw review requests, GitHub's `DELETE
    /// …/pulls/{n}/requested_reviewers`. A name with no pending request is
    /// skipped, as GitHub skips it; each request withdrawn is logged and lands
    /// on the timeline. The returned pull is read back inside the
    /// transaction, for the reason `request_reviewers` gives.
    pub fn remove_review_requests(
        &self,
        pull_id: i64,
        actor_id: i64,
        users: &[&str],
        teams: &[&str],
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let mut removed: Vec<RequestedReviewer> = Vec::new();
            for login in users {
                let gone: Option<String> = tx
                    .query_row(
                        "DELETE FROM review_requests
                         WHERE pull_id = ?1
                           AND reviewer_id = (SELECT id FROM actors WHERE login = ?2)
                         RETURNING (SELECT login FROM actors WHERE id = reviewer_id)",
                        params![pull_id, login],
                        |r| r.get(0),
                    )
                    .optional()?;
                removed.extend(gone.map(RequestedReviewer::User));
            }
            for slug in teams {
                let gone: Option<(i64, String)> = tx
                    .query_row(
                        "DELETE FROM review_requests
                         WHERE pull_id = ?1
                           AND team_id = (SELECT t.id FROM teams t JOIN repos r ON r.owner_id = t.org_id
                                          WHERE r.id = ?2 AND t.slug = ?3)
                         RETURNING team_id, (SELECT slug FROM teams WHERE id = team_id)",
                        params![pull_id, before.repo_id, slug],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                removed.extend(gone.map(|(id, slug)| RequestedReviewer::Team { id, slug }));
            }
            if removed.is_empty() {
                return Ok(before);
            }
            for rv in &removed {
                record_timeline(
                    &tx,
                    pull_id,
                    actor_id,
                    &TimelineKind::ReviewRequestRemoved(rv.clone()),
                )?;
            }
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
                &tx,
                actor_id,
                "pull.remove_review_requests",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "removed": removed,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// A pull's timeline, oldest first.
    pub fn list_pull_timeline(&self, pull_id: i64) -> Result<Vec<TimelineItem>> {
        self.with_conn(|conn| queries::list_timeline_for_pull(conn, pull_id))
    }

    pub fn close_pull(&self, pull_id: i64, actor_id: i64) -> Result<Pull> {
        let timeline = Some(TimelineKind::Closed);
        self.transition(pull_id, actor_id, "pull.close", timeline, |p| {
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
        self.transition(pull_id, actor_id, "pull.reopen", None, |p| {
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
    /// kernel never touches git. The merge closes the pull, so it lands on the
    /// timeline as a close by the merger.
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
            record_timeline(&tx, pull_id, actor_id, &TimelineKind::Closed)?;
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
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
        self.with_conn(|conn| sync_heads(conn, repo_id, branch, new_sha))
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
            // GitHub: once a requested reviewer submits a review, they are no
            // longer a requested reviewer. Their team's request stays. The
            // removal leaves no log or timeline entry of its own: the
            // `review.submit` entry below implies it, and GitHub's timeline
            // shows only the review.
            tx.execute(
                "DELETE FROM review_requests WHERE pull_id = ?1 AND reviewer_id = ?2",
                params![pull_id, author_id],
            )?;
            touch_pull(&tx, pull_id)?;
            self.log_locked(
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
            self.log_locked(
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

    /// A repo's pulls, optionally filtered by state, in GitHub's default
    /// listing order (created descending, id tiebreaker).
    pub fn list_pulls_for_repo(&self, repo_id: i64, state: Option<PullState>) -> Result<Vec<Pull>> {
        self.with_conn(|conn| {
            queries::list_pulls_for_repo(conn, repo_id, state.map(PullState::as_str))
        })
    }

    pub fn list_reviews_for_pull(&self, pull_id: i64) -> Result<Vec<Review>> {
        self.with_conn(|conn| queries::list_reviews_for_pull(conn, pull_id))
    }

    pub fn list_issue_comments_for_pull(&self, pull_id: i64) -> Result<Vec<IssueComment>> {
        self.with_conn(|conn| queries::list_comments_for_pull(conn, pull_id))
    }

    /// Shared shape of close/reopen/draft: guard the current state, run one
    /// UPDATE, record the timeline item if there is one, bump `updated`, log,
    /// return the stored row.
    fn transition(
        &self,
        pull_id: i64,
        actor_id: i64,
        action: &str,
        timeline: Option<TimelineKind>,
        guard: impl FnOnce(&Pull) -> Result<&'static str>,
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let sql = guard(&before)?;
            tx.execute(sql, params![pull_id])?;
            if let Some(kind) = &timeline {
                record_timeline(&tx, pull_id, actor_id, kind)?;
            }
            touch_pull(&tx, pull_id)?;
            let pull = queries::require_pull(&tx, pull_id)?;
            self.log_locked(
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

/// GitHub's limit on a label name.
fn valid_label(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= 50
}

fn record_timeline(
    conn: &Connection,
    pull_id: i64,
    actor_id: i64,
    kind: &TimelineKind,
) -> Result<()> {
    let (kind, label, reviewer) = match kind {
        TimelineKind::Labeled(name) => ("labeled", Some(name.as_str()), None),
        TimelineKind::Unlabeled(name) => ("unlabeled", Some(name.as_str()), None),
        TimelineKind::ReadyForReview => ("ready_for_review", None, None),
        TimelineKind::ReviewRequested(rv) => ("review_requested", None, Some(rv)),
        TimelineKind::ReviewRequestRemoved(rv) => ("review_request_removed", None, Some(rv)),
        TimelineKind::Closed => ("closed", None, None),
    };
    let (reviewer_login, team_id, team_slug) = match reviewer {
        Some(RequestedReviewer::User(login)) => (Some(login.as_str()), None, None),
        Some(RequestedReviewer::Team { id, slug }) => (None, Some(*id), Some(slug.as_str())),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO pull_timeline (pull_id, kind, label, reviewer_id, team_id, team_slug,
                                    actor_id, clock_unix)
         VALUES (?1, ?2, ?3, (SELECT id FROM actors WHERE login = ?4), ?5, ?6, ?7,
                 (SELECT clock_unix FROM world WHERE id = 1))",
        params![
            pull_id,
            kind,
            label,
            reviewer_login,
            team_id,
            team_slug,
            actor_id
        ],
    )?;
    Ok(())
}

/// A user who can be asked for a review: a person, not a bot or an
/// organization.
fn reviewer_user_id(conn: &Connection, login: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM actors WHERE login = ?1 AND kind = 'user'",
        params![login],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| WorldError::InvalidReviewer(login.to_string()))
}

/// A team of the organization that owns the pull's repository; a repository
/// a user owns has no teams to ask.
fn reviewer_team_id(conn: &Connection, repo_id: i64, slug: &str) -> Result<i64> {
    conn.query_row(
        "SELECT t.id FROM teams t JOIN repos r ON r.owner_id = t.org_id
         WHERE r.id = ?1 AND t.slug = ?2",
        params![repo_id, slug],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| WorldError::InvalidReviewer(slug.to_string()))
}

fn insert_request(
    conn: &Connection,
    pull_id: i64,
    reviewer_id: Option<i64>,
    team_id: Option<i64>,
) -> Result<bool> {
    Ok(conn.execute(
        "INSERT INTO review_requests (pull_id, reviewer_id, team_id) VALUES (?1, ?2, ?3)
         ON CONFLICT DO NOTHING",
        params![pull_id, reviewer_id, team_id],
    )? == 1)
}

fn login_of(conn: &Connection, actor_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT login FROM actors WHERE id = ?1",
        params![actor_id],
        |r| r.get(0),
    )?)
}

fn slug_of(conn: &Connection, team_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT slug FROM teams WHERE id = ?1",
        params![team_id],
        |r| r.get(0),
    )?)
}

fn touch_pull(conn: &Connection, pull_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE pulls SET updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
         WHERE id = ?1",
        params![pull_id],
    )?;
    Ok(())
}

/// [`World::sync_pull_heads`] through an already-held connection or an open
/// transaction.
pub(crate) fn sync_heads(
    conn: &Connection,
    repo_id: i64,
    branch: &str,
    new_sha: &str,
) -> Result<Vec<i64>> {
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
}

pub(crate) fn repo_full_name(conn: &Connection, repo_id: i64) -> Result<String> {
    Ok(conn.query_row(
        "SELECT a.login || '/' || r.name FROM repos r JOIN actors a ON a.id = r.owner_id
         WHERE r.id = ?1",
        params![repo_id],
        |r| r.get(0),
    )?)
}
