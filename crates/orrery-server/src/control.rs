//! The `/_orrery/` control plane: the operations reality performs that no
//! public GitHub API can express (spec/usage.md — door two). Never duplicates
//! a face capability.
//!
//! Unlike the faces, these are our own routes, so house API style applies
//! strictly: one intent per route, strict body decoding (unknown fields
//! rejected), invalid values rejected with the field named — never clamped —
//! and the one error shape `{"errors":[{"reason","message","field?"}]}`.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post, put};
use orrery_world::{
    Actor, ActorEmail, ActorKind, OAUTH_CLIENT_ID_PREFIX, SYSTEM_ACTOR_LOGIN, WorldError,
    validate_login,
};
use serde_json::value::RawValue;

use crate::{AppState, json};

const LIST_PAGE_MAX: usize = 500;
const LIST_PAGE_DEFAULT: usize = 100;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/clock", get(get_clock))
        .route("/clock/advance", post(advance_clock))
        .route("/actors", post(create_actor))
        // TODO(ORR-36): GET /actors/{login} and GET /oauth-apps/{client_id},
        // the point reads these two lists have no partner for.
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

async fn advance_clock(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let seconds: Option<i64> = fields.required("seconds");
    if seconds.is_some_and(|s| s <= 0) {
        fields.fault(Fault::invalid(
            "the clock only moves forward; seconds must be positive".to_string(),
            "seconds",
        ));
    }
    let seconds = match fields.finish(seconds) {
        Ok(s) => s,
        Err(resp) => return *resp,
    };
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state.world.advance_clock(seconds, system.id) {
        Ok(clock) => json(StatusCode::OK, clock_body(clock)),
        Err(e) => internal(e),
    }
}

async fn create_actor(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let login: Option<String> = fields.required("login");
    let kind: Option<ActorKind> = fields.required("kind");
    let (login, kind) = match fields.finish(login.zip(kind)) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    match state.world.create_actor(&login, kind) {
        Ok((actor, token)) => actor_token_created(&actor, &token),
        Err(e @ WorldError::InvalidLogin(_)) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("login"),
        ),
        Err(e @ WorldError::InvalidActorKind) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("kind"),
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

/// Replace an actor's addresses: verifying one and choosing the primary are
/// things a person does in GitHub's settings, with no API behind them.
async fn set_actor_emails(
    State(state): State<AppState>,
    UrlPath(login): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let emails: Option<Vec<ActorEmail>> = fields.required("emails");
    let emails = match fields.finish(emails) {
        Ok(e) => e,
        Err(resp) => return *resp,
    };
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state.world.set_actor_emails(&login, &emails, system.id) {
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

/// Register an OAuth app. GitHub offers this only in its web settings, so it
/// is reality's to do, and the control plane's.
async fn create_oauth_app(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let name: Option<String> = fields.required("name");
    let callback_url: Option<String> = fields.required("callback_url");
    let (name, callback_url) = match fields.finish(name.zip(callback_url)) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    let system = match system_actor(&state) {
        Ok(a) => a,
        Err(resp) => return *resp,
    };
    match state
        .world
        .create_oauth_app(&name, &callback_url, system.id)
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

async fn log_list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let after_seq: i64 = fields.optional("after_seq").unwrap_or(0);
    let page_size = page_size(&mut fields);
    let page_size = match fields.finish(page_size) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    match state.world.log_after(after_seq, page_size + 1) {
        Ok(items) => {
            let (items, next) = keyset_page(items, page_size, |e| e.seq);
            json(
                StatusCode::OK,
                serde_json::json!({ "items": items, "next_after_seq": next }),
            )
        }
        Err(e) => internal(e),
    }
}

/// Which actors a world holds, so a caller handed a world it did not build
/// knows whom it may issue tokens for. Never a token.
async fn list_actors(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let page_size = page_size(&mut fields);
    let after_login: Option<String> = fields.optional("after_login");
    if let Some(after) = &after_login
        && let Err(e) = validate_login(after)
    {
        fields.fault(Fault::invalid(e.to_string(), "after_login"));
    }
    let page_size = match fields.finish(page_size) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    match state
        .world
        .actors_after(after_login.as_deref(), page_size + 1)
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

/// Which OAuth apps a world holds, so a caller handed a world it did not
/// build knows which it may issue secrets for. Never a secret.
async fn list_oauth_apps(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let page_size = page_size(&mut fields);
    let after_client_id: Option<String> = fields.optional("after_client_id");
    if let Some(after) = &after_client_id
        && !after.starts_with(OAUTH_CLIENT_ID_PREFIX)
    {
        fields.fault(Fault::invalid(
            format!("{after:?} is not a client id: every one starts with {OAUTH_CLIENT_ID_PREFIX}"),
            "after_client_id",
        ));
    }
    let page_size = match fields.finish(page_size) {
        Ok(n) => n,
        Err(resp) => return *resp,
    };
    match state
        .world
        .oauth_apps_after(after_client_id.as_deref(), page_size + 1)
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

/// The page size a list body asks for, or `None` when it is out of range,
/// which is recorded. One that fails to decode reads as the default here:
/// its fault is already recorded, and a second one would report it twice.
fn page_size(fields: &mut Fields) -> Option<usize> {
    let page_size = fields.optional("page_size").unwrap_or(LIST_PAGE_DEFAULT);
    if (1..=LIST_PAGE_MAX).contains(&page_size) {
        Some(page_size)
    } else {
        fields.fault(Fault::out_of_range(
            format!("page_size must be 1..={LIST_PAGE_MAX}"),
            "page_size",
        ));
        None
    }
}

/// Cut `rows`, read with one row past the page, to the page and the cursor
/// that follows it. The extra row is how the last page is known to be last,
/// so a page that ends on the final row has a null cursor.
fn keyset_page<T, K>(
    mut rows: Vec<T>,
    page_size: usize,
    key: impl Fn(&T) -> K,
) -> (Vec<T>, Option<K>) {
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

/// A control-plane body, read one field at a time so that a refusal names
/// every fault in it: fields that are missing or fail to decode, fields the
/// route does not take, and whatever the route's own checks find.
///
/// Each field is kept as its JSON text and decoded straight into the route's
/// type, so a value inside a field is held to that type's own rules, such as
/// refusing a repeated or unknown key in a nested object.
pub(crate) struct Fields {
    unread: BTreeMap<String, Box<RawValue>>,
    read: Vec<&'static str>,
    faults: Vec<Fault>,
    /// Set when the body is not a JSON object. It has no fields to name, so
    /// this is its only fault.
    not_an_object: Option<Fault>,
}

impl Fields {
    /// An empty body is a valid spelling of "all defaults" for routes whose
    /// fields are all optional.
    pub(crate) fn parse(body: &[u8]) -> Fields {
        let mut fields = Fields {
            unread: BTreeMap::new(),
            read: Vec::new(),
            faults: Vec::new(),
            not_an_object: None,
        };
        if body.is_empty() {
            return fields;
        }
        match serde_json::from_slice::<Object>(body) {
            Ok(object) => {
                fields.unread = object.members;
                fields.faults = object
                    .repeated
                    .into_iter()
                    .map(|name| Fault::invalid(format!("{name} appears more than once"), name))
                    .collect();
            }
            Err(e) => {
                fields.not_an_object = Some(Fault {
                    reason: "INVALID_ARGUMENT",
                    message: e.to_string(),
                    field: None,
                });
            }
        }
        fields
    }

    /// A field the route cannot go without. `None` when it is missing or
    /// fails to decode, and either is recorded.
    pub(crate) fn required<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &'static str,
    ) -> Option<T> {
        self.read.push(name);
        match self.unread.remove(name) {
            Some(raw) => self.decode(name, &raw),
            None => {
                self.fault(Fault::invalid(format!("{name} is required"), name));
                None
            }
        }
    }

    /// A field the route can go without: absent and `null` are both `None`.
    /// One that fails to decode is also `None`, and is recorded.
    pub(crate) fn optional<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &'static str,
    ) -> Option<T> {
        self.read.push(name);
        let raw = self.unread.remove(name)?;
        self.decode::<Option<T>>(name, &raw).flatten()
    }

    fn decode<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &'static str,
        raw: &RawValue,
    ) -> Option<T> {
        match serde_json::from_str(raw.get()) {
            Ok(v) => Some(v),
            Err(e) => {
                let message = without_position(&e);
                self.fault(Fault::invalid(format!("{name}: {message}"), name));
                None
            }
        }
    }

    /// Record a fault the route's own checks found.
    pub(crate) fn fault(&mut self, fault: Fault) {
        self.faults.push(fault);
    }

    /// What the route read, if the body has no fault. Otherwise the error
    /// shape with every fault, in the order found, then one for each field
    /// the route does not take.
    pub(crate) fn finish<T>(self, read: Option<T>) -> Result<T, Box<Response<Body>>> {
        if let Some(fault) = self.not_an_object {
            return Err(Box::new(faults(StatusCode::BAD_REQUEST, vec![fault])));
        }
        let takes = self.read.join(", ");
        let mut found = self.faults;
        found.extend(self.unread.into_keys().map(|name| {
            Fault::invalid(
                format!("unknown field {name:?}; this body takes {takes}"),
                name,
            )
        }));
        match read {
            Some(value) if found.is_empty() => Ok(value),
            // A route reads `None` only where it recorded a fault, so this is
            // a bug in the route, not in the call.
            None if found.is_empty() => Err(Box::new(internal(
                "a body read came back empty without a fault",
            ))),
            _ => Err(Box::new(faults(StatusCode::BAD_REQUEST, found))),
        }
    }
}

/// A field's value is decoded from its own text, so the line and column
/// serde_json gives point into that value, not into the body.
fn without_position(e: &serde_json::Error) -> String {
    let message = e.to_string();
    let position = format!(" at line {} column {}", e.line(), e.column());
    match message.strip_suffix(&position) {
        Some(stripped) => stripped.to_string(),
        None => message,
    }
}

/// A JSON object's members, and the names of any that appear more than once.
/// A map keeps one value under each name, so a body that repeats one is
/// refused rather than read with a value dropped.
struct Object {
    members: BTreeMap<String, Box<RawValue>>,
    repeated: Vec<String>,
}

impl<'de> serde::Deserialize<'de> for Object {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Object, D::Error> {
        deserializer.deserialize_map(ObjectVisitor)
    }
}

struct ObjectVisitor;

impl<'de> serde::de::Visitor<'de> for ObjectVisitor {
    type Value = Object;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Object, A::Error> {
        let mut object = Object {
            members: BTreeMap::new(),
            repeated: Vec::new(),
        };
        while let Some((name, value)) = map.next_entry::<String, Box<RawValue>>()? {
            match object.members.entry(name) {
                Entry::Vacant(slot) => {
                    slot.insert(value);
                }
                Entry::Occupied(held) => {
                    if !object.repeated.contains(held.key()) {
                        object.repeated.push(held.key().clone());
                    }
                }
            }
        }
        Ok(object)
    }
}

/// One entry of the error shape's `errors` list.
pub(crate) struct Fault {
    reason: &'static str,
    message: String,
    field: Option<String>,
}

impl Fault {
    fn invalid(message: String, field: impl Into<String>) -> Fault {
        Fault {
            reason: "INVALID_ARGUMENT",
            message,
            field: Some(field.into()),
        }
    }

    pub(crate) fn out_of_range(message: String, field: &'static str) -> Fault {
        Fault {
            reason: "OUT_OF_RANGE",
            message,
            field: Some(field.to_string()),
        }
    }
}

/// The error shape with every fault found, so a caller fixes them all in one
/// round trip.
pub(crate) fn faults(status: StatusCode, faults: Vec<Fault>) -> Response<Body> {
    let errors: Vec<_> = faults
        .iter()
        .map(|f| error_entry(f.reason, &f.message, f.field.as_deref()))
        .collect();
    json(status, serde_json::json!({ "errors": errors }))
}

pub(crate) fn errors(
    status: StatusCode,
    reason: &str,
    message: &str,
    field: Option<&str>,
) -> Response<Body> {
    json(
        status,
        serde_json::json!({ "errors": [error_entry(reason, message, field)] }),
    )
}

fn error_entry(reason: &str, message: &str, field: Option<&str>) -> serde_json::Value {
    let mut err = serde_json::json!({ "reason": reason, "message": message });
    if let Some(f) = field {
        err["field"] = serde_json::Value::String(f.to_string());
    }
    err
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
