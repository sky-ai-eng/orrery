//! GitHub Apps: the routes an App calls as itself, with a JWT it signs, and
//! what an installation token may reach (spec/surface-github.md §1.11).
//!
//! An App's own routes carry no API token, so they mount outside face auth,
//! behind [`authenticate`]. An installation token is an API token like any
//! other: face auth resolves it to the App's bot, and [`limit_installation`]
//! keeps it to the repositories its installation covers.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, OriginalUri, Path as UrlPath, RawQuery, Request, State};
use axum::http::{HeaderMap, Method, Response, StatusCode, header};
use axum::middleware::{self, Next};
use axum::routing::{any, get, post};
use base64::Engine as _;
use orrery_world::{
    AccessLevel, App, Caller, ClockMode, Installation, Permissions, Repo, RepositorySelection,
    WorldError,
};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

use crate::reads::{page_params, paginate};
use crate::render::{actor_json, owner_type, repo_json, rfc3339};
use crate::{GithubFace, github_error, internal, json_response, media, resolve_repo};

/// The longest an App's JWT may last, from `iat` to `exp`: GitHub's ten
/// minutes.
const JWT_MAX_SECONDS: i64 = 600;

const JWT_NOT_DECODED: &str = "A JSON web token could not be decoded";
const JWT_EXP: &str = "'Expiration time' claim ('exp') must be a numeric value representing the future time at which the assertion expires";
const JWT_EXP_TOO_FAR: &str = "'Expiration time' claim ('exp') is too far in the future";
const JWT_IAT: &str = "'Issued at' claim ('iat') must be an Integer representing the time that the assertion was issued";

/// The routes an App calls as itself, and the one that checks an OAuth
/// client's credentials, at their paths under `/api/v3`. Every other path
/// under `/api/v3/app/` is an App's too, so it is authenticated the same way
/// before it answers that it is not mocked.
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
        .method_not_allowed_fallback(not_mocked)
        .layer(middleware::from_fn_with_state(face.clone(), authenticate));
    Router::new()
        .route("/api/v3/applications/{client_id}/token", post(check_token))
        .merge(as_app)
        .with_state(face)
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

/// The App a request's JWT names and one of its keys signed.
///
/// The JWT is RS256 over `iss`, `iat`, and `exp`. `iss` is the App's id, as
/// a number or a string, or its client id. `exp` is at most ten minutes
/// after `iat`. A `real` world also holds both to its clock, as GitHub
/// does. A `manual` world's clock is not the signer's, so it compares
/// neither to it (spec/usage.md, "Time").
fn app_of(face: &GithubFace, headers: &HeaderMap) -> Result<App, Box<Response<Body>>> {
    let not_decoded = || unauthorized(JWT_NOT_DECODED);
    let jwt = bearer(headers).ok_or_else(not_decoded)?;
    let mut parts = jwt.split('.');
    let (Some(head), Some(claims), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(not_decoded());
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
        return Err(not_decoded());
    }
    let claims_json = json(claims).ok_or_else(not_decoded)?;
    let signature = decode(signature).ok_or_else(not_decoded)?;

    let app = match issuer(face, claims_json.get("iss")) {
        Ok(Some(app)) => app,
        Ok(None) => return Err(not_decoded()),
        Err(e) => return Err(Box::new(internal(e))),
    };
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

    let exp = claims_json
        .get("exp")
        .and_then(numeric_date)
        .ok_or_else(|| unauthorized(JWT_EXP))?;
    let iat = claims_json
        .get("iat")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| unauthorized(JWT_IAT))?;
    if exp.saturating_sub(iat) > JWT_MAX_SECONDS {
        return Err(unauthorized(JWT_EXP_TOO_FAR));
    }
    let clock = face.world.read_clock().map_err(|e| Box::new(internal(e)))?;
    if clock.mode == ClockMode::Real {
        let now = clock.clock_unix;
        if exp <= now {
            return Err(unauthorized(JWT_EXP));
        }
        if exp > now.saturating_add(JWT_MAX_SECONDS) {
            return Err(unauthorized(JWT_EXP_TOO_FAR));
        }
        if iat > now {
            return Err(unauthorized(JWT_IAT));
        }
    }
    Ok(app)
}

/// A JWT `NumericDate`: seconds since the epoch, which may carry a fraction.
fn numeric_date(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|seconds| seconds.floor() as i64))
}

/// The App an `iss` claim names: its id, as a number or as digits, or its
/// client id.
fn issuer(face: &GithubFace, iss: Option<&serde_json::Value>) -> Result<Option<App>, WorldError> {
    match iss {
        Some(serde_json::Value::Number(id)) => match id.as_i64() {
            Some(id) => face.world.app_by_id(id),
            None => Ok(None),
        },
        Some(serde_json::Value::String(iss)) => match iss.parse::<i64>() {
            Ok(id) => face.world.app_by_id(id),
            Err(_) => face.world.app_by_client_id(iss),
        },
        _ => Ok(None),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = auth.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token"))
        .then(|| value.trim())
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
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let (installations, total) =
        match face
            .world
            .installations_slice(app.id, (page - 1) * per_page, per_page)
        {
            Ok(found) => found,
            Err(e) => return internal(e),
        };
    let items: Vec<_> = installations
        .iter()
        .map(|i| installation_json(&face, i))
        .collect();
    let mut resp = json_response(StatusCode::OK, serde_json::Value::Array(items));
    let base = face.base_url.trim_end_matches('/');
    let url = format!("{base}/api/v3/app/installations");
    if let Some(link) = link_header(&url, per_page, page, total as usize)
        && let Ok(value) = link.parse()
    {
        resp.headers_mut().insert(header::LINK, value);
    }
    resp
}

/// GitHub's `Link` header for page `page` of `total` items: `next` and
/// `last` while pages follow, `first` and `prev` after the first. `None`
/// when the list is one page.
fn link_header(url: &str, per_page: usize, page: usize, total: usize) -> Option<String> {
    let last = total.div_ceil(per_page).max(1);
    let to = |n: usize, rel: &str| format!("<{url}?per_page={per_page}&page={n}>; rel=\"{rel}\"");
    let mut links = Vec::new();
    if page > 1 {
        links.push(to((page - 1).min(last), "prev"));
    }
    if page < last {
        links.push(to(page + 1, "next"));
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

#[derive(serde::Deserialize, Default)]
struct TokenBody {
    repositories: Option<Vec<String>>,
    repository_ids: Option<serde_json::Value>,
    permissions: Option<Permissions>,
}

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
    let asked: TokenBody = match body.iter().all(u8::is_ascii_whitespace) {
        true => TokenBody::default(),
        false => match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
        },
    };
    if asked.repository_ids.is_some() {
        return crate::orrery_unsupported_field("Installation", "repository_ids");
    }
    let issued = face.world.create_installation_token(
        app.id,
        installation.id,
        asked.repositories.as_deref(),
        asked.permissions.as_ref(),
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
    UrlPath(client_id): UrlPath<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let bad_credentials = || github_error(StatusCode::UNAUTHORIZED, "Bad credentials");
    let Some((id, secret)) = crate::oauth::basic_credentials(&headers) else {
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
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let total = repos.len();
    let listed: Vec<_> = paginate(repos, per_page, page)
        .iter()
        .map(|repo| repo_json(&face, repo, &account))
        .collect();
    let selection = match grant.repos {
        Some(_) => RepositorySelection::Selected,
        None => RepositorySelection::All,
    };
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "total_count": total,
            "repository_selection": selection.as_str(),
            "repositories": listed,
        }),
    )
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
            "message": "Resource not accessible by integration",
            "documentation_url": "https://docs.github.com/rest",
            "status": "403",
        }),
    )
}

/// Keep an installation token to the repositories its installation covers,
/// on every REST route about one repository.
pub(crate) async fn limit_installation(
    State(face): State<GithubFace>,
    req: Request,
    next: Next,
) -> Response<Body> {
    let Some(caller) = req
        .extensions()
        .get::<Caller>()
        .filter(|caller| caller.installation.is_some())
    else {
        return next.run(req).await;
    };
    let mut segments = req.uri().path().trim_start_matches('/').split('/');
    let repo = match (segments.next(), segments.next(), segments.next()) {
        (Some("repos"), Some(owner), Some(name)) => match resolve_repo(&face, owner, name) {
            Ok((repo, _)) => Some(repo),
            // The route answers for a repository that does not exist.
            Err(_) => None,
        },
        (Some("repositories"), Some(id), _) => match id.parse::<i64>() {
            Ok(id) => match face.world.repo_by_id(id) {
                Ok(repo) => repo,
                Err(e) => return internal(e),
            },
            Err(_) => None,
        },
        _ => None,
    };
    let write = !matches!(*req.method(), Method::GET | Method::HEAD);
    if let Some(refused) = repo.and_then(|repo| refuse_unreached(caller, &repo, write)) {
        return refused;
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
fn app_json(face: &GithubFace, app: &App) -> serde_json::Value {
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
        "created_at": rfc3339(installation.created_clock_unix),
        "updated_at": rfc3339(installation.created_clock_unix),
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
        assert_eq!(link_header(URL, 30, 1, 0), None);
        assert_eq!(link_header(URL, 30, 1, 30), None);
    }

    #[test]
    fn a_link_names_the_pages_around_this_one() {
        assert_eq!(
            link_header(URL, 2, 1, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=2>; rel=\"next\", <{URL}?per_page=2&page=3>; rel=\"last\""
            )
        );
        assert_eq!(
            link_header(URL, 2, 2, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=1>; rel=\"prev\", <{URL}?per_page=2&page=3>; rel=\"next\", \
                 <{URL}?per_page=2&page=3>; rel=\"last\", <{URL}?per_page=2&page=1>; rel=\"first\""
            )
        );
        assert_eq!(
            link_header(URL, 2, 3, 5).unwrap(),
            format!(
                "<{URL}?per_page=2&page=2>; rel=\"prev\", <{URL}?per_page=2&page=1>; rel=\"first\""
            )
        );
    }
}
