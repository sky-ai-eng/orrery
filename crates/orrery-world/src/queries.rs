//! The one home for SELECT column lists and row mappers, shared by `World`
//! (live) and `WorldReader` (snapshots). A write's RETURNING clause and the
//! point read must project through the same shapes here, so they cannot drift.

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::types::*;
use crate::{Result, WorldError};

// The projections below name only their own table's columns, reaching other
// tables through scalar subqueries, so the same text serves a point read
// (`SELECT {cols} FROM <table>`) and the RETURNING clause of a write to that
// table: the write returns exactly what the read would.

pub(crate) const ACTOR_COLUMNS: &str = "id, login, kind, created_clock_unix";

pub(crate) const REPO_COLUMNS: &str = "id,
    (SELECT login FROM actors WHERE actors.id = repos.owner_id),
    name, description, private, default_branch, created_clock_unix, pushed_at_clock_unix";

pub(crate) const PULL_COLUMNS: &str = "id, repo_id, number, title, body,
    (SELECT login FROM actors WHERE actors.id = pulls.author_id),
    state, draft, head_ref, head_sha, base_ref, base_sha, merged, merge_commit_sha,
    (SELECT login FROM actors WHERE actors.id = pulls.merged_by_id),
    created_clock_unix, updated_clock_unix, closed_clock_unix,
    (SELECT json_group_array(name) FROM (
        SELECT name FROM pull_labels WHERE pull_id = pulls.id ORDER BY name COLLATE NOCASE)),
    (SELECT json_group_array(login) FROM (
        SELECT a.login FROM review_requests q JOIN actors a ON a.id = q.reviewer_id
        WHERE q.pull_id = pulls.id ORDER BY q.id)),
    (SELECT json_group_array(slug) FROM (
        SELECT t.slug FROM review_requests q JOIN teams t ON t.id = q.team_id
        WHERE q.pull_id = pulls.id ORDER BY q.id)),
    base_moved, test_merge_stale";

pub(crate) const TEAM_COLUMNS: &str = "id, org_id,
    (SELECT login FROM actors WHERE actors.id = teams.org_id),
    slug, name, description, created_clock_unix";

pub(crate) const ORG_MEMBER_COLUMNS: &str =
    "(SELECT login FROM actors WHERE actors.id = org_members.org_id),
    (SELECT login FROM actors WHERE actors.id = org_members.member_id),
    role, created_clock_unix";

pub(crate) const TEAM_MEMBER_COLUMNS: &str = "team_id,
    (SELECT login FROM actors WHERE actors.id = team_members.member_id),
    role, created_clock_unix";

pub(crate) const REVIEW_COLUMNS: &str = "id, pull_id,
    (SELECT login FROM actors WHERE actors.id = reviews.author_id),
    state, body, commit_sha, submitted_clock_unix";

pub(crate) const COMMENT_COLUMNS: &str = "id, pull_id,
    (SELECT login FROM actors WHERE actors.id = issue_comments.author_id),
    body, created_clock_unix, updated_clock_unix";

pub(crate) const CHECK_RUN_COLUMNS: &str = "id, repo_id, head_sha, name, status, conclusion,
    details_url, external_id, output,
    (SELECT login FROM actors WHERE actors.id = check_runs.creator_id),
    started_clock_unix, completed_clock_unix, created_clock_unix";

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
        labels: json_strings(r, 18)?,
        requested_reviewers: json_strings(r, 19)?,
        requested_teams: json_strings(r, 20)?,
        base_moved: r.get(21)?,
        test_merge_stale: r.get(22)?,
    })
}

fn json_strings(r: &Row<'_>, idx: usize) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(&r.get::<_, String>(idx)?).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(crate) fn row_to_team(r: &Row<'_>) -> rusqlite::Result<Team> {
    Ok(Team {
        id: r.get(0)?,
        org_id: r.get(1)?,
        org_login: r.get(2)?,
        slug: r.get(3)?,
        name: r.get(4)?,
        description: r.get(5)?,
        created_clock_unix: r.get(6)?,
    })
}

pub(crate) fn row_to_org_member(r: &Row<'_>) -> rusqlite::Result<OrgMembership> {
    Ok(OrgMembership {
        org_login: r.get(0)?,
        member_login: r.get(1)?,
        role: OrgRole::from_db(&r.get::<_, String>(2)?),
        created_clock_unix: r.get(3)?,
    })
}

pub(crate) fn row_to_team_member(r: &Row<'_>) -> rusqlite::Result<TeamMembership> {
    Ok(TeamMembership {
        team_id: r.get(0)?,
        member_login: r.get(1)?,
        role: TeamRole::from_db(&r.get::<_, String>(2)?),
        created_clock_unix: r.get(3)?,
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

/// The stored clock. In a `real` world a read answers the later of this and
/// the host's time, and a write moves it to the host's time before it runs.
pub(crate) fn clock(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT clock_unix FROM world WHERE id = 1", [], |r| {
            r.get(0)
        })?,
    )
}

/// The stored clock and its mode, in one read.
pub(crate) fn clock_state(conn: &Connection) -> Result<(i64, ClockMode)> {
    Ok(conn.query_row(
        "SELECT clock_unix, clock_mode FROM world WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, ClockMode::from_db(&r.get::<_, String>(1)?))),
    )?)
}

pub(crate) fn last_seq(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM action_log", [], |r| {
            r.get(0)
        })?,
    )
}

/// How many entries the log holds, as the append keeps it.
pub(crate) fn log_entries(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT log_entries FROM world WHERE id = 1", [], |r| {
            r.get(0)
        })?,
    )
}

pub(crate) fn log_count_after(conn: &Connection, after_seq: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM action_log WHERE seq > ?1",
        [after_seq],
        |r| r.get(0),
    )?)
}

pub(crate) fn log_holds(conn: &Connection, seq: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM action_log WHERE seq = ?1)",
        [seq],
        |r| r.get(0),
    )?)
}

pub(crate) fn log_seq_at_or_before(conn: &Connection, seq: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM action_log WHERE seq <= ?1",
        [seq],
        |r| r.get(0),
    )?)
}

pub(crate) fn list_actors(conn: &Connection) -> Result<Vec<Actor>> {
    list(
        conn,
        &format!("SELECT {ACTOR_COLUMNS} FROM actors ORDER BY id"),
        row_to_actor,
    )
}

pub(crate) fn list_repos(conn: &Connection) -> Result<Vec<Repo>> {
    list(
        conn,
        &format!("SELECT {REPO_COLUMNS} FROM repos ORDER BY id"),
        row_to_repo,
    )
}

pub(crate) fn list_pulls(conn: &Connection) -> Result<Vec<Pull>> {
    list(
        conn,
        &format!("SELECT {PULL_COLUMNS} FROM pulls ORDER BY id"),
        row_to_pull,
    )
}

pub(crate) fn list_reviews(conn: &Connection) -> Result<Vec<Review>> {
    list(
        conn,
        &format!("SELECT {REVIEW_COLUMNS} FROM reviews ORDER BY id"),
        row_to_review,
    )
}

pub(crate) fn list_issue_comments(conn: &Connection) -> Result<Vec<IssueComment>> {
    list(
        conn,
        &format!("SELECT {COMMENT_COLUMNS} FROM issue_comments ORDER BY id"),
        row_to_comment,
    )
}

pub(crate) fn list_check_runs(conn: &Connection) -> Result<Vec<CheckRun>> {
    list(
        conn,
        &format!("SELECT {CHECK_RUN_COLUMNS} FROM check_runs ORDER BY id"),
        row_to_check_run,
    )
}

/// A repo's pulls in GitHub's default listing order — created descending —
/// with an id tiebreaker for a deterministic total order. That order is
/// load-bearing: the pulls list's ETag hashes page 1 of exactly this listing,
/// and a poller skips discovery while the tag still matches.
pub(crate) fn list_pulls_for_repo(
    conn: &Connection,
    repo_id: i64,
    state: Option<&str>,
) -> Result<Vec<Pull>> {
    let mut sql = format!("SELECT {PULL_COLUMNS} FROM pulls WHERE repo_id = ?1");
    if state.is_some() {
        sql.push_str(" AND state = ?2");
    }
    sql.push_str(" ORDER BY created_clock_unix DESC, id DESC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = match state {
        Some(s) => stmt.query_map(params![repo_id, s], row_to_pull)?,
        None => stmt.query_map(params![repo_id], row_to_pull)?,
    };
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_reviews_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<Review>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REVIEW_COLUMNS} FROM reviews WHERE pull_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![pull_id], row_to_review)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_comments_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<IssueComment>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COMMENT_COLUMNS} FROM issue_comments WHERE pull_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![pull_id], row_to_comment)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

const TIMELINE_SELECT: &str = "SELECT t.id, t.pull_id, t.kind, t.label, a.login, t.clock_unix,
    rv.login, t.team_id, t.team_slug, t.ref_name
    FROM pull_timeline t JOIN actors a ON a.id = t.actor_id
    LEFT JOIN actors rv ON rv.id = t.reviewer_id";

fn row_to_timeline(r: &Row<'_>) -> rusqlite::Result<TimelineItem> {
    let label: Option<String> = r.get(3)?;
    let ref_name: Option<String> = r.get(9)?;
    let reviewer = match (
        r.get::<_, Option<String>>(6)?,
        r.get::<_, Option<i64>>(7)?,
        r.get::<_, Option<String>>(8)?,
    ) {
        (Some(login), None, None) => Some(RequestedReviewer::User(login)),
        (None, Some(id), Some(slug)) => Some(RequestedReviewer::Team { id, slug }),
        _ => None,
    };
    let kind = match (r.get::<_, String>(2)?.as_str(), label, ref_name, reviewer) {
        ("labeled", Some(name), None, None) => TimelineKind::Labeled(name),
        ("unlabeled", Some(name), None, None) => TimelineKind::Unlabeled(name),
        ("ready_for_review", None, None, None) => TimelineKind::ReadyForReview,
        ("review_requested", None, None, Some(rv)) => TimelineKind::ReviewRequested(rv),
        ("review_request_removed", None, None, Some(rv)) => TimelineKind::ReviewRequestRemoved(rv),
        ("closed", None, None, None) => TimelineKind::Closed,
        ("head_ref_deleted", None, Some(name), None) => TimelineKind::HeadRefDeleted(name),
        ("base_ref_deleted", None, Some(name), None) => TimelineKind::BaseRefDeleted(name),
        ("head_ref_restored", None, None, None) => TimelineKind::HeadRefRestored,
        (other, _, _, _) => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("timeline kind {other:?} with those columns").into(),
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
}

pub(crate) fn list_timeline_for_pull(conn: &Connection, pull_id: i64) -> Result<Vec<TimelineItem>> {
    let mut stmt = conn.prepare(&format!(
        "{TIMELINE_SELECT} WHERE t.pull_id = ?1 ORDER BY t.id"
    ))?;
    let rows = stmt.query_map(params![pull_id], row_to_timeline)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list_timeline(conn: &Connection) -> Result<Vec<TimelineItem>> {
    list(
        conn,
        &format!("{TIMELINE_SELECT} ORDER BY t.id"),
        row_to_timeline,
    )
}

pub(crate) fn list_actor_addresses(conn: &Connection) -> Result<Vec<ActorAddress>> {
    list(
        conn,
        "SELECT actor_id, email, is_primary, verified FROM actor_emails
         ORDER BY actor_id, is_primary DESC, email",
        |r| {
            Ok(ActorAddress {
                actor_id: r.get(0)?,
                email: ActorEmail {
                    email: r.get(1)?,
                    primary: r.get(2)?,
                    verified: r.get(3)?,
                },
            })
        },
    )
}

pub(crate) fn list_oauth_authorizations(conn: &Connection) -> Result<Vec<OAuthAuthorization>> {
    list(
        conn,
        "SELECT c.id, ap.client_id, ap.name, a.login, c.scope, c.created_clock_unix,
                c.exchanged_clock_unix
         FROM oauth_codes c
         JOIN oauth_apps ap ON ap.id = c.app_id
         JOIN actors a ON a.id = c.actor_id
         ORDER BY c.id",
        |r| {
            Ok(OAuthAuthorization {
                id: r.get(0)?,
                client_id: r.get(1)?,
                app_name: r.get(2)?,
                actor_login: r.get(3)?,
                scope: r.get(4)?,
                created_clock_unix: r.get(5)?,
                exchanged_clock_unix: r.get(6)?,
            })
        },
    )
}

pub(crate) fn list_org_members(conn: &Connection) -> Result<Vec<OrgMembership>> {
    list(
        conn,
        &format!("SELECT {ORG_MEMBER_COLUMNS} FROM org_members ORDER BY org_id, member_id"),
        row_to_org_member,
    )
}

pub(crate) fn list_teams(conn: &Connection) -> Result<Vec<Team>> {
    list(
        conn,
        &format!("SELECT {TEAM_COLUMNS} FROM teams ORDER BY id"),
        row_to_team,
    )
}

pub(crate) fn list_team_members(conn: &Connection) -> Result<Vec<TeamMembership>> {
    list(
        conn,
        &format!("SELECT {TEAM_MEMBER_COLUMNS} FROM team_members ORDER BY team_id, member_id"),
        row_to_team_member,
    )
}

pub(crate) fn list_review_requests(conn: &Connection) -> Result<Vec<ReviewRequest>> {
    list(
        conn,
        "SELECT q.pull_id, a.login, t.id, t.slug
         FROM review_requests q
         LEFT JOIN actors a ON a.id = q.reviewer_id
         LEFT JOIN teams t ON t.id = q.team_id
         ORDER BY q.id",
        |r| {
            let reviewer = match (r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?) {
                (Some(login), None) => RequestedReviewer::User(login),
                (None, Some(id)) => RequestedReviewer::Team {
                    id,
                    slug: r.get(3)?,
                },
                _ => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Null,
                        "a review request names exactly one user or team".into(),
                    ));
                }
            };
            Ok(ReviewRequest {
                pull_id: r.get(0)?,
                reviewer,
            })
        },
    )
}

pub(crate) fn list_check_runs_for_sha(
    conn: &Connection,
    repo_id: i64,
    head_sha: &str,
) -> Result<Vec<CheckRun>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CHECK_RUN_COLUMNS} FROM check_runs
         WHERE repo_id = ?1 AND head_sha = ?2 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![repo_id, head_sha], row_to_check_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// An owner's repos in `sort=pushed&direction=desc` order — what a consumer
/// walking the repos it can reach requests — never-pushed repos last, id
/// tiebreaker.
pub(crate) fn list_repos_for_owner(conn: &Connection, owner_id: i64) -> Result<Vec<Repo>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {REPO_COLUMNS} FROM repos WHERE owner_id = ?1
         ORDER BY pushed_at_clock_unix DESC NULLS LAST, id DESC"
    ))?;
    let rows = stmt.query_map(params![owner_id], row_to_repo)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn pull_by_id(conn: &Connection, id: i64) -> Result<Option<Pull>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PULL_COLUMNS} FROM pulls WHERE id = ?1"),
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
            &format!("SELECT {CHECK_RUN_COLUMNS} FROM check_runs WHERE id = ?1"),
            params![id],
            row_to_check_run,
        )
        .optional()?)
}

pub(crate) fn list<T>(
    conn: &Connection,
    sql: &str,
    map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], map)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
