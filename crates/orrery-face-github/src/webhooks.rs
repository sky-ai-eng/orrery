//! Webhooks: the deliveries a GitHub App is owed, the request each is sent
//! as, and the routes an App reads them by (spec/surface-github.md §1.12).
//!
//! The world tells [`Webhooks`] of each write as it lands, and it answers
//! the deliveries that write makes: one for each App whose webhook is
//! active, that subscribes to the event, and that has an installation
//! covering the repository. A payload is rendered here, from the world as
//! the write left it, with the renderers the REST routes answer with.

use std::sync::{Arc, Weak};

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{Response, StatusCode, header};
use base64::Engine as _;
use orrery_face_git::{GitFace, store};
use orrery_world::events::{self, Door, EventBody};
use orrery_world::{
    Actor, App, Attribution, CheckStatus, Delivery, DeliveryFailure, DeliveryFilter,
    DeliveryOutcome, Installation, NewDelivery, Notifier, Pull, PullState, Repo,
    RepositorySelection, SYSTEM_ACTOR_ID, World, WorldError, WorldView,
};

use crate::render::{
    Mergeability, actor_json, check_run_json, comment_json, full_pull_json, label_json, pull_json,
    repo_json, review_json, rfc3339, team_json, user_json,
};
use crate::{GithubFace, github_error, internal, invalid_request, json_response, schema_value};

/// What GitHub's sender calls itself: its name, then seven hex digits of
/// the build it runs, which here are zeros.
const USER_AGENT: &str = "GitHub-Hookshot/0000000";

/// The most deliveries a page of the list holds, and how many it holds when
/// the request names no size.
const MAX_PER_PAGE: usize = 100;
const DEFAULT_PER_PAGE: usize = 30;

// ---- the routes an App reads its webhook by ----

pub(crate) async fn get_config(Extension(app): Extension<App>) -> Response<Body> {
    json_response(StatusCode::OK, config_json(&app))
}

fn config_json(app: &App) -> serde_json::Value {
    let mut config = serde_json::json!({
        "content_type": "json",
        "insecure_ssl": "0",
        "url": app.hook.url,
    });
    if app.hook.has_secret {
        config["secret"] = "********".into();
    }
    config
}

/// Point the App's webhook at a URL, as the App does with its own
/// signature: `200` with the configuration. GitHub checks the body against
/// its schema first, then the URL. A world sends JSON, checks a receiver's
/// certificate, and derives a webhook's secret, so it takes `content_type`
/// and `insecure_ssl` only at the values it has and refuses `secret` by
/// name.
pub(crate) async fn patch_config(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    body: Bytes,
) -> Response<Body> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return invalid_request("For 'links/2/schema', nil is not an object.");
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON");
    };
    let Some(fields) = value.as_object() else {
        return invalid_request(&format!(
            "For 'links/2/schema', {} is not an object.",
            schema_value(&value)
        ));
    };
    const KEYS: [&str; 4] = ["url", "content_type", "insecure_ssl", "secret"];
    if let Some(key) = fields.keys().find(|key| !KEYS.contains(&key.as_str())) {
        return invalid_request(&format!("\"{key}\" is not a permitted key."));
    }
    let text = |key: &str| -> Result<Option<&str>, Box<Response<Body>>> {
        match fields.get(key) {
            None => Ok(None),
            Some(value) => match value.as_str() {
                Some(text) => Ok(Some(text)),
                None => Err(Box::new(invalid_request(&format!(
                    "For 'properties/{key}', {} is not a string.",
                    schema_value(value)
                )))),
            },
        }
    };
    let read = || -> Result<Option<&str>, Box<Response<Body>>> {
        let url = text("url")?;
        let fixed = [
            ("content_type", text("content_type")?, "json"),
            ("insecure_ssl", text("insecure_ssl")?, "0"),
            ("secret", text("secret")?, ""),
        ];
        for (key, given, held) in fixed {
            if given.is_some_and(|given| key == "secret" || given != held) {
                return Err(Box::new(crate::orrery_unsupported_field("AppHook", key)));
            }
        }
        Ok(url)
    };
    let url = match read() {
        Ok(url) => url,
        Err(resp) => return *resp,
    };
    let Some(url) = url else {
        return json_response(StatusCode::OK, config_json(&app));
    };
    if url.contains(char::is_whitespace) {
        return invalid_request(&format!("{url} is not a valid uri."));
    }
    if let Some(faults) = url_faults(url) {
        return json_response(
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "message": faults }),
        );
    }
    let by = Attribution::new(app.bot.id, Door::Rest).with_app(Some(app.id));
    match face.world.set_app_hook(&app.slug, Some(url), None, by) {
        Ok(app) => json_response(StatusCode::OK, config_json(&app)),
        Err(WorldError::InvalidApp { message, .. }) => github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("orrery: {message}"),
        ),
        Err(e) => internal(e),
    }
}

/// GitHub's faults with a webhook's URL, which it answers as a list. A
/// world matches the scheme in lower case, where GitHub takes any case.
fn url_faults(url: &str) -> Option<Vec<&'static str>> {
    if url.is_empty() {
        return Some(vec![
            "Config attribute records value can't be blank",
            "Url cannot be blank",
        ]);
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return Some(vec!["Url is missing a scheme"]);
    };
    if !matches!(scheme, "http" | "https") {
        return Some(vec!["Url scheme is invalid"]);
    }
    let host = rest
        .strip_prefix("//")
        .map(|rest| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    host.is_empty().then(|| vec!["Url host is invalid"])
}

/// An App's attempted deliveries, newest first. GitHub pages them by a
/// cursor, which names the first delivery of the page, and reads its query
/// strictly: a parameter it does not take is refused.
pub(crate) async fn list_deliveries(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let mut per_page = DEFAULT_PER_PAGE;
    let mut cursor = None;
    let mut filter = DeliveryFilter::All;
    // What a page's links repeat: every parameter but the cursor, in the
    // order the request gave them.
    let mut repeated = Vec::new();
    for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "per_page" => match value.parse::<usize>() {
                Ok(n) if value.bytes().all(|b| b.is_ascii_digit()) => {
                    per_page = n.min(MAX_PER_PAGE);
                }
                _ => return invalid_request(&format!("{value} does not match /^\\d+$/.")),
            },
            "cursor" => match Cursor::parse(&value) {
                Some(parsed) => cursor = Some(parsed),
                None => {
                    return github_error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Error parsing cursor parameter",
                    );
                }
            },
            "status" => {
                filter = match value.as_ref() {
                    "failure" => DeliveryFilter::Failed,
                    "success" => DeliveryFilter::Succeeded,
                    other => {
                        return invalid_request(&format!(
                            "{other} is not a member of [\"failure\", \"success\"]."
                        ));
                    }
                }
            }
            other => return invalid_request(&format!("\"{other}\" is not a permitted key.")),
        }
        if key != "cursor" {
            repeated.push(format!("{key}={value}"));
        }
    }
    // The delivery the page starts at. A cursor that pages backwards names
    // a delivery and how many before it to go back over, and with none
    // before it the page is empty.
    let first = match cursor {
        None => None,
        Some(Cursor::At(id)) => Some(id),
        Some(Cursor::Before { id, back }) => {
            match face.world.deliveries_newer(app.id, id, back, filter) {
                Ok(newer) => match newer.last() {
                    Some(farthest) => Some(farthest.id),
                    None => return json_response(StatusCode::OK, serde_json::json!([])),
                },
                Err(e) => return internal(e),
            }
        }
    };
    // One past the page says whether there is a next page, and names its
    // first delivery.
    let before = first.map(|id| id.saturating_add(1));
    let mut found = match face
        .world
        .deliveries_page(app.id, before, per_page + 1, filter)
    {
        Ok(found) => found,
        Err(e) => return internal(e),
    };
    let next = (found.len() > per_page).then(|| found.split_off(per_page)[0].id);
    let items: Vec<_> = found.iter().map(delivery_json).collect();
    let mut resp = json_response(StatusCode::OK, serde_json::Value::Array(items));
    let base = face.base_url.trim_end_matches('/');
    let link = |cursor: String, rel: &str| {
        let mut params = repeated.clone();
        params.push(format!("cursor={cursor}"));
        format!(
            "<{base}/api/v3/app/hook/deliveries?{}>; rel=\"{rel}\"",
            params.join("&")
        )
    };
    let mut links = Vec::new();
    if let Some(next) = next {
        links.push(link(format!("v1_{next}"), "next"));
    }
    // A page reached by a cursor names the page before it, whether or not
    // there is one, as GitHub's does.
    if let (Some(_), Some(first)) = (cursor, found.first()) {
        links.push(link(format!("v1_p{}l{per_page}", first.id), "prev"));
    }
    if !links.is_empty()
        && let Ok(value) = links.join(", ").parse()
    {
        resp.headers_mut().insert(header::LINK, value);
    }
    resp
}

/// The list's cursor: `v1_{id}` starts a page at a delivery, and
/// `v1_p{id}l{n}` starts one `n` deliveries before it.
#[derive(Clone, Copy)]
enum Cursor {
    At(i64),
    Before { id: i64, back: usize },
}

impl Cursor {
    fn parse(value: &str) -> Option<Cursor> {
        let rest = value.strip_prefix("v1_")?;
        match rest.strip_prefix('p') {
            Some(backwards) => {
                let (id, back) = backwards.split_once('l')?;
                // GitHub writes a page's size here, which is never more
                // than the most a page holds.
                Some(Cursor::Before {
                    id: id.parse().ok()?,
                    back: back.parse::<usize>().ok()?.min(MAX_PER_PAGE),
                })
            }
            None => rest.parse().ok().map(Cursor::At),
        }
    }
}

/// A delivery as GitHub lists it. GitHub reports an attempt that reached no
/// receiver as a `502`.
fn delivery_json(delivery: &Delivery) -> serde_json::Value {
    let (status, status_code, delivered_at, duration) = match &delivery.attempt {
        Some(attempt) => {
            let (status, code) = match &attempt.outcome {
                DeliveryOutcome::Answered { status_code, .. } => match status_code {
                    200..=299 => ("OK".to_string(), *status_code),
                    // GitHub lists these two as the status and a space.
                    500 | 502 => (format!("{status_code} "), *status_code),
                    code => (format!("Invalid HTTP Response: {code}"), *code),
                },
                DeliveryOutcome::Failed { step, .. } => match step {
                    DeliveryFailure::Timeout => ("timed out".to_string(), 504),
                    _ => ("failed to connect to host".to_string(), 502),
                },
            };
            (
                status,
                code,
                Some(rfc3339(attempt.clock_unix)),
                // Seconds, to the hundredth, as GitHub gives them.
                (attempt.duration_ms as f64 / 10.0).round() / 100.0,
            )
        }
        None => ("pending".to_string(), 0, None, 0.0),
    };
    serde_json::json!({
        "id": delivery.id,
        "guid": delivery.guid,
        "delivered_at": delivered_at,
        "redelivery": delivery.redelivery_of.is_some(),
        "duration": duration,
        "status": status,
        "status_code": status_code,
        "event": delivery.event,
        "action": delivery.action,
        // GitHub lists an `installation` event's delivery with no
        // installation.
        "installation_id": match delivery.event.as_str() {
            "installation" => None,
            _ => delivery.installation_id,
        },
        "repository_id": delivery.repo_id,
        "url": "",
        "throttled_at": serde_json::Value::Null,
    })
}

/// Send one of the App's deliveries again: `202`, and the delivery goes out
/// as a new one with the same GUID.
pub(crate) async fn redeliver(
    State(face): State<GithubFace>,
    Extension(app): Extension<App>,
    UrlPath(id): UrlPath<String>,
) -> Response<Body> {
    let Ok(id) = id.parse::<i64>() else {
        return github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The delivery_id parameter must be an integer.",
        );
    };
    match face.world.redeliver(app.id, id) {
        Ok(_) => json_response(StatusCode::ACCEPTED, serde_json::json!({})),
        Err(WorldError::NoSuchDelivery(_)) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(WorldError::HookInactive) => github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "orrery: this App's webhook is inactive, so the world sends nothing for it; \
             turn it on with PATCH /_orrery/apps/{slug}",
        ),
        Err(e) => internal(e),
    }
}

// ---- the request a delivery is sent as ----

/// The headers GitHub sends a delivery with, in its order, signed under
/// `secret` when the hook has one. `Host` and `Content-Length` are the
/// sender's to add.
pub fn delivery_headers(delivery: &Delivery, secret: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Accept", "*/*".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("User-Agent", USER_AGENT.to_string()),
        ("X-GitHub-Delivery", delivery.guid.clone()),
        ("X-GitHub-Event", delivery.event.clone()),
        ("X-GitHub-Hook-ID", delivery.app_id.to_string()),
        (
            "X-GitHub-Hook-Installation-Target-ID",
            delivery.app_id.to_string(),
        ),
        (
            "X-GitHub-Hook-Installation-Target-Type",
            "integration".to_string(),
        ),
    ];
    if let Some(secret) = secret {
        let sign = |algorithm, name: &str| {
            let key = ring::hmac::Key::new(algorithm, secret.as_bytes());
            let tag = ring::hmac::sign(&key, delivery.body.as_bytes());
            format!("{name}={}", hex::encode(tag.as_ref()))
        };
        headers.push((
            "X-Hub-Signature",
            sign(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, "sha1"),
        ));
        headers.push((
            "X-Hub-Signature-256",
            sign(ring::hmac::HMAC_SHA256, "sha256"),
        ));
    }
    headers
}

// ---- the deliveries a write makes ----

/// Makes an App's deliveries for the writes a world takes. The world keeps
/// this, and a face keeps its world, so this holds the face's parts weakly:
/// held strongly, a world would keep itself alive after its server is gone.
pub struct Webhooks {
    world: Weak<World>,
    git: Weak<GitFace>,
    base_url: String,
}

/// The notifier to register with the world the face serves. It makes
/// deliveries for as long as something else holds the face's parts.
pub fn notifier(face: &GithubFace) -> Arc<dyn Notifier> {
    Arc::new(Webhooks {
        world: Arc::downgrade(&face.world),
        git: Arc::downgrade(&face.git),
        base_url: face.base_url.clone(),
    })
}

/// The face a write's payloads are rendered with.
struct Rendering {
    face: GithubFace,
}

/// One payload to deliver, before it is addressed to an App: GitHub's
/// action and the payload, which every App receives alike but for its own
/// `installation`.
struct Outgoing {
    action: Option<String>,
    payload: serde_json::Value,
}

/// GitHub's name for the event one of a world's events sends, and the
/// repository it is about; `None` for an event that sends nothing.
fn addressed(event: &EventBody) -> Option<(&'static str, i64)> {
    Some(match event {
        EventBody::PullOpened(e) => ("pull_request", e.subject.repo.id),
        EventBody::PullEdited(e)
        | EventBody::PullHeadMoved(e)
        | EventBody::PullLabeled(e)
        | EventBody::PullUnlabeled(e)
        | EventBody::PullReviewRequested(e)
        | EventBody::PullReviewRequestRemoved(e) => ("pull_request", e.subject.repo.id),
        EventBody::PullClosed(e)
        | EventBody::PullMerged(e)
        | EventBody::PullReopened(e)
        | EventBody::PullReadyForReview(e)
        | EventBody::PullConvertedToDraft(e) => ("pull_request", e.subject.repo.id),
        EventBody::ReviewSubmitted(e) => ("pull_request_review", e.subject.repo.id),
        EventBody::CommentCreated(e) => ("issue_comment", e.subject.repo.id),
        EventBody::CheckRunCreated(e) => ("check_run", e.subject.repo.id),
        EventBody::CheckRunUpdated(e) => ("check_run", e.subject.repo.id),
        EventBody::CheckSuiteUpdated(e) => ("check_suite", e.subject.repo.id),
        EventBody::DependencyAlertCreated(e) => ("dependabot_alert", e.subject.repo.id),
        EventBody::DependencyAlertDismissed(e)
        | EventBody::DependencyAlertReopened(e)
        | EventBody::DependencyAlertFixed(e)
        | EventBody::DependencyAlertAutoDismissed(e)
        | EventBody::DependencyAlertReintroduced(e)
        | EventBody::DependencyAlertAutoReopened(e) => ("dependabot_alert", e.subject.repo.id),
        EventBody::RefCreated(e) => ("push", e.subject.repo.id),
        EventBody::RefUpdated(e) => ("push", e.subject.repo.id),
        EventBody::RefDeleted(e) => ("push", e.subject.repo.id),
        _ => return None,
    })
}

/// An event about one installation, which only that installation's App
/// receives, whether or not it subscribes.
struct OfInstallation {
    installation: Installation,
    action: &'static str,
}

impl Notifier for Webhooks {
    fn deliveries(&self, world: &World, by: Attribution, events: &[EventBody]) -> Vec<NewDelivery> {
        let (Some(served), Some(git)) = (self.world.upgrade(), self.git.upgrade()) else {
            return Vec::new();
        };
        let rendering = Rendering {
            face: GithubFace {
                world: served,
                git,
                base_url: self.base_url.clone(),
            },
        };
        match rendering.made(world, by, events) {
            Ok(made) => made,
            Err(e) => {
                tracing::error!(error = %e, "making a write's deliveries failed");
                Vec::new()
            }
        }
    }

    fn lost(&self, error: &WorldError) {
        tracing::error!(error = %error, "a write landed and its deliveries could not be stored");
    }
}

impl Rendering {
    fn made(
        &self,
        world: &World,
        by: Attribution,
        events: &[EventBody],
    ) -> Result<Vec<NewDelivery>, WorldError> {
        let apps: Vec<App> = world
            .list_apps()?
            .into_iter()
            .filter(|app| app.hook.active && app.hook.url.is_some())
            .collect();
        if apps.is_empty() {
            return Ok(Vec::new());
        }
        let installations = world.list_installations()?;
        let sender = world.actor_by_id(by.actor_id)?;
        let mut made = Vec::new();
        for event in events {
            if let Some(of) = self.of_installation(world, event)? {
                let Some(app) = apps.iter().find(|app| app.id == of.installation.app_id) else {
                    continue;
                };
                made.push(self.installation_delivery(world, app, &of, sender.as_ref(), by)?);
                continue;
            }
            // Rendering a payload reads the repository's git, and every
            // other write waits for it, so nothing is rendered until an
            // App is known to receive it.
            let Some((name, repo_id)) = addressed(event) else {
                continue;
            };
            let Some(repo) = world.repo_by_id(repo_id)? else {
                continue;
            };
            let receivers: Vec<&Installation> = installations
                .iter()
                .filter(|i| receives(i, &repo, name))
                .filter(|i| apps.iter().any(|app| app.id == i.app_id))
                .collect();
            if receivers.is_empty() {
                continue;
            }
            for outgoing in self.outgoing(world, event, sender.as_ref())? {
                for installation in &receivers {
                    let mut payload = outgoing.payload.clone();
                    payload["installation"] = serde_json::json!({
                        "id": installation.id,
                        "node_id": installation_node_id(installation.id),
                    });
                    made.push(NewDelivery {
                        app_id: installation.app_id,
                        installation_id: Some(installation.id),
                        repo_id: Some(repo.id),
                        event: name.to_string(),
                        action: outgoing.action.clone(),
                        body: payload.to_string(),
                    });
                }
            }
        }
        Ok(made)
    }

    /// The installation an event is about, and GitHub's action for it.
    fn of_installation(
        &self,
        world: &World,
        event: &EventBody,
    ) -> Result<Option<OfInstallation>, WorldError> {
        let (id, action) = match event {
            EventBody::InstallationCreated(e) => (e.subject.id, "created"),
            EventBody::InstallationSuspended(e) => (e.subject.id, "suspend"),
            EventBody::InstallationUnsuspended(e) => (e.subject.id, "unsuspend"),
            EventBody::InstallationDeleted(e) => {
                // Its row is gone, so it is read from the event.
                return Ok(
                    deleted_installation(world, e)?.map(|installation| OfInstallation {
                        installation,
                        action: "deleted",
                    }),
                );
            }
            _ => return Ok(None),
        };
        Ok(world
            .installation_by_id(id)?
            .map(|installation| OfInstallation {
                installation,
                action,
            }))
    }

    fn installation_delivery(
        &self,
        world: &World,
        app: &App,
        of: &OfInstallation,
        sender: Option<&Actor>,
        by: Attribution,
    ) -> Result<NewDelivery, WorldError> {
        let face = &self.face;
        let installation = &of.installation;
        // The control plane installs as the system actor, where on GitHub
        // a person does: the account stands for them.
        let sender = match sender {
            Some(actor) if by.actor_id != SYSTEM_ACTOR_ID => actor,
            _ => &installation.account,
        };
        let mut payload = serde_json::json!({
            "action": of.action,
            "installation": crate::apps::installation_json(face, installation),
            "sender": actor_json(sender),
        });
        if matches!(of.action, "created" | "deleted") {
            let repos: Vec<Repo> = match installation.repository_selection {
                RepositorySelection::All => world
                    .list_repos()?
                    .into_iter()
                    .filter(|r| {
                        r.owner_login
                            .eq_ignore_ascii_case(&installation.account.login)
                    })
                    .collect(),
                RepositorySelection::Selected => {
                    let mut listed = Vec::new();
                    for name in &installation.repositories {
                        listed.extend(world.repo_by_owner_name(&installation.account.login, name)?);
                    }
                    listed
                }
            };
            payload["repositories"] = repos
                .iter()
                .map(|repo| {
                    serde_json::json!({
                        "id": repo.id,
                        "node_id": crate::nodeid::repo(repo.id),
                        "name": repo.name,
                        "full_name": repo.full_name(),
                        "private": repo.private,
                    })
                })
                .collect();
            // GitHub names who asked for an installation, null for one
            // nobody had to approve, and only when it is created.
            if of.action == "created" {
                payload["requester"] = serde_json::Value::Null;
            }
        }
        Ok(NewDelivery {
            app_id: app.id,
            installation_id: Some(installation.id),
            repo_id: None,
            event: "installation".to_string(),
            action: Some(of.action.to_string()),
            body: payload.to_string(),
        })
    }

    /// What one of a write's events sends to every App that receives it.
    fn outgoing(
        &self,
        world: &World,
        event: &EventBody,
        sender: Option<&Actor>,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        let actor = sender;
        let sender = sender.map(actor_json).unwrap_or(serde_json::Value::Null);
        let about = |repo_id: i64| -> Result<Option<(Repo, Actor)>, WorldError> {
            let Some(repo) = world.repo_by_id(repo_id)? else {
                return Ok(None);
            };
            let owner = world.actor_by_login(&repo.owner_login)?;
            Ok(owner.map(|owner| (repo, owner)))
        };
        let pull_event = |subject: &events::PullSubject,
                          action: &str,
                          more: &dyn Fn(&mut serde_json::Value)|
         -> Result<Vec<Outgoing>, WorldError> {
            let (Some((repo, owner)), Some(pull)) =
                (about(subject.repo.id)?, world.pull_by_id(subject.id)?)
            else {
                return Ok(Vec::new());
            };
            let mut payload = serde_json::json!({
                "action": action,
                "number": pull.number,
                "pull_request": full_pull_json(face, &repo, &owner, &pull, Mergeability::Unknown),
                "repository": repo_json(face, &repo, &owner),
                "sender": sender,
            });
            more(&mut payload);
            Ok(vec![Outgoing {
                action: Some(action.to_string()),
                payload,
            }])
        };
        let plain = |_: &mut serde_json::Value| {};
        match event {
            EventBody::PullOpened(e) => pull_event(&e.subject, "opened", &plain),
            EventBody::PullEdited(e) => {
                let changes = &e.changes;
                if changes.title.is_none() && changes.body.is_none() && changes.base.is_none() {
                    return Ok(Vec::new());
                }
                pull_event(&e.subject, "edited", &|payload| {
                    let mut edited = serde_json::Map::new();
                    if let Some(title) = &changes.title {
                        edited.insert("title".into(), serde_json::json!({ "from": title.from }));
                    }
                    if let Some(body) = &changes.body {
                        edited.insert("body".into(), serde_json::json!({ "from": body.from }));
                    }
                    if let Some(base) = &changes.base {
                        edited.insert(
                            "base".into(),
                            serde_json::json!({
                                "ref": { "from": base.from.name },
                                "sha": { "from": base.from.sha },
                            }),
                        );
                    }
                    payload["changes"] = edited.into();
                })
            }
            EventBody::PullClosed(e) | EventBody::PullMerged(e) => {
                pull_event(&e.subject, "closed", &plain)
            }
            EventBody::PullReopened(e) => pull_event(&e.subject, "reopened", &plain),
            EventBody::PullReadyForReview(e) => pull_event(&e.subject, "ready_for_review", &plain),
            EventBody::PullConvertedToDraft(e) => {
                pull_event(&e.subject, "converted_to_draft", &plain)
            }
            EventBody::PullHeadMoved(e) => match &e.changes.head {
                Some(head) => pull_event(&e.subject, "synchronize", &|payload| {
                    payload["before"] = head.from.sha.clone().into();
                    payload["after"] = head.to.sha.clone().into();
                }),
                None => Ok(Vec::new()),
            },
            // GitHub sends one event for each label and each reviewer.
            EventBody::PullLabeled(e) | EventBody::PullUnlabeled(e) => {
                let Some(labels) = &e.changes.labels else {
                    return Ok(Vec::new());
                };
                let (action, moved): (_, Vec<&String>) = match event {
                    EventBody::PullLabeled(_) => (
                        "labeled",
                        labels
                            .to
                            .iter()
                            .filter(|l| !labels.from.contains(l))
                            .collect(),
                    ),
                    _ => (
                        "unlabeled",
                        labels
                            .from
                            .iter()
                            .filter(|l| !labels.to.contains(l))
                            .collect(),
                    ),
                };
                let mut out = Vec::new();
                for label in moved {
                    out.extend(pull_event(&e.subject, action, &|payload| {
                        payload["label"] = label_json(label);
                    })?);
                }
                Ok(out)
            }
            EventBody::PullReviewRequested(e) | EventBody::PullReviewRequestRemoved(e) => {
                let requested = matches!(event, EventBody::PullReviewRequested(_));
                let action = match requested {
                    true => "review_requested",
                    false => "review_request_removed",
                };
                let mut out = Vec::new();
                if let Some(reviewers) = &e.changes.requested_reviewers {
                    let (old, new) = (&reviewers.from, &reviewers.to);
                    let (gone, kept) = match requested {
                        true => (new, old),
                        false => (old, new),
                    };
                    for reviewer in gone.iter().filter(|r| !kept.contains(r)) {
                        out.extend(pull_event(&e.subject, action, &|payload| {
                            payload["requested_reviewer"] = user_json(face, &reviewer.login);
                        })?);
                    }
                }
                if let Some(teams) = &e.changes.requested_teams {
                    let (gone, kept) = match requested {
                        true => (&teams.to, &teams.from),
                        false => (&teams.from, &teams.to),
                    };
                    for team in gone.iter().filter(|t| !kept.contains(t)) {
                        let found = world.team_by_slug(&e.subject.owner.login, &team.slug)?;
                        out.extend(pull_event(&e.subject, action, &|payload| {
                            payload["requested_team"] = match &found {
                                Some(team) => team_json(face, team),
                                None => serde_json::json!({ "slug": team.slug }),
                            };
                        })?);
                    }
                }
                Ok(out)
            }
            EventBody::ReviewSubmitted(e) => {
                let (Some((repo, owner)), Some(pull), Some(review)) = (
                    about(e.subject.repo.id)?,
                    world.pull_by_id(e.subject.pull.id)?,
                    world.review_by_id(e.subject.id)?,
                ) else {
                    return Ok(Vec::new());
                };
                let mut review_json = review_json(face, &repo, &pull, &review);
                // A webhook spells a review's state in lower case, where
                // REST spells it in upper.
                review_json["state"] = review.state.as_str().into();
                review_json["updated_at"] = review_json["submitted_at"].clone();
                review_json["_links"] = serde_json::json!({
                    "html": { "href": review_json["html_url"] },
                    "pull_request": { "href": review_json["pull_request_url"] },
                });
                // The pull request a review's payload carries has none of
                // the fields a single read adds, nor these three.
                let mut pull_json = pull_json(face, &repo, &owner, &pull);
                if let Some(fields) = pull_json.as_object_mut() {
                    for field in ["merged", "mergeable", "merged_by"] {
                        fields.remove(field);
                    }
                }
                Ok(vec![Outgoing {
                    action: Some("submitted".to_string()),
                    payload: serde_json::json!({
                        "action": "submitted",
                        "review": review_json,
                        "pull_request": pull_json,
                        "repository": repo_json(face, &repo, &owner),
                        "sender": sender,
                    }),
                }])
            }
            EventBody::CommentCreated(e) => {
                let (Some((repo, owner)), Some(pull)) = (
                    about(e.subject.repo.id)?,
                    world.pull_by_id(e.subject.pull.id)?,
                ) else {
                    return Ok(Vec::new());
                };
                let comments = world.list_issue_comments_for_pull(pull.id)?;
                let Some(comment) = comments.iter().find(|c| c.id == e.subject.id) else {
                    return Ok(Vec::new());
                };
                // A comment in a payload carries three fields a world keeps
                // nothing for: nobody reacts to one, minimizes one, or
                // writes one through an App.
                let mut comment = comment_json(face, &repo, &pull, comment);
                comment["reactions"] = no_reactions(comment["url"].as_str().unwrap_or_default());
                comment["performed_via_github_app"] = serde_json::Value::Null;
                comment["minimized"] = serde_json::Value::Null;
                Ok(vec![Outgoing {
                    action: Some("created".to_string()),
                    payload: serde_json::json!({
                        "action": "created",
                        "issue": issue_json(face, &repo, &pull, comments.len()),
                        "comment": comment,
                        "repository": repo_json(face, &repo, &owner),
                        "sender": sender,
                    }),
                }])
            }
            // GitHub sends both for a run that is created already complete.
            EventBody::CheckRunCreated(e) => {
                let (id, repo_id) = (e.subject.id, e.subject.repo.id);
                let mut out = self.check_run(world, id, repo_id, "created")?;
                if e.object.status == CheckStatus::Completed {
                    out.extend(self.check_run(world, id, repo_id, "completed")?);
                }
                Ok(out)
            }
            // GitHub sends `completed` when a run completes, and again when
            // a complete run's conclusion changes. It sends nothing for any
            // other update.
            EventBody::CheckRunUpdated(e) => {
                let completes =
                    matches!(&e.changes.status, Some(s) if s.to == CheckStatus::Completed);
                let concludes = e.changes.status.is_none()
                    && e.changes.conclusion.is_some()
                    && world
                        .check_run_by_id(e.subject.id)?
                        .is_some_and(|run| run.status == CheckStatus::Completed);
                if completes || concludes {
                    self.check_run(world, e.subject.id, e.subject.repo.id, "completed")
                } else {
                    Ok(Vec::new())
                }
            }
            // GitHub sends `completed` each time a suite comes to a
            // conclusion, a different one from the last included.
            EventBody::CheckSuiteUpdated(e) => {
                self.check_suite(world, e.subject.id, e.subject.repo.id)
            }
            EventBody::DependencyAlertCreated(e) => {
                let about = about(e.subject.repo.id)?;
                self.alert(world, about, e.subject.number, "created", None)
            }
            // A person dismisses and reopens an alert. GitHub makes every
            // other move itself.
            EventBody::DependencyAlertDismissed(e)
            | EventBody::DependencyAlertReopened(e)
            | EventBody::DependencyAlertFixed(e)
            | EventBody::DependencyAlertAutoDismissed(e)
            | EventBody::DependencyAlertReintroduced(e)
            | EventBody::DependencyAlertAutoReopened(e) => {
                let (action, person) = match event {
                    EventBody::DependencyAlertDismissed(_) => ("dismissed", Some(sender)),
                    EventBody::DependencyAlertReopened(_) => ("reopened", Some(sender)),
                    EventBody::DependencyAlertFixed(_) => ("fixed", None),
                    EventBody::DependencyAlertAutoDismissed(_) => ("auto_dismissed", None),
                    EventBody::DependencyAlertReintroduced(_) => ("reintroduced", None),
                    _ => ("auto_reopened", None),
                };
                let about = about(e.subject.repo.id)?;
                self.alert(world, about, e.subject.number, action, person)
            }
            EventBody::RefCreated(e) => {
                let commits = Pushed {
                    before: None,
                    after: Some(&e.object.sha),
                    forced: false,
                    commits: &e.commits,
                };
                self.push(world, &e.subject, commits, actor, sender)
            }
            EventBody::RefUpdated(e) => match &e.changes.sha {
                Some(sha) => {
                    let commits = Pushed {
                        before: Some(&sha.from),
                        after: Some(&sha.to),
                        forced: e.forced,
                        commits: &e.commits,
                    };
                    self.push(world, &e.subject, commits, actor, sender)
                }
                None => Ok(Vec::new()),
            },
            EventBody::RefDeleted(e) => {
                let commits = Pushed {
                    before: Some(&e.object.sha),
                    after: None,
                    forced: false,
                    commits: &[],
                };
                self.push(world, &e.subject, commits, actor, sender)
            }
            _ => Ok(Vec::new()),
        }
    }

    fn check_run(
        &self,
        world: &World,
        id: i64,
        repo_id: i64,
        action: &str,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        let (Some(repo), Some(run)) = (world.repo_by_id(repo_id)?, world.check_run_by_id(id)?)
        else {
            return Ok(Vec::new());
        };
        let (Some(owner), Some(suite)) = (
            world.actor_by_login(&repo.owner_login)?,
            world.check_suite_by_id(run.suite_id)?,
        ) else {
            return Ok(Vec::new());
        };
        let mut check_run = check_run_json(face, &repo, &run)?;
        check_run["pull_requests"] = crate::suites::pulls_at(face, &repo, &run.head_sha)?;
        check_run["check_suite"] = crate::suites::suite_brief(face, &repo, &suite)?;
        Ok(vec![Outgoing {
            action: Some(action.to_string()),
            payload: serde_json::json!({
                "action": action,
                "check_run": check_run,
                "repository": repo_json(face, &repo, &owner),
                "sender": self.suite_sender(world, &suite)?,
            }),
        }])
    }

    /// GitHub's `check_suite` for a suite that has come to a conclusion, and
    /// nothing for one that has not.
    fn check_suite(
        &self,
        world: &World,
        id: i64,
        repo_id: i64,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        let (Some(repo), Some(suite)) = (world.repo_by_id(repo_id)?, world.check_suite_by_id(id)?)
        else {
            return Ok(Vec::new());
        };
        if suite.status != CheckStatus::Completed {
            return Ok(Vec::new());
        }
        let Some(owner) = world.actor_by_login(&repo.owner_login)? else {
            return Ok(Vec::new());
        };
        let check_suite = crate::suites::suite_json(face, &repo, &suite)?;
        Ok(vec![Outgoing {
            action: Some("completed".to_string()),
            payload: serde_json::json!({
                "action": "completed",
                "check_suite": check_suite,
                "repository": repo_json(face, &repo, &owner),
                "sender": self.suite_sender(world, &suite)?,
            }),
        }])
    }

    /// Who GitHub names as the sender of what happens to a suite and its
    /// check runs: whoever made the push the suite came from, whichever app
    /// wrote the check. For a suite no push made, it is GitHub's `ghost`,
    /// which is no actor of the world and carries GitHub's ids.
    fn suite_sender(
        &self,
        world: &World,
        suite: &orrery_world::CheckSuite,
    ) -> Result<serde_json::Value, WorldError> {
        let pusher = match &suite.pusher_login {
            Some(login) => world.actor_by_login(login)?,
            None => None,
        };
        Ok(match pusher {
            Some(pusher) => actor_json(&pusher),
            None => {
                let mut ghost = actor_json(&Actor {
                    id: 10137,
                    login: "ghost".to_string(),
                    kind: orrery_world::ActorKind::User,
                    created_clock_unix: 0,
                });
                ghost["node_id"] = "MDQ6VXNlcjEwMTM3".into();
                ghost
            }
        })
    }

    /// GitHub's `dependabot_alert` for an alert as the write left it.
    /// `person` is who dismissed or reopened it. What GitHub does to an
    /// alert itself, it sends as its own organization, `github`.
    fn alert(
        &self,
        world: &World,
        about: Option<(Repo, Actor)>,
        number: i64,
        action: &str,
        person: Option<serde_json::Value>,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        let Some((repo, owner)) = about else {
            return Ok(Vec::new());
        };
        let Some(alert) = world.dependency_alert(repo.id, number)? else {
            return Ok(Vec::new());
        };
        // GitHub's organization is no actor of the world, so its node id is
        // GitHub's own, never one a world's actor could hold.
        let sender = person.unwrap_or_else(|| {
            let mut github = actor_json(&Actor {
                id: 9919,
                login: "github".to_string(),
                kind: orrery_world::ActorKind::Org,
                created_clock_unix: 0,
            });
            github["node_id"] = "MDEyOk9yZ2FuaXphdGlvbjk5MTk=".into();
            github
        });
        Ok(vec![Outgoing {
            action: Some(action.to_string()),
            payload: serde_json::json!({
                "action": action,
                "alert": crate::dependabot::alert_json(face, &repo, &alert),
                "repository": repo_json(face, &repo, &owner),
                "sender": sender,
            }),
        }])
    }

    /// GitHub's `push` for a branch or a tag that was created, moved, or
    /// deleted. A pull request's own refs send none.
    fn push(
        &self,
        world: &World,
        subject: &events::RefSubject,
        pushed: Pushed<'_>,
        pusher: Option<&Actor>,
        sender: serde_json::Value,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        if !subject.name.starts_with("refs/heads/") && !subject.name.starts_with("refs/tags/") {
            return Ok(Vec::new());
        }
        let Some(repo) = world.repo_by_id(subject.repo.id)? else {
            return Ok(Vec::new());
        };
        let Some(owner) = world.actor_by_login(&repo.owner_login)? else {
            return Ok(Vec::new());
        };
        const NONE: &str = "0000000000000000000000000000000000000000";
        let base = face.base_url.trim_end_matches('/');
        let html = format!("{base}/{}", repo.full_name());
        let short = |sha: &str| sha.chars().take(12).collect::<String>();
        let compare = match (pushed.before, pushed.after) {
            (Some(before), Some(after)) => {
                format!("{html}/compare/{}...{}", short(before), short(after))
            }
            (None, Some(_)) => format!(
                "{html}/compare/{}",
                subject
                    .name
                    .trim_start_matches("refs/heads/")
                    .trim_start_matches("refs/tags/")
            ),
            (Some(before), None) => format!("{html}/compare/{}...{}", short(before), short(NONE)),
            (None, None) => html.clone(),
        };
        let bare = store::bare_dir(&world.repos_dir(), &repo.owner_login, &repo.name);
        let refs = store::list_refs(&bare).unwrap_or_default();
        let distinct = distinct_commits(&bare, &refs, &subject.name, &pushed);
        let commits = match commit_payloads(world, &bare, pushed.commits, &html, distinct.as_ref())
        {
            Ok(commits) => commits,
            Err(e) => {
                tracing::error!(error = %e, "reading a push's commits failed");
                Vec::new()
            }
        };
        // The head commit is the one the ref now names, which the push may
        // not have added: a branch made at an existing commit adds none.
        // GitHub calls it distinct whether or not the push added it.
        let head_commit = match pushed.after {
            Some(after) => commit_payloads(world, &bare, &[after.to_string()], &html, None)
                .ok()
                .and_then(|mut one| one.pop())
                .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        // A new ref made at a commit a branch already names is based on
        // that branch: the first in name order, when several name it.
        let base_ref = match (pushed.before, pushed.after) {
            (None, Some(after)) => refs
                .iter()
                .find(|(name, sha)| {
                    name.starts_with("refs/heads/")
                        && **name != subject.name
                        && sha.as_str() == after
                })
                .map(|(name, _)| name.clone()),
            _ => None,
        };
        // A push's repository carries its times as Unix seconds, and three
        // fields no other repository object has.
        let mut repository = repo_json(face, &repo, &owner);
        repository["created_at"] = repo.created_clock_unix.into();
        repository["pushed_at"] = repo.pushed_at_clock_unix.into();
        repository["master_branch"] = repo.default_branch.clone().into();
        repository["stargazers"] = 0.into();
        repository["owner"]["name"] = owner.login.clone().into();
        repository["owner"]["email"] = serde_json::Value::Null;
        // Who pushed, by login and primary address.
        let pusher = match pusher {
            Some(actor) => {
                let email = world
                    .actor_emails(actor.id)?
                    .into_iter()
                    .find(|address| address.primary)
                    .map(|address| address.email);
                serde_json::json!({ "name": crate::render::rest_login(actor), "email": email })
            }
            None => serde_json::Value::Null,
        };
        Ok(vec![Outgoing {
            action: None,
            payload: serde_json::json!({
                "ref": subject.name,
                "before": pushed.before.unwrap_or(NONE),
                "after": pushed.after.unwrap_or(NONE),
                "repository": repository,
                "pusher": pusher,
                "sender": sender,
                "created": pushed.before.is_none(),
                "deleted": pushed.after.is_none(),
                "forced": pushed.forced,
                "base_ref": base_ref,
                "compare": compare,
                "commits": commits,
                "head_commit": head_commit,
            }),
        }])
    }
}

/// What a push did to one ref: the commit it named before and names now,
/// either absent for a ref that was created or deleted, and the commits the
/// move added.
struct Pushed<'a> {
    before: Option<&'a str>,
    after: Option<&'a str>,
    forced: bool,
    commits: &'a [String],
}

/// Whether an installation's App is sent an event about `repo`: the
/// installation covers the repository, subscribes to the event, and is not
/// suspended.
fn receives(installation: &Installation, repo: &Repo, event: &str) -> bool {
    installation.suspended.is_none()
        && installation.events.iter().any(|e| e == event)
        && installation
            .account
            .login
            .eq_ignore_ascii_case(&repo.owner_login)
        && match installation.repository_selection {
            RepositorySelection::All => true,
            RepositorySelection::Selected => installation
                .repositories
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&repo.name)),
        }
}

/// The installation a deletion's event carries, as it was.
fn deleted_installation(
    world: &World,
    deleted: &events::InstallationDeleted,
) -> Result<Option<Installation>, WorldError> {
    let (Some(app), Some(account)) = (
        world.app_by_id(deleted.subject.app.id)?,
        world.actor_by_id(deleted.object.account.id)?,
    ) else {
        return Ok(None);
    };
    let suspended = match (&deleted.object.suspended_by, deleted.object.suspended_unix) {
        (Some(by), Some(clock_unix)) => world
            .actor_by_id(by.id)?
            .map(|by| orrery_world::Suspension { clock_unix, by }),
        _ => None,
    };
    Ok(Some(Installation {
        id: deleted.object.id,
        app_id: app.id,
        app_slug: app.slug,
        client_id: app.client_id,
        account,
        repository_selection: deleted.object.repository_selection,
        repositories: deleted
            .object
            .repositories
            .iter()
            .map(|repo| repo.name.clone())
            .collect(),
        permissions: deleted.object.permissions.clone(),
        events: deleted.object.events.clone(),
        created_clock_unix: deleted.object.created_unix,
        updated_clock_unix: deleted
            .object
            .updated_unix
            .unwrap_or(deleted.object.created_unix),
        suspended,
    }))
}

/// GitHub's node id for an installation, which a payload carries beside
/// its id.
fn installation_node_id(id: i64) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("023:IntegrationInstallation{id}"))
}

/// GitHub's count of the reactions to the issue or the comment at `url`,
/// which in a world has none.
fn no_reactions(url: &str) -> serde_json::Value {
    serde_json::json!({
        "url": format!("{url}/reactions"),
        "total_count": 0,
        "+1": 0,
        "-1": 0,
        "laugh": 0,
        "hooray": 0,
        "confused": 0,
        "heart": 0,
        "rocket": 0,
        "eyes": 0,
    })
}

/// A pull request as GitHub's issue object, which an `issue_comment`
/// carries: a pull request is an issue there, with a `pull_request` field
/// saying so.
fn issue_json(face: &GithubFace, repo: &Repo, pull: &Pull, comments: usize) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    let n = pull.number;
    let api = format!("{base}/api/v3/repos/{full_name}");
    let html = format!("{base}/{full_name}/pull/{n}");
    serde_json::json!({
        "url": format!("{api}/issues/{n}"),
        "repository_url": api,
        "labels_url": format!("{api}/issues/{n}/labels{{/name}}"),
        "comments_url": format!("{api}/issues/{n}/comments"),
        "events_url": format!("{api}/issues/{n}/events"),
        "html_url": html,
        "id": pull.id,
        "node_id": crate::nodeid::pull(pull.id),
        "number": n,
        "title": pull.title,
        "user": user_json(face, &pull.author_login),
        "labels": pull.labels.iter().map(|l| label_json(l)).collect::<Vec<_>>(),
        "state": match pull.state {
            PullState::Open => "open",
            _ => "closed",
        },
        "locked": false,
        "assignee": serde_json::Value::Null,
        "assignees": [],
        "milestone": serde_json::Value::Null,
        "comments": comments,
        "created_at": rfc3339(pull.created_clock_unix),
        "updated_at": rfc3339(pull.updated_clock_unix),
        "closed_at": pull.closed_clock_unix.map(rfc3339),
        "author_association": "NONE",
        "active_lock_reason": serde_json::Value::Null,
        "draft": pull.draft,
        "pull_request": {
            "url": format!("{api}/pulls/{n}"),
            "html_url": html,
            "diff_url": format!("{html}.diff"),
            "patch_url": format!("{html}.patch"),
            "merged_at": pull.merged.then_some(pull.closed_clock_unix).flatten().map(rfc3339),
        },
        "body": pull.body,
        "timeline_url": format!("{api}/issues/{n}/timeline"),
        "performed_via_github_app": serde_json::Value::Null,
        "state_reason": serde_json::Value::Null,
        "reactions": no_reactions(&format!("{api}/issues/{n}")),
    })
}

/// The commits a push added that no other branch or tag reaches: what
/// GitHub calls distinct. `None` when git could not say, and for a ref that
/// was deleted.
fn distinct_commits(
    bare: &std::path::Path,
    refs: &std::collections::BTreeMap<String, String>,
    name: &str,
    pushed: &Pushed<'_>,
) -> Option<std::collections::BTreeSet<String>> {
    let after = pushed.after?;
    let mut others: std::collections::BTreeSet<&str> = refs
        .iter()
        .filter(|(other, _)| {
            other.as_str() != name
                && (other.starts_with("refs/heads/") || other.starts_with("refs/tags/"))
        })
        .map(|(_, sha)| sha.as_str())
        .collect();
    others.extend(pushed.before);
    let others: Vec<&str> = others.into_iter().collect();
    let (commits, _) = store::added_commits(bare, after, &others, usize::MAX).ok()?;
    Some(commits.into_iter().collect())
}

/// Each of `shas` as a push's payload carries a commit, in their order:
/// its id and tree, its message, who wrote and committed it and when, and
/// the files it added, removed, and modified against its first parent, or
/// every file it holds as added when it has no parent. `distinct` holds the
/// commits to call distinct, or `None` to call every one distinct.
fn commit_payloads(
    world: &World,
    bare: &std::path::Path,
    shas: &[String],
    html: &str,
    distinct: Option<&std::collections::BTreeSet<String>>,
) -> Result<Vec<serde_json::Value>, String> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    // One record a commit, its fields apart by a unit separator.
    const FORMAT: &str =
        "--format=%H%x1f%T%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%B%x1e";
    let out = store::hermetic_git(bare)
        .args(["log", "--no-walk=unsorted", FORMAT])
        .args(shas)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let records: Vec<Vec<&str>> = text
        .split('\x1e')
        .map(|record| record.trim_start_matches('\n').split('\x1f').collect())
        .filter(|fields: &Vec<&str>| fields.len() == 10)
        .collect();
    let pairs: Vec<(&str, &str)> = records
        .iter()
        .filter_map(|f| {
            f[2].split(' ')
                .next()
                .filter(|p| !p.is_empty())
                .map(|p| (p, f[0]))
        })
        .collect();
    let mut changed = store::changed_files_many(bare, &pairs).into_iter();
    let mut commits = Vec::with_capacity(records.len());
    for f in &records {
        let (mut added, mut removed, mut modified) = (Vec::new(), Vec::new(), Vec::new());
        if f[2].is_empty() {
            added = store::tree_paths(bare, f[0]).map_err(|e| e.to_string())?;
        } else if let Some(Ok(files)) = changed.next() {
            for file in files {
                match file.status {
                    store::FileStatus::Added => added.push(file.path),
                    store::FileStatus::Removed => removed.push(file.path),
                    _ => modified.push(file.path),
                }
            }
        }
        // GitHub names the account that holds the address, when one does.
        let person = |name: &str, email: &str, date: &str| {
            let mut person = serde_json::json!({ "name": name, "email": email, "date": date });
            if let Ok(Some(login)) = world.login_by_email(email) {
                person["username"] = login.into();
            }
            person
        };
        commits.push(serde_json::json!({
            "id": f[0],
            "tree_id": f[1],
            "distinct": distinct.is_none_or(|set| set.contains(f[0])),
            "message": f[9].trim_end_matches('\n'),
            "timestamp": f[8],
            "url": format!("{html}/commit/{}", f[0]),
            "author": person(f[3], f[4], f[5]),
            "committer": person(f[6], f[7], f[8]),
            "added": added,
            "removed": removed,
            "modified": modified,
        }));
    }
    Ok(commits)
}
