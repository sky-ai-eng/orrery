//! Renderers from kernel domain types to GitHub wire objects, populated
//! generously (rule 1: fields liberal, endpoints strict). Field values a
//! consumer reads are listed in spec/surface-github.md.

use orrery_world::{Actor, ActorKind, CheckRun, IssueComment, Pull, Repo, Review};

use crate::GithubFace;

pub(crate) fn actor_json(actor: &Actor) -> serde_json::Value {
    serde_json::json!({
        "login": rest_login(actor),
        "id": actor.id,
        "node_id": crate::nodeid::actor(actor.id),
        "type": owner_type(actor.kind),
        "site_admin": false,
    })
}

/// A user object from a stored login. Kernel rows carry logins, not actor
/// rows; a lookup miss degrades to a login-only object rather than failing a
/// render (actors are never deleted, so a miss is already an anomaly).
pub(crate) fn user_json(face: &GithubFace, login: &str) -> serde_json::Value {
    match face.world.actor_by_login(login) {
        Ok(Some(a)) => actor_json(&a),
        _ => serde_json::json!({ "login": login }),
    }
}

/// The GitHub repo object.
pub fn repo_json(face: &GithubFace, repo: &Repo, owner: &Actor) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let host = base.split("://").nth(1).unwrap_or(base);
    let full_name = repo.full_name();
    serde_json::json!({
        "id": repo.id,
        "node_id": crate::nodeid::repo(repo.id),
        "name": repo.name,
        "full_name": full_name,
        "owner": actor_json(owner),
        "private": repo.private,
        "visibility": if repo.private { "private" } else { "public" },
        "description": repo.description,
        "fork": false,
        "html_url": format!("{base}/{full_name}"),
        "url": format!("{base}/api/v3/repos/{full_name}"),
        "clone_url": format!("{base}/{full_name}.git"),
        "git_url": format!("git://{host}/{full_name}.git"),
        "ssh_url": format!("git@{host}:{full_name}.git"),
        "default_branch": repo.default_branch,
        "language": serde_json::Value::Null,
        "archived": false,
        "disabled": false,
        "created_at": rfc3339(repo.created_clock_unix),
        "updated_at": rfc3339(repo.created_clock_unix),
        "pushed_at": repo.pushed_at_clock_unix.map(rfc3339),
    })
}

/// The GitHub pull request object. `merged_at` equals `closed_at` when
/// merged — one instant on GitHub too. `mergeable: null` is GitHub's
/// "not computed", which is honest here: Orrery computes mergeability at
/// merge time, not speculatively.
pub(crate) fn pull_json(
    face: &GithubFace,
    repo: &Repo,
    owner: &Actor,
    pull: &Pull,
) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    let n = pull.number;
    serde_json::json!({
        "id": pull.id,
        "node_id": crate::nodeid::pull(pull.id),
        "number": n,
        "state": pull.state.as_str(),
        "locked": false,
        "title": pull.title,
        "body": pull.body,
        "draft": pull.draft,
        "user": user_json(face, &pull.author_login),
        "html_url": format!("{base}/{full_name}/pull/{n}"),
        "url": format!("{base}/api/v3/repos/{full_name}/pulls/{n}"),
        "issue_url": format!("{base}/api/v3/repos/{full_name}/issues/{n}"),
        "merged": pull.merged,
        "mergeable": serde_json::Value::Null,
        "merge_commit_sha": pull.merge_commit_sha,
        "merged_by": pull.merged_by_login.as_deref().map(|l| user_json(face, l)),
        "created_at": rfc3339(pull.created_clock_unix),
        "updated_at": rfc3339(pull.updated_clock_unix),
        "closed_at": pull.closed_clock_unix.map(rfc3339),
        "merged_at": pull.merged.then_some(pull.closed_clock_unix).flatten().map(rfc3339),
        "head": {
            "label": format!("{}:{}", repo.owner_login, pull.head_ref),
            "ref": pull.head_ref,
            "sha": pull.head_sha,
            "user": actor_json(owner),
            "repo": repo_json(face, repo, owner),
        },
        "base": {
            "label": format!("{}:{}", repo.owner_login, pull.base_ref),
            "ref": pull.base_ref,
            "sha": pull.base_sha,
            "user": actor_json(owner),
            "repo": repo_json(face, repo, owner),
        },
        "labels": [],
        "requested_reviewers": [],
        "requested_teams": [],
        "assignees": [],
        "milestone": serde_json::Value::Null,
    })
}

/// The GitHub review object. Wire state is the neutral store state in
/// GitHub's casing: approved -> APPROVED, changes_requested ->
/// CHANGES_REQUESTED, commented -> COMMENTED.
pub(crate) fn review_json(
    face: &GithubFace,
    repo: &Repo,
    pull: &Pull,
    review: &Review,
) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    serde_json::json!({
        "id": review.id,
        "node_id": crate::nodeid::review(review.id),
        "user": user_json(face, &review.author_login),
        "body": review.body,
        "state": review.state.as_str().to_ascii_uppercase(),
        "commit_id": review.commit_sha,
        "html_url": format!("{base}/{full_name}/pull/{}#pullrequestreview-{}", pull.number, review.id),
        "pull_request_url": format!("{base}/api/v3/repos/{full_name}/pulls/{}", pull.number),
        "submitted_at": rfc3339(review.submitted_clock_unix),
        "author_association": "NONE",
    })
}

pub(crate) fn comment_json(
    face: &GithubFace,
    repo: &Repo,
    pull: &Pull,
    comment: &IssueComment,
) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    serde_json::json!({
        "id": comment.id,
        "node_id": crate::nodeid::comment(comment.id),
        "user": user_json(face, &comment.author_login),
        "body": comment.body,
        "html_url": format!("{base}/{full_name}/pull/{}#issuecomment-{}", pull.number, comment.id),
        "url": format!("{base}/api/v3/repos/{full_name}/issues/comments/{}", comment.id),
        "issue_url": format!("{base}/api/v3/repos/{full_name}/issues/{}", pull.number),
        "created_at": rfc3339(comment.created_clock_unix),
        "updated_at": rfc3339(comment.updated_clock_unix),
        "author_association": "NONE",
    })
}

pub(crate) fn check_run_json(face: &GithubFace, repo: &Repo, run: &CheckRun) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    serde_json::json!({
        "id": run.id,
        "node_id": crate::nodeid::check(run.id),
        "name": run.name,
        "head_sha": run.head_sha,
        "status": run.status.as_str(),
        "conclusion": run.conclusion.map(|c| c.as_str()),
        "external_id": run.external_id,
        "details_url": run.details_url,
        "url": format!("{base}/api/v3/repos/{full_name}/check-runs/{}", run.id),
        "html_url": format!("{base}/{full_name}/runs/{}", run.id),
        "started_at": run.started_clock_unix.map(rfc3339),
        "completed_at": run.completed_clock_unix.map(rfc3339),
        "output": run.output.clone().unwrap_or(serde_json::json!({
            "title": null, "summary": null, "text": null, "annotations_count": 0,
        })),
        "check_suite": serde_json::Value::Null,
        "app": {
            "slug": run.creator_login,
            "name": run.creator_login,
        },
    })
}

/// An actor's login as GitHub's REST API spells it. A bot actor stands for a
/// GitHub App, whose REST login carries a `[bot]` suffix; GraphQL spells the
/// bare slug, which is the stored login.
pub(crate) fn rest_login(actor: &Actor) -> String {
    match actor.kind {
        ActorKind::Bot => format!("{}[bot]", actor.login),
        _ => actor.login.clone(),
    }
}

pub(crate) fn owner_type(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Org => "Organization",
        ActorKind::Bot => "Bot",
        ActorKind::User | ActorKind::System => "User",
    }
}

pub(crate) fn rfc3339(unix: i64) -> String {
    jiff::Timestamp::from_second(unix)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
