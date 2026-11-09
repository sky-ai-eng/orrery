//! Check-run endpoints — the fabricated-CI door. On real GitHub only a
//! GitHub App can create check runs; Orrery's mapping of "App" is a `bot`
//! actor (spec/usage.md: a ci-bot is a synthetic actor like any other), so a
//! non-bot caller is refused with an Orrery-explained 403 rather than
//! GitHub's opaque "not accessible by integration" — a documented divergence
//! that teaches the caller which door to use.

use std::collections::BTreeMap;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_world::{Actor, ActorKind, CheckConclusion, CheckRunPatch, CheckStatus, NewCheckRun};

use crate::render::check_run_json;
use crate::{
    GithubFace, github_error, internal, json_response, media, orrery_unsupported_field,
    resolve_repo, validation_custom, validation_invalid, validation_missing,
};

#[derive(serde::Deserialize)]
struct CheckRunBody {
    name: Option<String>,
    head_sha: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    details_url: Option<String>,
    external_id: Option<String>,
    output: Option<serde_json::Value>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn create_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if actor.kind != ActorKind::Bot {
        return bot_only(&actor);
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckRun") {
        return refused;
    }
    let parsed: CheckRunBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    if parsed.rest.contains_key("actions") {
        return orrery_unsupported_field("CheckRun", "actions");
    }
    let Some(name) = parsed.name.as_deref().filter(|n| !n.is_empty()) else {
        return validation_missing("CheckRun", "name");
    };
    let Some(head_sha) = parsed.head_sha.as_deref() else {
        return validation_missing("CheckRun", "head_sha");
    };
    if !is_full_sha(head_sha) {
        return validation_invalid("CheckRun", "head_sha");
    }
    let fields = match resolve_status_fields(&parsed, None) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let clock = match face.world.clock() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    // GitHub stamps started_at at creation regardless of status; explicit
    // timestamps are caller inputs (deterministic, like commit dates).
    let started = fields.started.unwrap_or(clock);
    let completed = match fields.completed {
        Some(c) => Some(c),
        None if fields.status == CheckStatus::Completed => Some(clock),
        None => None,
    };
    let created = face.world.create_check_run(NewCheckRun {
        repo_id: repo.id,
        creator_id: actor.id,
        head_sha,
        name,
        status: fields.status,
        conclusion: fields.conclusion,
        details_url: parsed.details_url.as_deref(),
        external_id: parsed.external_id.as_deref(),
        output: parsed.output.as_ref(),
        started_clock_unix: Some(started),
        completed_clock_unix: completed,
    });
    match created {
        Ok(run) => json_response(StatusCode::CREATED, check_run_json(&face, &repo, &run)),
        Err(e) => internal(e),
    }
}

pub(crate) async fn patch_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if actor.kind != ActorKind::Bot {
        return bot_only(&actor);
    }
    let Ok(run_id) = id.parse::<i64>() else {
        return github_error(StatusCode::NOT_FOUND, "Not Found");
    };
    let existing = match face.world.check_run_by_id(run_id) {
        Ok(Some(r)) if r.repo_id == repo.id => r,
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckRun") {
        return refused;
    }
    let parsed: CheckRunBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    if parsed.rest.contains_key("actions") {
        return orrery_unsupported_field("CheckRun", "actions");
    }
    // Rebinding a check run to another commit is not a thing GitHub's PATCH
    // does; refuse rather than silently ignore the field.
    if parsed.head_sha.is_some() {
        return orrery_unsupported_field(
            "CheckRun",
            "head_sha (a check run cannot be moved to another commit)",
        );
    }
    let fields = match resolve_status_fields(&parsed, Some(existing.status)) {
        Ok(f) => f,
        Err(resp) => return *resp,
    };
    let clock = match face.world.clock() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let completed = match fields.completed {
        Some(c) => Some(c),
        None if fields.status == CheckStatus::Completed
            && existing.completed_clock_unix.is_none() =>
        {
            Some(clock)
        }
        None => None,
    };
    let updated = face.world.update_check_run(
        run_id,
        actor.id,
        CheckRunPatch {
            name: parsed.name.as_deref(),
            status: Some(fields.status),
            conclusion: fields.conclusion,
            details_url: parsed.details_url.as_deref(),
            external_id: parsed.external_id.as_deref(),
            output: parsed.output.as_ref(),
            started_clock_unix: fields.started,
            completed_clock_unix: completed,
        },
    );
    match updated {
        Ok(run) => json_response(StatusCode::OK, check_run_json(&face, &repo, &run)),
        Err(e) => internal(e),
    }
}

struct StatusFields {
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    started: Option<i64>,
    completed: Option<i64>,
}

/// Parse and couple status/conclusion the way GitHub does: a conclusion
/// forces `completed`, and contradicting that explicitly is a 422.
/// `current` is the stored status on PATCH (None on create, where the
/// default is `queued`).
fn resolve_status_fields(
    body: &CheckRunBody,
    current: Option<CheckStatus>,
) -> Result<StatusFields, Box<Response<Body>>> {
    let explicit = match body.status.as_deref() {
        None => None,
        Some("queued") => Some(CheckStatus::Queued),
        Some("in_progress") => Some(CheckStatus::InProgress),
        Some("completed") => Some(CheckStatus::Completed),
        Some(_) => return Err(Box::new(validation_invalid("CheckRun", "status"))),
    };
    let conclusion = match body.conclusion.as_deref() {
        None => None,
        Some(c) => Some(
            parse_conclusion(c)
                .ok_or_else(|| Box::new(validation_invalid("CheckRun", "conclusion")))?,
        ),
    };
    let status = if conclusion.is_some() {
        match explicit {
            Some(s) if s != CheckStatus::Completed => {
                return Err(Box::new(validation_custom(
                    "CheckRun",
                    "conclusion can only be provided when the status is completed",
                )));
            }
            _ => CheckStatus::Completed,
        }
    } else {
        explicit.or(current).unwrap_or(CheckStatus::Queued)
    };
    let started = match body.started_at.as_deref() {
        None => None,
        Some(t) => Some(
            parse_timestamp(t)
                .ok_or_else(|| Box::new(validation_invalid("CheckRun", "started_at")))?,
        ),
    };
    let completed = match body.completed_at.as_deref() {
        None => None,
        Some(t) => Some(
            parse_timestamp(t)
                .ok_or_else(|| Box::new(validation_invalid("CheckRun", "completed_at")))?,
        ),
    };
    Ok(StatusFields {
        status,
        conclusion,
        started,
        completed,
    })
}

fn parse_conclusion(s: &str) -> Option<CheckConclusion> {
    Some(match s {
        "success" => CheckConclusion::Success,
        "failure" => CheckConclusion::Failure,
        "neutral" => CheckConclusion::Neutral,
        "cancelled" => CheckConclusion::Cancelled,
        "skipped" => CheckConclusion::Skipped,
        "stale" => CheckConclusion::Stale,
        "timed_out" => CheckConclusion::TimedOut,
        "action_required" => CheckConclusion::ActionRequired,
        _ => return None,
    })
}

fn parse_timestamp(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

fn bot_only(actor: &Actor) -> Response<Body> {
    let body = serde_json::json!({
        "message": format!(
            "orrery: check runs are created by bot actors (real GitHub restricts them to Apps); {} is a {}",
            actor.login,
            actor.kind.as_str(),
        ),
        "orrery": {
            "not_mocked": false,
            "hint": "Mint a bot actor on the control plane (POST /_orrery/actors with kind \"bot\") and use its token — the ci-bot role.",
        },
    });
    crate::json_response(StatusCode::FORBIDDEN, body)
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}
