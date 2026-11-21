//! Check-run endpoints — the fabricated-CI door. On real GitHub only a
//! GitHub App can create check runs; Orrery's mapping of "App" is a `bot`
//! actor (spec/usage.md: a ci-bot is a synthetic actor like any other), so a
//! non-bot caller is refused with an Orrery-explained 403 rather than
//! GitHub's opaque "not accessible by integration" — a documented divergence
//! that teaches the caller which door to use.

use std::collections::BTreeMap;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_world::{
    Actor, ActorKind, Annotation, Caller, CheckConclusion, CheckRun, CheckRunPatch, CheckStatus,
    Door, NewCheckRun, Repo,
};

use crate::render::check_run_json;
use crate::{
    GithubFace, Sent, github_error, internal, invalid_request, json_response, media,
    orrery_unsupported_field, resolve_repo, validation_custom, validation_invalid,
    validation_missing,
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
    output: Option<Sent>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

pub(crate) async fn create_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let actor = caller.actor;
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
    let output = match Output::read(parsed.output.as_ref()) {
        Ok(output) => output,
        Err(resp) => return *resp,
    };
    let Some(name) = parsed.name.as_deref().filter(|n| !n.is_empty()) else {
        return validation_missing("CheckRun", "name");
    };
    let Some(head_sha) = parsed.head_sha.as_deref() else {
        return validation_missing("CheckRun", "head_sha");
    };
    if !is_full_sha(head_sha) {
        return validation_invalid("CheckRun", "head_sha");
    }
    // GitHub takes a sha in either case and answers the commit's own.
    let head_sha = &head_sha.to_ascii_lowercase();
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
    if output.crashes {
        return crash();
    }
    // A new check run is refused for an annotation an update would leave
    // out.
    if output.left_out {
        return validation_invalid("CheckRun", "annotations");
    }
    let created = face.world.create_check_run(
        by,
        NewCheckRun {
            repo_id: repo.id,
            head_sha,
            name,
            status: fields.status,
            conclusion: fields.conclusion,
            details_url: parsed.details_url.as_deref(),
            external_id: parsed.external_id.as_deref(),
            output: output.said.as_ref(),
            started_clock_unix: Some(started),
            completed_clock_unix: completed,
            annotations: &output.annotations,
        },
        clock,
    );
    match created.and_then(|run| check_run_json(&face, &repo, &run)) {
        Ok(run) => json_response(StatusCode::CREATED, run),
        Err(e) => internal(e),
    }
}

pub(crate) async fn patch_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let actor = caller.actor;
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
    // A job's check run is its runner's to move, and GitHub answers this to
    // any other App that tries.
    if existing.job.is_some() {
        let app_id = match crate::suites::app_id_of(&face, &existing.creator_login) {
            Ok(id) => id.unwrap_or(crate::actions::ACTIONS_APP_ID),
            Err(e) => return internal(e),
        };
        return github_error(
            StatusCode::FORBIDDEN,
            &format!(
                "Invalid app_id `{app_id}` - check run can only be modified by the GitHub App \
                 that created it."
            ),
        );
    }
    let parsed: CheckRunBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    if parsed.rest.contains_key("actions") {
        return orrery_unsupported_field("CheckRun", "actions");
    }
    let output = match Output::read(parsed.output.as_ref()) {
        Ok(output) => output,
        Err(resp) => return *resp,
    };
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
    if output.crashes {
        return crash();
    }
    let updated = face.world.update_check_run(
        run_id,
        by,
        CheckRunPatch {
            name: parsed.name.as_deref(),
            status: Some(fields.status),
            conclusion: fields.conclusion,
            details_url: parsed.details_url.as_deref(),
            external_id: parsed.external_id.as_deref(),
            output: output.said.as_ref(),
            started_clock_unix: fields.started,
            completed_clock_unix: completed,
            annotations: &output.annotations,
        },
        clock,
    );
    match updated.and_then(|run| check_run_json(&face, &repo, &run)) {
        Ok(run) => json_response(StatusCode::OK, run),
        Err(e) => internal(e),
    }
}

/// The longest summary, text, message, or raw details GitHub keeps, in
/// characters.
const MAX_TEXT: usize = 65_535;

/// The most annotations GitHub takes in one request.
const MAX_ANNOTATIONS: usize = 50;

/// GitHub's answer to a value of a type it fails on, where it refuses any
/// other: a `500` with no body.
fn crash() -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// What a request's `output` holds, once it has passed the checks GitHub
/// makes of its shape.
#[derive(Default)]
struct Output {
    /// The title, summary, and text to store, or `None` for a request that
    /// sent no output: an update then leaves the output as it is.
    said: Option<serde_json::Value>,
    /// The annotations GitHub keeps.
    annotations: Vec<Annotation>,
    /// Whether an annotation was one GitHub does not keep.
    left_out: bool,
    /// Whether a value was of a type GitHub fails on.
    crashes: bool,
}

/// What GitHub makes of one annotation it was sent.
enum Read {
    Kept(Annotation),
    LeftOut,
    Crashes,
}

impl Output {
    /// Read `output` as GitHub does, or its `422` for a shape it refuses.
    /// GitHub checks every member and names every fault, one to a line: the
    /// members in the order its schema has them, then the ones the output
    /// lacks.
    fn read(output: Option<&Sent>) -> Result<Output, Box<Response<Body>>> {
        let refuse = |detail: String| Box::new(invalid_request(&detail));
        let output = match output {
            None | Some(Sent::Null) => return Ok(Output::default()),
            Some(output @ Sent::Object(_)) => output,
            Some(other) => {
                return Err(refuse(format!(
                    "For 'properties/output', {} is not an object or null.",
                    other.spelled()
                )));
            }
        };
        let mut faults: Vec<String> = Vec::new();
        for key in ["title", "summary", "text"] {
            match output.member(key) {
                None => {}
                Some(Sent::String(text)) => {
                    let length = text.chars().count();
                    if key != "title" && length > MAX_TEXT {
                        faults.push(format!(
                            "Only {MAX_TEXT} characters are allowed; {length} were supplied."
                        ));
                    }
                }
                Some(other) => faults.push(format!(
                    "For 'properties/{key}', {} is not a string.",
                    other.spelled()
                )),
            }
        }
        // An image is checked and then kept nowhere: no read answers one.
        objects_of(output, "images", &["alt", "image_url"], None, &mut faults);
        let items = objects_of(
            output,
            "annotations",
            &[
                "annotation_level",
                "end_line",
                "message",
                "path",
                "start_line",
            ],
            Some(MAX_ANNOTATIONS),
            &mut faults,
        );
        let missing: Vec<&str> = ["summary", "title"]
            .into_iter()
            .filter(|key| output.member(key).is_none())
            .collect();
        if !missing.is_empty() {
            faults.push(not_supplied(&missing));
        }
        if !faults.is_empty() {
            return Err(refuse(faults.join("\n")));
        }
        let text = |key: &str| match output.member(key) {
            Some(Sent::String(text)) => Some(text.as_str()),
            _ => None,
        };
        let mut read = Output {
            said: Some(serde_json::json!({
                "title": text("title"),
                "summary": text("summary"),
                "text": text("text"),
            })),
            ..Output::default()
        };
        for item in items {
            match annotation(item) {
                Read::Kept(annotation) => read.annotations.push(annotation),
                Read::LeftOut => read.left_out = true,
                Read::Crashes => read.crashes = true,
            }
        }
        Ok(read)
    }
}

/// The objects in the array `output` holds as `key`, with GitHub's words
/// for each fault of the array's shape added to `faults`: an item that is
/// no object and an item that lacks a field of `required`, each where it
/// comes, then more items than `most`.
fn objects_of<'a>(
    output: &'a Sent,
    key: &str,
    required: &[&str],
    most: Option<usize>,
    faults: &mut Vec<String>,
) -> Vec<&'a Sent> {
    let sent = match output.member(key) {
        None => return Vec::new(),
        Some(Sent::Array(sent)) => sent,
        Some(other) => {
            faults.push(format!(
                "For 'properties/{key}', {} is not an array.",
                other.spelled()
            ));
            return Vec::new();
        }
    };
    let mut objects = Vec::with_capacity(sent.len());
    for item in sent {
        if !matches!(item, Sent::Object(_)) {
            faults.push(format!("For 'items', {} is not an object.", item.spelled()));
            continue;
        }
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|name| item.member(name).is_none())
            .collect();
        if !missing.is_empty() {
            faults.push(not_supplied(&missing));
        }
        objects.push(item);
    }
    if let Some(most) = most.filter(|most| sent.len() > *most) {
        faults.push(format!(
            "No more than {most} items are allowed; {} were supplied.",
            sent.len()
        ));
    }
    objects
}

/// GitHub's words for required fields a request lacks.
fn not_supplied(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|name| format!("\"{name}\"")).collect();
    match quoted.len() {
        1 => format!("{} wasn't supplied.", quoted[0]),
        _ => format!("{} weren't supplied.", quoted.join(", ")),
    }
}

/// A number GitHub reads from a line or a column: an integer, a number cut
/// to one, the digits a string starts with, which is 0 for a string that
/// starts with none, and 1 or 0 for true or false. `None` for anything
/// else.
fn whole(value: &Sent) -> Option<i64> {
    match value {
        Sent::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Sent::String(s) => {
            let s = s.trim_start();
            let (sign, digits) = match s.strip_prefix('-') {
                Some(rest) => (-1, rest),
                None => (1, s.strip_prefix('+').unwrap_or(s)),
            };
            let end = digits
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(digits.len());
            Some(sign * digits[..end].parse::<i64>().unwrap_or(0))
        }
        Sent::Bool(value) => Some(i64::from(*value)),
        Sent::Null | Sent::Array(_) | Sent::Object(_) => None,
    }
}

/// Text as GitHub keeps it: no more than [`MAX_TEXT`] characters of it.
fn kept(text: &str) -> String {
    text.chars().take(MAX_TEXT).collect()
}

/// One annotation as GitHub reads it, which is by what each field came to
/// when it was observed and not by its description of them. It leaves out
/// an annotation with no path, no message, no level, or no line, one that
/// ends before it starts, and one with a column whose lines are not one
/// line. Of the rest it keeps a level it does not define, a line below 1,
/// and a title of any length, and it fails on a path, a message, or a title
/// that is something other than a string.
fn annotation(item: &Sent) -> Read {
    let (path, message) = (item.at("path"), item.at("message"));
    if path.is_blank() || message.is_blank() {
        return Read::LeftOut;
    }
    let level = match item.at("annotation_level") {
        Sent::Null => return Read::LeftOut,
        Sent::String(level) if level.trim().is_empty() => return Read::LeftOut,
        Sent::String(level) => level.clone(),
        Sent::Bool(true) => "1".to_string(),
        Sent::Bool(false) => "0".to_string(),
        other => other.spelled(),
    };
    let (Some(start_line), Some(end_line)) =
        (whole(item.at("start_line")), whole(item.at("end_line")))
    else {
        return Read::LeftOut;
    };
    let (start_column, end_column) = (whole(item.at("start_column")), whole(item.at("end_column")));
    let columns = start_column.is_some() || end_column.is_some();
    let backwards = matches!((start_column, end_column), (Some(start), Some(end)) if end < start);
    if end_line < start_line || (columns && start_line != end_line) || backwards {
        return Read::LeftOut;
    }
    // GitHub fails on a type only in an annotation it would keep.
    let (Sent::String(path), Sent::String(message)) = (path, message) else {
        return Read::Crashes;
    };
    let title = match item.at("title") {
        Sent::Null => None,
        Sent::String(title) => Some(title.clone()),
        nothing if nothing.is_blank() => Some(nothing.spelled()),
        _ => return Read::Crashes,
    };
    let raw_details = match item.at("raw_details") {
        Sent::Null => None,
        Sent::String(raw) => Some(kept(raw)),
        Sent::Bool(true) => Some("t".to_string()),
        Sent::Bool(false) => Some("f".to_string()),
        other => Some(kept(&other.spelled())),
    };
    Read::Kept(Annotation {
        path: path.clone(),
        start_line,
        end_line,
        start_column,
        end_column,
        level,
        message: kept(message),
        title,
        raw_details,
    })
}

/// An annotation as GitHub answers it.
pub(crate) fn annotation_json(
    face: &GithubFace,
    repo: &Repo,
    run: &CheckRun,
    annotation: &Annotation,
) -> serde_json::Value {
    serde_json::json!({
        "path": annotation.path,
        "blob_href": blob_href(face, repo, run, &annotation.path),
        "start_line": annotation.start_line,
        "start_column": annotation.start_column,
        "end_line": annotation.end_line,
        "end_column": annotation.end_column,
        "annotation_level": annotation.level,
        "title": title_of(annotation),
        "message": annotation.message,
        "raw_details": annotation.raw_details,
    })
}

/// Where GitHub says an annotation's file is: the path at the check run's
/// commit, whether or not the commit holds it.
pub(crate) fn blob_href(face: &GithubFace, repo: &Repo, run: &CheckRun, path: &str) -> String {
    format!(
        "{}/{}/blob/{}/{path}",
        face.layout.web(),
        repo.full_name(),
        run.head_sha
    )
}

/// An annotation's title, which for one given none is its path and its
/// lines.
pub(crate) fn title_of(annotation: &Annotation) -> String {
    let Annotation {
        path,
        start_line,
        end_line,
        title,
        ..
    } = annotation;
    match title {
        Some(title) => title.clone(),
        None if start_line == end_line => format!("{path}#L{start_line}"),
        None => format!("{path}#L{start_line}-L{end_line}"),
    }
}

/// Whether GitHub lists the check run's annotations with the newest first,
/// as it does a job's, and not in the order they were given.
pub(crate) fn lists_newest_first(run: &CheckRun) -> bool {
    run.job.is_some()
}

/// `GET /repos/{o}/{r}/check-runs/{id}/annotations`: a page of the check
/// run's annotations, which is empty for a check run with none.
pub(crate) async fn list_annotations(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match check_run_in(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckAnnotation") {
        return refused;
    }
    annotations_page(&face, &repo, &run, query)
}

/// `GET /repositories/{id}/check-runs/{check_run_id}/annotations`: the same
/// list, at the path its own `Link` header names.
pub(crate) async fn list_annotations_by_id(
    State(face): State<GithubFace>,
    UrlPath((repo_id, id)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match crate::actions::repo_by_id(&face, &repo_id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match check_run_in(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckAnnotation") {
        return refused;
    }
    annotations_page(&face, &repo, &run, query)
}

/// The repository's check run of this id, or GitHub's `404` for an id that
/// is not one of the repository's.
fn check_run_in(face: &GithubFace, repo: &Repo, id: &str) -> Result<CheckRun, Box<Response<Body>>> {
    let not_found = || Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"));
    let run_id = id.parse::<i64>().map_err(|_| not_found())?;
    match face.world.check_run_by_id(run_id) {
        Ok(Some(run)) if run.repo_id == repo.id => Ok(run),
        Ok(_) => Err(not_found()),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn annotations_page(
    face: &GithubFace,
    repo: &Repo,
    run: &CheckRun,
    query: Option<String>,
) -> Response<Body> {
    let query = query.unwrap_or_default();
    let (per_page, page) = crate::reads::page_up_to_100(&query);
    let annotations = match face.world.page_check_run_annotations(
        run.id,
        lists_newest_first(run),
        per_page,
        (page - 1).saturating_mul(per_page),
    ) {
        Ok(annotations) => annotations,
        Err(e) => return internal(e),
    };
    let items: Vec<serde_json::Value> = annotations
        .iter()
        .map(|annotation| annotation_json(face, repo, run, annotation))
        .collect();
    let resp = json_response(StatusCode::OK, serde_json::Value::Array(items));
    let path = format!("check-runs/{}/annotations", run.id);
    crate::actions::page_links(
        resp,
        face,
        repo,
        &path,
        &query,
        run.annotations_count as usize,
        (per_page, page),
        true,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_is_read_in_the_order_it_was_sent() {
        let body: CheckRunBody = serde_json::from_str(
            r#"{"name": "x", "output": {"summary": "S", "annotations": {"path": "x", "end_line": 1}}, "more": 1}"#,
        )
        .unwrap();
        assert_eq!(
            body.output.unwrap().at("annotations").spelled(),
            r#"{"path" => "x", "end_line" => 1}"#
        );
    }
}
