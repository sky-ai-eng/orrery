//! The control plane's routes for apps: registering one, giving it a key,
//! and installing it on an account. GitHub offers each only in its web
//! pages, so they are reality's to do, and the control plane's. What an app
//! then does as itself, and with an installation token, is the GitHub
//! face's.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post};
use orrery_world::{
    App, AppKey, Installation, NewApp, Permissions, RepositorySelection, WorldError,
    validate_app_callback_urls, validate_app_events, validate_app_name, validate_app_permissions,
    validate_app_slug, validate_hook_url,
};

use crate::control::{Fault, Fields, SYSTEM, errors, internal};
use crate::list;
use crate::{AppState, json};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/apps", post(create_app))
        .route("/apps/list", post(list_apps).get(get_app_named_list))
        .route("/apps/{slug}", get(get_app).patch(patch_app))
        .route("/apps/{slug}/keys", post(add_key))
        .route("/apps/{slug}/webhook-secrets", post(issue_webhook_secret))
        .route("/apps/{slug}/installations", post(create_installation))
        .route("/apps/{slug}/installations/list", post(list_installations))
        .route("/apps/{slug}/installations/{id}", get(get_installation))
}

/// An app as the control plane answers it: by its public identity, with its
/// keys. Never a client secret.
#[derive(serde::Serialize)]
struct AppBody {
    id: i64,
    slug: String,
    name: String,
    client_id: String,
    /// The login of the user or organization that owns it.
    owner: String,
    callback_urls: Vec<String>,
    permissions: Permissions,
    events: Vec<String>,
    /// Where the app's deliveries go, or null for an app with no webhook.
    webhook_url: Option<String>,
    /// Whether the webhook makes deliveries. A fork starts with every
    /// webhook off.
    webhook_active: bool,
    keys: Vec<KeyBody>,
}

/// One of an app's keys as the control plane answers it.
#[derive(serde::Serialize)]
struct KeyBody {
    id: i64,
    /// One PEM block labelled `RSA PUBLIC KEY`, which is a form the route
    /// that adds a key takes, so a key read here can be sent there.
    public_key: String,
}

fn key_body(key: AppKey) -> KeyBody {
    let mut pem = String::from("-----BEGIN RSA PUBLIC KEY-----\n");
    for (n, c) in key.public_key.chars().enumerate() {
        if n > 0 && n % 64 == 0 {
            pem.push('\n');
        }
        pem.push(c);
    }
    pem.push_str("\n-----END RSA PUBLIC KEY-----\n");
    KeyBody {
        id: key.id,
        public_key: pem,
    }
}

fn app_body(state: &AppState, app: App) -> Result<AppBody, WorldError> {
    let keys = state.world.app_keys(app.id)?;
    Ok(AppBody {
        keys: keys.into_iter().map(key_body).collect(),
        id: app.id,
        slug: app.slug,
        name: app.name,
        client_id: app.client_id,
        owner: app.owner.login,
        callback_urls: app.callback_urls,
        permissions: app.permissions,
        events: app.events,
        webhook_url: app.hook.url,
        webhook_active: app.hook.active,
    })
}

/// An installation as the control plane answers it.
#[derive(serde::Serialize)]
struct InstallationBody {
    id: i64,
    /// The app's slug.
    app: String,
    /// The login of the account it is installed on.
    account: String,
    repository_selection: RepositorySelection,
    /// The repositories a `selected` installation lists, by name.
    repositories: Vec<String>,
    permissions: Permissions,
    events: Vec<String>,
}

fn installation_body(installation: Installation) -> InstallationBody {
    InstallationBody {
        id: installation.id,
        app: installation.app_slug,
        account: installation.account.login,
        repository_selection: installation.repository_selection,
        repositories: installation.repositories,
        permissions: installation.permissions,
        events: installation.events,
    }
}

/// The fault one of an app's checks found, under the field the route read
/// the value from.
fn app_fault(e: WorldError, field: &'static str) -> Fault {
    let message = match e {
        WorldError::InvalidApp { message, .. } => message,
        other => other.to_string(),
    };
    Fault::invalid(message, field)
}

fn no_such_app(slug: String) -> Response<Body> {
    errors(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        &WorldError::NoSuchApp(slug).to_string(),
        None,
    )
}

/// Register an app. `permissions` and `events` default to none, and so does
/// its webhook: `webhook_url` is where its deliveries go, and
/// `webhook_active`, false unless given, whether it makes any. The answer
/// carries the first client secret and the webhook's secret, which no read
/// returns again.
async fn create_app(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let slug: Option<String> = fields.required("slug");
    if let Some(slug) = &slug
        && let Err(e) = validate_app_slug(slug)
    {
        fields.fault(app_fault(e, "slug"));
    }
    let name: Option<String> = fields.required("name");
    if let Some(name) = &name
        && let Err(e) = validate_app_name(name)
    {
        fields.fault(app_fault(e, "name"));
    }
    let owner: Option<String> = fields.required("owner");
    let callback_urls: Option<Vec<String>> = fields.required("callback_urls");
    if let Some(urls) = &callback_urls
        && let Err(e) = validate_app_callback_urls(urls)
    {
        fields.fault(app_fault(e, "callback_urls"));
    }
    let permissions: Permissions = fields.optional("permissions").unwrap_or_default();
    if let Err(e) = validate_app_permissions(&permissions) {
        fields.fault(app_fault(e, "permissions"));
    }
    let events: Vec<String> = fields.optional("events").unwrap_or_default();
    if let Err(e) = validate_app_events(&events) {
        fields.fault(app_fault(e, "events"));
    }
    let webhook_url: Option<String> = fields.optional("webhook_url");
    if let Some(url) = &webhook_url
        && let Err(e) = validate_hook_url(url)
    {
        fields.fault(app_fault(e, "webhook_url"));
    }
    let webhook_active: bool = fields.optional("webhook_active").unwrap_or(false);
    if webhook_active && webhook_url.is_none() {
        fields.fault(Fault::invalid(
            "a webhook with no URL cannot be active".to_string(),
            "webhook_active",
        ));
    }
    let read = slug.zip(name).zip(owner).zip(callback_urls);
    let (((slug, name), owner), callback_urls) = match fields.finish(read) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    let new = NewApp {
        slug: &slug,
        name: &name,
        owner: &owner,
        callback_urls: &callback_urls,
        permissions: &permissions,
        events: &events,
        hook_url: webhook_url.as_deref(),
        hook_active: webhook_active,
    };
    let registered = match state.world.create_app(&new, SYSTEM) {
        Ok(registered) => registered,
        Err(e @ (WorldError::AppExists(_) | WorldError::AppSlugTaken(_))) => {
            return errors(
                StatusCode::CONFLICT,
                "ALREADY_EXISTS",
                &e.to_string(),
                Some("slug"),
            );
        }
        Err(WorldError::InvalidApp { field, message }) => {
            return errors(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                &message,
                Some(field),
            );
        }
        Err(e) => return internal(e),
    };
    match app_body(&state, registered.app) {
        Ok(app) => {
            let mut body = serde_json::json!(app);
            body["client_secret"] = serde_json::json!(registered.client_secret);
            body["webhook_secret"] = serde_json::json!(registered.hook_secret);
            json(StatusCode::CREATED, body)
        }
        Err(e) => internal(e),
    }
}

/// Which apps a world holds. Never a secret.
async fn list_apps(State(state): State<AppState>, body: Bytes) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    list::read_ascending(&mut fields, "slug");
    let window = list::read_window::<String>(&mut fields, "apps", &serde_json::json!({}));
    let window = match fields.finish(Some(window)) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    let (apps, total) = match state
        .world
        .apps_page(window.resume.as_deref(), window.fetch())
    {
        Ok(page) => page,
        Err(e) => return internal(e),
    };
    let rows: Result<Vec<AppBody>, _> = apps.into_iter().map(|a| app_body(&state, a)).collect();
    match rows {
        Ok(rows) => list::answer(rows, &window, total, |a| a.slug.clone()),
        Err(e) => internal(e),
    }
}

async fn get_app(State(state): State<AppState>, UrlPath(slug): UrlPath<String>) -> Response<Body> {
    read_app(&state, slug)
}

/// `list` is a valid slug, but the list's static path takes `GET /apps/list`
/// before `/apps/{slug}` can, so the point read is routed here too.
async fn get_app_named_list(State(state): State<AppState>) -> Response<Body> {
    read_app(&state, "list".to_string())
}

/// One app, read by a slug the caller already holds, answered as the list
/// of apps answers it.
fn read_app(state: &AppState, slug: String) -> Response<Body> {
    let app = match state.world.app_by_slug(&slug) {
        Ok(Some(app)) => app,
        Ok(None) => return no_such_app(slug),
        Err(e) => return internal(e),
    };
    match app_body(state, app) {
        Ok(app) => json(StatusCode::OK, serde_json::json!(app)),
        Err(e) => internal(e),
    }
}

/// Change an app's webhook: `webhook_url` points it at a URL, or with null
/// takes it off one, and `webhook_active` turns it on or off. A field left
/// out keeps its value. Answers the app as a read does.
async fn patch_app(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let url: Option<Option<String>> = fields.patched("webhook_url");
    if let Some(Some(url)) = &url
        && let Err(e) = validate_hook_url(url)
    {
        fields.fault(app_fault(e, "webhook_url"));
    }
    let active: Option<bool> = fields.optional_not_null("webhook_active");
    let (url, active) = match fields.finish(Some((url, active))) {
        Ok(read) => read,
        Err(resp) => return *resp,
    };
    let url = url.as_ref().map(|url| url.as_deref());
    match state.world.set_app_hook(&slug, url, active, SYSTEM) {
        Ok(app) => match app_body(&state, app) {
            Ok(app) => json(StatusCode::OK, serde_json::json!(app)),
            Err(e) => internal(e),
        },
        Err(WorldError::InvalidApp { field, message }) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &message,
            Some(field),
        ),
        Err(WorldError::NoSuchApp(slug)) => no_such_app(slug),
        Err(e) => internal(e),
    }
}

/// Issue a secret for the app's webhook, which replaces the one before: its
/// deliveries are signed with the newest. GitHub's settings page sets one,
/// and no API does. It takes no fields, and no read returns the secret
/// again.
async fn issue_webhook_secret(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let fields = Fields::parse(&body);
    if let Err(resp) = fields.finish(Some(())) {
        return *resp;
    }
    match state.world.issue_app_hook_secret(&slug, SYSTEM) {
        Ok(secret) => json(
            StatusCode::CREATED,
            serde_json::json!({ "slug": slug, "webhook_secret": secret }),
        ),
        Err(WorldError::NoSuchApp(slug)) => no_such_app(slug),
        Err(e) => internal(e),
    }
}

/// Add a public key the app's signatures are checked against, as GitHub's
/// settings page generates a private key and keeps its public half. A world
/// holds public keys only, so the caller makes the pair and sends that half.
async fn add_key(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let public_key: Option<String> = fields.required("public_key");
    let public_key = match fields.finish(public_key) {
        Ok(key) => key,
        Err(resp) => return *resp,
    };
    match state.world.add_app_key(&slug, &public_key, SYSTEM) {
        Ok(key) => json(StatusCode::CREATED, serde_json::json!(key_body(key))),
        Err(e @ WorldError::InvalidAppKey(_)) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &e.to_string(),
            Some("public_key"),
        ),
        Err(e @ WorldError::AppKeyExists) => errors(
            StatusCode::CONFLICT,
            "ALREADY_EXISTS",
            &e.to_string(),
            Some("public_key"),
        ),
        Err(WorldError::NoSuchApp(slug)) => no_such_app(slug),
        Err(e) => internal(e),
    }
}

/// Install the app on `account`, a user or an organization. `repositories`
/// names the account's repositories it covers, and absent or null covers
/// every repository the account owns, those created later included.
async fn create_installation(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    let account: Option<String> = fields.required("account");
    let repositories: Option<Vec<String>> = fields.optional("repositories");
    let account = match fields.finish(account) {
        Ok(account) => account,
        Err(resp) => return *resp,
    };
    match state
        .world
        .create_installation(&slug, &account, repositories.as_deref(), SYSTEM)
    {
        Ok(installation) => json(
            StatusCode::CREATED,
            serde_json::json!(installation_body(installation)),
        ),
        Err(WorldError::InvalidInstallation { field, message }) => errors(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            &message,
            Some(field),
        ),
        Err(e @ WorldError::InstallationExists { .. }) => errors(
            StatusCode::CONFLICT,
            "ALREADY_EXISTS",
            &e.to_string(),
            Some("account"),
        ),
        Err(WorldError::NoSuchApp(slug)) => no_such_app(slug),
        Err(e) => internal(e),
    }
}

/// An app's installations.
async fn list_installations(
    State(state): State<AppState>,
    UrlPath(slug): UrlPath<String>,
    body: Bytes,
) -> Response<Body> {
    let mut fields = Fields::parse(&body);
    list::read_ascending(&mut fields, "id");
    let query = serde_json::json!({ "app": slug });
    let window = list::read_window::<i64>(&mut fields, "installations", &query);
    let window = match fields.finish(Some(window)) {
        Ok(w) => w,
        Err(resp) => return *resp,
    };
    let app = match state.world.app_by_slug(&slug) {
        Ok(Some(app)) => app,
        Ok(None) => return no_such_app(slug),
        Err(e) => return internal(e),
    };
    match state
        .world
        .installations_page(app.id, window.resume, window.fetch())
    {
        Ok((rows, total)) => {
            let rows: Vec<_> = rows.into_iter().map(installation_body).collect();
            list::answer(rows, &window, total, |i| i.id)
        }
        Err(e) => internal(e),
    }
}

/// One of an app's installations, answered as the list answers it.
async fn get_installation(
    State(state): State<AppState>,
    UrlPath((slug, id)): UrlPath<(String, String)>,
) -> Response<Body> {
    let app = match state.world.app_by_slug(&slug) {
        Ok(Some(app)) => app,
        Ok(None) => return no_such_app(slug),
        Err(e) => return internal(e),
    };
    let not_found = || {
        errors(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("the app {slug:?} has no installation {id:?}"),
            None,
        )
    };
    let Ok(id) = id.parse::<i64>() else {
        return not_found();
    };
    match state.world.installation_by_id(id) {
        Ok(Some(installation)) if installation.app_id == app.id => json(
            StatusCode::OK,
            serde_json::json!(installation_body(installation)),
        ),
        Ok(_) => not_found(),
        Err(e) => internal(e),
    }
}
