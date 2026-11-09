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
    resolve_repo, validation_custom, validation_custom_field, validation_invalid,
    validation_invalid_message, validation_missing,
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
    let _guard = lock.write().await;

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

    // GitHub ignores any other state and writes the rest of the request.
    let state = match parsed.state.as_deref() {
        Some(s @ ("open" | "closed")) => Some(s),
        _ => None,
    };
    let title = match parsed.title.as_deref().map(pull_title).transpose() {
        Ok(t) => t,
        Err(resp) => return *resp,
    };
    // The world write gate keeps a snapshot from copying, or a restore from
    // replacing, the world between the re-read below and the writes after
    // it, a reopen's ref move among them, so the pull row and the shas read
    // from the bare repository belong to the world the edit writes into. The
    // repo write lock keeps a push from interleaving with the edit: a push
    // moves the refs of the pulls open on its branch before it records them,
    // so a close or reopen must not land in between, and a new base's sha
    // and a reopened pull's head must still be their branches' tips when the
    // edit lands. Opening a pull takes the lock too, so none opens on a
    // reopened pull's head and base between the check and the reopen. The
    // pull is read again once both are held, so the state checks below see
    // current truth.
    let _gate = face.git.gate().read_owned().await;
    // TODO(ORR-71): an edit of only the title or body takes the write side
    // too, and so waits for every fetch in progress; only an edit that sets
    // `state` or `base` needs it.
    let lock = face.git.repo_lock(repo.id);
    let _guard = lock.write().await;
    let mut pull = match face.world.pull_by_number(repo.id, pull.number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let body = parsed.body.as_ref().map(|b| b.as_deref());
    // GitHub writes a request in this order, refusals included: the body;
    // then, unless it refuses a reopen, the title and the state change; then
    // the base, judged against the state the pull is left in. So a refusal
    // keeps every write that comes before it.
    let reopen_at = match state {
        Some("open") if pull.state == PullState::Closed => {
            match reopen_head(&face, &repo, &bare, &pull) {
                Err(fault) => return *fault,
                Ok(Ok(head)) => Some(head),
                Ok(Err(refused)) => {
                    if body.is_some()
                        && let Err(e) = face.world.edit_pull(pull.id, actor.id, None, body, None)
                    {
                        return internal(e);
                    }
                    // A base beside it is judged against the pull it left
                    // closed, and that refusal is the one GitHub answers.
                    if parsed.base.is_some() {
                        return closed_base_refused();
                    }
                    return refused;
                }
            }
        }
        _ => None,
    };
    let ends_closed = match state {
        Some(s) => s == "closed",
        None => pull.state == PullState::Closed,
    };
    let head_sha = reopen_at.as_deref().unwrap_or(&pull.head_sha);
    let (base, base_refused) = match parsed.base.as_deref() {
        None => (None, None),
        Some(b) => match resolve_base(&face, &repo, &bare, &pull, head_sha, b, ends_closed) {
            Err(fault) => return *fault,
            Ok(Ok(resolved)) => (Some(resolved), None),
            Ok(Err(refused)) => (None, Some(refused)),
        },
    };
    // A reopened pull takes its new base after the reopen, in GitHub's order.
    let (base_now, base_after) = match reopen_at {
        Some(_) => (None, base),
        None => (base, None),
    };

    if title.is_some() || body.is_some() || base_now.is_some() {
        let edited = face.world.edit_pull(
            pull.id,
            actor.id,
            title,
            body,
            base_now.as_ref().map(|(r, s)| (r.as_str(), s.as_str())),
        );
        pull = match edited {
            Ok(p) => p,
            Err(e) => return internal(e),
        };
    }

    // A state the pull is already in is a GitHub no-op: nothing is written
    // for it.
    if let Some(head) = reopen_at {
        // The ref moves before the reopen commits, as a push's re-pins do, so
        // a reader that sees the pull open sees its ref at the head. A reopen
        // that fails to commit puts the ref back on the head the pull kept.
        if let Err(e) = face.git.pin_pull_ref(&repo, pull.number, &head) {
            return internal(e);
        }
        pull = match face.world.reopen_pull(pull.id, actor.id, &head) {
            Ok(p) => p,
            Err(e) => {
                if let Err(undo) = face.git.pin_pull_ref(&repo, pull.number, &pull.head_sha) {
                    tracing::error!(error = %undo, "restoring a pull ref after a failed reopen");
                }
                return match e {
                    WorldError::PullExists(_) => duplicate_reopen(&pull),
                    e => internal(e),
                };
            }
        };
    } else if state == Some("closed") && pull.state == PullState::Open {
        pull = match face.world.close_pull(pull.id, actor.id) {
            Ok(p) => p,
            Err(e) => return internal(e),
        };
    }
    if let Some((r, s)) = &base_after {
        pull = match face
            .world
            .edit_pull(pull.id, actor.id, None, None, Some((r, s)))
        {
            Ok(p) => p,
            Err(e) => return internal(e),
        };
    }
    if let Some(refused) = base_refused {
        return refused;
    }

    json_response(StatusCode::OK, pull_json(&face, &repo, &owner_actor, &pull))
}

/// A check's outcome: its value, or GitHub's refusal as the inner `Err`.
/// The outer `Err` is a server fault, which the caller answers before it
/// writes anything, so a 500 never follows a partial write.
type Judged<T> = Result<Result<T, Response<Body>>, Box<Response<Body>>>;

fn fault(e: impl std::fmt::Display) -> Box<Response<Body>> {
    Box::new(internal(e))
}

/// The branch and sha a `base` edit moves the pull to, or GitHub's refusal
/// of it. `ends_closed` is whether the pull is closed once the request's
/// state change lands, and `head_sha` is the head it has then, since GitHub
/// judges the base after that change.
fn resolve_base(
    face: &GithubFace,
    repo: &Repo,
    bare: &Path,
    pull: &Pull,
    head_sha: &str,
    base: &str,
    ends_closed: bool,
) -> Judged<(String, String)> {
    let refused = |message: String| -> Judged<(String, String)> {
        Ok(Err(validation_invalid_message(
            "PullRequest",
            "base",
            &message,
        )))
    };
    if ends_closed {
        return Ok(Err(closed_base_refused()));
    }
    let Some(sha) = store::rev_parse(bare, &format!("refs/heads/{base}")).map_err(fault)? else {
        return refused(format!("Proposed base branch '{base}' was not found"));
    };
    if base == pull.base_ref {
        return Ok(Ok((base.to_string(), sha)));
    }
    let head = &pull.head_ref;
    // Naming the pull's own head lands here: a branch has no commits that
    // are not in itself.
    if store::rev_list_count(bare, &sha, head_sha).map_err(fault)? == 0 {
        return refused(format!(
            "There are no new commits between base branch '{base}' and head branch '{head}'"
        ));
    }
    if face
        .world
        .open_pull_between(repo.id, head, base)
        .map_err(fault)?
        .is_some()
    {
        return refused(format!(
            "A pull request already exists for base branch '{base}' and head branch '{head}'"
        ));
    }
    Ok(Ok((base.to_string(), sha)))
}

/// GitHub's refusal of a base on a pull that is closed.
fn closed_base_refused() -> Response<Body> {
    validation_invalid_message(
        "PullRequest",
        "base",
        "Cannot change the base branch of a closed pull request.",
    )
}

/// The head a closed pull reopens at: its head branch's tip. GitHub refuses
/// the reopen when the pull was merged; when its base or head branch is gone;
/// when another pull is open from the same head into the same base; and when
/// the tip does not descend from the pull's head, since it cannot tell what
/// changed while the pull was closed.
fn reopen_head(face: &GithubFace, repo: &Repo, bare: &Path, pull: &Pull) -> Judged<String> {
    let refused = |why: String| -> Judged<String> { Ok(Err(reopen_refused(&why))) };
    let (base, head) = (&pull.base_ref, &pull.head_ref);
    if pull.merged {
        return refused("The pull request cannot be reopened.".into());
    }
    let tip_of =
        |branch: &str| store::rev_parse(bare, &format!("refs/heads/{branch}")).map_err(fault);
    let base_gone = tip_of(base)?.is_none();
    let tip = match (base_gone, tip_of(head)?) {
        (false, Some(tip)) => tip,
        (true, Some(_)) => return refused(format!("The {base} branch has been deleted.")),
        (false, None) => return refused(format!("The {head} branch has been deleted.")),
        (true, None) => {
            return refused(format!("The {base} and {head} branches have been deleted."));
        }
    };
    if face
        .world
        .open_pull_between(repo.id, head, base)
        .map_err(fault)?
        .is_some()
    {
        return Ok(Err(duplicate_reopen(pull)));
    }
    if store::is_ancestor(bare, &pull.head_sha, &tip).map_err(fault)? {
        Ok(Ok(tip))
    } else {
        refused(format!("The {head} branch was force-pushed or recreated."))
    }
}

/// GitHub's refusal of a reopen: a `custom` error on `state`.
fn reopen_refused(why: &str) -> Response<Body> {
    validation_custom_field(
        "PullRequest",
        "state",
        &format!("state cannot be changed. {why}"),
    )
}

/// The reopen refusal for a pull whose head and base another open pull holds.
fn duplicate_reopen(pull: &Pull) -> Response<Body> {
    reopen_refused(&format!(
        "There is already an open pull request from {} to {}.",
        pull.head_ref, pull.base_ref
    ))
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

    // Serialize against pushes and other merges, and keep fetches out until
    // the merge is recorded or unwound, then re-read the pull so the state
    // checks below see current truth.
    let _gate = face.git.gate().read_owned().await;
    let lock = face.git.repo_lock(repo.id);
    let _guard = lock.write().await;
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
    // moves, so a failed read moves nothing.
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
    let mut pinned = 0;
    let mut record = || -> Result<(), orrery_face_git::GitFaceError> {
        for &number in &base.repins {
            face.git.pin_pull_ref(&repo, number, &merge_sha)?;
            pinned += 1;
        }
        face.world.record_merge(pull.id, actor.id, &base)?;
        Ok(())
    };
    if let Err(e) = record() {
        unwind_merge(&face, &repo, &bare, &base, &base.repins[..pinned]);
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
/// base branch returns to `base.old` by compare-and-swap, and the ref of each
/// pull request in `pinned`, the ones the merge moved, to the head the world
/// still pins it at. A retry then starts from the state the failed merge
/// started from. The route answers 500 either way, so a ref that cannot be
/// put back is logged, not answered.
fn unwind_merge(face: &GithubFace, repo: &Repo, bare: &Path, base: &RefMove<'_>, pinned: &[i64]) {
    if let Err(e) = store::update_ref_cas(bare, base.name, base.old, base.new) {
        tracing::error!(error = %e, git_ref = base.name, "merge unwind left a ref moved");
    }
    for &number in pinned {
        let restored = match face.world.pull_by_number(repo.id, number) {
            Ok(Some(p)) => face.git.pin_pull_ref(repo, number, &p.head_sha),
            Ok(None) => Ok(()),
            Err(e) => Err(e.into()),
        };
        if let Err(e) = restored {
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task::{Context, Waker};

    use orrery_world::{ActorKind, DEFAULT_T0_UNIX, World, WorldSecret};

    use super::*;

    /// The test holds the gate exclusively, as a restore does. Nothing else
    /// holds the repo lock, so an edit that did not take the gate would
    /// finish on its first poll; this one waits, and lands once the gate is
    /// released.
    #[tokio::test]
    async fn a_pull_edit_waits_on_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let world = Arc::new(
            World::create(
                &dir.path().join("world"),
                DEFAULT_T0_UNIX,
                &WorldSecret::LOCAL,
            )
            .unwrap(),
        );
        let face = GithubFace {
            world: world.clone(),
            git: orrery_face_git::GitFace::new(world.clone()),
            base_url: "http://world.test".to_string(),
        };
        let (alice, _) = world.create_actor("alice", ActorKind::User).unwrap();
        let repo = world
            .create_repo(alice.id, "widgets", None, false, "main", alice.id)
            .unwrap();
        let sha = "a".repeat(40);
        let pull = world
            .create_pull(NewPull {
                repo_id: repo.id,
                author_id: alice.id,
                title: "before",
                body: None,
                head_ref: "feature",
                head_sha: &sha,
                base_ref: "main",
                base_sha: &sha,
                draft: false,
            })
            .unwrap();

        let held = face.git.gate().write_owned().await;
        let mut edit = std::pin::pin!(patch_pull(
            State(face.clone()),
            UrlPath((
                "alice".to_string(),
                "widgets".to_string(),
                pull.number.to_string(),
            )),
            Extension(alice),
            HeaderMap::new(),
            Bytes::from_static(br#"{"title": "after"}"#),
        ));
        assert!(
            edit.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "the edit did not wait"
        );
        assert_eq!(
            world
                .pull_by_number(repo.id, pull.number)
                .unwrap()
                .unwrap()
                .title,
            "before"
        );

        drop(held);
        assert_eq!(edit.await.status(), StatusCode::OK);
        assert_eq!(
            world
                .pull_by_number(repo.id, pull.number)
                .unwrap()
                .unwrap()
                .title,
            "after"
        );
    }
}
