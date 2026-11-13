//! GitHub Apps: the routes an App calls as itself, with a JWT it signs, and
//! what an installation token may reach (spec/surface-github.md §1.11).
//!
//! An App's own routes carry no API token, so they mount outside face auth,
//! behind [`authenticate`]. An installation token is an API token like any
//! other: face auth resolves it to the App's bot, and [`limit_installation`]
//! keeps it to the repositories its installation covers.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::RawPathParamsRejection;
use axum::extract::{
    Extension, MatchedPath, OriginalUri, Path as UrlPath, RawPathParams, RawQuery, Request, State,
};
use axum::http::{HeaderMap, Method, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::routing::{any, get, post};
use base64::Engine as _;
use orrery_world::{
    AccessLevel, App, Caller, ClockMode, Installation, OAuthApp, Permissions, Repo,
    RepositorySelection, WorldError,
};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

use crate::reads::{page_params, paginate};
use crate::render::{actor_json, owner_type, repo_json, rfc3339};
use crate::{GithubFace, github_error, internal, json_response, media, resolve_repo};

/// How far ahead of the clock a JWT's `exp` may be: GitHub's ten minutes.
const JWT_MAX_AHEAD_SECONDS: i64 = 600;

/// How far GitHub's clock and a signer's may differ: it takes an `exp` up
/// to about a minute past, and an `iat` up to about a minute ahead.
const JWT_LEEWAY_SECONDS: i64 = 60;

pub(crate) const NOT_ACCESSIBLE: &str = "Resource not accessible by integration";

/// What every token of a world's starts with (spec/grammar.md).
const TOKEN_PREFIX: &str = "orrfake_";

const BAD_CREDENTIALS: &str = "Bad credentials";
const REQUIRES_AUTHENTICATION: &str = "Requires authentication";
const JWT_NOT_DECODED: &str = "A JSON web token could not be decoded";
const JWT_ISS: &str = "'Issuer' claim ('iss') must be an Integer";
const JWT_IAT_MISSING: &str = "Missing 'issued at' claim ('iat') in assertion";
const JWT_EXP: &str = "'Expiration time' claim ('exp') must be a numeric value representing the future time at which the assertion expires";
const JWT_EXP_TOO_FAR: &str = "'Expiration time' claim ('exp') is too far in the future";

/// The routes an App calls as itself, and the ones an OAuth client calls
/// with its id and secret, at their paths under `/api/v3`.
///
/// GitHub serves more under each than Orrery does: every other path under
/// `/app` and `/applications/{client_id}`, and the lookups of the
/// installation on a repository, an organization, or a user, which take the
/// App's JWT too. Each is authenticated as its served neighbours are before
/// it answers that it is not mocked, so a caller with good credentials is
/// never told they are bad.
pub fn app_router(face: GithubFace) -> Router {
    let as_app = Router::new()
        .route("/api/v3/app", get(get_app))
        .route("/api/v3/app/installations", get(list_installations))
        .route("/api/v3/app/installations/{id}", get(get_installation))
        .route(
            "/api/v3/app/installations/{id}/access_tokens",
            post(create_token),
        )
        .route("/api/v3/app/{*rest}", any(not_mocked))
        .route("/api/v3/repos/{owner}/{repo}/installation", any(not_mocked))
        .route("/api/v3/orgs/{org}/installation", any(not_mocked))
        .route("/api/v3/users/{username}/installation", any(not_mocked))
        .method_not_allowed_fallback(not_mocked)
        .layer(middleware::from_fn_with_state(face.clone(), authenticate));
    let as_client = Router::new()
        .route("/api/v3/applications/{client_id}/token", post(check_token))
        .route("/api/v3/applications/{client_id}/{*rest}", any(not_mocked))
        .method_not_allowed_fallback(not_mocked)
        .layer(middleware::from_fn_with_state(
            face.clone(),
            authenticate_client,
        ));
    Router::new()
        .merge(as_client)
        .merge(as_app)
        .with_state(face)
}

/// Authenticate a request as the OAuth client whose id and secret it
/// carries as HTTP Basic, and attach the [`OAuthApp`].
async fn authenticate_client(
    State(face): State<GithubFace>,
    mut req: Request,
    next: Next,
) -> Response<Body> {
    let bad_credentials = || github_error(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS);
    if !req.headers().contains_key(header::AUTHORIZATION) {
        return github_error(StatusCode::UNAUTHORIZED, REQUIRES_AUTHENTICATION);
    }
    let Some((id, secret)) = crate::oauth::basic_credentials(req.headers()) else {
        return bad_credentials();
    };
    let client = match face.world.oauth_app_by_client_id(&id) {
        Ok(Some(client)) => client,
        Ok(None) => return bad_credentials(),
        Err(e) => return internal(e),
    };
    match face.world.oauth_app_holds_secret(client.id, &secret) {
        Ok(true) => {}
        Ok(false) => return bad_credentials(),
        Err(e) => return internal(e),
    }
    req.extensions_mut().insert(client);
    next.run(req).await
}

async fn not_mocked(method: Method, OriginalUri(uri): OriginalUri) -> Response<Body> {
    crate::not_mocked(method.as_str(), uri.path())
}

/// Authenticate a request as the App whose JWT it carries, and attach the
/// [`App`].
async fn authenticate(
    State(face): State<GithubFace>,
    mut req: Request,
    next: Next,
) -> Response<Body> {
    match app_of(&face, req.headers()) {
        Ok(app) => {
            req.extensions_mut().insert(app);
            next.run(req).await
        }
        Err(refused) => *refused,
    }
}

fn unauthorized(message: &str) -> Box<Response<Body>> {
    Box::new(github_error(StatusCode::UNAUTHORIZED, message))
}

/// The App a request's JWT names and one of its keys signed, held to the
/// answers github.com gave a scratch App (spec/surface-github.md §1.11).
///
/// The JWT is RS256 over `iss`, `iat`, and `exp`. `iss` is the App's id, as
/// a number or a string, or its client id. A `real` world also holds `exp`
/// and `iat` to its clock, as GitHub does. A `manual` world's clock is not
/// the signer's, so it compares neither to it (spec/usage.md, "Time").
fn app_of(face: &GithubFace, headers: &HeaderMap) -> Result<App, Box<Response<Body>>> {
    let bad_credentials = || unauthorized(BAD_CREDENTIALS);
    let not_decoded = || unauthorized(JWT_NOT_DECODED);
    let Some(authorization) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return Err(unauthorized(REQUIRES_AUTHENTICATION));
    };
    let jwt = match authorization.split_once(' ') {
        Some((scheme, value)) if scheme.eq_ignore_ascii_case("bearer") => value.trim(),
        _ => return Err(not_decoded()),
    };
    // A token of this world's, such as an installation token, is not a JWT.
    if jwt.starts_with(TOKEN_PREFIX) {
        return Err(not_decoded());
    }
    let mut parts = jwt.split('.');
    let (Some(head), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad_credentials());
    };
    let decode = |part: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part.trim_end_matches('='))
            .ok()
    };
    let json = |part: &str| -> Option<serde_json::Map<String, serde_json::Value>> {
        serde_json::from_slice(&decode(part)?).ok()
    };
    let head_json = json(head).ok_or_else(not_decoded)?;
    if head_json.get("alg").and_then(|a| a.as_str()) != Some("RS256") {
        return Err(bad_credentials());
    }
    let claims_json = json(claims).ok_or_else(not_decoded)?;
    let signature = decode(signature).ok_or_else(not_decoded)?;

    let app = issuer(face, claims_json.get("iss"))?;
    let keys = face
        .world
        .app_keys(app.id)
        .map_err(|e| Box::new(internal(e)))?;
    let signed = &jwt.as_bytes()[..head.len() + 1 + claims.len()];
    let verified = keys.iter().any(|key| {
        base64::engine::general_purpose::STANDARD
            .decode(&key.public_key)
            .is_ok_and(|der| {
                UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, der)
                    .verify(signed, &signature)
                    .is_ok()
            })
    });
    if !verified {
        return Err(not_decoded());
    }

    let iat = claims_json
        .get("iat")
        .and_then(issued_at)
        .ok_or_else(|| unauthorized(JWT_IAT_MISSING))?;
    let exp = match claims_json.get("exp") {
        None | Some(serde_json::Value::Null) => return Err(bad_credentials()),
        Some(exp) => expiration(exp).ok_or_else(|| unauthorized(JWT_EXP))?,
    };
    let clock = face.world.read_clock().map_err(|e| Box::new(internal(e)))?;
    if clock.mode == ClockMode::Real {
        let now = clock.clock_unix;
        if exp <= now.saturating_sub(JWT_LEEWAY_SECONDS)
            || iat > now.saturating_add(JWT_LEEWAY_SECONDS)
        {
            return Err(bad_credentials());
        }
        if exp > now.saturating_add(JWT_MAX_AHEAD_SECONDS) {
            return Err(unauthorized(JWT_EXP_TOO_FAR));
        }
    }
    Ok(app)
}

/// An `iat` claim as GitHub takes one: a number, with or without a
/// fraction, or a string of one.
fn issued_at(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|s| s.floor() as i64)),
        serde_json::Value::String(s) => s.parse::<f64>().ok().map(|s| s.floor() as i64),
        _ => None,
    }
}

/// An `exp` claim as GitHub takes one: an integer, or a string of one. It
/// refuses one with a fraction.
fn expiration(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// The App an `iss` claim names: its id, as a number or as digits, or its
/// client id. GitHub answers a JWT with no `iss` as bad credentials, an id
/// no App has as `404`, and any other string as not an integer.
fn issuer(face: &GithubFace, iss: Option<&serde_json::Value>) -> Result<App, Box<Response<Body>>> {
    let by_id = |id: i64| match face.world.app_by_id(id) {
        Ok(Some(app)) => Ok(app),
        Ok(None) => Err(Box::new(github_error(
            StatusCode::NOT_FOUND,
            "Integration not found",
        ))),
        Err(e) => Err(Box::new(internal(e))),
    };
    match iss {
        Some(serde_json::Value::Number(id)) => match id.as_i64() {
            Some(id) => by_id(id),
            None => Err(unauthorized(JWT_ISS)),
        },
        Some(serde_json::Value::String(iss)) => match iss.parse::<i64>() {
            Ok(id) => by_id(id),
            Err(_) => match face.world.app_by_client_id(iss) {
                Ok(Some(app)) => Ok(app),
                Ok(None) => Err(unauthorized(JWT_ISS)),
                Err(e) => Err(Box::new(internal(e))),
            },
        },
        _ => Err(unauthorized(BAD_CREDENTIALS)),
    }
}

async fn get_app(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = media::refuse_unrendered(&headers, "Integration") {
        return refused;
    }
    let installations = match face.world.installations_slice(app.id, 0, 0) {
        Ok((_, total)) => total,
        Err(e) => return internal(e),
    };
    let mut json = app_json(&face, &app);
    json["installations_count"] = serde_json::json!(installations);
    json_response(StatusCode::OK, json)
}

async fn list_installations(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = media::refuse_unrendered(&headers, "Installation") {
        return refused;
    }
    let query = query.unwrap_or_default();
    let (per_page, page) = page_params(&query);
    let (installations, total) =
        match face
            .world
            .installations_slice(app.id, (page - 1).saturating_mul(per_page), per_page)
        {
            Ok(found) => found,
            Err(e) => return internal(e),
        };
    let items: Vec<_> = installations
        .iter()
        .map(|i| installation_json(&face, i))
        .collect();
    let resp = json_response(StatusCode::OK, serde_json::Value::Array(items));
    let base = face.base_url.trim_end_matches('/');
    let url = format!("{base}/api/v3/app/installations");
    with_link(resp, &url, &query, total as usize)
}

/// `resp` with GitHub's `Link` header for the page `query` asks of `total`
/// items at `url`, when the list is more than that page.
fn with_link(mut resp: Response<Body>, url: &str, query: &str, total: usize) -> Response<Body> {
    let (per_page, page) = page_params(query);
    let asked = crate::parse_form(query.as_bytes()).contains_key("per_page");
    if let Some(link) = link_header(url, asked.then_some(per_page), per_page, page, total)
        && let Ok(value) = link.parse()
    {
        resp.headers_mut().insert(header::LINK, value);
    }
    resp
}

/// GitHub's `Link` header for page `page` of `total` items, `per_page` to a
/// page: `prev`, then `next`, then `last`, then `first`, each when there is
/// such a page and this is not it. A page past the end names the last page
/// as both `prev` and `last`. `None` for the only page. A link carries
/// `per_page` when the request did (`asked`), as GitHub's does.
fn link_header(
    url: &str,
    asked: Option<usize>,
    per_page: usize,
    page: usize,
    total: usize,
) -> Option<String> {
    let last = total.div_ceil(per_page).max(1);
    let to = |n: usize, rel: &str| match asked {
        Some(per_page) => format!("<{url}?per_page={per_page}&page={n}>; rel=\"{rel}\""),
        None => format!("<{url}?page={n}>; rel=\"{rel}\""),
    };
    let mut links = Vec::new();
    if page > 1 {
        links.push(to((page - 1).min(last), "prev"));
    }
    if page < last {
        links.push(to(page + 1, "next"));
    }
    if page != last {
        links.push(to(last, "last"));
    }
    if page > 1 {
        links.push(to(1, "first"));
    }
    (!links.is_empty()).then(|| links.join(", "))
}

/// One of the App's installations, or GitHub's 404 for an id that is not
/// one of them.
fn own_installation(
    face: &GithubFace,
    app: &App,
    id: &str,
) -> Result<Installation, Box<Response<Body>>> {
    let not_found = || Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"));
    let id: i64 = id.parse().map_err(|_| not_found())?;
    match face.world.installation_by_id(id) {
        Ok(Some(installation)) if installation.app_id == app.id => Ok(installation),
        Ok(_) => Err(not_found()),
        Err(e) => Err(Box::new(internal(e))),
    }
}

async fn get_installation(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let installation = match own_installation(&face, &app, &id) {
        Ok(i) => i,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Installation") {
        return refused;
    }
    json_response(StatusCode::OK, installation_json(&face, &installation))
}

/// The permission every installation token holds beside the ones it asks
/// for, when its installation holds it.
const METADATA: &str = "metadata";

/// Issue a token for an installation. GitHub's clients send no body at all
/// for a token that reaches the whole installation.
async fn create_token(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let installation = match own_installation(&face, &app, &id) {
        Ok(i) => i,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Installation") {
        return refused;
    }
    // A client with no options to send may send no body, or `null`. GitHub
    // reads the body laxly: a field of the wrong type narrows nothing.
    let asked: serde_json::Value = match body.iter().all(u8::is_ascii_whitespace) {
        true => serde_json::Value::Null,
        false => match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
        },
    };
    if asked.get("repository_ids").is_some() {
        return crate::orrery_unsupported_field("Installation", "repository_ids");
    }
    let repositories: Option<Vec<String>> = asked
        .get("repositories")
        .and_then(|r| r.as_array())
        .map(|r| {
            r.iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        });
    let mut permissions = Permissions::new();
    for (name, level) in asked
        .get("permissions")
        .and_then(|p| p.as_object())
        .into_iter()
        .flatten()
    {
        let level = match level.as_str() {
            Some("read") => AccessLevel::Read,
            Some("write") => AccessLevel::Write,
            Some("admin") => AccessLevel::Admin,
            _ => {
                return github_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "There is at least one permission action that is not supported. It should be one of: \"read\", \"write\" or \"admin\".",
                );
            }
        };
        permissions.insert(name.clone(), level);
    }
    // A token narrowed to some permissions still reads metadata.
    if !permissions.is_empty() && installation.permissions.contains_key(METADATA) {
        permissions
            .entry(METADATA.to_string())
            .or_insert(AccessLevel::Read);
    }
    let issued = face.world.create_installation_token(
        app.id,
        installation.id,
        repositories.as_deref(),
        Some(&permissions),
    );
    let token = match issued {
        Ok(token) => token,
        Err(WorldError::NoSuchInstallation(_)) => {
            return github_error(StatusCode::NOT_FOUND, "Not Found");
        }
        Err(WorldError::InstallationRepository(_)) => {
            return github_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "There is at least one repository that does not exist or is not accessible to the parent installation.",
            );
        }
        Err(WorldError::InstallationPermission(_)) => {
            return github_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "The permissions requested are not granted to this installation.",
            );
        }
        Err(WorldError::InstallationPermissionLevel(_)) => {
            return github_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "The level of access for permissions requested are not granted to this installation.",
            );
        }
        Err(e) => return internal(e),
    };
    let selection = match token.repositories {
        Some(_) => RepositorySelection::Selected,
        None => token.installation.repository_selection,
    };
    let mut json = serde_json::json!({
        "token": token.token,
        "expires_at": rfc3339(token.expires_clock_unix),
        "permissions": permissions_json(&token.permissions),
        "repository_selection": selection.as_str(),
    });
    if let Some(repos) = &token.repositories {
        json["repositories"] = repos
            .iter()
            .map(|repo| repo_json(&face, repo, &token.installation.account))
            .collect();
    }
    json_response(StatusCode::CREATED, json)
}

/// GitHub's check of a token against the OAuth client it was issued to,
/// with the client's id and secret as HTTP Basic. A wrong secret is `401`,
/// and a token the client did not issue is `404`, which is how a caller
/// holding only a client secret learns whether it is right.
async fn check_token(
    State(face): State<GithubFace>,
    Extension(client): Extension<OAuthApp>,
    UrlPath(client_id): UrlPath<String>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response<Body> {
    let not_found = || github_error(StatusCode::NOT_FOUND, "Not Found");
    if client.client_id != client_id {
        return not_found();
    }
    let token = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|b| b.get("access_token")?.as_str().map(str::to_string));
    let Some(token) = token else {
        return crate::validation_missing("OauthAccess", "access_token");
    };
    match face.world.caller_by_token(&token) {
        // The client did issue it, and GitHub answers the authorization it
        // stands for, which Orrery does not render.
        Ok(Some(caller)) if caller.app_id == Some(client.id) && caller.installation.is_none() => {
            crate::not_mocked("POST", uri.path())
        }
        Ok(_) => not_found(),
        Err(e) => internal(e),
    }
}

/// The repositories the caller's installation token reaches.
pub(crate) async fn list_installation_repos(
    State(face): State<GithubFace>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let Some(grant) = &caller.installation else {
        return github_error(
            StatusCode::FORBIDDEN,
            "You must authenticate with an installation access token in order to list repositories for an installation.",
        );
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    let repos = match face.world.granted_repos(grant) {
        Ok(repos) => repos,
        Err(e) => return internal(e),
    };
    let account = match face.world.actor_by_login(&grant.account) {
        Ok(Some(account)) => account,
        Ok(None) => return internal("installation account missing"),
        Err(e) => return internal(e),
    };
    let query = query.unwrap_or_default();
    let (per_page, page) = page_params(&query);
    let total = repos.len();
    let listed: Vec<_> = paginate(repos, per_page, page)
        .iter()
        .map(|repo| repo_json(&face, repo, &account))
        .collect();
    let selection = match grant.repos {
        Some(_) => RepositorySelection::Selected,
        None => RepositorySelection::All,
    };
    let resp = json_response(
        StatusCode::OK,
        serde_json::json!({
            "total_count": total,
            "repository_selection": selection.as_str(),
            "repositories": listed,
        }),
    );
    let base = face.base_url.trim_end_matches('/');
    let url = format!("{base}/api/v3/installation/repositories");
    with_link(resp, &url, &query, total)
}

/// What an installation token is refused when it does not reach `repo`, or
/// `None` when the request may go on: a private repository does not exist
/// for it, and a public one can be read by anyone but not written.
pub(crate) fn refuse_unreached(
    caller: &Caller,
    repo: &Repo,
    write: bool,
) -> Option<Response<Body>> {
    if caller.reaches(repo) {
        return None;
    }
    if repo.private {
        return Some(github_error(StatusCode::NOT_FOUND, "Not Found"));
    }
    write.then(not_accessible)
}

/// GitHub's refusal of something an installation token may not do.
pub(crate) fn not_accessible() -> Response<Body> {
    json_response(
        StatusCode::FORBIDDEN,
        serde_json::json!({
            "message": NOT_ACCESSIBLE,
            "documentation_url": "https://docs.github.com/rest",
            "status": "403",
        }),
    )
}

/// Keep an installation token to what its installation covers, on every
/// REST route about one repository or one organization.
///
/// The repository and the organization are read from the route's own path
/// parameters, as the handler reads them, so the two never disagree about
/// which one a path names, however the path spells it.
pub(crate) async fn limit_installation(
    State(face): State<GithubFace>,
    matched: Option<MatchedPath>,
    params: Result<RawPathParams, RawPathParamsRejection>,
    req: Request,
    next: Next,
) -> Response<Body> {
    let caller = req
        .extensions()
        .get::<Caller>()
        .filter(|caller| caller.installation.is_some());
    let (Some(caller), Ok(params)) = (caller, params) else {
        return next.run(req).await;
    };
    let param = |name: &str| params.iter().find(|(key, _)| *key == name).map(|(_, v)| v);
    let write = !matches!(*req.method(), Method::GET | Method::HEAD);
    let by_id = matched.is_some_and(|m| m.as_str().ends_with("/repositories/{id}"));
    let repo = match (param("owner"), param("repo"), param("id")) {
        (Some(owner), Some(name), _) => match resolve_repo(&face, owner, name) {
            Ok((repo, _)) => Some(repo),
            // The route answers for a repository that does not exist.
            Err(_) => None,
        },
        (_, _, Some(id)) if by_id => match id.parse::<i64>() {
            Ok(id) => match face.world.repo_by_id(id) {
                Ok(repo) => repo,
                Err(e) => return internal(e),
            },
            Err(_) => None,
        },
        _ => None,
    };
    if let Some(refused) = repo.and_then(|repo| refuse_unreached(caller, &repo, write)) {
        return refused;
    }
    // An organization the App is not installed on is not the token's to
    // change: it creates no repository and no team there.
    let elsewhere = param("org").is_some_and(|org| {
        caller
            .installation
            .as_ref()
            .is_some_and(|grant| !grant.account.eq_ignore_ascii_case(org))
    });
    if write && elsewhere {
        return not_accessible();
    }
    next.run(req).await
}

fn permissions_json(permissions: &Permissions) -> serde_json::Value {
    permissions
        .iter()
        .map(|(name, level)| {
            let level = match level {
                AccessLevel::Read => "read",
                AccessLevel::Write => "write",
                AccessLevel::Admin => "admin",
            };
            (name.clone(), serde_json::json!(level))
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// GitHub's App object.
pub(crate) fn app_json(face: &GithubFace, app: &App) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let html_url = format!("{base}/apps/{}", app.slug);
    serde_json::json!({
        "id": app.id,
        "slug": app.slug,
        "client_id": app.client_id,
        "node_id": crate::nodeid::app(app.id),
        "owner": actor_json(&app.owner),
        "name": app.name,
        "description": serde_json::Value::Null,
        "external_url": html_url,
        "html_url": html_url,
        "created_at": rfc3339(app.created_clock_unix),
        "updated_at": rfc3339(app.created_clock_unix),
        "permissions": permissions_json(&app.permissions),
        "events": app.events,
    })
}

/// A time as GitHub writes an installation's: with milliseconds, where its
/// other objects have none.
fn rfc3339_millis(unix: i64) -> String {
    rfc3339(unix).replacen('Z', ".000Z", 1)
}

/// GitHub's installation object.
fn installation_json(face: &GithubFace, installation: &Installation) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let account = &installation.account;
    let html_url = match account.kind {
        orrery_world::ActorKind::Org => format!(
            "{base}/organizations/{}/settings/installations/{}",
            account.login, installation.id
        ),
        _ => format!("{base}/settings/installations/{}", installation.id),
    };
    serde_json::json!({
        "id": installation.id,
        "account": actor_json(account),
        "repository_selection": installation.repository_selection.as_str(),
        "access_tokens_url": format!(
            "{base}/api/v3/app/installations/{}/access_tokens",
            installation.id
        ),
        "repositories_url": format!("{base}/api/v3/installation/repositories"),
        "html_url": html_url,
        "app_id": installation.app_id,
        "app_slug": installation.app_slug,
        "client_id": installation.client_id,
        "target_id": account.id,
        "target_type": owner_type(account.kind),
        "permissions": permissions_json(&installation.permissions),
        "events": installation.events,
        "created_at": rfc3339_millis(installation.created_clock_unix),
        "updated_at": rfc3339_millis(installation.created_clock_unix),
        "single_file_name": serde_json::Value::Null,
        "has_multiple_single_files": false,
        "single_file_paths": [],
        "suspended_by": serde_json::Value::Null,
        "suspended_at": serde_json::Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::link_header;

    const URL: &str = "http://h/api/v3/app/installations";

    #[test]
    fn a_list_of_one_page_has_no_link() {
        assert_eq!(link_header(URL, None, 30, 1, 0), None);
        assert_eq!(link_header(URL, Some(30), 30, 1, 30), None);
    }

    // Each as github.com answered it.
    #[test]
    fn a_page_past_the_end_names_the_last_page_twice() {
        assert_eq!(
            link_header(URL, Some(1), 1, 3, 2).unwrap(),
            format!(
                "<{URL}?per_page=1&page=2>; rel=\"prev\", <{URL}?per_page=1&page=2>; rel=\"last\", \
                 <{URL}?per_page=1&page=1>; rel=\"first\""
            )
        );
        assert_eq!(
            link_header(URL, None, 30, 2, 2).unwrap(),
            format!(
                "<{URL}?page=1>; rel=\"prev\", <{URL}?page=1>; rel=\"last\", <{URL}?page=1>; rel=\"first\""
            )
        );
    }

    #[test]
    fn a_link_names_the_pages_around_this_one() {
        assert_eq!(
            link_header(URL, Some(2), 2, 1, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=2>; rel=\"next\", <{URL}?per_page=2&page=3>; rel=\"last\""
            )
        );
        assert_eq!(
            link_header(URL, Some(2), 2, 2, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=1>; rel=\"prev\", <{URL}?per_page=2&page=3>; rel=\"next\", \
                 <{URL}?per_page=2&page=3>; rel=\"last\", <{URL}?per_page=2&page=1>; rel=\"first\""
            )
        );
        assert_eq!(
            link_header(URL, Some(2), 2, 3, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=2>; rel=\"prev\", <{URL}?per_page=2&page=1>; rel=\"first\""
            )
        );
    }
}
