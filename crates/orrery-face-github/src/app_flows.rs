//! The pages a person's browser is sent to for a GitHub App: registering one
//! from a manifest, and installing one on an account. They are on the web
//! host, as the OAuth pages are, and carry no API token.
//!
//! Each page is a stand-in for GitHub's. A world has no passwords and no
//! browser sessions (rule 12), so a page asks for a login, and naming an
//! existing user is what signing in means, as on the authorize page. What
//! GitHub decides from who is signed in and what they may do, a page asks
//! for: the account to install on, the repositories, and whether the person
//! may install or can only ask to.

use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{Response, StatusCode};
use axum::routing::{get, post};
use orrery_world::{
    Actor, ActorKind, App, Attribution, Door, Installation, NewApp, Permissions, WorldError,
};

use crate::oauth::{escape, html, page, redirect, with_query};
use crate::{GithubFace, github_error, internal, json_response, parse_form};

/// The longest name GitHub gives an App.
const MAX_NAME: usize = 34;

/// The pages, and the call that trades a manifest's code. With `pages`
/// off, each page answers that this world does not serve it: a page signs
/// a person in by their login alone, which a world whose faces anyone can
/// reach cannot offer. The trade stays, and finds no code to trade.
pub fn web_router(face: GithubFace, pages: bool) -> Router {
    let conversions =
        Router::new().route("/api/v3/app-manifests/{code}/conversions", post(convert));
    let pages = match pages {
        true => Router::new()
            .route("/settings/apps/new", post(register))
            .route(
                "/organizations/{org}/settings/apps/new",
                post(register_for_org),
            )
            .route(
                "/apps/{slug}/installations/new",
                get(install_page).post(install),
            )
            .route(
                "/apps/{slug}/installations/new/permissions",
                get(install_page).post(install),
            ),
        false => Router::new()
            .route("/settings/apps/new", post(closed))
            .route("/organizations/{org}/settings/apps/new", post(closed))
            .route("/apps/{slug}/installations/new", get(closed).post(closed))
            .route(
                "/apps/{slug}/installations/new/permissions",
                get(closed).post(closed),
            ),
    };
    pages
        .merge(conversions)
        .route("/settings/installations/{id}", get(installation_page))
        .route(
            "/organizations/{org}/settings/installations/{id}",
            get(org_installation_page),
        )
        .with_state(face)
}

/// What a page answers in a world that does not serve it.
async fn closed() -> Response<Body> {
    html(
        StatusCode::FORBIDDEN,
        &page(
            "Not served",
            "<h1>Not served</h1><p>orrery: this world does not serve the pages that register \
             and install a GitHub App (server.app_pages = false). Register and install an App \
             on the control plane.</p>",
        ),
    )
}

// ---- registering an App from a manifest ----

/// What a manifest asks for, as the fields a world reads. `url` is the
/// App's home page, which GitHub requires and a world keeps only as a
/// callback URL of last resort. GitHub's manifest also carries
/// `description` and `public`, which a world has no use for, and
/// `setup_on_update`, which is about a page a world does not serve.
#[derive(serde::Deserialize)]
struct Manifest {
    name: Option<String>,
    url: Option<String>,
    hook_attributes: Option<HookAttributes>,
    redirect_url: Option<String>,
    #[serde(default)]
    callback_urls: Vec<String>,
    setup_url: Option<String>,
    #[serde(default)]
    default_events: Vec<String>,
    #[serde(default)]
    default_permissions: Permissions,
    #[serde(default)]
    request_oauth_on_install: bool,
}

#[derive(serde::Deserialize)]
struct HookAttributes {
    url: String,
    /// GitHub turns a manifest's webhook on unless it says otherwise.
    #[serde(default = "yes")]
    active: bool,
}

fn yes() -> bool {
    true
}

async fn register(
    State(face): State<GithubFace>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response<Body> {
    registration(&face, None, query.as_deref().unwrap_or(""), &body)
}

async fn register_for_org(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response<Body> {
    match face.world.actor_by_login(&org) {
        Ok(Some(actor)) if actor.kind == ActorKind::Org => {
            registration(&face, Some(&actor), query.as_deref().unwrap_or(""), &body)
        }
        Ok(_) => not_found(),
        Err(e) => internal(e),
    }
}

/// The manifest form's post. The caller's page posts the manifest alone,
/// and the answer is the page a person confirms on. That page posts the
/// manifest back with a login, which registers the App and sends the person
/// to the manifest's `redirect_url` with the code its caller trades for the
/// App's credentials.
fn registration(
    face: &GithubFace,
    org: Option<&Actor>,
    query: &str,
    body: &[u8],
) -> Response<Body> {
    let form = parse_form(body);
    let raw = form.get("manifest").map(String::as_str).unwrap_or("");
    let manifest: Manifest = match serde_json::from_str(raw) {
        Ok(manifest) => manifest,
        Err(e) => {
            return html(
                StatusCode::UNPROCESSABLE_ENTITY,
                &page(
                    "Invalid GitHub App manifest",
                    &format!(
                        "<h1>Invalid GitHub App manifest</h1><p role=\"alert\">{}</p>",
                        escape(&e.to_string())
                    ),
                ),
            );
        }
    };
    let state = parse_form(query.as_bytes()).get("state").cloned();
    // The page a person confirms on posts the name back, and they may have
    // changed it. The caller's own post carries none.
    let name = match form.get("name") {
        Some(name) => name.trim().to_string(),
        None => manifest.name.clone().unwrap_or_default(),
    };
    let back = manifest
        .redirect_url
        .as_deref()
        .filter(|url| !url.is_empty());
    let again = |status: StatusCode, error: Option<&str>| {
        html(status, &registration_form(org, query, raw, &name, error))
    };
    let Some(login) = form.get("login") else {
        return again(StatusCode::OK, None);
    };
    let person = match face.world.actor_by_login(login) {
        Ok(Some(actor)) if actor.kind == ActorKind::User => actor,
        Ok(_) => return again(StatusCode::OK, Some("Incorrect username or password.")),
        Err(e) => return internal(e),
    };
    if name.is_empty() {
        return again(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Name can't be blank"),
        );
    }
    if name.chars().count() > MAX_NAME {
        return again(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Name is too long (maximum is 34 characters)"),
        );
    }
    let home = manifest.url.as_deref().unwrap_or("");
    if home.is_empty() {
        return again(StatusCode::UNPROCESSABLE_ENTITY, Some("Url can't be blank"));
    }
    // The person is redirected there once the App is registered, so it is
    // checked before anything is.
    if back.is_some_and(|url| orrery_world::validate_hook_url(url).is_err()) {
        return again(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Redirect url must be an http or https URL"),
        );
    }
    // An App that asks for authorization on install sends the person to its
    // callback URL with a code, so it has to name one of its own.
    if manifest.request_oauth_on_install && manifest.callback_urls.is_empty() {
        return again(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("Callback url can't be blank for an App that requests authorization on install"),
        );
    }
    // An App here holds at least one callback URL, where GitHub lets one
    // have none: a manifest that names none is given the URL it sends the
    // person back to, or else its home page.
    let callback_urls = match manifest.callback_urls.is_empty() {
        false => manifest.callback_urls.clone(),
        true => vec![back.unwrap_or(home).to_string()],
    };
    let slug = slug_of(&name);
    let owner = org.unwrap_or(&person);
    let new = NewApp {
        slug: &slug,
        name: &name,
        owner: &owner.login,
        callback_urls: &callback_urls,
        permissions: &manifest.default_permissions,
        events: &manifest.default_events,
        hook_url: manifest.hook_attributes.as_ref().map(|h| h.url.as_str()),
        hook_active: manifest.hook_attributes.as_ref().is_some_and(|h| h.active),
        setup_url: manifest.setup_url.as_deref(),
        request_oauth_on_install: manifest.request_oauth_on_install,
    };
    let by = Attribution::new(person.id, Door::Web);
    let made = match face.world.create_app_from_manifest(&new, by) {
        Ok(made) => made,
        Err(WorldError::AppExists(_) | WorldError::AppSlugTaken(_)) => {
            return again(
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("Name is already in use"),
            );
        }
        Err(WorldError::InvalidApp { field, message }) => {
            return again(
                StatusCode::UNPROCESSABLE_ENTITY,
                Some(&format!("{field}: {message}")),
            );
        }
        Err(e) => return internal(e),
    };
    let Some(back) = back else {
        // With nowhere to send the person, GitHub shows them the App.
        return html(
            StatusCode::OK,
            &page(
                &made.app.name,
                &format!("<h1>{}</h1><p>Registered.</p>", escape(&made.app.name)),
            ),
        );
    };
    let mut params = vec![("code", made.code.as_str())];
    if let Some(state) = &state {
        params.push(("state", state.as_str()));
    }
    redirect(&with_query(back, &params))
}

/// GitHub's slug for an App's name: lower case, with every run of other
/// characters a hyphen.
fn slug_of(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => slug.push(c),
            _ if slug.ends_with('-') => {}
            _ => slug.push('-'),
        }
    }
    slug.trim_matches('-').to_string()
}

fn registration_form(
    org: Option<&Actor>,
    query: &str,
    manifest: &str,
    name: &str,
    error: Option<&str>,
) -> String {
    let action = match org {
        Some(org) => format!("/organizations/{}/settings/apps/new", org.login),
        None => "/settings/apps/new".to_string(),
    };
    let action = match query.is_empty() {
        true => action,
        false => format!("{action}?{query}"),
    };
    let title = match org {
        Some(org) => format!("Create GitHub App for {}", org.login),
        None => "Create GitHub App".to_string(),
    };
    page(
        &title,
        &format!(
            r#"<h1>{title}</h1>{flash}
<form method="post" action="{action}">
<input type="hidden" name="manifest" value="{manifest}">
<label for="name">GitHub App name</label>
<input id="name" name="name" type="text" value="{name}">
<label for="login">Username</label>
<input id="login" name="login" type="text" autocomplete="username" autofocus>
<button type="submit">Create GitHub App</button>
</form>"#,
            title = escape(&title),
            flash = flash(error),
            action = escape(&action),
            manifest = escape(manifest),
            name = escape(name),
        ),
    )
}

fn flash(error: Option<&str>) -> String {
    error
        .map(|e| format!(r#"<p role="alert">{}</p>"#, escape(e)))
        .unwrap_or_default()
}

fn not_found() -> Response<Body> {
    html(
        StatusCode::NOT_FOUND,
        &page("Not Found", "<p>Not Found</p>"),
    )
}

/// `POST /app-manifests/{code}/conversions`: `201` with the App and the
/// credentials it is given once. GitHub takes no credential but the code,
/// and answers `404` for one it does not know, has traded, or has let
/// expire.
async fn convert(State(face): State<GithubFace>, UrlPath(code): UrlPath<String>) -> Response<Body> {
    match face.world.convert_app_manifest(&code) {
        Ok(converted) => {
            let mut json = crate::apps::app_json(&face, &converted.app);
            json["client_secret"] = converted.client_secret.into();
            json["webhook_secret"] = converted.hook_secret.into();
            json["pem"] = converted.private_key_pem.into();
            json_response(StatusCode::CREATED, json)
        }
        Err(WorldError::NoSuchAppManifest(_)) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => internal(e),
    }
}

// ---- installing an App ----

/// The account the URL's `target_id` names, when it carries one. GitHub's
/// `…/installations/new/permissions?target_id=` is the install page with its
/// account chosen.
fn target(
    face: &GithubFace,
    params: &BTreeMap<String, String>,
) -> Result<Option<Actor>, Box<Response<Body>>> {
    let Some(raw) = params.get("target_id") else {
        return Ok(None);
    };
    let found = match raw.parse::<i64>() {
        Ok(id) => face
            .world
            .actor_by_id(id)
            .map_err(|e| Box::new(internal(e)))?,
        Err(_) => None,
    };
    match found {
        Some(actor) if matches!(actor.kind, ActorKind::User | ActorKind::Org) => Ok(Some(actor)),
        _ => Err(Box::new(not_found())),
    }
}

fn app_by_slug(face: &GithubFace, slug: &str) -> Result<App, Box<Response<Body>>> {
    match face.world.app_by_slug(slug) {
        Ok(Some(app)) => Ok(app),
        Ok(None) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(internal(e))),
    }
}

async fn install_page(
    State(face): State<GithubFace>,
    UrlPath(slug): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    let app = match app_by_slug(&face, &slug) {
        Ok(app) => app,
        Err(resp) => return *resp,
    };
    let account = match target(&face, &parse_form(query.as_bytes())) {
        Ok(account) => account,
        Err(resp) => return *resp,
    };
    html(
        StatusCode::OK,
        &install_form(&app, &query, account.as_ref(), &BTreeMap::new(), None),
    )
}

/// The install page's post: installs the App on the account for the person
/// named, or, with `setup_action=request`, only asks to, which is what
/// GitHub lets a person do who may not install on that account. Then the
/// person is sent on as GitHub sends them: through the App's authorization
/// to its callback URL when it asks for that, else to its setup URL, else
/// to the installation's own page.
async fn install(
    State(face): State<GithubFace>,
    UrlPath(slug): UrlPath<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    let app = match app_by_slug(&face, &slug) {
        Ok(app) => app,
        Err(resp) => return *resp,
    };
    let asked = parse_form(query.as_bytes());
    let form = parse_form(&body);
    let chosen = match target(&face, &asked) {
        Ok(account) => account,
        Err(resp) => return *resp,
    };
    let again = |status: StatusCode, error: &str| {
        html(
            status,
            &install_form(&app, &query, chosen.as_ref(), &form, Some(error)),
        )
    };
    let field = |name: &str| form.get(name).map(String::as_str).unwrap_or("").trim();
    let person = match face.world.actor_by_login(field("login")) {
        Ok(Some(actor)) if actor.kind == ActorKind::User => actor,
        Ok(_) => return again(StatusCode::OK, "Incorrect username or password."),
        Err(e) => return internal(e),
    };
    // The account is the one the URL chose, else the one the form names,
    // else the person's own.
    let account = match (&chosen, field("account")) {
        (Some(account), _) => account.login.clone(),
        (None, "") => person.login.clone(),
        (None, named) => named.to_string(),
    };
    let state = asked.get("state").map(String::as_str);
    if field("setup_action") == "request" {
        return sent_on(&face, &app, &person, None, state);
    }
    let repositories: Option<Vec<String>> = match field("repository_selection") {
        "selected" => Some(
            field("repositories")
                .split([',', ' ', '\n'])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        _ => None,
    };
    let by = Attribution::new(person.id, Door::Web).with_app(Some(app.id));
    match face
        .world
        .create_installation(&app.slug, &account, repositories.as_deref(), by)
    {
        Ok(installation) => sent_on(&face, &app, &person, Some(&installation), state),
        // GitHub shows a person the installation an account already has.
        Err(WorldError::InstallationExists { .. }) => {
            let held = face
                .world
                .actor_by_login(&account)
                .and_then(|account| match account {
                    Some(account) => face.world.installation_on(app.id, account.id),
                    None => Ok(None),
                });
            match held {
                Ok(Some(installation)) => redirect(&installation_url(&face, &installation)),
                Ok(None) => internal("the installation an account holds is missing"),
                Err(e) => internal(e),
            }
        }
        Err(WorldError::InvalidInstallation { field, message }) => again(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("{field}: {message}"),
        ),
        Err(e) => internal(e),
    }
}

/// Where a person goes once they have installed an App, or asked to:
/// `installation` is `None` for a request. GitHub adds `installation_id`
/// for an installation, `setup_action`, the `state` the install URL
/// carried, and, for an App that asks for authorization on install, the
/// `code` of the authorization the person has just given.
fn sent_on(
    face: &GithubFace,
    app: &App,
    person: &Actor,
    installation: Option<&Installation>,
    state: Option<&str>,
) -> Response<Body> {
    let id = installation.map(|installation| installation.id.to_string());
    let action = match installation {
        Some(_) => "install",
        None => "request",
    };
    let mut params: Vec<(&str, &str)> = Vec::new();
    let code;
    let to = if app.request_oauth_on_install {
        let oauth = match face.world.oauth_app_by_client_id(&app.client_id) {
            Ok(Some(oauth)) => oauth,
            Ok(None) => return internal("an app's OAuth app is missing"),
            Err(e) => return internal(e),
        };
        let callback = app.callback_urls[0].clone();
        code = match face.world.issue_oauth_code(&oauth, person, &callback, "") {
            Ok(code) => code,
            Err(e) => return internal(e),
        };
        params.push(("code", code.as_str()));
        Some(callback)
    } else {
        app.setup_url.clone()
    };
    if let Some(id) = &id {
        params.push(("installation_id", id.as_str()));
    }
    params.push(("setup_action", action));
    if let Some(state) = state {
        params.push(("state", state));
    }
    match (to, installation) {
        (Some(to), _) => redirect(&with_query(&to, &params)),
        (None, Some(installation)) => redirect(&installation_url(face, installation)),
        (None, None) => html(
            StatusCode::OK,
            &page(
                &format!("Request to install {}", app.name),
                &format!(
                    "<h1>Request to install {}</h1><p>Requested.</p>",
                    escape(&app.name)
                ),
            ),
        ),
    }
}

/// The page of an installation: the `html_url` its REST object gives.
fn installation_url(face: &GithubFace, installation: &Installation) -> String {
    crate::apps::installation_json(face, installation)["html_url"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn install_form(
    app: &App,
    query: &str,
    account: Option<&Actor>,
    form: &BTreeMap<String, String>,
    error: Option<&str>,
) -> String {
    let path = match account {
        Some(_) => format!("/apps/{}/installations/new/permissions", app.slug),
        None => format!("/apps/{}/installations/new", app.slug),
    };
    let action = match query.is_empty() {
        true => path,
        false => format!("{path}?{query}"),
    };
    let kept = |name: &str| escape(form.get(name).map(String::as_str).unwrap_or(""));
    let account_field = match account {
        Some(account) => format!("<p>On {}</p>", escape(&account.login)),
        None => format!(
            r#"<label for="account">Account</label>
<input id="account" name="account" type="text" value="{}">"#,
            kept("account")
        ),
    };
    // A form shown again keeps the choice the person made: one that fell
    // back to every repository would install on more than they chose.
    let (all, selected) = match form.get("repository_selection").map(String::as_str) {
        Some("selected") => ("", " checked"),
        _ => (" checked", ""),
    };
    let title = format!("Install {}", app.name);
    page(
        &title,
        &format!(
            r#"<h1>{title}</h1>{flash}
<form method="post" action="{action}">
<label for="login">Username</label>
<input id="login" name="login" type="text" autocomplete="username" value="{login}" autofocus>
{account_field}
<label><input type="radio" name="repository_selection" value="all"{all}> All repositories</label>
<label><input type="radio" name="repository_selection" value="selected"{selected}> Only select repositories</label>
<label for="repositories">Repositories</label>
<input id="repositories" name="repositories" type="text" value="{repositories}">
<button type="submit" name="setup_action" value="install">Install</button>
<button type="submit" name="setup_action" value="request">Request</button>
</form>"#,
            title = escape(&title),
            flash = flash(error),
            action = escape(&action),
            login = kept("login"),
            repositories = kept("repositories"),
            all = all,
            selected = selected,
        ),
    )
}

async fn installation_page(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
) -> Response<Body> {
    installed(&face, None, &id)
}

async fn org_installation_page(
    State(face): State<GithubFace>,
    UrlPath((org, id)): UrlPath<(String, String)>,
) -> Response<Body> {
    installed(&face, Some(&org), &id)
}

/// The stand-in for the page GitHub shows of one installation, at the path
/// its `html_url` gives.
fn installed(face: &GithubFace, org: Option<&str>, id: &str) -> Response<Body> {
    let found = match id.parse::<i64>() {
        Ok(id) => match face.world.installation_by_id(id) {
            Ok(found) => found,
            Err(e) => return internal(e),
        },
        Err(_) => None,
    };
    let Some(installation) = found.filter(|installation| match (org, installation.account.kind) {
        (Some(org), ActorKind::Org) => installation.account.login.eq_ignore_ascii_case(org),
        (None, kind) => kind != ActorKind::Org,
        _ => false,
    }) else {
        return not_found();
    };
    let title = format!(
        "{} on {}",
        installation.app_slug, installation.account.login
    );
    html(
        StatusCode::OK,
        &page(
            &title,
            &format!("<h1>{}</h1><p>Installed.</p>", escape(&title)),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::slug_of;

    #[test]
    fn a_name_becomes_a_slug() {
        assert_eq!(slug_of("My App"), "my-app");
        assert_eq!(slug_of("probe-app-fe70c7"), "probe-app-fe70c7");
        assert_eq!(slug_of("  A  (new) App! "), "a-new-app");
    }
}
