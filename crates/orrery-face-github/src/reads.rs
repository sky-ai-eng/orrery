//! The poller read surface (spec/surface-github.md §1): what a polling
//! consumer reads on every cycle, plus the immediate PR-detail reads.
//! Pagination is GitHub's hand-rolled regime — `?per_page&page`, no Link
//! headers, so a client pages until a short page (spec/grammar.md) — and the
//! pulls list and a branch's ref carry an ETag: `If-None-Match` answers 304
//! with the body elided, the fast path a consumer takes by keeping the last
//! tag it saw.
//!
//! Rate-limit headers are deliberately absent everywhere: a client reads
//! missing `X-RateLimit-*` as "rate limiting off" (a real GHES shape) and
//! leaves its budget state untouched.

use axum::body::Body;
use axum::extract::{Extension, OriginalUri, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use base64::Engine as _;
use orrery_face_git::store;
use orrery_world::{Actor, ActorKind, Caller, Door, PullState};
use sha2::{Digest, Sha256};

use crate::pulls::pull_by_path_number;
use crate::render::{
    MergeRefs, check_run_json, comment_json, full_pull_json, mergeability, org_full_json,
    pull_json, repo_json, review_json, team_full_json, team_json, user_full_json,
};
use crate::{
    GithubFace, github_error, internal, json_response, media, orrery_unsupported_field, parse_form,
    resolve_org, resolve_repo, validation_invalid,
};

/// GitHub's listing defaults: 30 per page, cap 100, silently clamped —
/// the face is lax where GitHub is lax.
pub(crate) fn page_params(query: &str) -> (usize, usize) {
    let per_page = query_param(query, "per_page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let page = query_param(query, "page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
    (per_page, page)
}

fn query_param(query: &str, name: &str) -> Option<String> {
    parse_form(query.as_bytes()).remove(name)
}

pub(crate) fn paginate<T>(
    items: impl IntoIterator<Item = T>,
    per_page: usize,
    page: usize,
) -> Vec<T> {
    items
        .into_iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .collect()
}

pub(crate) async fn get_repo(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    json_response(StatusCode::OK, repo_json(&face, &repo, &owner_actor))
}

/// A repository by GitHub's id for it, answered as `GET /repos/{o}/{r}`
/// answers it.
pub(crate) async fn get_repo_by_id(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    // TODO(ORR-88): GitHub answers a renamed repository here under its new
    // name, and a deleted one 404; a world neither renames nor deletes one,
    // so every repository answers under the name it was created with.
    let repo = match id.parse::<i64>().map(|id| face.world.repo_by_id(id)) {
        Ok(Ok(Some(repo))) => repo,
        Ok(Ok(None)) | Err(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Ok(Err(e)) => return internal(e),
    };
    let owner = match face.world.actor_by_login(&repo.owner_login) {
        Ok(Some(owner)) => owner,
        Ok(None) => return internal("repo owner actor missing"),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    json_response(StatusCode::OK, repo_json(&face, &repo, &owner))
}

pub(crate) async fn list_pulls(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let query = query.unwrap_or_default();
    let state = match query_param(&query, "state").as_deref() {
        None | Some("open") => Some(PullState::Open),
        Some("closed") => Some(PullState::Closed),
        Some("all") => None,
        Some(_) => return validation_invalid("PullRequest", "state"),
    };
    let (per_page, page) = page_params(&query);
    let pulls = match face.world.list_pulls_for_repo(repo.id, state) {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    let items: Vec<serde_json::Value> = paginate(pulls, per_page, page)
        .iter()
        .map(|p| pull_json(&face, &repo, &owner_actor, p))
        .collect();
    let body = serde_json::Value::Array(items).to_string();

    // A poller's conditional read of a repo's pulls. The tag is derived from
    // the exact body, so it changes iff the answer changes — and an
    // unchanged page 1 costs a 304 with no body, which is what lets a quiet
    // repo skip discovery entirely.
    let etag = format!("W/\"{}\"", hex::encode(&Sha256::digest(&body)[..16]));
    let matched = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.trim() == etag);
    let status = if matched {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header("etag", etag)
        .header("content-type", "application/json; charset=utf-8")
        .body(if matched {
            Body::empty()
        } else {
            Body::from(body)
        })
        .expect("static response parts are valid")
}

pub(crate) async fn get_pull(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its refs holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    // The pull and its branches' tips are read under one hold, so a merge or
    // a push lands before all of them or after all of them.
    let held = face.git.read_lock(repo.id).await;
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(pull)) => pull,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered_unless_diff(&headers, "PullRequest") {
        return refused;
    }
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let refs = match MergeRefs::read(&bare, &pull) {
        Ok(refs) => refs,
        Err(e) => return internal(e),
    };
    drop(held);
    if let Some(media) = media::diff_media(&headers) {
        return pull_diff(&bare, &refs.diff_base(&pull), &pull.head_sha, &media);
    }
    let verdict = match mergeability(&face, &repo, &pull, &refs, caller.via(Door::Rest)) {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    json_response(
        StatusCode::OK,
        full_pull_json(&face, &repo, &owner_actor, &pull, verdict),
    )
}

/// A pull request's diff in the diff media type, or GitHub's 406 when it
/// changes more files than GitHub renders a pull request's diff for.
fn pull_diff(bare: &std::path::Path, base: &str, head: &str, media: &str) -> Response<Body> {
    let diff = match store::diff(bare, base, head) {
        Ok(diff) => diff,
        Err(e) => return internal(e),
    };
    if store::file_count(&diff) > PULL_DIFF_FILE_CAP {
        return json_response(
            StatusCode::NOT_ACCEPTABLE,
            serde_json::json!({
                    "message": format!(
                        "Sorry, the diff exceeded the maximum number of files ({PULL_DIFF_FILE_CAP}). \
                         Consider using 'List pull requests files' API or locally cloning the \
                         repository instead."
                    ),
                    "errors": [{ "resource": "PullRequest", "field": "diff", "code": "too_large" }],
            }),
        );
    }
    diff_response(media, diff)
}

/// A diff, its content type the diff media type the request listed.
fn diff_response(media: &str, diff: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", format!("{media}; charset=utf-8"))
        .body(Body::from(diff))
        .expect("static response parts are valid")
}

pub(crate) async fn list_reviews(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
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
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequestReview") {
        return refused;
    }
    let (per_page, page) = page_params(&query.unwrap_or_default());
    match face.world.list_reviews_for_pull(pull.id) {
        Ok(reviews) => {
            let items: Vec<serde_json::Value> = paginate(reviews, per_page, page)
                .iter()
                .map(|r| review_json(&face, &repo, &pull, r))
                .collect();
            json_response(StatusCode::OK, serde_json::Value::Array(items))
        }
        Err(e) => internal(e),
    }
}

/// Inline review comments. Always empty, truthfully: the review write path
/// refuses `comments[]` loudly, so no world contains any — an empty list is
/// this world's real answer, not a stub.
pub(crate) async fn list_review_comments(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(_)) => {}
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequestReviewComment") {
        return refused;
    }
    json_response(StatusCode::OK, serde_json::json!([]))
}

pub(crate) async fn list_issue_comments(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
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
    if let Some(refused) = media::refuse_unrendered(&headers, "IssueComment") {
        return refused;
    }
    let (per_page, page) = page_params(&query.unwrap_or_default());
    match face.world.list_issue_comments_for_pull(pull.id) {
        Ok(comments) => {
            let items: Vec<serde_json::Value> = paginate(comments, per_page, page)
                .iter()
                .map(|c| comment_json(&face, &repo, &pull, c))
                .collect();
            json_response(StatusCode::OK, serde_json::Value::Array(items))
        }
        Err(e) => internal(e),
    }
}

pub(crate) async fn list_check_runs_for_ref(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, git_ref)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    // GitHub accepts a sha or a ref name here; a poller sends the PR head sha.
    let sha = if is_full_sha(&git_ref) {
        git_ref.clone()
    } else {
        let _refs = face.git.read_lock(repo.id).await;
        let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
        match store::rev_parse(&bare, &format!("refs/heads/{git_ref}")) {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                return github_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &format!("No commit found for ref {git_ref}"),
                );
            }
            Err(e) => return internal(e),
        }
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckRun") {
        return refused;
    }
    let (per_page, page) = page_params(&query.unwrap_or_default());
    match face.world.list_check_runs_for_sha(repo.id, &sha) {
        Ok(runs) => {
            let total = runs.len();
            let items: Vec<serde_json::Value> = paginate(runs, per_page, page)
                .iter()
                .map(|r| check_run_json(&face, &repo, r))
                .collect();
            json_response(
                StatusCode::OK,
                serde_json::json!({ "total_count": total, "check_runs": items }),
            )
        }
        Err(e) => internal(e),
    }
}

pub(crate) async fn get_contents(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, path)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let query = query.unwrap_or_default();
    let rev = query_param(&query, "ref").unwrap_or_else(|| "HEAD".to_string());
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let spec = format!("{rev}:{path}");
    let refs = face.git.read_lock(repo.id).await;
    // A directory answers 404 here where GitHub would return a listing —
    // accepted divergence, because a consumer reading contents fetches a file
    // and treats exactly 404 as absent.
    let blob_sha = match store::rev_parse(&bare, &spec) {
        Ok(Some(sha)) => sha,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    drop(refs);
    // Read by its id, the blob is the one `rev` named above, whatever the
    // refs have done since.
    let bytes = match store::cat_file_blob(&bare, &blob_sha) {
        Ok(Some(b)) => b,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    // For a file, the object media type answers the object the JSON one does.
    if let Some(refused) = media::refuse_unrendered_except(&headers, "Content", &["object"]) {
        return refused;
    }
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    let name = path.rsplit('/').next().unwrap_or(&path);
    // GitHub wraps this base64 in newlines; consumers strip them, so the
    // unwrapped form is compatible and simpler.
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "type": "file",
            "encoding": "base64",
            "name": name,
            "path": path,
            "sha": blob_sha,
            "size": bytes.len(),
            "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "url": format!("{base}/api/v3/repos/{full_name}/contents/{path}"),
            "html_url": format!("{base}/{full_name}/blob/{rev}/{path}"),
            "download_url": serde_json::Value::Null,
        }),
    )
}

pub(crate) async fn list_branches(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Branch") {
        return refused;
    }
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let refs = {
        let _refs = face.git.read_lock(repo.id).await;
        match store::list_refs(&bare) {
            Ok(r) => r,
            Err(e) => return internal(e),
        }
    };
    let branches: Vec<serde_json::Value> = refs
        .iter()
        .filter_map(|(name, sha)| {
            name.strip_prefix("refs/heads/").map(|branch| {
                serde_json::json!({
                    "name": branch,
                    "commit": { "sha": sha },
                    "protected": false,
                })
            })
        })
        .collect();
    let (per_page, page) = page_params(&query.unwrap_or_default());
    json_response(
        StatusCode::OK,
        serde_json::Value::Array(paginate(branches, per_page, page)),
    )
}

/// GitHub's own limits on a compare: it lists at most this many files and,
/// unpaginated, this many commits, however large the range.
const COMPARE_FILE_CAP: usize = 300;
const COMPARE_COMMIT_CAP: usize = 250;

/// The most files a pull request's diff media type renders; past it GitHub
/// answers 406.
const PULL_DIFF_FILE_CAP: usize = 300;

/// The size at which a changed file's `patch` is left out, the file keeping
/// its line counts.
const PATCH_BYTE_LIMIT: usize = 100 * 1024;

/// The most lines, and bytes, the patches of one response's changed files
/// hold. A patch's bytes count one more for the newline that ends it.
const RESPONSE_PATCH_LINE_CAP: usize = 20_000;
const RESPONSE_PATCH_BYTE_CAP: usize = 1 << 20;

/// Changed files as one response lists them. Once the next file's patch
/// would take the response's patches past either cap, that file and every
/// file after it read zero counts and carry no patch. A patch left out for
/// its size counts toward neither.
fn changed_files_json(diffs: &[store::FileDiff]) -> Vec<serde_json::Value> {
    let mut lines = 0;
    let mut bytes = 0;
    let mut counted = true;
    diffs
        .iter()
        .map(|diff| {
            if counted && let Some(patch) = shown_patch(diff) {
                lines += patch.split('\n').count();
                bytes += patch.len() + 1;
                counted = lines <= RESPONSE_PATCH_LINE_CAP && bytes <= RESPONSE_PATCH_BYTE_CAP;
            }
            changed_file_json(diff, counted)
        })
        .collect()
}

/// A changed file as GitHub lists it. A file past its response's caps is not
/// `counted`: its counts read zero and it has no patch.
fn changed_file_json(diff: &store::FileDiff, counted: bool) -> serde_json::Value {
    let file = &diff.file;
    let (additions, deletions) = if counted {
        (diff.additions, diff.deletions)
    } else {
        (0, 0)
    };
    let mut json = serde_json::json!({
        "filename": file.path,
        "status": file.status.rest(),
        "additions": additions,
        "deletions": deletions,
        "changes": additions + deletions,
    });
    if counted && let Some(patch) = shown_patch(diff) {
        json["patch"] = serde_json::Value::String(patch.to_string());
    }
    if let Some(previous) = &file.previous {
        json["previous_filename"] = serde_json::Value::String(previous.clone());
    }
    json
}

/// The patch a changed file carries, unless it is too large to.
fn shown_patch(diff: &store::FileDiff) -> Option<&str> {
    diff.patch
        .as_deref()
        .filter(|patch| patch.len() < PATCH_BYTE_LIMIT)
}

/// The tip of a branch, with the tag a conditional read keys on. The tag is
/// derived from the exact body, so it changes iff the tip does; a replayed tag
/// answers 304 with no body, which costs nothing against GitHub's rate limit.
pub(crate) async fn get_branch_ref(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, branch)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let sha = {
        let _refs = face.git.read_lock(repo.id).await;
        match store::rev_parse(&bare, &format!("refs/heads/{branch}")) {
            Ok(Some(sha)) => sha,
            Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
            Err(e) => return internal(e),
        }
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Ref") {
        return refused;
    }
    let api = format!(
        "{}/api/v3/repos/{}",
        face.base_url.trim_end_matches('/'),
        repo.full_name()
    );
    let body = serde_json::json!({
        "ref": format!("refs/heads/{branch}"),
        "url": format!("{api}/git/refs/heads/{branch}"),
        "object": { "sha": sha, "type": "commit", "url": format!("{api}/git/commits/{sha}") },
    })
    .to_string();
    let etag = format!("W/\"{}\"", hex::encode(&Sha256::digest(&body)[..16]));
    let matched = headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.trim() == etag);
    Response::builder()
        .status(if matched {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header("etag", etag)
        .header("content-type", "application/json; charset=utf-8")
        .body(if matched {
            Body::empty()
        } else {
            Body::from(body)
        })
        .expect("static response parts are valid")
}

/// A commit-ish as a commit id, or `None` when it names nothing.
pub(crate) fn resolve_commit(
    bare: &std::path::Path,
    rev: &str,
) -> Result<Option<String>, orrery_face_git::GitFaceError> {
    if rev.is_empty() {
        return Ok(None);
    }
    store::rev_parse(bare, &format!("{rev}^{{commit}}"))
}

fn compare_json(
    bare: &std::path::Path,
    base: &str,
    head: &str,
) -> Result<serde_json::Value, orrery_face_git::GitFaceError> {
    let ahead_by = store::rev_list_count(bare, base, head)?;
    let behind_by = store::rev_list_count(bare, head, base)?;
    let status = match (ahead_by, behind_by) {
        (0, 0) => "identical",
        (_, 0) => "ahead",
        (0, _) => "behind",
        _ => "diverged",
    };
    let mut diffs = store::file_diffs(bare, base, head)?;
    diffs.truncate(COMPARE_FILE_CAP);
    let files = changed_files_json(&diffs);
    let commits: Vec<serde_json::Value> = store::commits_between(bare, base, head)?
        .iter()
        .take(COMPARE_COMMIT_CAP)
        .map(|c| {
            let parents: Vec<serde_json::Value> = c
                .parents
                .iter()
                .map(|p| serde_json::json!({ "sha": p }))
                .collect();
            serde_json::json!({ "sha": c.sha, "parents": parents })
        })
        .collect();
    Ok(serde_json::json!({
        "status": status,
        "ahead_by": ahead_by,
        "behind_by": behind_by,
        "total_commits": ahead_by,
        "commits": commits,
        "files": files,
    }))
}

/// `base...head` as JSON.
pub(crate) async fn compare(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, basehead)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let Some((base_rev, head_rev)) = basehead.split_once("...") else {
        return github_error(StatusCode::NOT_FOUND, "Not Found");
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    // Both ends resolve against one state of the refs; the comparison after
    // reads only the commits they name.
    let refs = face.git.read_lock(repo.id).await;
    let mut ends = [String::new(), String::new()];
    for (end, rev) in ends.iter_mut().zip([base_rev, head_rev]) {
        match resolve_commit(&bare, rev) {
            Ok(Some(sha)) => *end = sha,
            Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
            Err(e) => return internal(e),
        }
    }
    drop(refs);
    if let Some(refused) = media::refuse_unrendered_unless_diff(&headers, "Comparison") {
        return refused;
    }
    if let Some(media) = media::diff_media(&headers) {
        return match store::diff(&bare, &ends[0], &ends[1]) {
            Ok(diff) => diff_response(&media, diff),
            Err(e) => internal(e),
        };
    }
    match compare_json(&bare, &ends[0], &ends[1]) {
        Ok(json) => json_response(StatusCode::OK, json),
        Err(e) => internal(e),
    }
}

/// The files a pull request changes, paged, compared as the pull's diff is
/// ([`MergeRefs::diff_base`]).
pub(crate) async fn list_pull_files(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its files holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    // The pull and its base branch's tip are read under one hold, so a merge
    // or a push to the base lands before both or after both.
    let refs = face.git.read_lock(repo.id).await;
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(p)) => p,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "PullRequest") {
        return refused;
    }
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let base = match MergeRefs::read(&bare, &pull) {
        Ok(tips) => tips.diff_base(&pull),
        Err(e) => return internal(e),
    };
    drop(refs);
    let files = match face.git.file_diffs(&repo, &base, &pull.head_sha) {
        Ok(f) => f,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let items = changed_files_json(&paginate(files.iter().cloned(), per_page, page));
    json_response(StatusCode::OK, serde_json::Value::Array(items))
}

pub(crate) async fn get_user(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "User") {
        return refused;
    }
    json_response(StatusCode::OK, user_full_json(&face, &actor))
}

/// The account `username` names, as REST spells a login: a user's, an
/// organization's, or an App's bot as `<slug>[bot]`. Any other login is
/// GitHub's 404.
pub(crate) fn resolve_account(
    face: &GithubFace,
    username: &str,
) -> Result<Actor, Box<Response<Body>>> {
    let (login, bot) = match username.strip_suffix("[bot]") {
        Some(slug) => (slug, true),
        None => (username, false),
    };
    match face.world.actor_by_login(login) {
        Ok(Some(a)) if (a.kind == ActorKind::Bot) == bot && a.kind != ActorKind::System => Ok(a),
        Ok(_) => Err(Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// An account by the login REST spells it with ([`resolve_account`]).
pub(crate) async fn get_user_by_login(
    State(face): State<GithubFace>,
    UrlPath(username): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let account = match resolve_account(&face, &username) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "User") {
        return refused;
    }
    json_response(StatusCode::OK, user_full_json(&face, &account))
}

/// An account by GitHub's id for it, under the login it holds now: a
/// user, an organization, or an App's bot, answered as
/// `GET /users/{username}` answers it. The system actor, and an id no
/// account has, are GitHub's 404.
///
/// The route takes every `GET /user/<segment>`, and GitHub serves other
/// endpoints there (`/user/keys`, `/user/orgs`, ...), so a segment that is
/// not an id is surface Orrery does not serve, never a plausible 404.
pub(crate) async fn get_user_by_id(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    let Ok(id) = id.parse::<i64>() else {
        return crate::not_mocked("GET", uri.path());
    };
    let account = match face.world.actor_by_id(id) {
        Ok(Some(a)) if a.kind != ActorKind::System => a,
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "User") {
        return refused;
    }
    json_response(StatusCode::OK, user_full_json(&face, &account))
}

/// An organization by login. A user's login, or one nobody holds, is
/// GitHub's 404.
pub(crate) async fn get_org(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let org = match resolve_org(&face, &org) {
        Ok(org) => org,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Organization") {
        return refused;
    }
    json_response(StatusCode::OK, org_full_json(&face, &org))
}

/// A bot actor stands for a GitHub App installation, and GitHub refuses an
/// installation token on the authenticated-user routes: there is no user
/// behind it.
pub(crate) fn refuse_integration(actor: &Actor) -> Option<Response<Body>> {
    (actor.kind == orrery_world::ActorKind::Bot).then(|| {
        json_response(
            StatusCode::FORBIDDEN,
            serde_json::json!({
                "message": "Resource not accessible by integration",
                "documentation_url": "https://docs.github.com/rest",
                "status": "403",
            }),
        )
    })
}

/// The actor's stored addresses, primary first. Every actor starts with one
/// derived `.invalid` address; the control plane sets the rest.
pub(crate) async fn get_user_emails(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "UserEmail") {
        return refused;
    }
    match face.world.actor_emails(actor.id) {
        Ok(emails) => json_response(
            StatusCode::OK,
            serde_json::Value::Array(
                emails
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "email": e.email,
                            "primary": e.primary,
                            "verified": e.verified,
                            "visibility": if e.primary { serde_json::json!("private") } else { serde_json::Value::Null },
                        })
                    })
                    .collect(),
            ),
        ),
        Err(e) => internal(e),
    }
}

/// An organization's teams, oldest first. A login that is not an
/// organization is GitHub's 404.
pub(crate) async fn list_org_teams(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let org = match resolve_org(&face, &org) {
        Ok(org) => org,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Team") {
        return refused;
    }
    let teams = match face.world.list_org_teams(org.id, None) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let items: Vec<serde_json::Value> = paginate(teams, per_page, page)
        .iter()
        .map(|t| team_json(&face, t))
        .collect();
    json_response(StatusCode::OK, serde_json::Value::Array(items))
}

/// The teams the authenticated user is on, across organizations, as full
/// team objects. An installation token has no user to ask about.
pub(crate) async fn list_user_teams(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "Team") {
        return refused;
    }
    let teams = match face.world.list_teams_for_member(actor.id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let items = paginate(teams, per_page, page)
        .iter()
        .map(|t| team_full_json(&face, t))
        .collect::<Result<Vec<_>, _>>();
    match items {
        Ok(items) => json_response(StatusCode::OK, serde_json::Value::Array(items)),
        Err(e) => internal(e),
    }
}

pub(crate) async fn list_user_repos(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    let query = query.unwrap_or_default();
    // Only `sort=pushed` is built; a different sort would be
    // silently wrong, so it refuses loudly instead.
    match query_param(&query, "sort").as_deref() {
        None | Some("pushed") => {}
        Some(other) => return orrery_unsupported_field("Repository", &format!("sort={other}")),
    }
    let ascending = match query_param(&query, "direction").as_deref() {
        None | Some("desc") => false,
        Some("asc") => true,
        Some(_) => return validation_invalid("Repository", "direction"),
    };
    let mut repos = match face.world.list_repos_owned_by(actor.id) {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    if ascending {
        repos.reverse();
    }
    let (per_page, page) = page_params(&query);
    let items: Vec<serde_json::Value> = paginate(repos, per_page, page)
        .iter()
        .map(|r| repo_json(&face, r, &actor))
        .collect();
    json_response(StatusCode::OK, serde_json::Value::Array(items))
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
