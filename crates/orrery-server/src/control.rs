//! The `/_orrery/` control plane: the operations reality performs that no
//! public GitHub API can express (spec/usage.md — door two). Never duplicates
//! a face capability.
//!
//! Unlike the faces, these are our own routes, so house API style applies
//! strictly: one intent per route, strict body decoding (unknown fields
//! rejected), invalid values rejected with the field named — never clamped —
//! and the one error shape `{"errors":[{"reason","message","field?"}]}`.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post, put};
use orrery_world::{
    Actor, ActorEmail, ActorKind, OAUTH_CLIENT_ID_PREFIX, SYSTEM_ACTOR_LOGIN, WorldError,
    validate_login,
};

use crate::{AppState, json};

const LIST_PAGE_MAX: usize = 500;
const LIST_PAGE_DEFAULT: usize = 100;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/clock", get(get_clock))
        .route("/clock/advance", post(advance_clock))
        .route("/actors", post(create_actor))
        .route("/actors/list", post(list_actors))
        .route("/actors/{login}/emails", put(set_actor_emails))
        .route("/actors/{login}/tokens", post(issue_actor_token))
        .route("/oauth-apps", post(create_oauth_app))
        .route("/oauth-apps/list", post(list_oauth_apps))
        .route(
            "/oauth-apps/{client_id}/secrets",
            post(issue_oauth_client_secret),
        )
        .route("/log/list", post(log_list))
        .merge(crate::snapshots::router())
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> Response<Body> {
    match state.world.clock() {
        Ok(clock) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": "ok",
                "clock_unix": clock,
                "build": crate::BUILD_COMMIT,
            }),
        ),
        Err(e) => internal(e),
    }
}

async fn get_clock(State(state): State<AppState>) -> Response<Body> {
    match state.world.clock() {
        Ok(clock) => json(StatusCode::OK, clock_body(clock)),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdvanceClockBody {
    seconds: i64,
}

async fn advance_clock(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: AdvanceClockBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    if parsed.seconds <= 0 {
        return errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "the clock only moves forward; seconds must be positive",
            Some("seconds"),
        );
    }
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state.world.advance_clock(parsed.seconds, system.id) {
        Ok(clock) => json(StatusCode::OK, clock_body(clock)),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateActorBody {
    login: String,
    kind: ActorKind,
}

async fn create_actor(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: CreateActorBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    match state.world.create_actor(&parsed.login, parsed.kind) {
        Ok((actor, token)) => actor_token_created(&actor, &token),
        Err(e @ (WorldError::InvalidLogin(_) | WorldError::InvalidActorKind)) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("login"),
        ),
        Err(e @ WorldError::ActorExists(_)) => errors(
            StatusCode::CONFLICT,
            "ALREADY_EXISTS",
            &e.to_string(),
            Some("login"),
        ),
        Err(e) => internal(e),
    }
}

/// Issue another token for an existing actor. No read returns a stored
/// token, so this is the way in to an actor someone else created, or one a
/// re-keyed world inherited.
async fn issue_actor_token(
    State(state): State<AppState>,
    UrlPath(login): UrlPath<String>,
) -> Response<Body> {
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state.world.issue_actor_token(&login, system.id) {
        Ok((actor, token)) => actor_token_created(&actor, &token),
        Err(e @ WorldError::SystemActorToken) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("login"),
        ),
        Err(e @ WorldError::NoSuchActor(_)) => {
            errors(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string(), None)
        }
        Err(e) => internal(e),
    }
}

fn actor_token_created(actor: &Actor, token: &str) -> Response<Body> {
    json(
        StatusCode::CREATED,
        serde_json::json!({
            "actor": { "id": actor.id, "login": actor.login, "kind": actor.kind },
            "token": token,
        }),
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetEmailsBody {
    emails: Vec<ActorEmail>,
}

/// Replace an actor's addresses: verifying one and choosing the primary are
/// things a person does in GitHub's settings, with no API behind them.
async fn set_actor_emails(
    State(state): State<AppState>,
    UrlPath(login): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let parsed: SetEmailsBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state
        .world
        .set_actor_emails(&login, &parsed.emails, system.id)
    {
        Ok(emails) => json(StatusCode::OK, serde_json::json!({ "emails": emails })),
        Err(e @ WorldError::InvalidEmails(_)) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("emails"),
        ),
        Err(e @ WorldError::NoSuchActor(_)) => {
            errors(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string(), None)
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateOAuthAppBody {
    name: String,
    callback_url: String,
}

/// Register an OAuth app. GitHub offers this only in its web settings, so it
/// is reality's to do, and the control plane's.
async fn create_oauth_app(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: CreateOAuthAppBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state
        .world
        .create_oauth_app(&parsed.name, &parsed.callback_url, system.id)
    {
        Ok(app) => json(StatusCode::CREATED, serde_json::json!(app)),
        Err(WorldError::InvalidOAuthApp { field, message }) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &message,
            Some(field),
        ),
        Err(e) => internal(e),
    }
}

/// Issue another client secret for a registered app, as GitHub's settings
/// page generates one. No read returns a stored secret, so this is the way
/// in to an app a re-keyed world inherited.
async fn issue_oauth_client_secret(
    State(state): State<AppState>,
    UrlPath(client_id): UrlPath<String>,
) -> Response<Body> {
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state.world.issue_oauth_client_secret(&client_id, system.id) {
        Ok(secret) => json(
            StatusCode::CREATED,
            serde_json::json!({ "client_id": client_id, "client_secret": secret }),
        ),
        Err(e @ WorldError::NoSuchOAuthApp(_)) => {
            errors(StatusCode::NOT_FOUND, "NOT_FOUND", &e.to_string(), None)
        }
        Err(e) => internal(e),
    }
}

fn system_actor(state: &AppState) -> Result<Actor, Box<Response<Body>>> {
    match state.world.actor_by_login(SYSTEM_ACTOR_LOGIN) {
        Ok(Some(a)) => Ok(a),
        Ok(None) => Err(Box::new(internal("system actor missing"))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LogListBody {
    #[serde(default)]
    after_seq: i64,
    page_size: Option<usize>,
}

async fn log_list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: LogListBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let page_size = match page_size(parsed.page_size) {
        Ok(n) => n,
        Err(fault) => return faults(StatusCode::BAD_REQUEST, vec![fault]),
    };
    match state.world.log_after(parsed.after_seq, page_size) {
        Ok(items) => {
            // Keyset pagination over seq, which is a total order by
            // construction; a full page signals a possible next page.
            let next_after_seq = (items.len() == page_size)
                .then(|| items.last().map(|e| e.seq))
                .flatten();
            json(
                StatusCode::OK,
                serde_json::json!({ "items": items, "next_after_seq": next_after_seq }),
            )
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActorListBody {
    page_size: Option<usize>,
    after_login: Option<String>,
}

/// Which actors a world holds, so a caller handed a world it did not build
/// knows whom it may issue tokens for. Never a token.
async fn list_actors(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: ActorListBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut found = Vec::new();
    let page_size = page_size(parsed.page_size).unwrap_or_else(|fault| {
        found.push(fault);
        0
    });
    if let Some(after) = &parsed.after_login
        && let Err(e) = validate_login(after)
    {
        found.push(Fault::invalid(e.to_string(), "after_login"));
    }
    if !found.is_empty() {
        return faults(StatusCode::BAD_REQUEST, found);
    }
    match state
        .world
        .actors_after(parsed.after_login.as_deref(), page_size + 1)
    {
        Ok(items) => {
            let (items, next) = keyset_page(items, page_size, |a| a.login.clone());
            json(
                StatusCode::OK,
                serde_json::json!({ "items": items, "next_after_login": next }),
            )
        }
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuthAppListBody {
    page_size: Option<usize>,
    after_client_id: Option<String>,
}

/// Which OAuth apps a world holds, so a caller handed a world it did not
/// build knows which it may issue secrets for. Never a secret.
async fn list_oauth_apps(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let parsed: OAuthAppListBody = match decode_strict(&body) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let mut found = Vec::new();
    let page_size = page_size(parsed.page_size).unwrap_or_else(|fault| {
        found.push(fault);
        0
    });
    if let Some(after) = &parsed.after_client_id
        && !after.starts_with(OAUTH_CLIENT_ID_PREFIX)
    {
        found.push(Fault::invalid(
            format!("{after:?} is not a client id: every one starts with {OAUTH_CLIENT_ID_PREFIX}"),
            "after_client_id",
        ));
    }
    if !found.is_empty() {
        return faults(StatusCode::BAD_REQUEST, found);
    }
    match state
        .world
        .oauth_apps_after(parsed.after_client_id.as_deref(), page_size + 1)
    {
        Ok(items) => {
            let (items, next) = keyset_page(items, page_size, |a| a.client_id.clone());
            json(
                StatusCode::OK,
                serde_json::json!({ "items": items, "next_after_client_id": next }),
            )
        }
        Err(e) => internal(e),
    }
}

fn page_size(requested: Option<usize>) -> Result<usize, Fault> {
    let page_size = requested.unwrap_or(LIST_PAGE_DEFAULT);
    if (1..=LIST_PAGE_MAX).contains(&page_size) {
        Ok(page_size)
    } else {
        Err(Fault {
            reason: "OUT_OF_RANGE",
            message: format!("page_size must be 1..={LIST_PAGE_MAX}"),
            field: Some("page_size"),
        })
    }
}

/// Cut `rows`, read with one row past the page, to the page and the cursor
/// that follows it. The extra row is how the last page is known to be last,
/// so a page that ends on the final row has a null cursor.
fn keyset_page<T>(
    mut rows: Vec<T>,
    page_size: usize,
    key: impl Fn(&T) -> String,
) -> (Vec<T>, Option<String>) {
    let more = rows.len() > page_size;
    rows.truncate(page_size);
    let next = more.then(|| rows.last().map(key)).flatten();
    (rows, next)
}

fn clock_body(clock: i64) -> serde_json::Value {
    let rfc3339 = jiff::Timestamp::from_second(clock)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| "invalid".to_string());
    serde_json::json!({ "clock_unix": clock, "rfc3339": rfc3339 })
}

/// An empty body is a valid spelling of "all defaults" for bodies whose fields
/// are all optional; callers with required fields still fail on the missing
/// field, named by serde.
pub(crate) fn decode_strict<T: serde::de::DeserializeOwned>(
    body: &[u8],
) -> Result<T, Box<Response<Body>>> {
    let raw: &[u8] = if body.is_empty() { b"{}" } else { body };
    serde_json::from_slice(raw).map_err(|e| {
        Box::new(errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            None,
        ))
    })
}

/// One entry of the error shape's `errors` list.
pub(crate) struct Fault {
    reason: &'static str,
    message: String,
    field: Option<&'static str>,
}

impl Fault {
    fn invalid(message: String, field: &'static str) -> Fault {
        Fault {
            reason: "INVALID_ARGUMENT",
            message,
            field: Some(field),
        }
    }
}

/// The error shape with every fault found, so a caller fixes them all in one
/// round trip.
pub(crate) fn faults(status: StatusCode, faults: Vec<Fault>) -> Response<Body> {
    let errors: Vec<_> = faults
        .into_iter()
        .map(|f| {
            let mut err = serde_json::json!({ "reason": f.reason, "message": f.message });
            if let Some(field) = f.field {
                err["field"] = serde_json::Value::String(field.to_string());
            }
            err
        })
        .collect();
    json(status, serde_json::json!({ "errors": errors }))
}

pub(crate) fn errors(
    status: StatusCode,
    reason: &str,
    message: &str,
    field: Option<&str>,
) -> Response<Body> {
    let mut err = serde_json::json!({ "reason": reason, "message": message });
    if let Some(f) = field {
        err["field"] = serde_json::Value::String(f.to_string());
    }
    json(status, serde_json::json!({ "errors": [err] }))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response<Body> {
    tracing::error!(error = %e, "control plane internal error");
    errors(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL",
        "internal error",
        None,
    )
}
