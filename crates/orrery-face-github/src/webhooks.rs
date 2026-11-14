//! Webhooks: the deliveries a GitHub App is owed, the request each is sent
//! as, and the routes an App reads them by (spec/surface-github.md §1.12).
//!
//! The world tells [`Webhooks`] of each write as it lands, and it answers
//! the deliveries that write makes: one for each App whose webhook is
//! active, that subscribes to the event, and that has an installation
//! covering the repository. A payload is rendered here, from the world as
//! the write left it, with the renderers the REST routes answer with.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{Response, StatusCode, header};
use base64::Engine as _;
use orrery_face_git::store;
use orrery_world::events::{self, EventBody};
use orrery_world::{
    Actor, App, Attribution, CheckRun, CheckStatus, Delivery, DeliveryFailure, DeliveryFilter,
    DeliveryOutcome, Installation, NewDelivery, Notifier, Pull, PullState, Repo,
    RepositorySelection, SYSTEM_ACTOR_ID, World, WorldError, WorldView,
};

use crate::render::{
    Mergeability, actor_json, check_run_json, comment_json, full_pull_json, label_json, pull_json,
    repo_json, review_json, rfc3339, team_json, user_json,
};
use crate::{GithubFace, github_error, internal, json_response};

/// What GitHub's sender calls itself, before the build it runs.
const USER_AGENT: &str = "GitHub-Hookshot/orrery";

/// The most deliveries a page of the list holds, and how many it holds when
/// the request names no size.
const MAX_PER_PAGE: usize = 100;
const DEFAULT_PER_PAGE: usize = 30;

// ---- the routes an App reads its webhook by ----

pub(crate) async fn get_config(Extension(app): Extension<App>) -> Response<Body> {
    let mut config = serde_json::json!({
        "content_type": "json",
        "insecure_ssl": "0",
        "url": app.hook.url,
    });
    if app.hook.has_secret {
        config["secret"] = "********".into();
    }
    json_response(StatusCode::OK, config)
}

fn invalid_request(detail: &str) -> Response<Body> {
    github_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        &format!("Invalid request.\n\n{detail}"),
    )
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
    let mut first: Option<i64> = None;
    let mut filter = DeliveryFilter::All;
    // What the next page's link repeats: every parameter but the cursor.
    let mut repeated = Vec::new();
    for (key, value) in form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "per_page" => per_page = value.parse().unwrap_or(DEFAULT_PER_PAGE).min(MAX_PER_PAGE),
            "cursor" => match value.strip_prefix("v1_").and_then(|id| id.parse().ok()) {
                Some(id) => first = Some(id),
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
    if let Some(next) = next {
        let base = face.base_url.trim_end_matches('/');
        repeated.push(format!("cursor=v1_{next}"));
        let link = format!(
            "<{base}/api/v3/app/hook/deliveries?{}>; rel=\"next\"",
            repeated.join("&")
        );
        if let Ok(value) = link.parse() {
            resp.headers_mut().insert(header::LINK, value);
        }
    }
    resp
}

/// A delivery as GitHub lists it. GitHub reports an attempt that reached no
/// receiver as a `502`.
fn delivery_json(delivery: &Delivery) -> serde_json::Value {
    let (status, status_code, delivered_at, duration) = match &delivery.attempt {
        Some(attempt) => {
            let (status, code) = match &attempt.outcome {
                DeliveryOutcome::Answered { status_code, .. } => match status_code {
                    200..=299 => ("OK".to_string(), *status_code),
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
             turn it on with PATCH /_orrery/apps/{slug}/webhook",
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

/// Makes an App's deliveries for the writes a world takes.
pub struct Webhooks {
    face: GithubFace,
}

/// The notifier to register with the world the face serves.
pub fn notifier(face: GithubFace) -> Arc<dyn Notifier> {
    Arc::new(Webhooks { face })
}

/// One event to deliver, before it is addressed to an App: GitHub's event
/// name and action, the repository it is about, and the payload, which
/// every App receives alike but for its own `installation`.
struct Outgoing {
    event: &'static str,
    action: Option<String>,
    repo: Option<Repo>,
    payload: serde_json::Value,
}

/// An event about one installation, which only that installation's App
/// receives, whether or not it subscribes.
struct OfInstallation {
    installation: Installation,
    action: &'static str,
}

impl Notifier for Webhooks {
    fn deliveries(&self, world: &World, by: Attribution, events: &[EventBody]) -> Vec<NewDelivery> {
        match self.made(world, by, events) {
            Ok(made) => made,
            Err(e) => {
                tracing::error!(error = %e, "making a write's deliveries failed");
                Vec::new()
            }
        }
    }
}

impl Webhooks {
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
            for outgoing in self.outgoing(world, event, sender.as_ref())? {
                let Some(repo) = &outgoing.repo else {
                    continue;
                };
                for installation in installations
                    .iter()
                    .filter(|i| receives(i, repo, &outgoing))
                {
                    if !apps.iter().any(|app| app.id == installation.app_id) {
                        continue;
                    }
                    let mut payload = outgoing.payload.clone();
                    payload["installation"] = serde_json::json!({
                        "id": installation.id,
                        "node_id": installation_node_id(installation.id),
                    });
                    made.push(NewDelivery {
                        app_id: installation.app_id,
                        installation_id: Some(installation.id),
                        repo_id: Some(repo.id),
                        event: outgoing.event.to_string(),
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
                event: "pull_request",
                action: Some(action.to_string()),
                repo: Some(repo),
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
                let moved = |from: usize, to: usize| match requested {
                    true => to > from,
                    false => from > to,
                };
                let mut out = Vec::new();
                if let Some(reviewers) = &e.changes.requested_reviewers {
                    let (old, new) = (&reviewers.from, &reviewers.to);
                    let (gone, kept) = match requested {
                        true => (new, old),
                        false => (old, new),
                    };
                    if moved(old.len(), new.len()) || gone != kept {
                        for reviewer in gone.iter().filter(|r| !kept.contains(r)) {
                            out.extend(pull_event(&e.subject, action, &|payload| {
                                payload["requested_reviewer"] = user_json(face, &reviewer.login);
                            })?);
                        }
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
                Ok(vec![Outgoing {
                    event: "pull_request_review",
                    action: Some("submitted".to_string()),
                    payload: serde_json::json!({
                        "action": "submitted",
                        "review": review_json,
                        "pull_request": pull_json(face, &repo, &owner, &pull),
                        "repository": repo_json(face, &repo, &owner),
                        "sender": sender,
                    }),
                    repo: Some(repo),
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
                Ok(vec![Outgoing {
                    event: "issue_comment",
                    action: Some("created".to_string()),
                    payload: serde_json::json!({
                        "action": "created",
                        "issue": issue_json(face, &repo, &pull, comments.len()),
                        "comment": comment_json(face, &repo, &pull, comment),
                        "repository": repo_json(face, &repo, &owner),
                        "sender": sender,
                    }),
                    repo: Some(repo),
                }])
            }
            EventBody::CheckRunCreated(e) => {
                self.check_run(world, e.subject.id, e.subject.repo.id, "created", sender)
            }
            // GitHub sends `completed` when a run completes, and nothing
            // for any other update.
            EventBody::CheckRunUpdated(e) => match &e.changes.status {
                Some(status) if status.to == CheckStatus::Completed => {
                    self.check_run(world, e.subject.id, e.subject.repo.id, "completed", sender)
                }
                _ => Ok(Vec::new()),
            },
            EventBody::RefCreated(e) => {
                let commits = Pushed {
                    before: None,
                    after: Some(&e.object.sha),
                    forced: false,
                    commits: &e.commits,
                };
                self.push(world, &e.subject, commits, sender)
            }
            EventBody::RefUpdated(e) => match &e.changes.sha {
                Some(sha) => {
                    let commits = Pushed {
                        before: Some(&sha.from),
                        after: Some(&sha.to),
                        forced: e.forced,
                        commits: &e.commits,
                    };
                    self.push(world, &e.subject, commits, sender)
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
                self.push(world, &e.subject, commits, sender)
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
        sender: serde_json::Value,
    ) -> Result<Vec<Outgoing>, WorldError> {
        let face = &self.face;
        let (Some(repo), Some(run)) = (world.repo_by_id(repo_id)?, world.check_run_by_id(id)?)
        else {
            return Ok(Vec::new());
        };
        let Some(owner) = world.actor_by_login(&repo.owner_login)? else {
            return Ok(Vec::new());
        };
        let mut check_run = check_run_json(face, &repo, &run)?;
        check_run["pull_requests"] = self.pulls_of(world, &repo, &run)?;
        Ok(vec![Outgoing {
            event: "check_run",
            action: Some(action.to_string()),
            payload: serde_json::json!({
                "action": action,
                "check_run": check_run,
                "repository": repo_json(face, &repo, &owner),
                "sender": sender,
            }),
            repo: Some(repo),
        }])
    }

    /// The open pull requests whose head is the commit a check run is on,
    /// as GitHub lists them in a check run.
    fn pulls_of(
        &self,
        world: &World,
        repo: &Repo,
        run: &CheckRun,
    ) -> Result<serde_json::Value, WorldError> {
        let base = self.face.base_url.trim_end_matches('/');
        let url = format!("{base}/api/v3/repos/{}", repo.full_name());
        let of = |pull: &Pull, name: &str, sha: &str| {
            serde_json::json!({
                "ref": name,
                "sha": sha,
                "repo": { "id": pull.repo_id, "url": url, "name": repo.name },
            })
        };
        Ok(world
            .list_pulls_for_repo(repo.id, Some(PullState::Open))?
            .iter()
            .filter(|pull| pull.head_sha == run.head_sha)
            .map(|pull| {
                serde_json::json!({
                    "url": format!("{url}/pulls/{}", pull.number),
                    "id": pull.id,
                    "number": pull.number,
                    "head": of(pull, &pull.head_ref, &pull.head_sha),
                    "base": of(pull, &pull.base_ref, &pull.base_sha),
                })
            })
            .collect())
    }

    /// GitHub's `push` for a branch or a tag that was created, moved, or
    /// deleted. A pull request's own refs send none.
    fn push(
        &self,
        world: &World,
        subject: &events::RefSubject,
        pushed: Pushed<'_>,
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
        let commits = match commit_payloads(&bare, pushed.commits, &html) {
            Ok(commits) => commits,
            Err(e) => {
                tracing::error!(error = %e, "reading a push's commits failed");
                Vec::new()
            }
        };
        // The head commit is the one the ref now names, which the push may
        // not have added: a branch made at an existing commit adds none.
        let head_commit = match pushed.after {
            Some(after) => match commits.iter().find(|c| c["id"] == *after) {
                Some(commit) => commit.clone(),
                None => commit_payloads(&bare, &[after.to_string()], &html)
                    .ok()
                    .and_then(|mut one| one.pop())
                    .unwrap_or(serde_json::Value::Null),
            },
            None => serde_json::Value::Null,
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
        let pusher = match sender.get("login").and_then(|l| l.as_str()) {
            Some(login) => {
                serde_json::json!({ "name": login.trim_end_matches("[bot]"), "email": null })
            }
            None => serde_json::Value::Null,
        };
        Ok(vec![Outgoing {
            event: "push",
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
                "base_ref": serde_json::Value::Null,
                "compare": compare,
                "commits": commits,
                "head_commit": head_commit,
            }),
            repo: Some(repo),
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
fn receives(installation: &Installation, repo: &Repo, outgoing: &Outgoing) -> bool {
    installation.suspended.is_none()
        && installation.events.iter().any(|e| e == outgoing.event)
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
        suspended,
    }))
}

/// GitHub's node id for an installation, which a payload carries beside
/// its id.
fn installation_node_id(id: i64) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("023:IntegrationInstallation{id}"))
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
    })
}

/// Each of `shas` as a push's payload carries a commit, in their order:
/// its id and tree, its message, who wrote and committed it and when, and
/// the files it added, removed, and modified against its first parent.
fn commit_payloads(
    bare: &std::path::Path,
    shas: &[String],
    html: &str,
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
        let has_parent = !f[2].is_empty();
        if has_parent && let Some(Ok(files)) = changed.next() {
            for file in files {
                match file.status {
                    store::FileStatus::Added => added.push(file.path),
                    store::FileStatus::Removed => removed.push(file.path),
                    _ => modified.push(file.path),
                }
            }
        }
        let person = |name: &str, email: &str, date: &str| serde_json::json!({ "name": name, "email": email, "date": date });
        commits.push(serde_json::json!({
            "id": f[0],
            "tree_id": f[1],
            "distinct": true,
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
