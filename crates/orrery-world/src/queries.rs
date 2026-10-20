//! The one home for SELECT column lists and row mappers, shared by `World`
//! (live) and `WorldReader` (snapshots). A write's RETURNING/read-back and the
//! point read must project through the same shapes here, so they cannot drift.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::types::*;
use crate::{Result, WorldError};

pub(crate) const ACTOR_SELECT: &str = "SELECT id, login, kind, created_clock_unix FROM actors a";

pub(crate) const REPO_SELECT: &str = "SELECT r.id, a.login, r.name, r.description, r.private,
    r.default_branch, r.created_clock_unix, r.pushed_at_clock_unix
    FROM repos r JOIN actors a ON a.id = r.owner_id";

pub(crate) const PULL_SELECT: &str = "SELECT p.id, p.repo_id, p.number, p.title, p.body,
    au.login, p.state, p.draft, p.head_ref, p.head_sha, p.base_ref, p.base_sha,
    p.merged, p.merge_commit_sha, mb.login, p.created_clock_unix, p.updated_clock_unix,
    p.closed_clock_unix,
    (SELECT json_group_array(name) FROM (
        SELECT name FROM pull_labels WHERE pull_id = p.id ORDER BY name COLLATE NOCASE))
    FROM pulls p
    JOIN actors au ON au.id = p.author_id
    LEFT JOIN actors mb ON mb.id = p.merged_by_id";

pub(crate) const REVIEW_SELECT: &str = "SELECT v.id, v.pull_id, a.login, v.state, v.body,
    v.commit_sha, v.submitted_clock_unix
    FROM reviews v JOIN actors a ON a.id = v.author_id";

pub(crate) const COMMENT_SELECT: &str = "SELECT c.id, c.pull_id, a.login, c.body,
    c.created_clock_unix, c.updated_clock_unix
    FROM issue_comments c JOIN actors a ON a.id = c.author_id";

pub(crate) const CHECK_RUN_SELECT: &str = "SELECT k.id, k.repo_id, k.head_sha, k.name,
    k.status, k.conclusion, k.details_url, k.external_id, k.output, a.login,
    k.started_clock_unix, k.completed_clock_unix, k.created_clock_unix
    FROM check_runs k JOIN actors a ON a.id = k.creator_id";

pub(crate) fn row_to_actor(r: &Row<'_>) -> rusqlite::Result<Actor> {
    Ok(Actor {
        id: r.get(0)?,
        login: r.get(1)?,
        kind: ActorKind::from_db(&r.get::<_, String>(2)?),
        created_clock_unix: r.get(3)?,
    })
}

pub(crate) fn row_to_repo(r: &Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get(0)?,
        owner_login: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        private: r.get(4)?,
        default_branch: r.get(5)?,
        created_clock_unix: r.get(6)?,
        pushed_at_clock_unix: r.get(7)?,
    })
}

pub(crate) fn row_to_pull(r: &Row<'_>) -> rusqlite::Result<Pull> {
    Ok(Pull {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        number: r.get(2)?,
        title: r.get(3)?,
        body: r.get(4)?,
        author_login: r.get(5)?,
        state: PullState::from_db(&r.get::<_, String>(6)?),
        draft: r.get(7)?,
        head_ref: r.get(8)?,
        head_sha: r.get(9)?,
        base_ref: r.get(10)?,
        base_sha: r.get(11)?,
        merged: r.get(12)?,
        merge_commit_sha: r.get(13)?,
        merged_by_login: r.get(14)?,
        created_clock_unix: r.get(15)?,
        updated_clock_unix: r.get(16)?,
        closed_clock_unix: r.get(17)?,
        labels: serde_json::from_str(&r.get::<_, String>(18)?).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(18, rusqlite::types::Type::Text, Box::new(e))
        })?,
    })
}

pub(crate) fn row_to_review(r: &Row<'_>) -> rusqlite::Result<Review> {
    Ok(Review {
        id: r.get(0)?,
        pull_id: r.get(1)?,
        author_login: r.get(2)?,
        state: ReviewState::from_db(&r.get::<_, String>(3)?),
        body: r.get(4)?,
        commit_sha: r.get(5)?,
        submitted_clock_unix: r.get(6)?,
    })
}

pub(crate) fn row_to_comment(r: &Row<'_>) -> rusqlite::Result<IssueComment> {
    Ok(IssueComment {
        id: r.get(0)?,
        pull_id: r.get(1)?,
        author_login: r.get(2)?,
        body: r.get(3)?,
        created_clock_unix: r.get(4)?,
        updated_clock_unix: r.get(5)?,
    })
}

pub(crate) fn row_to_check_run(r: &Row<'_>) -> rusqlite::Result<CheckRun> {
    Ok(CheckRun {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        head_sha: r.get(2)?,
        name: r.get(3)?,
        status: CheckStatus::from_db(&r.get::<_, String>(4)?),
        conclusion: r
            .get::<_, Option<String>>(5)?
            .map(|s| CheckConclusion::from_db(&s)),
        details_url: r.get(6)?,
        external_id: r.get(7)?,
        output: r
            .get::<_, Option<String>>(8)?
            .and_then(|s| serde_json::from_str(&s).ok()),
        creator_login: r.get(9)?,
        started_clock_unix: r.get(10)?,
        completed_clock_unix: r.get(11)?,
        created_clock_unix: r.get(12)?,
    })
}

pub(crate) fn clock(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT clock_unix FROM world WHERE id = 1", [], |r| {
            r.get(0)
        })?,
    )
}

pub(crate) fn last_seq(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM action_log", [], |r| {
            r.get(0)
        })?,
    )
}

pub(crate) fn list_actors(conn: &Connection) -> Result<Vec<Actor>> {
    list(conn, &format!("{ACTOR_SELECT} ORDER BY id"), row_to_actor)
}

pub(crate) fn list_repos(conn: &Connection) -> Result<Vec<Repo>> {
    list(conn, &format!("{REPO_SELECT} ORDER BY r.id"), row_to_repo)
}

pub(crate) fn list_pulls(conn: &Connection) -> Result<Vec<Pull>> {
    list(conn, &format!("{PULL_SELECT} ORDER BY p.id"), row_to_pull)
}

pub(crate) fn list_reviews(conn: &Connection) -> Result<Vec<Review>> {
    list(
        conn,
        &format!("{REVIEW_SELECT} ORDER BY v.id"),
        row_to_review,
    )
}

pub(crate) fn list_issue_comments(conn: &Connection) -> Result<Vec<IssueComment>> {
    list(
        conn,
        &format!("{COMMENT_SELECT} ORDER BY c.id"),
        row_to_comment,
    )
}

pub(crate) fn list_check_runs(conn: &Connection) -> Result<Vec<CheckRun>> {
    list(
        conn,
        &format!("{CHECK_RUN_SELECT} ORDER BY k.id"),
        row_to_check_run,
    )
}

/// A repo's pulls in GitHub's default listing order — created descending —
/// with an id tiebreaker for a deterministic total order. That order is
/// load-bearing: TF's ETag fast path hashes page 1 of exactly this listing.
pub(crate) fn list_pulls_for_repo(
    conn: &Connection,
    repo_id: i64,
    state: Option<&str>,
) -> Result<Vec<Pull>> {
    let mut sql = format!("{PULL_SELECT} WHERE p.repo_id = ?1");
    if state.is_some() {
        sql.push_str(" AND p.state = ?2");
    }
    sql.push_str(" ORDER BY p.created_clock_unix DESC, p.id DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = match state {
        Some(s) => stmt.query_map(params![repo_id, s], row_to_pull)?,
        None => stmt.query_map(params![repo_id], row_to_pull)?,
    };
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_reviews_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<Review>> {
    let mut stmt = conn.prepare(&format!(
        "{REVIEW_SELECT} WHERE v.pull_id = ?1 ORDER BY v.id"
    ))?;
    let rows = stmt.query_map(params![pull_id], row_to_review)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_comments_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<IssueComment>> {
    let mut stmt = conn.prepare(&format!(
        "{COMMENT_SELECT} WHERE c.pull_id = ?1 ORDER BY c.id"
    ))?;
    let rows = stmt.query_map(params![pull_id], row_to_comment)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_timeline_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<TimelineItem>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.pull_id, t.kind, t.label, a.login, t.clock_unix
         FROM pull_timeline t JOIN actors a ON a.id = t.actor_id
         WHERE t.pull_id = ?1 ORDER BY t.id",
    )?;
    let rows = stmt.query_map(params![pull_id], |r| {
        let label: Option<String> = r.get(3)?;
        let kind = match (r.get::<_, String>(2)?.as_str(), label) {
            ("labeled", Some(name)) => TimelineKind::Labeled(name),
            ("unlabeled", Some(name)) => TimelineKind::Unlabeled(name),
            ("ready_for_review", None) => TimelineKind::ReadyForReview,
            (other, _) => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("timeline kind {other:?} with that label").into(),
                ));
            }
        };
        Ok(TimelineItem {
            id: r.get(0)?,
            pull_id: r.get(1)?,
            kind,
            actor_login: r.get(4)?,
            clock_unix: r.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_check_runs_for_sha(
    conn: &Connection,
    repo_id: i64,
    head_sha: &str,
) -> Result<Vec<CheckRun>> {
    let mut stmt = conn.prepare(&format!(
        "{CHECK_RUN_SELECT} WHERE k.repo_id = ?1 AND k.head_sha = ?2 ORDER BY k.id"
    ))?;
    let rows = stmt.query_map(params![repo_id, head_sha], row_to_check_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// An owner's repos in `sort=pushed&direction=desc` order — what TF's
/// reachable-mirror walk requests — never-pushed repos last, id tiebreaker.
pub(crate) fn list_repos_for_owner(conn: &Connection, owner_id: i64) -> Result<Vec<Repo>> {
    let mut stmt = conn.prepare(&format!(
        "{REPO_SELECT} WHERE r.owner_id = ?1
         ORDER BY r.pushed_at_clock_unix DESC NULLS LAST, r.id DESC"
    ))?;
    let rows = stmt.query_map(params![owner_id], row_to_repo)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn pull_by_id(conn: &Connection, id: i64) -> Result<Option<Pull>> {
    Ok(conn
        .query_row(
            &format!("{PULL_SELECT} WHERE p.id = ?1"),
            params![id],
            row_to_pull,
        )
        .optional()?)
}

pub(crate) fn require_pull(conn: &Connection, id: i64) -> Result<Pull> {
    pull_by_id(conn, id)?.ok_or(WorldError::NoSuchPull(id))
}

pub(crate) fn check_run_by_id(conn: &Connection, id: i64) -> Result<Option<CheckRun>> {
    Ok(conn
        .query_row(
            &format!("{CHECK_RUN_SELECT} WHERE k.id = ?1"),
            params![id],
            row_to_check_run,
        )
        .optional()?)
}

fn list<T>(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
