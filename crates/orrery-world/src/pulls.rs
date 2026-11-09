//! Pull requests, reviews, and issue comments. Numbers share one per-repo
//! sequence (GitHub numbers issues and PRs together; when issues land they
//! join this sequence). Every state transition is an action-log entry —
//! the log is the benchmark substrate, so nothing changes state silently.

use rusqlite::{Connection, OptionalExtension, params};

use crate::queries;
use crate::types::*;
use crate::{RefChange, RefMove, Result, World, WorldError};

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
    /// GitHub's rule; a closed one does not block opening a fresh PR.
    pub fn create_pull(&self, new: NewPull<'_>) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let clock = queries::clock(&tx)?;
            if open_pull_between(&tx, new.repo_id, new.head_ref, new.base_ref)?.is_some() {
                return Err(WorldError::PullExists(new.head_ref.to_string()));
            }
            let number: i64 = tx.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM pulls WHERE repo_id = ?1",
                params![new.repo_id],
                |r| r.get(0),
            )?;
            let pull = tx.query_row(
                &format!(
                    "INSERT INTO pulls (repo_id, number, title, body, author_id, state, draft,
                                        head_ref, head_sha, base_ref, base_sha,
                                        created_clock_unix, updated_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7, ?8, ?9, ?10, ?11, ?11)
                     RETURNING {}",
                    queries::PULL_COLUMNS
                ),
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
                queries::row_to_pull,
            )?;
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
                        "SELECT {} FROM pulls WHERE repo_id = ?1 AND number = ?2",
                        queries::PULL_COLUMNS
                    ),
                    params![repo_id, number],
                    queries::row_to_pull,
                )
                .optional()?)
        })
    }

    /// The number of the open pull from `head_ref` into `base_ref`, if there
    /// is one: the pull that refuses a create or a reopen on the same pair.
    pub fn open_pull_between(
        &self,
        repo_id: i64,
        head_ref: &str,
        base_ref: &str,
    ) -> Result<Option<i64>> {
        self.with_conn(|conn| open_pull_between(conn, repo_id, head_ref, base_ref))
    }

    pub fn pull_by_id(&self, id: i64) -> Result<Option<Pull>> {
        self.with_conn(|conn| queries::pull_by_id(conn, id))
    }

    pub fn review_by_id(&self, id: i64) -> Result<Option<Review>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {} FROM reviews WHERE id = ?1",
                        queries::REVIEW_COLUMNS
                    ),
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
        self.transition(pull_id, actor_id, action, timeline, None, |_, p| {
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
    /// pinned sha, resolved by the caller who owns the git side. A `base`
    /// that would give an open pull the head and base of another open pull
    /// refuses the whole edit, as `create_pull` and `reopen_pull` refuse.
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
            if let Some((base_ref, _)) = base
                && base_ref != before.base_ref
                && before.state == PullState::Open
                && open_pull_between(&tx, before.repo_id, &before.head_ref, base_ref)?.is_some()
            {
                return Err(WorldError::PullExists(before.head_ref.clone()));
            }
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
                    "UPDATE pulls SET base_ref = ?1, base_sha = ?2, base_moved = 0 WHERE id = ?3",
                    params![base_ref, base_sha, pull_id],
                )?;
                fields.push("base");
            }
            if fields.is_empty() {
                return Ok(before);
            }
            let pull = touch_pull(&tx, pull_id)?;
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
            let pull = touch_pull(&tx, pull_id)?;
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
            let pull = touch_pull(&tx, pull_id)?;
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
            let pull = touch_pull(&tx, pull_id)?;
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
    /// on the timeline.
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
            let pull = touch_pull(&tx, pull_id)?;
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
        self.transition(pull_id, actor_id, "pull.close", timeline, None, |_, p| {
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

    /// Reopen a closed pull with `head_sha` as its head: the reopen and the
    /// head move commit together. The caller resolves the head from its head
    /// branch, since the kernel never touches git. Refused while another pull
    /// is open from the same head into the same base, as `create_pull` is.
    pub fn reopen_pull(&self, pull_id: i64, actor_id: i64, head_sha: &str) -> Result<Pull> {
        let head = Some(head_sha);
        self.transition(pull_id, actor_id, "pull.reopen", None, head, |conn, p| {
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
            if open_pull_between(conn, p.repo_id, &p.head_ref, &p.base_ref)?.is_some() {
                return Err(WorldError::PullExists(p.head_ref.clone()));
            }
            Ok("UPDATE pulls SET state = 'open', closed_clock_unix = NULL WHERE id = ?1")
        })
    }

    /// Record a merge the git side already performed. `base` is the move of
    /// the pull's base branch to the merge commit, recorded as a push of it
    /// would be: its `ref.update` entry, the `pushed_at` bump, and the
    /// re-pins of the pulls it names. Then the pull closes as merged, which
    /// lands on the timeline as a close by the merger, and its `pull.merge`
    /// entry follows. It is one transaction, so a reader that sees either
    /// entry sees all of the merge, and a record that fails appends nothing.
    /// The kernel never touches git: the caller has moved the base branch,
    /// and each named pull's ref, before it calls.
    ///
    /// A pull that is not open or is a draft, a `base` that is not the pull's
    /// base branch, or a named pull that is not open refuses the whole record.
    pub fn record_merge(&self, pull_id: i64, actor_id: i64, base: &RefMove<'_>) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            if before.state != PullState::Open || before.merged {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is not open",
                    before.number
                )));
            }
            if before.draft {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} is a draft",
                    before.number
                )));
            }
            if base.name != format!("refs/heads/{}", before.base_ref) {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{} merges into {}, not {}",
                    before.number, before.base_ref, base.name
                )));
            }
            if base.change != RefChange::Moved {
                return Err(WorldError::InvalidTransition(format!(
                    "pull #{}'s merge must move {}, not create or delete it",
                    before.number, base.name
                )));
            }
            self.record_moves(
                &tx,
                before.repo_id,
                actor_id,
                std::slice::from_ref(base),
                None,
            )?;
            tx.execute(
                "UPDATE pulls SET state = 'closed', merged = 1, merge_commit_sha = ?1,
                 merged_by_id = ?2,
                 closed_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
                 WHERE id = ?3",
                params![base.new, actor_id, pull_id],
            )?;
            record_timeline(&tx, pull_id, actor_id, &TimelineKind::Closed)?;
            let pull = touch_pull(&tx, pull_id)?;
            self.log_locked(
                &tx,
                actor_id,
                "pull.merge",
                &serde_json::json!({
                    "repo": repo_full_name(&tx, pull.repo_id)?,
                    "number": pull.number,
                    "merge_commit_sha": base.new,
                }),
            )?;
            tx.commit()?;
            Ok(pull)
        })
    }

    /// The open pulls whose head is `branch` and not already at `new_sha`, by
    /// number: the pulls a push or a merge that moves the branch to `new_sha`
    /// re-pins, GitHub's "synchronize".
    pub fn pulls_to_sync(&self, repo_id: i64, branch: &str, new_sha: &str) -> Result<Vec<i64>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT number FROM pulls
                 WHERE repo_id = ?1 AND head_ref = ?2 AND state = 'open' AND head_sha <> ?3
                 ORDER BY number",
            )?;
            let numbers = stmt
                .query_map(params![repo_id, branch, new_sha], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(numbers)
        })
    }

    /// What a push's creation or deletion of `branch` does to the pulls on
    /// it, inside the push's transaction, as [`World::record_push`] describes.
    /// The order is GitHub's: on a deleted head, every close comes before any
    /// deletion item, across pulls as well as within one.
    pub(crate) fn record_branch_change(
        &self,
        tx: &Connection,
        repo_id: i64,
        actor_id: i64,
        branch: &str,
        change: RefChange,
    ) -> Result<()> {
        let pulls_on = |side: &str, state: Option<&str>| -> Result<Vec<i64>> {
            let sql = format!(
                "SELECT id FROM pulls WHERE repo_id = ?1 AND {side}_ref = ?2
                 AND (?3 IS NULL OR state = ?3) ORDER BY number"
            );
            let ids = tx
                .prepare(&sql)?
                .query_map(params![repo_id, branch, state], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(ids)
        };
        match change {
            RefChange::Moved => {}
            RefChange::Deleted => {
                for id in pulls_on("head", Some("open"))? {
                    self.close_for_deleted_branch(tx, id, actor_id)?;
                }
                for id in pulls_on("head", None)? {
                    let deleted = TimelineKind::HeadRefDeleted(branch.to_string());
                    record_timeline(tx, id, actor_id, &deleted)?;
                    touch_pull(tx, id)?;
                }
                for id in pulls_on("base", Some("open"))? {
                    let deleted = TimelineKind::BaseRefDeleted(branch.to_string());
                    record_timeline(tx, id, actor_id, &deleted)?;
                    self.close_for_deleted_branch(tx, id, actor_id)?;
                }
            }
            RefChange::Created => {
                for id in pulls_on("head", Some("closed"))? {
                    record_timeline(tx, id, actor_id, &TimelineKind::HeadRefRestored)?;
                    touch_pull(tx, id)?;
                }
            }
        }
        Ok(())
    }

    /// Close an open pull because a push deleted its head or base, as
    /// [`World::close_pull`] closes one: the same state change, timeline
    /// item, and `pull.close` entry, by the pusher.
    fn close_for_deleted_branch(&self, tx: &Connection, pull_id: i64, actor_id: i64) -> Result<()> {
        let (repo_id, number): (i64, i64) = tx.query_row(
            "UPDATE pulls SET state = 'closed',
             closed_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
             WHERE id = ?1 AND state = 'open'
             RETURNING repo_id, number",
            params![pull_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        record_timeline(tx, pull_id, actor_id, &TimelineKind::Closed)?;
        touch_pull(tx, pull_id)?;
        self.log_locked(
            tx,
            actor_id,
            "pull.close",
            &serde_json::json!({
                "repo": repo_full_name(tx, repo_id)?,
                "number": number,
            }),
        )?;
        Ok(())
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
            let review = tx.query_row(
                &format!(
                    "INSERT INTO reviews (pull_id, author_id, state, body, commit_sha,
                                          submitted_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     RETURNING {}",
                    queries::REVIEW_COLUMNS
                ),
                params![pull_id, author_id, state.as_str(), body, sha, clock],
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
            let comment = tx.query_row(
                &format!(
                    "INSERT INTO issue_comments (pull_id, author_id, body,
                                                 created_clock_unix, updated_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?4)
                     RETURNING {}",
                    queries::COMMENT_COLUMNS
                ),
                params![pull_id, author_id, body, clock],
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

    /// Shared shape of close/reopen/draft: guard the current state, reading
    /// inside the transaction where the rule needs other rows, run one
    /// UPDATE, move the head to `head` if given, record the timeline item if
    /// there is one, bump `updated`, log, return the stored row.
    fn transition(
        &self,
        pull_id: i64,
        actor_id: i64,
        action: &str,
        timeline: Option<TimelineKind>,
        head: Option<&str>,
        guard: impl FnOnce(&Connection, &Pull) -> Result<&'static str>,
    ) -> Result<Pull> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let before = queries::require_pull(&tx, pull_id)?;
            let sql = guard(&tx, &before)?;
            tx.execute(sql, params![pull_id])?;
            if let Some(sha) = head {
                repin_open_pull(&tx, before.repo_id, before.number, sha)?;
            }
            if let Some(kind) = &timeline {
                record_timeline(&tx, pull_id, actor_id, kind)?;
            }
            let pull = touch_pull(&tx, pull_id)?;
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

fn open_pull_between(
    conn: &Connection,
    repo_id: i64,
    head_ref: &str,
    base_ref: &str,
) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT number FROM pulls
             WHERE repo_id = ?1 AND head_ref = ?2 AND base_ref = ?3 AND state = 'open'",
            params![repo_id, head_ref, base_ref],
            |r| r.get(0),
        )
        .optional()?)
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
    let (kind, label, ref_name, reviewer) = match kind {
        TimelineKind::Labeled(name) => ("labeled", Some(name.as_str()), None, None),
        TimelineKind::Unlabeled(name) => ("unlabeled", Some(name.as_str()), None, None),
        TimelineKind::ReadyForReview => ("ready_for_review", None, None, None),
        TimelineKind::ReviewRequested(rv) => ("review_requested", None, None, Some(rv)),
        TimelineKind::ReviewRequestRemoved(rv) => ("review_request_removed", None, None, Some(rv)),
        TimelineKind::Closed => ("closed", None, None, None),
        TimelineKind::HeadRefDeleted(name) => ("head_ref_deleted", None, Some(name.as_str()), None),
        TimelineKind::BaseRefDeleted(name) => ("base_ref_deleted", None, Some(name.as_str()), None),
        TimelineKind::HeadRefRestored => ("head_ref_restored", None, None, None),
    };
    let (reviewer_login, team_id, team_slug) = match reviewer {
        Some(RequestedReviewer::User(login)) => (Some(login.as_str()), None, None),
        Some(RequestedReviewer::Team { id, slug }) => (None, Some(*id), Some(slug.as_str())),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO pull_timeline (pull_id, kind, label, ref_name, reviewer_id, team_id,
                                    team_slug, actor_id, clock_unix)
         VALUES (?1, ?2, ?3, ?4, (SELECT id FROM actors WHERE login = ?5), ?6, ?7, ?8,
                 (SELECT clock_unix FROM world WHERE id = 1))",
        params![
            pull_id,
            kind,
            label,
            ref_name,
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

/// Move an open pull's head to `sha`. A pull that is not open is an error,
/// not a skip: a closed pull's head stays where it closed.
pub(crate) fn repin_open_pull(
    conn: &Connection,
    repo_id: i64,
    number: i64,
    sha: &str,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE pulls SET head_sha = ?1, base_moved = 0,
         updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
         WHERE repo_id = ?2 AND number = ?3 AND state = 'open'",
        params![sha, repo_id, number],
    )?;
    if n == 0 {
        return Err(WorldError::InvalidTransition(format!(
            "pull #{number} is not open"
        )));
    }
    Ok(())
}

/// Bump the pull's `updated` clock and return the row as stored. The
/// projection's subqueries read the labels and review requests as they stand
/// when this runs, so it must be the last write a transaction makes to the
/// pull or its child tables.
fn touch_pull(conn: &Connection, pull_id: i64) -> Result<Pull> {
    conn.query_row(
        &format!(
            "UPDATE pulls SET updated_clock_unix = (SELECT clock_unix FROM world WHERE id = 1)
             WHERE id = ?1
             RETURNING {}",
            queries::PULL_COLUMNS
        ),
        params![pull_id],
        queries::row_to_pull,
    )
    .optional()?
    .ok_or(WorldError::NoSuchPull(pull_id))
}

/// Mark the unmerged pulls whose base is `branch` as having had their base
/// move. It moves no `updated_at`: GitHub's does not move on a push to the
/// base.
pub(crate) fn mark_base_moved(conn: &Connection, repo_id: i64, branch: &str) -> Result<()> {
    conn.execute(
        "UPDATE pulls SET base_moved = 1
         WHERE repo_id = ?1 AND base_ref = ?2 AND merged = 0 AND base_moved = 0",
        params![repo_id, branch],
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
