//! The GitHub REST face. Grows strictly on demand (spec/README.md rule 1):
//! every endpoint here exists because a consumer in `spec/surface-github.md`
//! or `spec/usage.md` needs it. Currently the authoring surface: repo
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

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{delete, get, patch, post, put};
use orrery_face_git::GitFace;
use orrery_world::{Actor, ActorKind, Repo, World, WorldError};

mod checks;
pub mod graphql;
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
    Router::new()
        .route("/orgs/{org}", get(reads::get_org))
        .route("/orgs/{org}/repos", post(create_org_repo))
        .route(
            "/orgs/{org}/teams",
            get(reads::list_org_teams).post(orgs::create_team),
        )
        .route("/orgs/{org}/teams/{team_slug}", delete(orgs::delete_team))
        .route(
            "/orgs/{org}/teams/{team_slug}/memberships/{username}",
            put(orgs::set_team_membership).delete(orgs::remove_team_membership),
        )
        .route(
            "/orgs/{org}/memberships/{username}",
            put(orgs::set_org_membership),
        )
        .route("/users/{username}", get(reads::get_user_by_login))
        .route("/user", get(reads::get_user))
        .route("/user/emails", get(reads::get_user_emails))
        .route("/user/teams", get(reads::list_user_teams))
        .route(
            "/user/repos",
            get(reads::list_user_repos).post(create_user_repo),
        )
        .route("/repos/{owner}/{repo}", get(reads::get_repo))
        .route(
            "/repos/{owner}/{repo}/pulls",
            get(reads::list_pulls).post(pulls::create_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            get(reads::get_pull).patch(pulls::patch_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/merge",
            put(pulls::merge_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/files",
            get(reads::list_pull_files),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/reviews",
            get(reads::list_reviews).post(pulls::create_review),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            post(pulls::request_reviewers).delete(pulls::remove_review_requests),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments",
            get(reads::list_review_comments),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            get(reads::list_issue_comments).post(pulls::create_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels",
            post(pulls::add_labels),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels/{name}",
            delete(pulls::remove_label),
        )
        .route(
            "/repos/{owner}/{repo}/commits/{sha}/check-runs",
            get(reads::list_check_runs_for_ref),
        )
        .route(
            "/repos/{owner}/{repo}/contents/{*path}",
            get(reads::get_contents),
        )
        .route("/repos/{owner}/{repo}/branches", get(reads::list_branches))
        .route(
            "/repos/{owner}/{repo}/git/ref/heads/{*branch}",
            get(reads::get_branch_ref),
        )
        .route(
            "/repos/{owner}/{repo}/compare/{basehead}",
            get(reads::compare),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs",
            post(checks::create_check_run),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs/{id}",
            patch(checks::patch_check_run),
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
            return orrery_unsupported_field(field);
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
        Some(other) => return orrery_unsupported_field(&format!("visibility={other}")),
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

/// The GitHub 422 creation-failure envelope, as consumers parse it
/// (spec/surface-github.md §4.1: `message` + `errors[].{code,field,message}`).
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

/// Loud refusal for a real-GitHub field Orrery chooses not to fake. Shaped
/// like a GitHub 422 so strict clients parse it, but self-describing so a
/// human reads exactly what to do (NOT-MOCKED at field granularity).
pub(crate) fn orrery_unsupported_field(field: &str) -> Response<Body> {
    orrery_unsupported_field_on("Repository", field)
}

/// [`orrery_unsupported_field`] for a request about another `resource`.
pub(crate) fn orrery_unsupported_field_on(resource: &str, field: &str) -> Response<Body> {
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

pub(crate) fn json_response(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}
