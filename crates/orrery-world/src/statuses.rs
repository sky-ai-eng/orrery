//! Commit statuses: what a caller says of a commit, under a context of its
//! own naming. A commit keeps every status it is given, and the newest of
//! each context is what the commit comes to.

use rusqlite::params;

use crate::events::{self, Attribution, EventBody};
use crate::types::*;
use crate::{Result, World, WorldError, objects, queries};

/// How many statuses GitHub keeps for one commit and context.
pub const MAX_STATUSES_PER_CONTEXT: i64 = 1000;

pub struct NewCommitStatus<'a> {
    pub repo_id: i64,
    pub sha: &'a str,
    pub context: &'a str,
    pub state: StatusState,
    pub description: Option<&'a str>,
    pub target_url: Option<&'a str>,
}

/// What a commit's statuses come to, given the newest of each context: a
/// failure when any failed or errored, pending when any is or there is
/// none, and otherwise a success. An error counts as a failure, so no
/// commit comes to one.
pub fn combined_state<'a>(newest: impl IntoIterator<Item = &'a CommitStatus>) -> StatusState {
    let states: Vec<StatusState> = newest.into_iter().map(|status| status.state).collect();
    let any = |state| states.contains(&state);
    if any(StatusState::Failure) || any(StatusState::Error) {
        StatusState::Failure
    } else if states.is_empty() || any(StatusState::Pending) {
        StatusState::Pending
    } else {
        StatusState::Success
    }
}

impl World {
    /// Give a commit a status. A context the commit has a status of gets
    /// another, until it holds [`MAX_STATUSES_PER_CONTEXT`]. A context is
    /// the same one in either case.
    pub fn create_commit_status(
        &self,
        by: Attribution,
        new: NewCommitStatus<'_>,
    ) -> Result<CommitStatus> {
        self.write(by, |w| {
            let tx = w.tx;
            let held: i64 = tx.query_row(
                "SELECT COUNT(*) FROM commit_statuses
                 WHERE repo_id = ?1 AND sha = ?2 AND context = ?3 COLLATE NOCASE",
                params![new.repo_id, new.sha, new.context],
                |r| r.get(0),
            )?;
            if held >= MAX_STATUSES_PER_CONTEXT {
                return Err(WorldError::TooManyStatuses {
                    sha: new.sha.to_string(),
                    context: new.context.to_string(),
                });
            }
            let status = tx.query_row(
                &format!(
                    "INSERT INTO commit_statuses (repo_id, sha, context, state, description,
                                                  target_url, creator_id, created_clock_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                             (SELECT clock_unix FROM world WHERE id = 1))
                     RETURNING {}",
                    queries::COMMIT_STATUS_COLUMNS
                ),
                params![
                    new.repo_id,
                    new.sha,
                    new.context,
                    new.state.as_str(),
                    new.description,
                    new.target_url,
                    by.actor_id
                ],
                queries::row_to_commit_status,
            )?;
            w.operation(EventBody::CommitStatusCreated(
                events::CommitStatusCreated {
                    subject: objects::commit_status_subject(tx, status.id)?,
                    object: objects::commit_status_object(tx, status.id)?,
                },
            ));
            Ok(status)
        })
    }

    /// Every status of one commit, newest first.
    pub fn list_commit_statuses_for_sha(
        &self,
        repo_id: i64,
        sha: &str,
    ) -> Result<Vec<CommitStatus>> {
        self.with_conn(|conn| queries::list_commit_statuses_for_sha(conn, repo_id, sha))
    }
}

/// The newest status of each context among `statuses`, which are newest
/// first, in the order GitHub's combined status lists them: by that newest
/// status, oldest first. A context is the same one in either case, and is
/// spelled as its newest status spells it.
pub fn newest_of_each_context(statuses: &[CommitStatus]) -> Vec<&CommitStatus> {
    let mut newest: Vec<&CommitStatus> = Vec::new();
    for status in statuses {
        if !newest
            .iter()
            .any(|kept| kept.context.eq_ignore_ascii_case(&status.context))
        {
            newest.push(status);
        }
    }
    newest.reverse();
    newest
}
