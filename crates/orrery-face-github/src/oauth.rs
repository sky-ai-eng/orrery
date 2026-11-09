//! GitHub's OAuth web flow, on the web host rather than under `/api/v3`:
//! the authorize page an actor approves an app on, and the endpoint the app
//! trades the resulting code at. Neither carries an API token, so these
//! routes mount outside face auth.
//!
//! The authorize page signs an actor in by login alone. A world has no
//! passwords and no browser sessions (rule 12: every credential in it is
//! fake), so naming an existing user actor is what signing in means here.

use std::collections::BTreeMap;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::routing::{get, post};
use base64::Engine as _;
use orrery_world::{ActorKind, OAuthApp, WorldError};

use crate::{GithubFace, header_str, internal};

const ERROR_DOC: &str = "https://docs.github.com/apps/managing-oauth-apps/troubleshooting-oauth-app-access-token-request-errors";

pub fn web_router(face: GithubFace) -> Router {
    Router::new()
        .route(
            "/login/oauth/authorize",
            get(authorize_page).post(authorize),
        )
        .route("/login/oauth/access_token", post(access_token))
        .with_state(face)
}

async fn authorize_page(
    State(face): State<GithubFace>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let params = parse_form(query.as_deref().unwrap_or("").as_bytes());
    let app = match resolve_app(&face, &params) {
        Ok(app) => app,
        Err(resp) => return *resp,
    };
    let redirect_uri = match effective_redirect(&app, &params) {
        Ok(uri) => uri,
        Err(resp) => return *resp,
    };
    html(
        StatusCode::OK,
        &authorize_form(&app, &redirect_uri, &params, None),
    )
}

async fn authorize(State(face): State<GithubFace>, body: Bytes) -> Response<Body> {
    let params = parse_form(&body);
    let app = match resolve_app(&face, &params) {
        Ok(app) => app,
        Err(resp) => return *resp,
    };
    let redirect_uri = match effective_redirect(&app, &params) {
        Ok(uri) => uri,
        Err(resp) => return *resp,
    };
    let login = params.get("login").map(String::as_str).unwrap_or("");
    let actor = match face.world.actor_by_login(login) {
        Ok(Some(a)) if a.kind == ActorKind::User => a,
        Ok(_) => {
            return html(
                StatusCode::OK,
                &authorize_form(
                    &app,
                    &redirect_uri,
                    &params,
                    Some("Incorrect username or password."),
                ),
            );
        }
        Err(e) => return internal(e),
    };
    let scope = params.get("scope").cloned().unwrap_or_default();
    let code = match face
        .world
        .issue_oauth_code(&app, &actor, &redirect_uri, &scope)
    {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let mut back = vec![("code", code.as_str())];
    if let Some(state) = params.get("state") {
        back.push(("state", state.as_str()));
    }
    redirect(&with_query(&redirect_uri, &back))
}

/// `client_id`, `client_secret`, `code` and `redirect_uri` arrive in the query
/// string, a form body or a JSON body, as GitHub accepts all three; the client
/// pair may instead come as HTTP Basic. The answer is form-encoded unless the
/// caller asks for JSON, and a refused exchange is still a 200 carrying
/// `error`, as GitHub's is.
async fn access_token(
    State(face): State<GithubFace>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let accept = header_str(&headers, header::ACCEPT);
    if accept.contains("xml") {
        return crate::orrery_unsupported_field("OauthAccess", "Accept");
    }
    let as_json = accept.contains("application/json");
    let mut params = parse_form(query.as_deref().unwrap_or("").as_bytes());
    if header_str(&headers, header::CONTENT_TYPE).contains("application/json") {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_slice(&body) {
            for (k, v) in map {
                if let serde_json::Value::String(s) = v {
                    params.insert(k, s);
                }
            }
        }
    } else {
        params.extend(parse_form(&body));
    }
    if let Some((id, secret)) = basic_credentials(&headers) {
        params.entry("client_id".to_string()).or_insert(id);
        params.entry("client_secret".to_string()).or_insert(secret);
    }
    let get = |k: &str| params.get(k).map(String::as_str).unwrap_or("");
    match face.world.exchange_oauth_code(
        get("client_id"),
        get("client_secret"),
        get("code"),
        params.get("redirect_uri").map(String::as_str),
    ) {
        Ok(t) => token_response(
            as_json,
            &[
                ("access_token", t.token.as_str()),
                ("token_type", "bearer"),
                ("scope", t.scope.as_str()),
            ],
        ),
        Err(WorldError::OAuthClientCredentials) => token_error(
            as_json,
            "incorrect_client_credentials",
            "The client_id and/or client_secret passed are incorrect.",
        ),
        Err(WorldError::OAuthBadCode) => token_error(
            as_json,
            "bad_verification_code",
            "The code passed is incorrect or expired.",
        ),
        Err(WorldError::OAuthRedirectMismatch) => token_error(
            as_json,
            "redirect_uri_mismatch",
            "The redirect_uri MUST match the registered callback URL for this application.",
        ),
        Err(e) => internal(e),
    }
}

fn resolve_app(
    face: &GithubFace,
    params: &BTreeMap<String, String>,
) -> Result<OAuthApp, Box<Response<Body>>> {
    let client_id = params.get("client_id").map(String::as_str).unwrap_or("");
    match face.world.oauth_app_by_client_id(client_id) {
        Ok(Some(app)) => Ok(app),
        Ok(None) => Err(Box::new(html(
            StatusCode::NOT_FOUND,
            &page("Not Found", "<p>Not Found</p>"),
        ))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// GitHub's rule for `redirect_uri`: optional, defaulting to the app's
/// callback URL; when given, its scheme, host and port must equal the
/// callback's and its path must be the callback's path or below it. A mismatch
/// is sent back to the callback URL as `error=redirect_uri_mismatch`.
/// GitHub's sub-domain allowance and loopback-port exception are not modeled:
/// a redirect relying on either is refused.
fn effective_redirect(
    app: &OAuthApp,
    params: &BTreeMap<String, String>,
) -> Result<String, Box<Response<Body>>> {
    let Some(requested) = params.get("redirect_uri").filter(|r| !r.is_empty()) else {
        return Ok(app.callback_url.clone());
    };
    if redirect_allowed(&app.callback_url, requested) {
        return Ok(requested.clone());
    }
    let mut back = vec![
        ("error", "redirect_uri_mismatch"),
        (
            "error_description",
            "The redirect_uri MUST match the registered callback URL for this application.",
        ),
        ("error_uri", ERROR_DOC),
    ];
    if let Some(state) = params.get("state") {
        back.push(("state", state.as_str()));
    }
    Err(Box::new(redirect(&with_query(&app.callback_url, &back))))
}

fn redirect_allowed(callback: &str, requested: &str) -> bool {
    let (Some((cb_origin, cb_path)), Some((rq_origin, rq_path))) =
        (split_url(callback), split_url(requested))
    else {
        return false;
    };
    if !cb_origin.eq_ignore_ascii_case(rq_origin) {
        return false;
    }
    let base = cb_path.trim_end_matches('/');
    rq_path == cb_path
        || rq_path == base
        || rq_path
            .strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `scheme://host[:port]` and the path, query and fragment dropped.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let scheme_end = url.find("://")? + 3;
    let rest = &url[scheme_end..];
    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = &url[..scheme_end + path_start];
    let after = &rest[path_start..];
    let path = &after[..after.find(['?', '#']).unwrap_or(after.len())];
    Some((origin, if path.is_empty() { "/" } else { path }))
}

fn authorize_form(
    app: &OAuthApp,
    redirect_uri: &str,
    params: &BTreeMap<String, String>,
    error: Option<&str>,
) -> String {
    let hidden = |name: &str, value: &str| {
        format!(
            r#"<input type="hidden" name="{name}" value="{}">"#,
            escape(value)
        )
    };
    let mut fields = vec![
        hidden("client_id", &app.client_id),
        hidden("redirect_uri", redirect_uri),
    ];
    for name in ["scope", "state"] {
        if let Some(v) = params.get(name) {
            fields.push(hidden(name, v));
        }
    }
    let flash = error
        .map(|e| format!(r#"<p role="alert">{}</p>"#, escape(e)))
        .unwrap_or_default();
    page(
        &format!("Authorize {}", app.name),
        &format!(
            r#"<h1>Authorize {name}</h1>{flash}
<form method="post" action="/login/oauth/authorize">
{fields}
<label for="login">Username</label>
<input id="login" name="login" type="text" autocomplete="username" autofocus>
<button type="submit">Authorize</button>
</form>"#,
            name = escape(&app.name),
            fields = fields.join("\n"),
        ),
    )
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body>{body}</body></html>",
        escape(title)
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn token_error(as_json: bool, error: &str, description: &str) -> Response<Body> {
    let uri = format!("{ERROR_DOC}#{}", error.replace('_', "-"));
    token_response(
        as_json,
        &[
            ("error", error),
            ("error_description", description),
            ("error_uri", uri.as_str()),
        ],
    )
}

fn token_response(as_json: bool, fields: &[(&str, &str)]) -> Response<Body> {
    let (content_type, body) = if as_json {
        let map: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                )
            })
            .collect();
        (
            "application/json; charset=utf-8",
            serde_json::Value::Object(map).to_string(),
        )
    } else {
        (
            "application/x-www-form-urlencoded; charset=utf-8",
            encode_form(fields),
        )
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .expect("static response parts are valid")
}

fn html(status: StatusCode, body: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}

fn redirect(location: &str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .expect("static response parts are valid")
}

fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    let (base, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut out = format!("{base}{sep}{}", encode_form(pairs));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = auth.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .ok()?;
    let (id, secret) = String::from_utf8(decoded)
        .ok()?
        .split_once(':')
        .map(|(a, b)| (a.to_string(), b.to_string()))?;
    Some((id, secret))
}

/// `application/x-www-form-urlencoded` (or a query string) into its pairs.
/// A repeated key keeps its first value.
fn parse_form(raw: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in String::from_utf8_lossy(raw)
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.entry(percent_decode(k))
            .or_insert_with(|| percent_decode(v));
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn encode_form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
