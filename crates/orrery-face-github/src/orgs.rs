//! Organization membership and team writes (spec/surface-github.md §1.2).
//! Responses carry GitHub's shapes; Orrery presents as a GitHub Enterprise
//! Server, which adds an organization member directly, so every membership
//! answers `state: active`.

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use orrery_world::{Actor, OrgRole, TeamRole, WorldError};

use crate::render::{actor_json, org_json, team_full_json};
use crate::{
    GithubFace, github_error, internal, json_response, orrery_unsupported_field, validation_custom,
    validation_invalid, validation_missing,
};

/// Decode a body GitHub treats as optional: an empty one is `{}`.
fn optional_body<T: serde::de::DeserializeOwned + Default>(
    body: &[u8],
) -> Result<T, Box<Response<Body>>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|_| {
        Box::new(github_error(
            StatusCode::BAD_REQUEST,
            "Problems parsing JSON",
        ))
    })
}

fn no_content() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response parts are valid")
}

#[derive(serde::Deserialize, Default)]
struct RoleBody {
    role: Option<String>,
}

/// `PUT /orgs/{org}/memberships/{username}`: `{"role": "admin"|"member"}`,
/// `member` when absent.
pub(crate) async fn set_org_membership(
    State(face): State<GithubFace>,
    UrlPath((org, username)): UrlPath<(String, String)>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> Response<Body> {
    let parsed: RoleBody = match optional_body(&body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let role = match parsed.role.as_deref() {
        None | Some("member") => OrgRole::Member,
        Some("admin") => OrgRole::Admin,
        Some(_) => return validation_invalid("Membership", "role"),
    };
    match face
        .world
        .set_org_membership(&org, &username, role, actor.id)
    {
        Ok(m) => {
            let base = face.base_url.trim_end_matches('/');
            let user = match face.world.actor_by_login(&m.member_login) {
                Ok(Some(a)) => actor_json(&a),
                Ok(None) => return internal("member actor missing"),
                Err(e) => return internal(e),
            };
            json_response(
                StatusCode::OK,
                serde_json::json!({
                    "url": format!("{base}/api/v3/orgs/{}/memberships/{}", m.org_login, m.member_login),
                    "state": "active",
                    "role": m.role.as_str(),
                    "organization_url": format!("{base}/api/v3/orgs/{}", m.org_login),
                    "organization": org_json(&face, &m.org_login),
                    "user": user,
                }),
            )
        }
        Err(WorldError::NoSuchOrg(_) | WorldError::NoSuchActor(_)) => {
            github_error(StatusCode::NOT_FOUND, "Not Found")
        }
        Err(e) => internal(e),
    }
}

/// Fields GitHub's create-team accepts that Orrery does not model. Each would
/// change the team that gets created, so sending one is a loud refusal.
const UNSUPPORTED_TEAM_FIELDS: &[&str] = &["maintainers", "repo_names", "parent_team_id"];

#[derive(serde::Deserialize, Default)]
struct CreateTeamBody {
    name: Option<serde_json::Value>,
    description: Option<String>,
    privacy: Option<String>,
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, serde_json::Value>,
}

/// `POST /orgs/{org}/teams`: 201 with the full team object.
pub(crate) async fn create_team(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> Response<Body> {
    let parsed: CreateTeamBody = match optional_body(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    for field in UNSUPPORTED_TEAM_FIELDS {
        if parsed.rest.contains_key(*field) {
            return orrery_unsupported_field("Team", field);
        }
    }
    match parsed.privacy.as_deref() {
        None | Some("closed") => {}
        Some("secret") => return orrery_unsupported_field("Team", "privacy=secret"),
        Some(_) => return validation_invalid("Team", "privacy"),
    }
    let name = match &parsed.name {
        None | Some(serde_json::Value::Null) => return validation_missing("Team", "name"),
        Some(serde_json::Value::String(n)) if !n.trim().is_empty() => n.as_str(),
        Some(serde_json::Value::String(_)) => return validation_missing("Team", "name"),
        Some(_) => return validation_invalid("Team", "name"),
    };
    match face
        .world
        .create_team(&org, name, parsed.description.as_deref(), actor.id)
    {
        Ok(team) => match team_full_json(&face, &team) {
            Ok(json) => json_response(StatusCode::CREATED, json),
            Err(e) => internal(e),
        },
        Err(WorldError::NoSuchOrg(_)) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(WorldError::TeamExists(_)) => json_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "message": "Validation Failed",
                "errors": [{ "resource": "Team", "code": "already_exists", "field": "name" }],
            }),
        ),
        Err(WorldError::InvalidTeamName(_)) => validation_invalid("Team", "name"),
        Err(e) => internal(e),
    }
}

/// `DELETE /orgs/{org}/teams/{team_slug}`: 204.
pub(crate) async fn delete_team(
    State(face): State<GithubFace>,
    UrlPath((org, slug)): UrlPath<(String, String)>,
    Extension(actor): Extension<Actor>,
) -> Response<Body> {
    match face.world.delete_team(&org, &slug, actor.id) {
        Ok(()) => no_content(),
        Err(WorldError::NoSuchOrg(_) | WorldError::NoSuchTeam(_)) => {
            github_error(StatusCode::NOT_FOUND, "Not Found")
        }
        Err(e) => internal(e),
    }
}

/// `PUT /orgs/{org}/teams/{team_slug}/memberships/{username}`:
/// `{"role": "member"|"maintainer"}`, `member` when absent.
pub(crate) async fn set_team_membership(
    State(face): State<GithubFace>,
    UrlPath((org, slug, username)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    body: Bytes,
) -> Response<Body> {
    let parsed: RoleBody = match optional_body(&body) {
        Ok(b) => b,
        Err(resp) => return *resp,
    };
    let role = match parsed.role.as_deref() {
        None | Some("member") => TeamRole::Member,
        Some("maintainer") => TeamRole::Maintainer,
        Some(_) => return validation_invalid("TeamMember", "role"),
    };
    match face
        .world
        .set_team_membership(&org, &slug, &username, role, actor.id)
    {
        Ok(m) => {
            let team = match face.world.team_by_slug(&org, &slug) {
                Ok(Some(t)) => t,
                Ok(None) => return internal("team missing after membership write"),
                Err(e) => return internal(e),
            };
            let base = face.base_url.trim_end_matches('/');
            json_response(
                StatusCode::OK,
                serde_json::json!({
                    "url": format!(
                        "{base}/api/v3/organizations/{}/team/{}/memberships/{}",
                        team.org_id, team.id, m.member_login
                    ),
                    "role": m.role.as_str(),
                    "state": "active",
                }),
            )
        }
        Err(WorldError::NoSuchOrg(_) | WorldError::NoSuchTeam(_) | WorldError::NoSuchActor(_)) => {
            github_error(StatusCode::NOT_FOUND, "Not Found")
        }
        Err(WorldError::NotOrgMember { .. }) => validation_custom(
            "TeamMember",
            "orrery: only a member of the organization can join its teams",
        ),
        Err(e) => internal(e),
    }
}

/// `DELETE /orgs/{org}/teams/{team_slug}/memberships/{username}`: 204,
/// including for a user who is not on the team (GitHub documents only 204 and
/// 403 here); 404 for an unknown org, team or user.
pub(crate) async fn remove_team_membership(
    State(face): State<GithubFace>,
    UrlPath((org, slug, username)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
) -> Response<Body> {
    match face
        .world
        .remove_team_membership(&org, &slug, &username, actor.id)
    {
        Ok(()) => no_content(),
        Err(WorldError::NoSuchOrg(_) | WorldError::NoSuchTeam(_) | WorldError::NoSuchActor(_)) => {
            github_error(StatusCode::NOT_FOUND, "Not Found")
        }
        Err(e) => internal(e),
    }
}
