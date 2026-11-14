//! The `/_orrery/` control plane: the operations reality performs that no
//! public GitHub API can express (spec/usage.md — door two). Never duplicates
//! a face capability.
//!
//! Unlike the faces, these are our own routes, so house API style applies
//! strictly: one intent per route, strict body decoding (unknown fields
//! rejected), invalid values rejected with the field named — never clamped —
//! and the one error shape `{"errors":[{"reason","message","field?"}]}`.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Path as UrlPath, State};
use axum::http::{Method, Response, StatusCode};
use axum::routing::{get, post, put};
use orrery_world::{
    Actor, ActorEmail, ActorKind, Attribution, ClockMode, ClockReading, Door, LogOrder,
    SYSTEM_ACTOR_ID, WorldError, validate_actor_kind, validate_login,
    validate_oauth_app_callback_url, validate_oauth_app_name,
};
use serde_json::value::RawValue;

use crate::list::{self, SortDir};
use crate::{AppState, json};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/clock", get(get_clock).patch(patch_clock))
        .route("/clock/advance", post(advance_clock))
        .route("/actors", post(create_actor))
        .route("/actors/list", post(list_actors).get(get_actor_named_list))
        .route("/actors/{login}", get(get_actor))
        .route("/actors/{login}/emails", put(set_actor_emails))
        .route("/actors/{login}/tokens", post(issue_actor_token))
        .route("/oauth-apps", post(create_oauth_app))
        .route(
            "/oauth-apps/list",
            post(list_oauth_apps).get(get_oauth_app_named_list),
        )
        .route("/oauth-apps/{client_id}", get(get_oauth_app))
        .route(
            "/oauth-apps/{client_id}/secrets",
            post(issue_oauth_client_secret),
        )
        .route("/log/list", post(log_list))
        .route("/log/schema", get(log_schema))
        .route("/log/stream", get(crate::log_stream::stream))
        .merge(crate::apps::router())
        .merge(crate::snapshots::router())
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state)
}

/// A path under `/_orrery` that names no route.
pub(crate) async fn no_such_route(method: Method, OriginalUri(uri): OriginalUri) -> Response<Body> {
    errors(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &format!("{method} {} is not a control-plane route", uri.path()),
        None,
    )
}

/// A route called with a method it does not take. The path is read from the
/// request as it arrived, since a nested route sees its prefix stripped.
async fn method_not_allowed(method: Method, OriginalUri(uri): OriginalUri) -> Response<Body> {
    errors(
        StatusCode::METHOD_NOT_ALLOWED,
        "METHOD_NOT_ALLOWED",
        &format!(
            "{} does not take {method}; the Allow header lists the methods it takes",
            uri.path()
        ),
        None,
    )
}

async fn healthz(State(state): State<AppState>) -> Response<Body> {
    match state.world.clock() {
        Ok(clock) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": "ok",
                "clock_unix": clock,
                "build": crate::BUILD_COMMIT,
                "world_schema": orrery_world::SCHEMA_VERSION,
                "config_sha256": state.config_sha256,
            }),
        ),
        Err(e) => internal(e),
    }
}

async fn get_clock(State(state): State<AppState>) -> Response<Body> {
    match state.world.read_clock() {
        Ok(reading) => json(StatusCode::OK, clock_body(reading)),
        Err(e) => internal(e),
    }
}

/// Change how the clock moves. `mode` is the one field, and absent keeps
/// it.
async fn patch_clock(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let mode: Option<ClockMode> = fields.optional_not_null("mode");
    let mode = match fields.finish(Some(mode)) {
        Ok(m) => m,
        Err(resp) => return *resp,
    };
    let changed = match mode {
        Some(mode) => state.world.set_clock_mode(mode, SYSTEM),
        None => state.world.read_clock(),
    };
    match changed {
        Ok(reading) => json(StatusCode::OK, clock_body(reading)),
        Err(e @ WorldError::ClockAhead { .. }) => {
            errors(StatusCode::CONFLICT, "CLOCK_AHEAD", &e.to_string(), None)
        }
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
    match state.world.advance_clock(seconds, SYSTEM) {
        Ok(reading) => json(StatusCode::OK, clock_body(reading)),
        Err(e @ WorldError::ClockOutOfRange { .. }) => errors(
            StatusCode::BAD_REQUEST,
            "OUT_OF_RANGE",
            &e.to_string(),
            Some("seconds"),
        ),
        Err(e @ WorldError::ClockIsReal) => {
            errors(StatusCode::CONFLICT, "CLOCK_IS_REAL", &e.to_string(), None)
        }
        Err(e) => internal(e),
    }
}

async fn create_actor(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let login: Option<String> = fields.required("login");
    if let Some(login) = &login
        && let Err(e) = validate_login(login)
    {
        fields.fault(Fault::invalid(e.to_string(), "login"));
    }
    let kind: Option<ActorKind> = fields.required("kind");
    if let Some(kind) = kind
        && let Err(e) = validate_actor_kind(kind)
    {
        fields.fault(Fault::invalid(e.to_string(), "kind"));
    }
    let (login, kind) = match fields.finish(login.zip(kind)) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    match state.world.create_actor(&login, kind) {
        Ok((actor, token)) => actor_token_created(&actor, &token),
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
    body: Bytes,
) -> Response<Body> {
    if let Err(resp) = Fields::parse(&body).finish(Some(())) {
        return *resp;
    }
    match state.world.issue_actor_token(&login, SYSTEM) {
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
    match state.world.set_actor_emails(&login, &emails, SYSTEM) {
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
    if let Some(name) = &name
        && let Err(e) = validate_oauth_app_name(name)
    {
        fields.fault(oauth_app_fault(e, "name"));
    }
    let callback_url: Option<String> = fields.required("callback_url");
    if let Some(callback_url) = &callback_url
        && let Err(e) = validate_oauth_app_callback_url(callback_url)
    {
        fields.fault(oauth_app_fault(e, "callback_url"));
    }
    let (name, callback_url) = match fields.finish(name.zip(callback_url)) {
        Ok(p) => p,
        Err(resp) => return *resp,
    };
    match state.world.create_oauth_app(&name, &callback_url, SYSTEM) {
        Ok(app) => json(StatusCode::CREATED, serde_json::json!(app)),
        Err(e) => internal(e),
    }
}

/// The fault an OAuth app value's check found, under the field the route
/// read the value from. The check's own message is written to follow the
/// field's name.
fn oauth_app_fault(e: WorldError, field: &'static str) -> Fault {
    let message = match e {
        WorldError::InvalidOAuthApp { message, .. } => message,
        other => other.to_string(),
    };
    Fault::invalid(message, field)
}

/// Issue another client secret for a registered app, as GitHub's settings
/// page generates one. No read returns a stored secret, so this is the way
/// in to an app a re-keyed world inherited.
async fn issue_oauth_client_secret(
    State(state): State<AppState>,
    UrlPath(client_id): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    if let Err(resp) = Fields::parse(&body).finish(Some(())) {
        return *resp;
    }
    match state.world.issue_oauth_client_secret(&client_id, SYSTEM) {
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

/// Every control-plane write is the system actor's, through the control
/// plane.
pub(crate) const SYSTEM: Attribution = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);

/// The log's events, oldest first unless the body orders them newest first.
/// `after_seq` narrows it to what followed a position another call reported,
/// such as a diff's `log.shared_seq` or a snapshot's `last_seq`. A position
/// the log does not hold is refused: what follows it in this log did not
/// follow it.
async fn log_list(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let after_seq: i64 = fields.optional("after_seq").unwrap_or(0);
    if after_seq < 0 {
        fields.fault(Fault::out_of_range(
            "after_seq must be 0 or more".to_string(),
            "after_seq",
        ));
    }
    let dir = list::read_dir(&mut fields, "seq");
    let query = serde_json::json!({ "after_seq": after_seq, "sort_dir": dir });
    let window = list::read_window::<i64>(&mut fields, "log", &query);
    let window = match fields.finish(Some(window)) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    let order = match dir {
        SortDir::Asc => LogOrder::OldestFirst,
        SortDir::Desc => LogOrder::NewestFirst,
    };
    match state
        .world
        .log_page(after_seq, order, window.resume, window.fetch())
    {
        Ok((rows, total)) => list::answer(rows, &window, total, |e| e.seq),
        Err(WorldError::NoSuchLogPosition { seq, newest_seq }) => {
            crate::log_stream::not_in_log("after_seq", Some("after_seq"), seq, newest_seq)
        }
        Err(WorldError::ResumeNotInLog(_)) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "page_token resumes from an event the log no longer holds: \
             a restore replaced the log since the token was issued; \
             start again from the first page",
            Some("page_token"),
        ),
        Err(e) => internal(e),
    }
}

/// The JSON Schema of the log's events, as this build writes them: what
/// `log/list` and `log/stream` serve each event as.
async fn log_schema() -> Response<Body> {
    json(StatusCode::OK, orrery_world::events::schema())
}

/// Which actors a world holds, so a caller handed a world it did not build
/// knows whom it may issue tokens for. Never a token.
async fn list_actors(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    list::read_ascending(&mut fields, "login");
    let window = list::read_window::<String>(&mut fields, "actors", &serde_json::json!({}));
    let window = match fields.finish(Some(window)) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    match state
        .world
        .actors_page(window.resume.as_deref(), window.fetch())
    {
        Ok((rows, total)) => list::answer(rows, &window, total, |a| a.login.clone()),
        Err(e) => internal(e),
    }
}

async fn get_actor(
    State(state): State<AppState>,
    UrlPath(login): UrlPath<String>,
) -> Response<Body> {
    read_actor(&state, login)
}

/// `list` is a valid login, but the list's static path takes
/// `GET /actors/list` before `/actors/{login}` can, so the point read is
/// routed here too.
async fn get_actor_named_list(State(state): State<AppState>) -> Response<Body> {
    read_actor(&state, "list".to_string())
}

/// One actor, read by a login the caller already holds, answered as the list
/// of actors answers it. Never a token.
fn read_actor(state: &AppState, login: String) -> Response<Body> {
    match state.world.listed_actor_by_login(&login) {
        Ok(Some(actor)) => json(StatusCode::OK, serde_json::json!(actor)),
        Ok(None) => errors(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &WorldError::NoSuchActor(login).to_string(),
            None,
        ),
        Err(e) => internal(e),
    }
}

/// Which OAuth apps a world holds, so a caller handed a world it did not
/// build knows which it may issue secrets for. Never a secret.
async fn list_oauth_apps(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    list::read_ascending(&mut fields, "client_id");
    let window = list::read_window::<String>(&mut fields, "oauth_apps", &serde_json::json!({}));
    let window = match fields.finish(Some(window)) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    match state
        .world
        .oauth_apps_page(window.resume.as_deref(), window.fetch())
    {
        Ok((rows, total)) => list::answer(rows, &window, total, |a| a.client_id.clone()),
        Err(e) => internal(e),
    }
}

async fn get_oauth_app(
    State(state): State<AppState>,
    UrlPath(client_id): UrlPath<String>,
) -> Response<Body> {
    read_oauth_app(&state, client_id)
}

/// No client id is `list`, but `GET /oauth-apps/list` is still a point read
/// of one, so it answers 404 in the error shape like any other.
async fn get_oauth_app_named_list(State(state): State<AppState>) -> Response<Body> {
    read_oauth_app(&state, "list".to_string())
}

/// One OAuth app, read by a client id the caller already holds, answered as
/// the list of apps answers it. Never a secret.
fn read_oauth_app(state: &AppState, client_id: String) -> Response<Body> {
    match state.world.listed_oauth_app_by_client_id(&client_id) {
        Ok(Some(app)) => json(StatusCode::OK, serde_json::json!(app)),
        Ok(None) => errors(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &WorldError::NoSuchOAuthApp(client_id).to_string(),
            None,
        ),
        Err(e) => internal(e),
    }
}

fn clock_body(reading: ClockReading) -> serde_json::Value {
    let rfc3339 = jiff::Timestamp::from_second(reading.clock_unix)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| "invalid".to_string());
    serde_json::json!({
        "clock_unix": reading.clock_unix,
        "rfc3339": rfc3339,
        "mode": reading.mode,
    })
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

    /// A field the route can go without and that has no null value: absent
    /// is `None`, and `null` is a fault, recorded like one that fails to
    /// decode.
    pub(crate) fn optional_not_null<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &'static str,
    ) -> Option<T> {
        self.read.push(name);
        let raw = self.unread.remove(name)?;
        let value = self.decode::<Option<T>>(name, &raw)?;
        if value.is_none() {
            self.fault(Fault::invalid(
                format!("{name} cannot be null; leave it out to keep its value"),
                name,
            ));
        }
        value
    }

    /// A PATCH's field that can be cleared: absent is `None`, which keeps
    /// the value, `null` is `Some(None)`, which clears it, and a value sets
    /// it. One that fails to decode is `None`, and is recorded.
    pub(crate) fn patched<T: serde::de::DeserializeOwned>(
        &mut self,
        name: &'static str,
    ) -> Option<Option<T>> {
        self.read.push(name);
        let raw = self.unread.remove(name)?;
        self.decode::<Option<T>>(name, &raw)
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

    /// Whether any fault has been found in what was read so far.
    pub(crate) fn has_faults(&self) -> bool {
        self.not_an_object.is_some() || !self.faults.is_empty()
    }

    /// What the route read, if the body has no fault. Otherwise the error
    /// shape with every fault, in the order found, then one for each field
    /// the route does not take.
    pub(crate) fn finish<T>(self, read: Option<T>) -> Result<T, Box<Response<Body>>> {
        if let Some(fault) = self.not_an_object {
            return Err(Box::new(faults(StatusCode::BAD_REQUEST, vec![fault])));
        }
        let takes = if self.read.is_empty() {
            "no fields".to_string()
        } else {
            self.read.join(", ")
        };
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

/// A JSON object's members, and the names of any that appear more than once,
/// in the order their first repeat was found. A map keeps one value under
/// each name, so a body that repeats one is refused rather than read with a
/// value dropped.
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
        // `repeated` keeps the order; this set answers membership, so a body
        // repeating many names is still read in O(n log n).
        let mut seen_repeated = BTreeSet::new();
        while let Some((name, value)) = map.next_entry::<String, Box<RawValue>>()? {
            match object.members.entry(name) {
                Entry::Vacant(slot) => {
                    slot.insert(value);
                }
                Entry::Occupied(held) => {
                    if seen_repeated.insert(held.key().clone()) {
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
    pub(crate) fn new(reason: &'static str, message: String, field: Option<&str>) -> Fault {
        Fault {
            reason,
            message,
            field: field.map(str::to_string),
        }
    }

    pub(crate) fn invalid(message: String, field: impl Into<String>) -> Fault {
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
