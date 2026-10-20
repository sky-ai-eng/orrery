//! The poller read surface (spec/surface-github.md §1): what TF's Go client
//! reads on every poll cycle, plus the immediate PR-detail reads. Pagination
//! is GitHub's hand-rolled regime — `?per_page&page`, no Link headers, which
//! the main TF client never parses (spec/grammar.md) — and the pulls list
//! carries the one ETag in the poll path: `If-None-Match` answers 304 with
//! the body elided, exactly the fast path `repositories.pulls_etag` drives.
//!
//! Rate-limit headers are deliberately absent everywhere: TF treats missing
//! `X-RateLimit-*` as "rate limiting off" (a real GHES shape) and leaves its
//! budget state untouched.

use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use base64::Engine as _;
use orrery_face_git::store;
use orrery_world::{Actor, PullState};
use sha2::{Digest, Sha256};

use crate::pulls::pull_by_path_number;
use crate::render::{check_run_json, comment_json, pull_json, repo_json, review_json};
use crate::{GithubFace, github_error, internal, json_response, resolve_repo, validation_invalid};

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
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
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

    // The one conditional read in TF's poll path. The tag is derived from
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
) -> Response<Body> {
    let (repo, owner_actor) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    match pull_by_path_number(&face, &repo, &number) {
        Ok(Some(pull)) => {
            json_response(StatusCode::OK, pull_json(&face, &repo, &owner_actor, &pull))
        }
        Ok(None) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => *resp,
    }
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
    // GitHub accepts a sha or a ref name here; TF sends the PR head sha.
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
    // accepted divergence: TF's only consumer fetches files and treats
    // exactly 404 as absent.
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

pub(crate) async fn get_user(Extension(actor): Extension<Actor>) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "login": crate::render::rest_login(&actor),
            "id": actor.id,
            "node_id": format!("O_actor_{}", actor.id),
            "name": actor.login,
            "avatar_url": format!("https://orrery.invalid/avatars/{}", actor.id),
            "type": crate::render::owner_type(actor.kind),
            "site_admin": false,
        }),
    )
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

/// Teams are unmodeled in the kernel, so an organization truthfully has none:
/// an empty page, as GitHub answers for an org that never created a team. A
/// login that is not an organization is GitHub's 404.
pub(crate) async fn list_org_teams(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
) -> Response<Body> {
    match face.world.actor_by_login(&org) {
        Ok(Some(a)) if a.kind == orrery_world::ActorKind::Org => {
            json_response(StatusCode::OK, serde_json::json!([]))
        }
        Ok(_) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => internal(e),
    }
}

/// The teams the authenticated user belongs to. Teams are unmodeled, so a user
/// belongs to none. An installation token has no user to ask about.
pub(crate) async fn list_user_teams(Extension(actor): Extension<Actor>) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    json_response(StatusCode::OK, serde_json::json!([]))
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
    // Only the ordering TF requests is built; a different sort would be
    // silently wrong, so it refuses loudly instead.
    match query_param(&query, "sort").as_deref() {
        None | Some("pushed") => {}
        Some(other) => return crate::orrery_unsupported_field(&format!("sort={other}")),
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
