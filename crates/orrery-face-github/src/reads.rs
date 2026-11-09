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
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use base64::Engine as _;
use orrery_face_git::store;
use orrery_world::{Actor, ActorKind, PullState};
use sha2::{Digest, Sha256};

use crate::pulls::pull_by_path_number;
use crate::render::{
    check_run_json, comment_json, org_full_json, pull_json, repo_json, review_json, team_full_json,
    team_json, user_full_json,
};
use crate::{
    GithubFace, github_error, header_str, internal, json_response, orrery_unsupported_field,
    parse_form, resolve_repo, validation_invalid,
};

/// GitHub's listing defaults: 30 per page, cap 100, silently clamped —
/// the face is lax where GitHub is lax.
fn page_params(query: &str) -> (usize, usize) {
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

fn paginate<T>(items: Vec<T>, per_page: usize, page: usize) -> Vec<T> {
    items
        .into_iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .collect()
}

pub(crate) async fn get_repo(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    json_response(StatusCode::OK, repo_json(&face, &repo, &owner_actor))
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

/// The diff and patch media types, refused on `resource`: answering them
/// with JSON would be a plausible wrong answer.
// TODO(ORR-50): refuse every GitHub media type the face doesn't render, on
// every REST route; the body renderings (`full+json` and the like) on pulls,
// reviews, and comments, and raw contents, still get the plain JSON answer.
fn refuse_diff_media(headers: &HeaderMap, resource: &str) -> Option<Response<Body>> {
    let accept = header_str(headers, header::ACCEPT);
    (accept.contains("diff") || accept.contains("patch"))
        .then(|| orrery_unsupported_field(resource, "Accept"))
}

pub(crate) async fn get_pull(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let pull = match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(pull)) => pull,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = refuse_diff_media(&headers, "PullRequest") {
        return refused;
    }
    json_response(StatusCode::OK, pull_json(&face, &repo, &owner_actor, &pull))
}

pub(crate) async fn list_reviews(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
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
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(_)) => json_response(StatusCode::OK, serde_json::json!([])),
        Ok(None) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => *resp,
    }
}

pub(crate) async fn list_issue_comments(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
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
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    // GitHub accepts a sha or a ref name here; a poller sends the PR head sha.
    let sha = if is_full_sha(&git_ref) {
        git_ref.clone()
    } else {
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
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let query = query.unwrap_or_default();
    let rev = query_param(&query, "ref").unwrap_or_else(|| "HEAD".to_string());
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let spec = format!("{rev}:{path}");
    // A directory answers 404 here where GitHub would return a listing —
    // accepted divergence, because a consumer reading contents fetches a file
    // and treats exactly 404 as absent.
    let blob_sha = match store::rev_parse(&bare, &spec) {
        Ok(Some(sha)) => sha,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    let bytes = match store::cat_file_blob(&bare, &spec) {
        Ok(Some(b)) => b,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
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
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let refs = match store::list_refs(&bare) {
        Ok(r) => r,
        Err(e) => return internal(e),
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

fn changed_file_json(file: &store::ChangedFile) -> serde_json::Value {
    let mut json = serde_json::json!({ "filename": file.path, "status": file.status.rest() });
    if let Some(previous) = &file.previous {
        json["previous_filename"] = serde_json::Value::String(previous.clone());
    }
    json
}

/// The tip of a branch, with the tag a conditional read keys on. The tag is
/// derived from the exact body, so it changes iff the tip does; a replayed tag
/// answers 304 with no body, which costs nothing against GitHub's rate limit.
pub(crate) async fn get_branch_ref(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, branch)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let sha = match store::rev_parse(&bare, &format!("refs/heads/{branch}")) {
        Ok(Some(sha)) => sha,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
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
fn resolve_commit(
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
    let files: Vec<serde_json::Value> = store::changed_files(bare, base, head)?
        .iter()
        .take(COMPARE_FILE_CAP)
        .map(changed_file_json)
        .collect();
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
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = refuse_diff_media(&headers, "Comparison") {
        return refused;
    }
    let Some((base_rev, head_rev)) = basehead.split_once("...") else {
        return github_error(StatusCode::NOT_FOUND, "Not Found");
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let mut ends = [String::new(), String::new()];
    for (end, rev) in ends.iter_mut().zip([base_rev, head_rev]) {
        match resolve_commit(&bare, rev) {
            Ok(Some(sha)) => *end = sha,
            Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
            Err(e) => return internal(e),
        }
    }
    match compare_json(&bare, &ends[0], &ends[1]) {
        Ok(json) => json_response(StatusCode::OK, json),
        Err(e) => internal(e),
    }
}

/// The files a pull request changes, paged. The base is the base branch's
/// tip while the pull is open and the pinned base sha once it is terminal,
/// as for the pull's diff totals.
pub(crate) async fn list_pull_files(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, number)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
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
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    let base = if pull.state == PullState::Open {
        match store::rev_parse(&bare, &format!("refs/heads/{}", pull.base_ref)) {
            Ok(Some(tip)) => tip,
            Ok(None) => pull.base_sha.clone(),
            Err(e) => return internal(e),
        }
    } else {
        pull.base_sha.clone()
    };
    let files = match store::changed_files(&bare, &base, &pull.head_sha) {
        Ok(f) => f,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let items: Vec<serde_json::Value> = paginate(files, per_page, page)
        .iter()
        .map(changed_file_json)
        .collect();
    json_response(StatusCode::OK, serde_json::Value::Array(items))
}

pub(crate) async fn get_user(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    json_response(StatusCode::OK, user_full_json(&face, &actor))
}

/// An account by the login REST spells it with: a user's, an organization's,
/// or an App's bot as `<slug>[bot]`. Any other login is GitHub's 404.
pub(crate) async fn get_user_by_login(
    State(face): State<GithubFace>,
    UrlPath(username): UrlPath<String>,
) -> Response<Body> {
    let (login, bot) = match username.strip_suffix("[bot]") {
        Some(slug) => (slug, true),
        None => (username.as_str(), false),
    };
    match face.world.actor_by_login(login) {
        Ok(Some(a)) if (a.kind == ActorKind::Bot) == bot && a.kind != ActorKind::System => {
            json_response(StatusCode::OK, user_full_json(&face, &a))
        }
        Ok(_) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => internal(e),
    }
}

/// An organization by login. A user's login, or one nobody holds, is
/// GitHub's 404.
pub(crate) async fn get_org(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
) -> Response<Body> {
    match face.world.actor_by_login(&org) {
        Ok(Some(a)) if a.kind == ActorKind::Org => {
            json_response(StatusCode::OK, org_full_json(&face, &a))
        }
        Ok(_) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => internal(e),
    }
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
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
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
) -> Response<Body> {
    let org = match face.world.actor_by_login(&org) {
        Ok(Some(a)) if a.kind == orrery_world::ActorKind::Org => a,
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
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
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
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
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
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
