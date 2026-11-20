//! Dependabot alerts: a repository's vulnerable dependencies, as GitHub's
//! REST API lists, reads, dismisses, and reopens them, and the switch that
//! turns them on (spec/surface-github.md §1.13). GitHub makes an alert
//! itself, and fixes one itself, so those are the control plane's.

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use base64::Engine as _;
use orrery_world::{
    AlertPosition, AlertQuery, AlertScope, AlertSeverity, AlertState, AlertTransition, Caller,
    DependencyAlert, DismissReason, Door, Repo, WorldError,
};
use sha2::{Digest, Sha256};

use crate::render::{actor_json, rfc3339};
use crate::{
    GithubFace, github_error, internal, invalid_request, json_response, media, no_content,
    resolve_repo,
};

const DISABLED: &str = "Dependabot alerts are disabled for this repository.";

/// The most alerts a page holds, and how many it holds when the request
/// names no size.
const MAX_PER_PAGE: usize = 100;
const DEFAULT_PER_PAGE: usize = 30;

/// The ecosystems GitHub filters the list by.
const ECOSYSTEMS: [&str; 9] = [
    "composer", "go", "maven", "npm", "nuget", "pip", "pub", "rubygems", "rust",
];

/// The longest comment GitHub takes with a dismissal, in characters.
const MAX_COMMENT: usize = 280;

/// GitHub's alert object. A world keeps the package, the advisory's
/// identity and severity, and the one vulnerable range it was given: the
/// rest of an advisory (its references, scores, and weaknesses) is rendered
/// empty, and its dates are the alert's.
pub(crate) fn alert_json(
    face: &GithubFace,
    repo: &Repo,
    alert: &DependencyAlert,
) -> serde_json::Value {
    let api = face.layout.api();
    let web = face.layout.web();
    let full_name = repo.full_name();
    let package = serde_json::json!({ "ecosystem": alert.ecosystem, "name": alert.package });
    let vulnerability = serde_json::json!({
        "package": package,
        "severity": alert.severity.as_str(),
        "vulnerable_version_range": alert.vulnerable_version_range,
        "first_patched_version": alert
            .first_patched_version
            .as_ref()
            .map(|identifier| serde_json::json!({ "identifier": identifier })),
    });
    let mut identifiers = vec![serde_json::json!({ "value": alert.advisory_id, "type": "GHSA" })];
    if let Some(cve) = &alert.cve_id {
        identifiers.push(serde_json::json!({ "value": cve, "type": "CVE" }));
    }
    let no_score = serde_json::json!({ "vector_string": null, "score": 0.0 });
    let created = rfc3339(alert.created_clock_unix);
    serde_json::json!({
        "number": alert.number,
        "state": alert.state.as_str(),
        "dependency": {
            "package": package,
            "manifest_path": alert.manifest_path,
            "version_requirement": alert.version_requirement,
            "scope": alert.scope.map(AlertScope::as_str),
            "relationship": "unknown",
        },
        "security_advisory": {
            "ghsa_id": alert.advisory_id,
            "cve_id": alert.cve_id,
            "summary": alert.summary,
            "description": alert.description,
            "severity": alert.severity.as_str(),
            "identifiers": identifiers,
            "references": [],
            "published_at": created,
            "updated_at": created,
            "withdrawn_at": null,
            "vulnerabilities": [vulnerability],
            "cvss_severities": { "cvss_v3": no_score, "cvss_v4": no_score },
            "epss": null,
            "cvss": no_score,
            "cwes": [],
            "classification": "general",
        },
        "security_vulnerability": vulnerability,
        "url": format!("{api}/repos/{full_name}/dependabot/alerts/{}", alert.number),
        "html_url": format!("{web}/{full_name}/security/dependabot/{}", alert.number),
        "created_at": created,
        "updated_at": rfc3339(alert.updated_clock_unix),
        "dismissal_request": null,
        "assignees": [],
        "reachability": { "verdict": "disabled" },
        "dismissed_at": alert.dismissed.as_ref().map(|d| rfc3339(d.clock_unix)),
        "dismissed_by": alert.dismissed.as_ref().map(|d| actor_json(&d.by)),
        "dismissed_reason": alert.dismissed.as_ref().map(|d| d.reason.as_str()),
        "dismissed_comment": alert.dismissed.as_ref().and_then(|d| d.comment.clone()),
        "fixed_at": alert.fixed_clock_unix.map(rfc3339),
        "auto_dismissed_at": alert.auto_dismissed_clock_unix.map(rfc3339),
    })
}

fn disabled() -> Response<Body> {
    github_error(StatusCode::FORBIDDEN, DISABLED)
}

/// The refusal of an installation token that does not reach `repo`. A
/// public repository's code is anyone's to read, and its alerts are not: an
/// App reads them only where it is installed.
fn unreached(caller: &Caller, repo: &Repo) -> Option<Response<Body>> {
    (!caller.reaches(repo)).then(crate::apps::not_accessible)
}

// ---- the switch ----

/// `GET /repos/{o}/{r}/vulnerability-alerts`: `204` while alerts are on.
pub(crate) async fn alerts_enabled(
    State(face): State<GithubFace>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let repo = match resolve_repo(&face, &owner, &name) {
        Ok((repo, _)) => repo,
        Err(resp) => return *resp,
    };
    if let Some(refused) = unreached(&caller, &repo) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    match repo.dependency_alerts {
        true => no_content(),
        false => github_error(StatusCode::NOT_FOUND, "Vulnerability alerts are disabled."),
    }
}

/// `PUT /repos/{o}/{r}/vulnerability-alerts`: `204`, and `204` again.
pub(crate) async fn enable_alerts(
    State(face): State<GithubFace>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let repo = match resolve_repo(&face, &owner, &name) {
        Ok((repo, _)) => repo,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    switch(&face, &repo, true, &caller)
}

/// `DELETE /repos/{o}/{r}/vulnerability-alerts`: `204`, and `204` again.
pub(crate) async fn disable_alerts(
    State(face): State<GithubFace>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let repo = match resolve_repo(&face, &owner, &name) {
        Ok((repo, _)) => repo,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    switch(&face, &repo, false, &caller)
}

fn switch(face: &GithubFace, repo: &Repo, on: bool, caller: &Caller) -> Response<Body> {
    match face
        .world
        .set_dependency_alerts(repo.id, on, caller.via(Door::Rest))
    {
        Ok(_) => no_content(),
        Err(e) => internal(e),
    }
}

// ---- the list ----

/// `GET /repos/{o}/{r}/dependabot/alerts`.
pub(crate) async fn list_alerts(
    State(face): State<GithubFace>,
    UrlPath((owner, name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let repo = match resolve_repo(&face, &owner, &name) {
        Ok((repo, _)) => repo,
        Err(resp) => return *resp,
    };
    if let Some(refused) = unreached(&caller, &repo) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "DependabotAlert") {
        return refused;
    }
    listing(&face, &repo, query.as_deref().unwrap_or(""), &headers)
}

/// `GET /repositories/{id}/dependabot/alerts`: the same list, at the path
/// its own `Link` header names.
pub(crate) async fn list_alerts_by_id(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    Extension(caller): Extension<Caller>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let repo = match id.parse::<i64>().map(|id| face.world.repo_by_id(id)) {
        Ok(Ok(Some(repo))) => repo,
        Ok(Ok(None)) | Err(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Ok(Err(e)) => return internal(e),
    };
    if let Some(refused) = unreached(&caller, &repo) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "DependabotAlert") {
        return refused;
    }
    listing(&face, &repo, query.as_deref().unwrap_or(""), &headers)
}

/// A cursor names where in the order a page starts or ends. GitHub's is
/// opaque, and so is this: the base64 of the position.
fn cursor(position: AlertPosition) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!(
        "cursor:v2:{}:{}",
        position.clock_unix, position.number
    ))
}

fn position(cursor: &str) -> Option<AlertPosition> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cursor)
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    let (clock_unix, number) = text.strip_prefix("cursor:v2:")?.split_once(':')?;
    Some(AlertPosition {
        clock_unix: clock_unix.parse().ok()?,
        number: number.parse().ok()?,
    })
}

/// Each value of a comma-separated list that `parse` reads.
fn listed<T>(value: &str, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    value
        .split(',')
        .filter_map(|word| parse(word.trim()))
        .collect()
}

/// The list as GitHub answers it: newest first unless asked otherwise,
/// paged by a cursor its `Link` header carries, with an `ETag` a caller
/// sends back to learn that nothing changed.
fn listing(face: &GithubFace, repo: &Repo, raw_query: &str, headers: &HeaderMap) -> Response<Body> {
    if !repo.dependency_alerts {
        return disabled();
    }
    let mut query = AlertQuery::default();
    let mut per_page = DEFAULT_PER_PAGE;
    // What a page's links repeat: every parameter but the cursors, in the
    // order the request gave them.
    let mut repeated = form_urlencoded::Serializer::new(String::new());
    for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
        let bad_cursor = |name: &str| {
            github_error(
                StatusCode::BAD_REQUEST,
                &format!(":{name} does not appear to be a valid cursor"),
            )
        };
        match key.as_ref() {
            "page" => {
                return github_error(
                    StatusCode::BAD_REQUEST,
                    "Pagination using the `page` parameter is not supported.",
                );
            }
            // GitHub reads a size that is not a number as none at all.
            "per_page" => match value.parse::<usize>().unwrap_or(0) {
                n if n > MAX_PER_PAGE => {
                    return github_error(
                        StatusCode::BAD_REQUEST,
                        ":per_page must be between 0 and 100",
                    );
                }
                n => per_page = n,
            },
            "after" => match position(&value) {
                Some(at) => query.after = Some(at),
                None => return bad_cursor("after"),
            },
            "before" => match position(&value) {
                Some(at) => query.before = Some(at),
                None => return bad_cursor("before"),
            },
            // A state GitHub does not know matches no alert.
            "state" => {
                query.states = Some(listed(&value, |word| {
                    AlertState::parse(&word.to_ascii_lowercase())
                }));
            }
            // A severity it does not know is no filter.
            "severity" => {
                let severities = listed(&value, AlertSeverity::parse);
                query.severities = (!severities.is_empty()).then_some(severities);
            }
            // An ecosystem GitHub does not know is no filter either.
            "ecosystem" => {
                let known = listed(&value, |word| {
                    let word = word.to_ascii_lowercase();
                    ECOSYSTEMS.contains(&word.as_str()).then_some(word)
                });
                query.ecosystems = (!known.is_empty()).then_some(known);
            }
            "package" => query.packages = Some(listed(&value, |w| Some(w.to_string()))),
            "manifest" => query.manifests = Some(listed(&value, |w| Some(w.to_string()))),
            // A scope it does not know matches no alert.
            "scope" => {
                query.scopes = Some(listed(&value, |word| {
                    AlertScope::parse(&word.to_ascii_lowercase())
                }));
            }
            // GitHub orders by an advisory's exploit score too, which a
            // world keeps nothing of. Any other order it does not know is
            // the default.
            "sort" if value == "epss_percentage" => {
                return crate::orrery_unsupported_field("DependabotAlert", "sort");
            }
            "sort" => query.by_updated = value == "updated",
            "direction" => query.ascending = value == "asc",
            // Filters on what a world keeps nothing of. Ignoring one would
            // answer a wider list than the caller asked for.
            "epss_percentage" | "has" | "assignee" => {
                return crate::orrery_unsupported_field("DependabotAlert", &key);
            }
            // GitHub ignores what it does not know, `first` and `last`
            // among it.
            _ => {}
        }
        if key != "after" && key != "before" {
            repeated.append_pair(&key, &value);
        }
    }
    let repeated = repeated.finish();
    let read =
        |query: &AlertQuery, limit: usize| face.world.dependency_alerts(repo.id, query, limit);
    let page = match per_page {
        0 => Ok(Vec::new()),
        n => read(&query, n),
    };
    let page = match page {
        Ok(page) => page,
        Err(e) => return internal(e),
    };
    // Whether an alert stands past each end of the page, which is what a
    // link in that direction is for.
    let beyond = |after: Option<AlertPosition>, before: Option<AlertPosition>| {
        let probe = AlertQuery {
            after,
            before,
            ..query.clone()
        };
        read(&probe, 1).map(|found| !found.is_empty())
    };
    let ends = match (page.first(), page.last()) {
        (Some(first), Some(last)) => beyond(None, Some(query.position(first))).and_then(|prev| {
            beyond(Some(query.position(last)), None).map(|next| {
                (
                    prev.then(|| cursor(query.position(first))),
                    next.then(|| cursor(query.position(last))),
                )
            })
        }),
        _ => Ok((None, None)),
    };
    let (prev, next) = match ends {
        Ok(ends) => ends,
        Err(e) => return internal(e),
    };
    let api = face.layout.api();
    let link = |name: &str, cursor: Option<&str>, rel: &str| {
        let mut params = repeated.clone();
        if let Some(cursor) = cursor {
            let pair = form_urlencoded::Serializer::new(String::new())
                .append_pair(name, cursor)
                .finish();
            if !params.is_empty() {
                params.push('&');
            }
            params.push_str(&pair);
        }
        format!(
            "<{api}/repositories/{}/dependabot/alerts?{params}>; rel=\"{rel}\"",
            repo.id
        )
    };
    let mut links = Vec::new();
    // A page of no size has a next page, as GitHub answers one: itself.
    if per_page == 0 {
        links.push(link("after", None, "next"));
    }
    if let Some(next) = &next {
        links.push(link("after", Some(next), "next"));
    }
    if let Some(prev) = &prev {
        links.push(link("before", Some(prev), "prev"));
    }

    let items: Vec<_> = page.iter().map(|a| alert_json(face, repo, a)).collect();
    let body = serde_json::Value::Array(items).to_string();
    // The tag covers the links with the body: a page whose alerts are the
    // same and that has gained a next page is another answer, and a caller
    // told nothing changed would never ask for that page.
    let links = links.join(", ");
    let etag = format!(
        "\"{}\"",
        hex::encode(
            Sha256::new()
                .chain_update(&body)
                .chain_update(&links)
                .finalize()
        )
    );
    let matched = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|sent| sent.trim().trim_start_matches("W/") == etag);
    let mut resp = Response::builder()
        .status(match matched {
            true => StatusCode::NOT_MODIFIED,
            false => StatusCode::OK,
        })
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8");
    if !links.is_empty() && !matched {
        resp = resp.header(header::LINK, links);
    }
    match resp.body(match matched {
        true => Body::empty(),
        false => Body::from(body),
    }) {
        Ok(resp) => resp,
        Err(e) => internal(e),
    }
}

// ---- one alert ----

/// A value as GitHub's refusals of a body spell it, which is how Ruby
/// prints one: a hash's keys point at their values with `=>`.
fn ruby(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "nil".to_string(),
        serde_json::Value::Array(items) => {
            format!(
                "[{}]",
                items.iter().map(ruby).collect::<Vec<_>>().join(", ")
            )
        }
        serde_json::Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!("{} => {}", serde_json::json!(key), ruby(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

/// The alert a path names, or GitHub's refusal: for a token that does not
/// reach the repository, alerts that are off, a number that is not one, and
/// a number no alert has.
fn alert_at(
    face: &GithubFace,
    caller: &Caller,
    owner: &str,
    name: &str,
    number: &str,
) -> Result<(Repo, DependencyAlert), Box<Response<Body>>> {
    let (repo, _) = resolve_repo(face, owner, name)?;
    if let Some(refused) = unreached(caller, &repo) {
        return Err(Box::new(refused));
    }
    if !repo.dependency_alerts {
        return Err(Box::new(disabled()));
    }
    let Ok(number) = number.parse::<i64>() else {
        return Err(Box::new(github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The alert_number parameter must be an integer.",
        )));
    };
    match face.world.dependency_alert(repo.id, number) {
        Ok(Some(alert)) => Ok((repo, alert)),
        Ok(None) => Err(Box::new(no_alert(number))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn no_alert(number: i64) -> Response<Body> {
    github_error(
        StatusCode::NOT_FOUND,
        &format!("No alert found for alert number {number}"),
    )
}

/// `GET /repos/{o}/{r}/dependabot/alerts/{alert_number}`.
pub(crate) async fn get_alert(
    State(face): State<GithubFace>,
    UrlPath((owner, name, number)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, alert) = match alert_at(&face, &caller, &owner, &name, &number) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "DependabotAlert") {
        return refused;
    }
    json_response(StatusCode::OK, alert_json(&face, &repo, &alert))
}

/// `PATCH /repos/{o}/{r}/dependabot/alerts/{alert_number}`: a person
/// dismisses an open alert, with a reason, or reopens one that was
/// dismissed. GitHub checks the body against its schema first.
pub(crate) async fn update_alert(
    State(face): State<GithubFace>,
    UrlPath((owner, name, number)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, alert) = match alert_at(&face, &caller, &owner, &name, &number) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "DependabotAlert") {
        return refused;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON");
    };
    const NO_INPUT: &str =
        "Invalid input: data matches no possible input. See `documentation_url`.";
    let Some(fields) = value.as_object() else {
        return invalid_request(&format!(
            "Invalid input: `{}` is not of type `object`.",
            ruby(&value)
        ));
    };
    // A world keeps nobody an alert is assigned to, which GitHub takes
    // here.
    if fields.contains_key("assignees") {
        return crate::orrery_unsupported_field("DependabotAlert", "assignees");
    }
    const KEYS: [&str; 3] = ["state", "dismissed_reason", "dismissed_comment"];
    if let Some(key) = fields.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return invalid_request(&format!(
            "{NO_INPUT}\nInvalid input: \"{key}\" is not a permitted key."
        ));
    }
    // Each is a string when it is there at all.
    let mut text: [Option<&str>; 3] = [None; 3];
    for (slot, key) in text.iter_mut().zip(KEYS) {
        *slot = match fields.get(key) {
            None => None,
            Some(serde_json::Value::String(given)) => Some(given.as_str()),
            Some(serde_json::Value::Null) => {
                return invalid_request(&format!("Invalid property /{key}: data cannot be null."));
            }
            Some(other) => {
                return invalid_request(&format!(
                    "Invalid property /{key}: `{}` is not of type `string`.",
                    ruby(other)
                ));
            }
        };
    }
    let [state, reason, comment] = text;
    let not_possible = |key: &str, given: &str, values: &str| {
        invalid_request(&format!(
            "Invalid property /{key}: `{given}` is not a possible value. Must be one of the following: {values}."
        ))
    };
    let Some(state) = state else {
        return invalid_request(NO_INPUT);
    };
    if !matches!(state, "dismissed" | "open") {
        return not_possible("state", state, "dismissed, open");
    }
    let reason = match reason {
        Some(given) => match DismissReason::parse(given) {
            Some(reason) => Some(reason),
            None => {
                return not_possible(
                    "dismissed_reason",
                    given,
                    "fix_started, inaccurate, no_bandwidth, not_used, tolerable_risk",
                );
            }
        },
        None => None,
    };
    if let Some(n) = comment
        .map(|c| c.chars().count())
        .filter(|n| *n > MAX_COMMENT)
    {
        return invalid_request(&format!(
            "Invalid property /dismissed_comment: Only {MAX_COMMENT} characters are allowed; {n} were supplied."
        ));
    }
    let transition = match (state, reason) {
        ("dismissed", Some(reason)) => AlertTransition::Dismiss { reason, comment },
        ("dismissed", None) => {
            return github_error(
                StatusCode::BAD_REQUEST,
                "Setting an alert to 'dismissed' requires a 'dismissed_reason'",
            );
        }
        (_, Some(_)) => {
            return github_error(
                StatusCode::BAD_REQUEST,
                "Can't set 'dismissed_reason' when setting an alert to 'open'",
            );
        }
        (_, None) => AlertTransition::Reopen,
    };
    match face.world.move_dependency_alert(
        repo.id,
        alert.number,
        &transition,
        caller.via(Door::Rest),
    ) {
        Ok(moved) => json_response(StatusCode::OK, alert_json(&face, &repo, &moved)),
        Err(WorldError::DependencyAlertState { number, state }) => github_error(
            StatusCode::CONFLICT,
            &match state {
                AlertState::Fixed => "Fixed alerts cannot be dismissed or reopened".to_string(),
                AlertState::Open => format!("Alert {number} is already open"),
                AlertState::Dismissed | AlertState::AutoDismissed => {
                    format!("Alert {number} is already dismissed")
                }
            },
        ),
        Err(WorldError::NoSuchDependencyAlert(number)) => no_alert(number),
        Err(WorldError::DependencyAlertsOff(_)) => disabled(),
        Err(e) => internal(e),
    }
}
