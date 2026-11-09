//! Renderers from kernel domain types to GitHub wire objects, populated
//! generously (rule 1: fields liberal, endpoints strict). The fields each
//! object carries are listed in spec/surface-github.md §1.0.

use orrery_face_git::{GitFaceError, store};
use orrery_world::{Actor, ActorKind, CheckRun, IssueComment, Pull, PullState, Repo, Review, Team};

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
    let host = host_name(base);
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

/// The base URL's host name, without its scheme, port, or path. Orrery serves
/// neither `git://` nor ssh, so `git_url` and `ssh_url` only need to be
/// well-formed, and a port in an scp-style address is read as part of the
/// path.
fn host_name(base: &str) -> String {
    base.parse::<axum::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_string))
        .unwrap_or_default()
}

/// The GitHub pull request object as a list carries it. `merged_at` equals
/// `closed_at` when merged — one instant on GitHub too. `mergeable` is null,
/// GitHub's "not computed": a list's ETag hashes its body, and a test merge
/// in it would change the tag whenever a base branch moves.
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
        "labels": pull.labels.iter().map(|n| label_json(n)).collect::<Vec<_>>(),
        "requested_reviewers": pull
            .requested_reviewers
            .iter()
            .map(|l| user_json(face, l))
            .collect::<Vec<_>>(),
        "requested_teams": pull
            .requested_teams
            .iter()
            .filter_map(|slug| face.world.team_by_slug(&repo.owner_login, slug).ok().flatten())
            .map(|t| team_json(face, &t))
            .collect::<Vec<_>>(),
        "assignees": [],
        "milestone": serde_json::Value::Null,
    })
}

/// The pull request object a single read and the create and edit routes
/// answer with: the list's, with `mergeable`, `mergeable_state`, and
/// `rebaseable` from a test merge of the head onto the base branch's tip,
/// computed when it is rendered.
pub(crate) fn full_pull_json(
    face: &GithubFace,
    repo: &Repo,
    owner: &Actor,
    pull: &Pull,
) -> serde_json::Value {
    let mut json = pull_json(face, repo, owner, pull);
    // A git failure answers GitHub's "not computed yet" rather than failing
    // a response whose write has already landed.
    let verdict = mergeability(face, repo, pull).unwrap_or_else(|e| {
        tracing::error!(error = %e, number = pull.number, "computing mergeability failed");
        Mergeability::Unknown
    });
    let (mergeable, state, rebaseable) = match verdict {
        Mergeability::Unknown => (None, "unknown", None),
        Mergeability::Conflicting => (Some(false), "dirty", Some(false)),
        Mergeability::Clean => (Some(true), "clean", Some(pull.state == PullState::Open)),
    };
    json["mergeable"] = mergeable.into();
    json["mergeable_state"] = state.into();
    json["rebaseable"] = rebaseable.into();
    json
}

/// A pull request's mergeability, which REST renders as `mergeable`,
/// `mergeable_state`, and `rebaseable`, and GraphQL as `mergeable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mergeability {
    /// There is no test merge to report.
    Unknown,
    /// The base branch is gone, or the test merge conflicts.
    Conflicting,
    /// The test merge of the head onto the base branch's tip is clean.
    Clean,
}

/// The mergeability GitHub reports for a pull request. A merged one has
/// none. A closed one whose head branch is gone keeps the test merge GitHub
/// made against `base_sha` and cannot make another, so it has none once its
/// base branch has moved.
pub(crate) fn mergeability(
    face: &GithubFace,
    repo: &Repo,
    pull: &Pull,
) -> Result<Mergeability, GitFaceError> {
    if pull.merged {
        return Ok(Mergeability::Unknown);
    }
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let Some(base_tip) = store::rev_parse(&bare, &format!("refs/heads/{}", pull.base_ref))? else {
        return Ok(Mergeability::Conflicting);
    };
    if pull.state != PullState::Open
        && base_tip != pull.base_sha
        && store::rev_parse(&bare, &format!("refs/heads/{}", pull.head_ref))?.is_none()
    {
        return Ok(Mergeability::Unknown);
    }
    Ok(match store::merge_tree(&bare, &base_tip, &pull.head_sha)? {
        Some(_) => Mergeability::Clean,
        None => Mergeability::Conflicting,
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

/// GitHub's label object, holding what a pull's label carries. Orrery keeps
/// no repository labels, so there is no id, and the colour is the one GitHub
/// gives a label it creates on first use.
pub(crate) fn label_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "color": "ededed",
        "default": false,
        "description": serde_json::Value::Null,
    })
}

/// GitHub's organization object as it appears inside a team: the org-kind
/// actor's identity plus its API addresses.
pub(crate) fn org_json(face: &GithubFace, org_login: &str) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let mut json = user_json(face, org_login);
    json["url"] = serde_json::json!(format!("{base}/api/v3/orgs/{org_login}"));
    json["repos_url"] = serde_json::json!(format!("{base}/api/v3/orgs/{org_login}/repos"));
    json["members_url"] =
        serde_json::json!(format!("{base}/api/v3/orgs/{org_login}/members{{/member}}"));
    json["description"] = serde_json::Value::Null;
    json
}

/// GitHub's organization object as `GET /orgs/{org}` answers it: the object
/// a team carries, plus the profile fields.
pub(crate) fn org_full_json(face: &GithubFace, org: &Actor) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let mut json = org_json(face, &org.login);
    json["avatar_url"] = serde_json::json!(avatar_url(org));
    json["html_url"] = serde_json::json!(format!("{base}/{}", org.login));
    json["created_at"] = serde_json::json!(rfc3339(org.created_clock_unix));
    json["updated_at"] = serde_json::json!(rfc3339(org.created_clock_unix));
    json
}

/// GitHub's user object as `GET /user` and `GET /users/{username}` answer
/// it. Orrery keeps no display name, so `name` is the login.
pub(crate) fn user_full_json(face: &GithubFace, actor: &Actor) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let login = rest_login(actor);
    let html_url = match actor.kind {
        ActorKind::Bot => format!("{base}/apps/{}", actor.login),
        _ => format!("{base}/{login}"),
    };
    let mut json = actor_json(actor);
    json["name"] = serde_json::json!(actor.login);
    json["avatar_url"] = serde_json::json!(avatar_url(actor));
    json["url"] = serde_json::json!(format!("{base}/api/v3/users/{login}"));
    json["html_url"] = serde_json::json!(html_url);
    json["created_at"] = serde_json::json!(rfc3339(actor.created_clock_unix));
    json["updated_at"] = serde_json::json!(rfc3339(actor.created_clock_unix));
    json
}

fn avatar_url(actor: &Actor) -> String {
    format!("https://orrery.invalid/avatars/{}", actor.id)
}

/// GitHub's team object as an organization's team listing and a pull's
/// `requested_teams` carry it. Orrery keeps no secret teams and no nesting,
/// so every team is `closed` with no parent.
pub(crate) fn team_json(face: &GithubFace, team: &Team) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let api = format!(
        "{base}/api/v3/organizations/{}/team/{}",
        team.org_id, team.id
    );
    serde_json::json!({
        "id": team.id,
        "node_id": crate::nodeid::team(team.id),
        "url": api,
        "html_url": format!("{base}/orgs/{}/teams/{}", team.org_login, team.slug),
        "name": team.name,
        "slug": team.slug,
        "description": team.description,
        "privacy": "closed",
        "notification_setting": "notifications_enabled",
        "permission": "pull",
        "members_url": format!("{api}/members{{/member}}"),
        "repositories_url": format!("{api}/repos"),
        "parent": serde_json::Value::Null,
    })
}

/// The full team object — what creating a team and the authenticated user's
/// team listing answer with: the listing shape plus counts, timestamps and
/// the organization.
pub(crate) fn team_full_json(
    face: &GithubFace,
    team: &Team,
) -> Result<serde_json::Value, orrery_world::WorldError> {
    let mut json = team_json(face, team);
    json["members_count"] = serde_json::json!(face.world.team_members_count(team.id)?);
    json["repos_count"] = serde_json::json!(0);
    json["created_at"] = serde_json::json!(rfc3339(team.created_clock_unix));
    json["updated_at"] = serde_json::json!(rfc3339(team.created_clock_unix));
    json["organization"] = org_json(face, &team.org_login);
    Ok(json)
}
