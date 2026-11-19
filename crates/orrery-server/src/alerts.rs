//! The control plane's routes for Dependabot alerts: what GitHub does to an
//! alert itself, and offers no API for. It makes one when a scan finds a
//! vulnerable dependency, marks one fixed or dismisses one by rule, opens
//! one again, and reports what its updater did. A person's dismissal and
//! reopening, the reads, and the switch that turns a repository's alerts on
//! are the GitHub face's.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{patch, post, put};
use orrery_world::{
    AlertScope, AlertSeverity, AlertState, AlertTransition, AlertUpdate, DependencyAlert,
    NewAlertUpdate, NewDependencyAlert, Repo, WorldError,
};

use crate::control::{Fault, Fields, SYSTEM, errors, internal};
use crate::{AppState, json};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/repos/{owner}/{repo}/dependabot-alerts",
            post(create_alert),
        )
        .route(
            "/repos/{owner}/{repo}/dependabot-alerts/{number}",
            patch(patch_alert),
        )
        .route(
            "/repos/{owner}/{repo}/dependabot-alerts/{number}/update",
            put(put_update).delete(delete_update),
        )
}

/// An alert as the control plane answers it.
#[derive(serde::Serialize)]
struct AlertBody {
    number: i64,
    state: AlertState,
    ecosystem: String,
    package: String,
    manifest_path: String,
    scope: Option<AlertScope>,
    version_requirement: Option<String>,
    ghsa_id: String,
    cve_id: Option<String>,
    summary: String,
    description: String,
    severity: AlertSeverity,
    vulnerable_version_range: String,
    first_patched_version: Option<String>,
    /// What Dependabot's updater did about the alert: null for nothing.
    update: Option<UpdateBody>,
}

/// An attempt with neither a pull request nor an error is one that has
/// started.
#[derive(serde::Serialize)]
struct UpdateBody {
    /// The number of the pull request it opened.
    pull_request: Option<i64>,
    error: Option<UpdateError>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateError {
    /// GitHub's name for what failed, such as `dependency_file_not_found`.
    r#type: String,
    title: String,
}

fn alert_body(alert: DependencyAlert) -> AlertBody {
    let update = match alert.update {
        AlertUpdate::None => None,
        AlertUpdate::Pending => Some(UpdateBody {
            pull_request: None,
            error: None,
        }),
        AlertUpdate::PullRequest { number, .. } => Some(UpdateBody {
            pull_request: Some(number),
            error: None,
        }),
        AlertUpdate::Error { error_type, title } => Some(UpdateBody {
            pull_request: None,
            error: Some(UpdateError {
                r#type: error_type,
                title,
            }),
        }),
    };
    AlertBody {
        number: alert.number,
        state: alert.state,
        ecosystem: alert.ecosystem,
        package: alert.package,
        manifest_path: alert.manifest_path,
        scope: alert.scope,
        version_requirement: alert.version_requirement,
        ghsa_id: alert.advisory_id,
        cve_id: alert.cve_id,
        summary: alert.summary,
        description: alert.description,
        severity: alert.severity,
        vulnerable_version_range: alert.vulnerable_version_range,
        first_patched_version: alert.first_patched_version,
        update,
    }
}

fn not_found(message: &str) -> Response<Body> {
    errors(StatusCode::NOT_FOUND, "NOT_FOUND", message, None)
}

fn repo_at(state: &AppState, owner: &str, name: &str) -> Result<Repo, Box<Response<Body>>> {
    match state.world.repo_by_owner_name(owner, name) {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err(Box::new(not_found(&format!(
            "no repository {owner}/{name}"
        )))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// The path's alert number, which is a positive integer or names no alert.
fn number_at(raw: &str) -> Result<i64, Box<Response<Body>>> {
    raw.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Box::new(not_found(&format!("no alert numbered {raw:?}"))))
}

/// What a write to an alert answers: the alert, or why not.
fn answered(status: StatusCode, written: Result<DependencyAlert, WorldError>) -> Response<Body> {
    match written {
        Ok(alert) => json(status, serde_json::json!(alert_body(alert))),
        Err(e @ WorldError::NoSuchDependencyAlert(_)) => not_found(&e.to_string()),
        Err(WorldError::NoSuchPullNumber { number, .. }) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &format!("the repository has no pull request numbered {number}"),
            Some("pull_request"),
        ),
        Err(WorldError::DependencyAlertsOff(_)) => errors(
            StatusCode::BAD_REQUEST,
            "FAILED_PRECONDITION",
            "Dependabot alerts are off for this repository; turn them on with the face's \
             PUT /repos/{owner}/{repo}/vulnerability-alerts",
            None,
        ),
        Err(WorldError::DependencyAlertState { number, state }) => errors(
            StatusCode::BAD_REQUEST,
            "FAILED_PRECONDITION",
            &format!(
                "alert {number} is {}, which that state does not follow",
                state.as_str()
            ),
            Some("state"),
        ),
        Err(e) => internal(e),
    }
}

/// Make an alert, as GitHub does when it finds a vulnerable dependency:
/// open, and numbered next in its repository. The repository's alerts must
/// be on.
async fn create_alert(
    State(state): State<AppState>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let text = |fields: &mut Fields, name: &'static str| -> Option<String> {
        let value: Option<String> = fields.required(name);
        if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
            fields.fault(Fault::invalid(format!("{name} cannot be blank"), name));
        }
        value
    };
    let ecosystem = text(&mut fields, "ecosystem");
    let package = text(&mut fields, "package");
    let manifest_path = text(&mut fields, "manifest_path");
    let ghsa_id = text(&mut fields, "ghsa_id");
    let summary = text(&mut fields, "summary");
    let range = text(&mut fields, "vulnerable_version_range");
    let severity: Option<AlertSeverity> = fields.required("severity");
    let scope: Option<AlertScope> = fields.optional("scope");
    let version_requirement: Option<String> = fields.optional("version_requirement");
    let cve_id: Option<String> = fields.optional("cve_id");
    let description: Option<String> = fields.optional("description");
    let first_patched_version: Option<String> = fields.optional("first_patched_version");
    let read = ecosystem
        .zip(package)
        .zip(manifest_path)
        .zip(ghsa_id)
        .zip(summary)
        .zip(range)
        .zip(severity);
    let ((((((ecosystem, package), manifest_path), ghsa_id), summary), range), severity) =
        match fields.finish(read) {
            Ok(read) => read,
            Err(resp) => return *resp,
        };
    let repo = match repo_at(&state, &owner, &name) {
        Ok(repo) => repo,
        Err(resp) => return *resp,
    };
    let new = NewDependencyAlert {
        ecosystem: &ecosystem,
        package: &package,
        manifest_path: &manifest_path,
        scope,
        version_requirement: version_requirement.as_deref(),
        advisory_id: &ghsa_id,
        cve_id: cve_id.as_deref(),
        summary: &summary,
        description: description.as_deref().unwrap_or(""),
        severity,
        vulnerable_version_range: &range,
        first_patched_version: first_patched_version.as_deref(),
    };
    answered(
        StatusCode::CREATED,
        state.world.create_dependency_alert(repo.id, &new, SYSTEM),
    )
}

/// Move an alert to a state GitHub itself moves one to: `fixed`,
/// `auto_dismissed`, or, from either, `open` again. A person's dismissal
/// and reopening are the face's (`PATCH
/// /repos/{owner}/{repo}/dependabot/alerts/{number}`). A field left out
/// keeps its value.
async fn patch_alert(
    State(state): State<AppState>,
    UrlPath((owner, name, number)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let to: Option<String> = fields.optional_not_null("state");
    let transition = match to.as_deref() {
        None => None,
        Some("fixed") => Some(AlertTransition::Fix),
        Some("auto_dismissed") => Some(AlertTransition::AutoDismiss),
        Some("open") => Some(AlertTransition::Reintroduce),
        Some("dismissed") => {
            fields.fault(Fault::invalid(
                "a person dismisses an alert, on the face: PATCH \
                 /repos/{owner}/{repo}/dependabot/alerts/{number}"
                    .to_string(),
                "state",
            ));
            None
        }
        Some(other) => {
            fields.fault(Fault::invalid(
                format!("state: {other:?} is not fixed, auto_dismissed, or open"),
                "state",
            ));
            None
        }
    };
    let transition = match fields.finish(Some(transition)) {
        Ok(transition) => transition,
        Err(resp) => return *resp,
    };
    let (repo, number) =
        match repo_at(&state, &owner, &name).and_then(|r| Ok((r, number_at(&number)?))) {
            Ok(found) => found,
            Err(resp) => return *resp,
        };
    let written = match transition {
        Some(transition) => state
            .world
            .move_dependency_alert(repo.id, number, &transition, SYSTEM),
        None => state
            .world
            .dependency_alert(repo.id, number)
            .and_then(|found| found.ok_or(WorldError::NoSuchDependencyAlert(number))),
    };
    answered(StatusCode::OK, written)
}

/// Say what Dependabot's updater did about an alert: opened `pull_request`,
/// a pull request of the repository by its number, or reported `error`.
/// With neither, it has started and done neither yet. An updater does one
/// or the other, so both is refused.
async fn put_update(
    State(state): State<AppState>,
    UrlPath((owner, name, number)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let pull_request: Option<i64> = fields.optional("pull_request");
    let error: Option<UpdateError> = fields.optional("error");
    if pull_request.is_some() && error.is_some() {
        fields.fault(Fault::invalid(
            "an update names a pull request or an error, not both".to_string(),
            "error",
        ));
    }
    if let Err(resp) = fields.finish(Some(())) {
        return *resp;
    }
    let update = match (pull_request, &error) {
        (Some(number), _) => NewAlertUpdate::PullRequest { number },
        (None, Some(error)) => NewAlertUpdate::Error {
            error_type: &error.r#type,
            title: &error.title,
        },
        (None, None) => NewAlertUpdate::Pending,
    };
    set_update(&state, &owner, &name, &number, &update)
}

/// Take back what was said of the updater: it has done nothing about the
/// alert. It takes no body.
async fn delete_update(
    State(state): State<AppState>,
    UrlPath((owner, name, number)): UrlPath<(String, String, String)>,
) -> Response<Body> {
    set_update(&state, &owner, &name, &number, &NewAlertUpdate::None)
}

fn set_update(
    state: &AppState,
    owner: &str,
    name: &str,
    number: &str,
    update: &NewAlertUpdate<'_>,
) -> Response<Body> {
    let (repo, number) = match repo_at(state, owner, name).and_then(|r| Ok((r, number_at(number)?)))
    {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    answered(
        StatusCode::OK,
        state
            .world
            .set_dependency_alert_update(repo.id, number, update, SYSTEM),
    )
}
