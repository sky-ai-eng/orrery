//! The GitHub REST face. Grows strictly on demand (spec/README.md rule 1):
//! every endpoint here exists because a consumer needs it, and is listed in
//! `spec/surface-github.md`. Currently the authoring surface: repo
//! creation, org membership and teams, pull requests (open/edit/merge),
//! review requests, reviews, issue comments, labels, and check runs — the
//! writes a scenario driver performs (spec/usage.md).
//!
//! Face endpoints mimic real GitHub, including its lax body decoding: unknown
//! request fields are ignored, exactly as GitHub ignores them. The exception
//! is a known-but-unsupported *behavioral* field (`auto_init`, ...): silently
//! ignoring one would make the caller's world silently wrong, so those are
//! rejected loudly with the field named (the NOT-MOCKED principle applied at
//! field granularity).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::routing::{delete, get, patch, post, put};
use orrery_face_git::GitFace;
use orrery_world::{Actor, ActorKind, Repo, World, WorldError};

mod checks;
pub mod graphql;
mod media;
pub mod nodeid;
pub mod oauth;
mod orgs;
mod pulls;
mod reads;
mod render;

pub use render::repo_json;

#[derive(Clone)]
pub struct GithubFace {
    pub world: Arc<World>,
    pub git: Arc<GitFace>,
    /// External base URL of this instance, e.g. `http://127.0.0.1:4180` —
    /// what `html_url`/`clone_url` are rendered against.
    pub base_url: String,
}

pub fn router(face: GithubFace) -> Router {
    // Each route names the resource it is about, which a refused media type
    // reports, and any rendering it serves beside JSON (`media`).
    Router::new()
        .route(
            "/orgs/{org}",
            media::json("Organization", get(reads::get_org)),
        )
        .route(
            "/orgs/{org}/repos",
            media::json("Repository", post(create_org_repo)),
        )
        .route(
            "/orgs/{org}/teams",
            media::json("Team", get(reads::list_org_teams).post(orgs::create_team)),
        )
        .route(
            "/orgs/{org}/teams/{team_slug}",
            media::json("Team", delete(orgs::delete_team)),
        )
        .route(
            "/orgs/{org}/teams/{team_slug}/memberships/{username}",
            media::json(
                "TeamMember",
                put(orgs::set_team_membership).delete(orgs::remove_team_membership),
            ),
        )
        .route(
            "/orgs/{org}/memberships/{username}",
            media::json("Membership", put(orgs::set_org_membership)),
        )
        .route(
            "/users/{username}",
            media::json("User", get(reads::get_user_by_login)),
        )
        .route("/user", media::json("User", get(reads::get_user)))
        .route(
            "/user/emails",
            media::json("UserEmail", get(reads::get_user_emails)),
        )
        .route(
            "/user/teams",
            media::json("Team", get(reads::list_user_teams)),
        )
        .route(
            "/user/repos",
            media::json(
                "Repository",
                get(reads::list_user_repos).post(create_user_repo),
            ),
        )
        .route(
            "/repos/{owner}/{repo}",
            media::json("Repository", get(reads::get_repo)),
        )
        .route(
            "/repos/{owner}/{repo}/pulls",
            media::json(
                "PullRequest",
                get(reads::list_pulls).post(pulls::create_pull),
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            media::json("PullRequest", get(reads::get_pull).patch(pulls::patch_pull)),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/merge",
            media::json("PullRequest", put(pulls::merge_pull)),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/files",
            media::json("PullRequest", get(reads::list_pull_files)),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/reviews",
            media::json(
                "PullRequestReview",
                get(reads::list_reviews).post(pulls::create_review),
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            media::json(
                "PullRequest",
                post(pulls::request_reviewers).delete(pulls::remove_review_requests),
            ),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments",
            media::json("PullRequestReviewComment", get(reads::list_review_comments)),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            media::json(
                "IssueComment",
                get(reads::list_issue_comments).post(pulls::create_comment),
            ),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels",
            media::json("Label", post(pulls::add_labels)),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels/{name}",
            media::json("Label", delete(pulls::remove_label)),
        )
        .route(
            "/repos/{owner}/{repo}/commits/{sha}/check-runs",
            media::json("CheckRun", get(reads::list_check_runs_for_ref)),
        )
        // For a file, the object media type answers the object the JSON
        // one does.
        .route(
            "/repos/{owner}/{repo}/contents/{*path}",
            media::rendering("Content", &["object"], get(reads::get_contents)),
        )
        .route(
            "/repos/{owner}/{repo}/branches",
            media::json("Branch", get(reads::list_branches)),
        )
        .route(
            "/repos/{owner}/{repo}/git/ref/heads/{*branch}",
            media::json("Ref", get(reads::get_branch_ref)),
        )
        .route(
            "/repos/{owner}/{repo}/compare/{basehead}",
            media::json("Comparison", get(reads::compare)),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs",
            media::json("CheckRun", post(checks::create_check_run)),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs/{id}",
            media::json("CheckRun", patch(checks::patch_check_run)),
        )
        .with_state(face)
}

/// Fields GitHub's create-repo accepts that Orrery deliberately does not
/// implement. Sending one is a loud 422, never a silent ignore.
const UNSUPPORTED_BEHAVIORAL_FIELDS: &[&str] = &[
    "auto_init",
    "gitignore_template",
    "license_template",
    "is_template",
    "team_id",
];

#[derive(serde::Deserialize)]
struct CreateRepoBody {
    name: Option<String>,
    description: Option<String>,
    private: Option<bool>,
    visibility: Option<String>,
    // GitHub ignores unknown fields; we capture the remainder only to detect
    // known-behavioral ones we refuse to fake.
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, serde_json::Value>,
}

async fn create_org_repo(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    Extension(actor): Extension<Actor>,
    body: axum::body::Bytes,
) -> Response<Body> {
    let owner = match face.world.actor_by_login(&org) {
        Ok(Some(a)) if a.kind == ActorKind::Org => a,
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    create_repo(face, owner, actor, &body).await
}

async fn create_user_repo(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    body: axum::body::Bytes,
) -> Response<Body> {
    if let Some(refused) = reads::refuse_integration(&actor) {
        return refused;
    }
    let owner = actor.clone();
    create_repo(face, owner, actor, &body).await
}

async fn create_repo(
    face: GithubFace,
    owner: Actor,
    creator: Actor,
    body: &[u8],
) -> Response<Body> {
    let parsed: CreateRepoBody = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    for field in UNSUPPORTED_BEHAVIORAL_FIELDS {
        if parsed.rest.contains_key(*field) {
            return orrery_unsupported_field("Repository", field);
        }
    }
    let Some(name) = parsed
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return creation_failed("missing_field", "name", "name is required");
    };
    let private = match parsed.visibility.as_deref() {
        Some("private") => true,
        Some("public") => false,
        Some(other) => {
            return orrery_unsupported_field("Repository", &format!("visibility={other}"));
        }
        None => parsed.private.unwrap_or(false),
    };

    let created = face.world.create_repo(
        owner.id,
        name,
        parsed.description.as_deref(),
        private,
        "main",
        creator.id,
    );
    let repo = match created {
        Ok(r) => r,
        Err(WorldError::RepoExists(_)) => {
            return creation_failed("custom", "name", "name already exists on this account");
        }
        // GitHub slugifies invalid names; Orrery rejects them instead —
        // divergence accepted loudly until a consumer needs slugification.
        Err(WorldError::InvalidRepoName(_)) => {
            return creation_failed(
                "custom",
                "name",
                "name is invalid (orrery does not slugify)",
            );
        }
        Err(e) => return internal(e),
    };
    if let Err(e) = face.git.init_repo_dir(&repo) {
        return internal(e);
    }

    let json = render::repo_json(&face, &repo, &owner);
    json_response(StatusCode::CREATED, json)
}

/// Resolve `{owner}/{repo}` to the stored row and its owner actor, or the
/// vendor-faithful 404 (a matched route with missing data — never a 501).
pub(crate) fn resolve_repo(
    face: &GithubFace,
    owner: &str,
    name: &str,
) -> Result<(Repo, Actor), Box<Response<Body>>> {
    let repo = match face.world.repo_by_owner_name(owner, name) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"))),
        Err(e) => return Err(Box::new(internal(e))),
    };
    let owner_actor = match face.world.actor_by_login(&repo.owner_login) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(Box::new(internal("repo owner actor missing"))),
        Err(e) => return Err(Box::new(internal(e))),
    };
    Ok((repo, owner_actor))
}

/// A query string or an `application/x-www-form-urlencoded` body as its
/// pairs, each key and value decoded as a URL query is (`%XX`, and `+` as a
/// space). A repeated key keeps its first value.
pub(crate) fn parse_form(raw: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in form_urlencoded::parse(raw) {
        out.entry(k.into_owned()).or_insert_with(|| v.into_owned());
    }
    out
}

/// The GitHub 422 creation-failure envelope, as consumers parse it
/// (spec/grammar.md: `message` + `errors[].{code,field,message}`).
pub(crate) fn creation_failed(code: &str, field: &str, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Repository creation failed.",
        "errors": [{ "resource": "Repository", "code": code, "field": field, "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's generic 422 for an invalid field value on `resource`.
pub(crate) fn validation_invalid(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "invalid", "field": field }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's generic 422 for a required field absent on `resource`.
pub(crate) fn validation_missing(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "missing_field", "field": field }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's 422 with a prose `custom` error — the shape of "No commits
/// between x and y" and "A pull request already exists".
pub(crate) fn validation_custom(resource: &str, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "custom", "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// Loud refusal for a real-GitHub field Orrery chooses not to fake, on the
/// `resource` the request is about. Shaped like a GitHub 422 so strict
/// clients parse it, but self-describing so a human reads exactly what to do
/// (NOT-MOCKED at field granularity).
pub(crate) fn orrery_unsupported_field(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": format!("orrery: request field {field:?} is not mocked"),
        "orrery": {
            "not_mocked": true,
            "field": field,
            "hint": "If a consumer needs this behavior, add it to the surface inventory (spec/surface-github.md) and implement it.",
        },
        "errors": [{ "resource": resource, "code": "custom", "field": field,
                     "message": "not mocked by orrery" }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

pub(crate) fn github_error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, serde_json::json!({ "message": message }))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response<Body> {
    tracing::error!(error = %e, "github face internal error");
    github_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
}

/// A header's value, lowercased because the values matched against it (media
/// types, mostly) compare ignoring case; empty when absent or not ASCII.
pub(crate) fn header_str(headers: &HeaderMap, name: header::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub(crate) fn json_response(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}
