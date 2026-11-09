//! Pull request endpoints: open, edit/close/reopen, merge, reviews, review
//! requests, issue comments and labels (a PR is an issue for commenting and labelling, so
//! those routes live under `/issues/`, exactly as on GitHub).

use std::collections::BTreeMap;
use std::path::Path;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_face_git::store;
use orrery_world::{Actor, NewPull, Pull, PullState, RefMove, Repo, ReviewState, WorldError};

use crate::render::{comment_json, label_json, pull_json, review_json};
use crate::{
    GithubFace, github_error, internal, json_response, media, orrery_unsupported_field,
    resolve_repo, validation_custom, validation_invalid, validation_missing,
};

#[derive(serde::Deserialize)]
struct CreatePullBody {
    title: Option<String>,
    body: Option<String>,
    head: Option<String>,
    base: Option<String>,
    draft: Option<bool>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn create_pull(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let parsed: CreatePullBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    // `issue` turns an existing issue into a PR; `head_repo` is the fork
    // spelling. Both are behavior we refuse to fake silently.
    for field in ["issue", "head_repo"] {
        if parsed.rest.contains_key(field) {
            return orrery_unsupported_field("PullRequest", field);
        }
    }
    let title = match pull_title(parsed.title.as_deref().unwrap_or("")) {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    let Some(head_raw) = parsed.head.as_deref().filter(|h| !h.is_empty()) else {
        return validation_missing("PullRequest", "head");
    };
    let Some(base_ref) = parsed.base.as_deref().filter(|b| !b.is_empty()) else {
        return validation_missing("PullRequest", "base");
    };
    // "owner:branch" is GitHub's cross-repo spelling; same-owner is just a
    // long way of writing the branch, anything else is a fork.
    let head_ref = match head_raw.split_once(':') {
        None => head_raw,
        Some((prefix, branch)) if prefix.eq_ignore_ascii_case(&repo.owner_login) => branch,
        Some(_) => {
            return orrery_unsupported_field(
                "PullRequest",
                "head (cross-repository pull requests / forks)",
            );
        }
    };

    // The world write gate keeps a snapshot from copying, or a restore from
    // replacing, the world between the kernel insert and the ref write. The
    // repo write lock keeps a concurrent push from moving the head between
    // sha resolution and the kernel insert — the pinned head_sha must be the
    // tip the PR actually proposes.
    let _gate = face.git.gate().read_owned().await;
    let lock = face.git.repo_lock(repo.id);
    let _guard = lock.lock().await;

    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let base_sha = match store::rev_parse(&bare, &format!("refs/heads/{base_ref}")) {
        Ok(Some(sha)) => sha,
        Ok(None) => return validation_invalid("PullRequest", "base"),
        Err(e) => return internal(e),
    };
    let head_sha = match store::rev_parse(&bare, &format!("refs/heads/{head_ref}")) {
        Ok(Some(sha)) => sha,
        Ok(None) => return validation_invalid("PullRequest", "head"),
        Err(e) => return internal(e),
    };
    match store::rev_list_count(&bare, &base_sha, &head_sha) {
        Ok(0) => {
            return validation_custom(
                "PullRequest",
                &format!("No commits between {base_ref} and {head_ref}"),
            );
        }
        Ok(_) => {}
        Err(e) => return internal(e),
    }

    let created = face.world.create_pull(NewPull {
        repo_id: repo.id,
        author_id: actor.id,
        title,
        body: parsed.body.as_deref(),
        head_ref,
        head_sha: &head_sha,
        base_ref,
        base_sha: &base_sha,
        draft: parsed.draft.unwrap_or(false),
    });
    match created {
        Ok(pull) => match face.git.pin_pull_ref(&repo, pull.number, &pull.head_sha) {
            Ok(()) => json_response(
                StatusCode::CREATED,
                pull_json(&face, &repo, &owner_actor, &pull),
            ),
            Err(e) => internal(e),
        },
        Err(WorldError::PullExists(head)) => validation_custom(
            "PullRequest",
            &format!(
                "A pull request already exists for {}:{head}.",
                repo.owner_login
            ),
        ),
        Err(e) => internal(e),
    }
}

/// A pull request's title as stored, trimmed. A blank one is GitHub's
/// `missing_field`, on an edit as on a create.
fn pull_title(raw: &str) -> Result<&str, Box<Response<Body>>> {
    match raw.trim() {
        "" => Err(Box::new(validation_missing("PullRequest", "title"))),
        title => Ok(title),
    }
}

#[derive(serde::Deserialize)]
struct PatchPullBody {
    title: Option<String>,
    /// Absent = keep; explicit null = clear (GitHub's nullable body).
    #[serde(default, deserialize_with = "double_option")]
    body: Option<Option<String>>,
    state: Option<String>,
    base: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

fn double_option<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(d).map(Some)
}

pub(crate) async fn patch_pull(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let parsed: PatchPullBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    let _ = parsed.rest; // unknown fields are ignored, GitHub-lax

    let state = match parsed.state.as_deref() {
        None => None,
        Some(s @ ("open" | "closed")) => Some(s),
        Some(_) => return validation_invalid("PullRequest", "state"),
    };
    let title = match parsed.title.as_deref().map(pull_title).transpose() {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    // The repo write lock keeps a push from interleaving with the edit: a
    // push moves the refs of the pulls open on its branch before it records
    // them, so a close or reopen must not land in between, and a new base's
    // sha must still be the base's tip when the edit lands. The pull is read
    // again once the lock is held, so the state checks below see current
    // truth.
    let lock = face.git.repo_lock(repo.id);
    let _guard = lock.lock().await;
    let mut pull = match face.world.pull_by_number(repo.id, pull.number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    let base = match parsed.base.as_deref() {
        None => None,
        Some(b) => {
            let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
            match store::rev_parse(&bare, &format!("refs/heads/{b}")) {
                Ok(Some(sha)) => Some((b.to_string(), sha)),
                Ok(None) => return validation_invalid("PullRequest", "base"),
                Err(e) => return internal(e),
            }
        }
    };

    if title.is_some() || parsed.body.is_some() || base.is_some() {
        let edited = face.world.edit_pull(
            pull.id,
            actor.id,
            title,
            parsed.body.as_ref().map(|b| b.as_deref()),
            base.as_ref().map(|(r, s)| (r.as_str(), s.as_str())),
        );
        pull = match edited {
            Ok(p) => p,
            Err(e) => return internal(e),
        };
    }

    match state {
        Some("closed") if pull.state == PullState::Open => {
            pull = match face.world.close_pull(pull.id, actor.id) {
                Ok(p) => p,
                Err(e) => return internal(e),
            };
        }
        Some("open") if pull.state == PullState::Closed => {
            pull = match face.world.reopen_pull(pull.id, actor.id) {
                Ok(p) => p,
                Err(WorldError::InvalidTransition(_)) => {
                    return validation_custom(
                        "PullRequest",
                        "state cannot be changed. The pull request has been merged.",
                    );
                }
                Err(e) => return internal(e),
            };
        }
        // Same-state writes are GitHub no-ops: the PR is returned unchanged.
        _ => {}
    }

    json_response(StatusCode::OK, pull_json(&face, &repo, &owner_actor, &pull))
}

#[derive(serde::Deserialize)]
struct MergePullBody {
    commit_title: Option<String>,
    commit_message: Option<String>,
    /// Expected head sha — GitHub's stale-merge guard.
    sha: Option<String>,
    merge_method: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn merge_pull(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let parsed: MergePullBody =
        match serde_json::from_slice(if body.is_empty() { b"{}" } else { &body }) {
            Ok(p) => p,
            Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
        };
    let _ = parsed.rest;
    match parsed.merge_method.as_deref() {
        None | Some("merge") => {}
        // Squash and rebase are real GitHub methods Orrery has not built —
        // a loud refusal, never a silently different history.
        Some(other) => {
            return orrery_unsupported_field("PullRequest", &format!("merge_method={other}"));
        }
    }

    // Serialize against pushes and other merges, then re-read the pull so
    // the state checks below see current truth.
    let _gate = face.git.gate().read_owned().await;
    let lock = face.git.repo_lock(repo.id);
    let _guard = lock.lock().await;
    let pull = match face.world.pull_by_number(repo.id, pull.number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };

    if pull.state != PullState::Open {
        return github_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Pull Request is not mergeable",
        );
    }
    if pull.draft {
        return github_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "Pull Request is still a work in progress",
        );
    }
    if let Some(expected) = parsed.sha.as_deref()
        && expected != pull.head_sha
    {
        return github_error(
            StatusCode::CONFLICT,
            "Head branch was modified. Review and try the merge again.",
        );
    }

    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let base_tip = match store::rev_parse(&bare, &format!("refs/heads/{}", pull.base_ref)) {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return github_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Pull Request is not mergeable",
            );
        }
        Err(e) => return internal(e),
    };
    let tree = match store::merge_tree(&bare, &base_tip, &pull.head_sha) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return github_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Pull Request is not mergeable",
            );
        }
        Err(e) => return internal(e),
    };

    let clock = match face.world.clock() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let title_line = parsed
        .commit_title
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "Merge pull request #{} from {}/{}",
                pull.number, repo.owner_login, pull.head_ref
            )
        });
    let desc = parsed.commit_message.as_deref().unwrap_or(&pull.title);
    let message = format!("{title_line}\n\n{desc}");
    let merge_sha = match store::commit_tree(
        &bare,
        &tree,
        &[&base_tip, &pull.head_sha],
        &message,
        &actor.login,
        clock,
    ) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    // The open pull requests whose head is the base branch take the merge
    // commit as their head, as on a push of it. They are read before the base
    // moves, so a failed read moves nothing; the repository lock keeps the set
    // from changing before the record.
    let base_refname = format!("refs/heads/{}", pull.base_ref);
    let repins = match face
        .world
        .pulls_to_sync(repo.id, &pull.base_ref, &merge_sha)
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    if let Err(e) = store::update_ref_cas(&bare, &base_refname, &merge_sha, &base_tip) {
        return internal(e);
    }
    let base = RefMove {
        name: &base_refname,
        old: &base_tip,
        new: &merge_sha,
        repins,
    };

    // Every ref the merge moves is in place before its record commits, so a
    // reader that can see the merge's entries also sees its refs.
    let record = || -> Result<(), orrery_face_git::GitFaceError> {
        for &number in &base.repins {
            face.git.pin_pull_ref(&repo, number, &merge_sha)?;
        }
        face.world.record_merge(pull.id, actor.id, &base)?;
        Ok(())
    };
    if let Err(e) = record() {
        unwind_merge(&face, &repo, &bare, &base);
        return internal(e);
    }

    json_response(
        StatusCode::OK,
        serde_json::json!({
            "sha": merge_sha,
            "merged": true,
            "message": "Pull Request successfully merged",
        }),
    )
}

/// Put back the refs a merge moved when recording it failed, so the
/// repository agrees with the world, which holds no trace of the merge: the
/// base branch returns to `base.old` by compare-and-swap, and each pull
/// request's ref to the head the world still pins it at. A retry then starts
/// from the state the failed merge started from. The route answers 500
/// either way, so a ref that cannot be put back is logged, not answered.
fn unwind_merge(face: &GithubFace, repo: &Repo, bare: &Path, base: &RefMove<'_>) {
    if let Err(e) = store::update_ref_cas(bare, base.name, base.old, base.new) {
        tracing::error!(error = %e, git_ref = base.name, "merge unwind left a ref moved");
    }
    for &number in &base.repins {
        let pinned = match face.world.pull_by_number(repo.id, number) {
            Ok(Some(p)) => face.git.pin_pull_ref(repo, number, &p.head_sha),
            Ok(None) => Ok(()),
            Err(e) => Err(e.into()),
        };
        if let Err(e) = pinned {
            tracing::error!(
                error = %e,
                git_ref = store::pull_head_ref(number),
                "merge unwind left a ref moved"
            );
        }
    }
}

#[derive(serde::Deserialize)]
struct CreateReviewBody {
    body: Option<String>,
    event: Option<String>,
    commit_id: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn create_review(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequestReview") {
        return refused;
    }
    let parsed: CreateReviewBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    // Inline review comments are their own subsystem; refuse loudly rather
    // than dropping them.
    if parsed.rest.contains_key("comments") {
        return orrery_unsupported_field("PullRequestReview", "comments");
    }
    // GitHub's default for a missing event is a PENDING (draft) review —
    // stateful UI machinery Orrery does not fake. Divergence, loudly.
    let Some(event) = parsed.event.as_deref() else {
        return orrery_unsupported_field(
            "PullRequestReview",
            "event (absent — pending reviews are not mocked; pass APPROVE, REQUEST_CHANGES, or COMMENT)",
        );
    };
    let state = match event {
        "APPROVE" => ReviewState::Approved,
        "REQUEST_CHANGES" => ReviewState::ChangesRequested,
        "COMMENT" => ReviewState::Commented,
        _ => return validation_invalid("PullRequestReview", "event"),
    };
    if actor.login.eq_ignore_ascii_case(&pull.author_login) {
        match state {
            ReviewState::Approved => {
                return validation_custom(
                    "PullRequestReview",
                    "Can not approve your own pull request",
                );
            }
            ReviewState::ChangesRequested => {
                return validation_custom(
                    "PullRequestReview",
                    "Can not request changes on your own pull request",
                );
            }
            ReviewState::Commented => {}
        }
    }
    let commit_id = match parsed.commit_id.as_deref() {
        None => None,
        Some(sha) if is_full_sha(sha) => Some(sha),
        Some(_) => return validation_invalid("PullRequestReview", "commit_id"),
    };

    match face
        .world
        .create_review(pull.id, actor.id, state, parsed.body.as_deref(), commit_id)
    {
        Ok(review) => json_response(StatusCode::OK, review_json(&face, &repo, &pull, &review)),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
struct CreateCommentBody {
    body: Option<String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn create_comment(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "IssueComment") {
        return refused;
    }
    let parsed: CreateCommentBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    let _ = parsed.rest;
    let Some(text) = parsed.body.as_deref().filter(|b| !b.is_empty()) else {
        return validation_missing("Issue", "body");
    };
    match face.world.create_issue_comment(pull.id, actor.id, text) {
        Ok(comment) => json_response(
            StatusCode::CREATED,
            comment_json(&face, &repo, &pull, &comment),
        ),
        Err(e) => internal(e),
    }
}

/// GitHub takes the labels to add as `{"labels": [...]}` or as a bare array.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AddLabelsBody {
    Wrapped {
        labels: Vec<serde_json::Value>,
        #[serde(flatten)]
        rest: BTreeMap<String, serde_json::Value>,
    },
    Bare(Vec<serde_json::Value>),
}

/// `POST /repos/{o}/{r}/issues/{n}/labels`: answers 200 with every label the
/// pull now carries.
pub(crate) async fn add_labels(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Label") {
        return refused;
    }
    let values = match serde_json::from_slice(&body) {
        Ok(AddLabelsBody::Wrapped { labels, rest }) => {
            let _ = rest;
            labels
        }
        Ok(AddLabelsBody::Bare(labels)) => labels,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    // GitHub also takes `{"name": ...}` objects here; no consumer sends them.
    let Some(names) = values
        .iter()
        .map(|v| v.as_str())
        .collect::<Option<Vec<&str>>>()
    else {
        return orrery_unsupported_field("Label", "labels[] as objects");
    };
    match face.world.add_pull_labels(pull.id, actor.id, &names) {
        Ok(pull) => json_response(
            StatusCode::OK,
            serde_json::Value::Array(pull.labels.iter().map(|n| label_json(n)).collect()),
        ),
        Err(WorldError::InvalidLabel(_)) => validation_invalid("Label", "name"),
        Err(e) => internal(e),
    }
}

/// `DELETE /repos/{o}/{r}/issues/{n}/labels/{name}`: answers 200 with the
/// labels left, or GitHub's 404 when the pull does not carry the label.
pub(crate) async fn remove_label(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number, name)): UrlPath<(String, String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Label") {
        return refused;
    }
    match face.world.remove_pull_label(pull.id, actor.id, &name) {
        Ok(pull) => json_response(
            StatusCode::OK,
            serde_json::Value::Array(pull.labels.iter().map(|n| label_json(n)).collect()),
        ),
        Err(WorldError::NoSuchLabel(_)) => {
            github_error(StatusCode::NOT_FOUND, "Label does not exist")
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize, Default)]
struct ReviewersBody {
    reviewers: Option<Vec<String>>,
    team_reviewers: Option<Vec<String>>,
}

/// `POST /repos/{o}/{r}/pulls/{n}/requested_reviewers`: 201 with the pull.
/// GitHub refuses the pull's author, and refuses anyone or any team that
/// cannot review the repository with one message naming none of them.
pub(crate) async fn request_reviewers(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let parsed: ReviewersBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    let users = parsed.reviewers.unwrap_or_default();
    let teams = parsed.team_reviewers.unwrap_or_default();
    if users.is_empty() && teams.is_empty() {
        return validation_missing("PullRequest", "reviewers");
    }
    let users: Vec<&str> = users.iter().map(String::as_str).collect();
    let teams: Vec<&str> = teams.iter().map(String::as_str).collect();
    match face
        .world
        .request_reviewers(pull.id, actor.id, &users, &teams)
    {
        Ok(pull) => json_response(
            StatusCode::CREATED,
            pull_json(&face, &repo, &owner_actor, &pull),
        ),
        Err(WorldError::ReviewerIsAuthor) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "message": "Validation Failed",
                "errors": ["Review cannot be requested from pull request author."],
            }),
        ),
        Err(WorldError::InvalidReviewer(_)) => github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!(
                "Reviews may only be requested from collaborators. One or more of the users or teams you specified is not a collaborator of the {} repository.",
                repo.full_name()
            ),
        ),
        Err(e) => internal(e),
    }
}

/// `DELETE /repos/{o}/{r}/pulls/{n}/requested_reviewers`: 200 with the
/// pull. GitHub requires `reviewers` (it may be empty); a name with no
/// pending request is skipped.
pub(crate) async fn remove_review_requests(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let parsed: ReviewersBody = if body.iter().all(u8::is_ascii_whitespace) {
        ReviewersBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
        }
    };
    let Some(users) = parsed.reviewers else {
        return validation_missing("PullRequest", "reviewers");
    };
    let teams = parsed.team_reviewers.unwrap_or_default();
    let users: Vec<&str> = users.iter().map(String::as_str).collect();
    let teams: Vec<&str> = teams.iter().map(String::as_str).collect();
    match face
        .world
        .remove_review_requests(pull.id, actor.id, &users, &teams)
    {
        Ok(pull) => json_response(StatusCode::OK, pull_json(&face, &repo, &owner_actor, &pull)),
        Err(e) => internal(e),
    }
}

/// A pull addressed by its number path segment. Non-numeric segments are a
/// GitHub 404, not a routing error — the route matched, the resource doesn't
/// exist. A store failure stays a 500, never a plausible 404.
pub(crate) fn pull_by_path_number(
    face: &GithubFace,
    repo: &Repo,
    number: &str,
) -> Result<Option<Pull>, Box<Response<Body>>> {
    let Ok(n) = number.parse::<i64>() else {
        return Ok(None);
    };
    face.world
        .pull_by_number(repo.id, n)
        .map_err(|e| Box::new(internal(e)))
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
