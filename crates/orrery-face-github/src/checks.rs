//! Check-run endpoints — the fabricated-CI door. On real GitHub only a
//! GitHub App can create check runs; Orrery's mapping of "App" is a `bot`
//! actor (spec/usage.md: a ci-bot is a synthetic actor like any other), so a
//! non-bot caller is refused with an Orrery-explained 403 rather than
//! GitHub's opaque "not accessible by integration" — a documented divergence
//! that teaches the caller which door to use.

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_world::{
    Actor, ActorKind, Annotation, Caller, CheckConclusion, CheckRun, CheckRunPatch, CheckStatus,
    Door, NewCheckRun, Repo,
};

use orrery_face_git::store;

use crate::render::check_run_json;
use crate::schema::{Kind, Schema, instant};
use crate::{
    GithubFace, Sent, github_error, internal, json_response, media, orrery_unsupported_field,
    resolve_repo,
};

/// What GitHub checks of an annotation's shape. It asks nothing of a
/// field's type: a wrong one is kept, left out, or fails, as [`annotation`]
/// has it.
const ANNOTATION: Schema = Schema {
    kind: Some(Kind::Object),
    required: &[
        "annotation_level",
        "end_line",
        "message",
        "path",
        "start_line",
    ],
    ..Schema::ANY
};

/// What GitHub checks of an image, which it then keeps nowhere a read
/// shows.
const IMAGE: Schema = Schema {
    kind: Some(Kind::Object),
    required: &["alt", "image_url"],
    ..Schema::ANY
};

const OUTPUT: Schema = Schema {
    kind: Some(Kind::ObjectOrNull),
    properties: &[
        ("title", Schema::STRING),
        (
            "summary",
            Schema {
                max_length: Some(MAX_TEXT),
                ..Schema::STRING
            },
        ),
        (
            "text",
            Schema {
                max_length: Some(MAX_TEXT),
                ..Schema::STRING
            },
        ),
        (
            "images",
            Schema {
                kind: Some(Kind::Array),
                items: Some(&IMAGE),
                ..Schema::ANY
            },
        ),
        (
            "annotations",
            Schema {
                kind: Some(Kind::Array),
                items: Some(&ANNOTATION),
                max_items: Some(MAX_ANNOTATIONS),
                ..Schema::ANY
            },
        ),
    ],
    required: &["summary", "title"],
    ..Schema::ANY
};

const STATUS: Schema = Schema {
    members: &["queued", "in_progress", "completed"],
    ..Schema::STRING
};

/// The conclusions an App may give. `stale` is GitHub's own to give.
const CONCLUSION: Schema = Schema {
    members: &[
        "success",
        "failure",
        "neutral",
        "cancelled",
        "timed_out",
        "action_required",
        "skipped",
    ],
    ..Schema::STRING
};

const TIME: Schema = Schema {
    date_time: true,
    ..Schema::STRING
};

/// A body passes one of these: `completed` with a conclusion, or a status
/// that is not `completed`. One with no status passes the second.
const STATUS_AND_CONCLUSION: &[Schema] = &[
    Schema {
        properties: &[(
            "status",
            Schema {
                members: &["completed"],
                ..Schema::ANY
            },
        )],
        required: &["conclusion"],
        ..Schema::ANY
    },
    Schema {
        properties: &[(
            "status",
            Schema {
                members: &["queued", "in_progress"],
                ..Schema::ANY
            },
        )],
        ..Schema::ANY
    },
];

/// What GitHub checks of a new check run's body, its members in the order
/// GitHub names their faults.
const NEW_CHECK_RUN: Schema = Schema {
    any_of: STATUS_AND_CONCLUSION,
    kind: Some(Kind::Object),
    properties: &[
        ("head_sha", Schema::STRING),
        ("name", Schema::STRING),
        ("details_url", Schema::STRING),
        ("external_id", Schema::STRING),
        ("status", STATUS),
        ("conclusion", CONCLUSION),
        ("started_at", TIME),
        ("completed_at", TIME),
        ("output", OUTPUT),
    ],
    required: &["head_sha", "name"],
    ..Schema::ANY
};

/// What GitHub checks of an update's body. It has no `head_sha`, which an
/// update ignores whatever it is, and a body that holds nothing passes.
const CHECK_RUN_UPDATE: Schema = Schema {
    any_of: STATUS_AND_CONCLUSION,
    kind: Some(Kind::ObjectOrNull),
    properties: &[
        ("name", Schema::STRING),
        ("details_url", Schema::STRING),
        ("external_id", Schema::STRING),
        ("status", STATUS),
        ("conclusion", CONCLUSION),
        ("started_at", TIME),
        ("completed_at", TIME),
        ("output", OUTPUT),
    ],
    ..Schema::ANY
};

/// The longest name GitHub keeps, in bytes.
const MAX_NAME_BYTES: usize = 1024;

/// The longest `external_id` GitHub keeps, in characters.
const MAX_EXTERNAL_ID: usize = 255;

/// The fields of a body that passed its schema, apart from its times and
/// its output.
struct Fields<'a> {
    name: Option<&'a str>,
    details_url: Option<&'a str>,
    external_id: Option<&'a str>,
    status: Option<CheckStatus>,
    conclusion: Option<CheckConclusion>,
}

impl<'a> Fields<'a> {
    fn read(sent: &'a Sent) -> Fields<'a> {
        Fields {
            name: sent.at("name").as_str(),
            details_url: sent.at("details_url").as_str(),
            external_id: sent.at("external_id").as_str(),
            status: sent.at("status").as_str().and_then(|status| match status {
                "queued" => Some(CheckStatus::Queued),
                "in_progress" => Some(CheckStatus::InProgress),
                "completed" => Some(CheckStatus::Completed),
                _ => None,
            }),
            conclusion: sent
                .at("conclusion")
                .as_str()
                .and_then(|conclusion| match conclusion {
                    "success" => Some(CheckConclusion::Success),
                    "failure" => Some(CheckConclusion::Failure),
                    "neutral" => Some(CheckConclusion::Neutral),
                    "cancelled" => Some(CheckConclusion::Cancelled),
                    "timed_out" => Some(CheckConclusion::TimedOut),
                    "action_required" => Some(CheckConclusion::ActionRequired),
                    "skipped" => Some(CheckConclusion::Skipped),
                    _ => None,
                }),
        }
    }

    /// What GitHub's own checks refuse of these fields, in its order.
    fn faults(&self) -> Vec<serde_json::Value> {
        let mut faults = Vec::new();
        if self
            .external_id
            .is_some_and(|id| id.chars().any(|c| c.len_utf8() == 4))
        {
            faults.push(fault(
                "custom",
                "external_id",
                Some("external_id doesn't accept 4-byte Unicode"),
            ));
        }
        if let Some(message) = self.details_url.and_then(url_fault) {
            faults.push(fault("custom", "details_url", Some(message)));
        }
        faults
    }

    /// The `external_id` GitHub keeps: its first 255 characters.
    fn kept_external_id(&self) -> Option<String> {
        self.external_id
            .map(|id| id.chars().take(MAX_EXTERNAL_ID).collect())
    }
}

/// One entry of a "Validation Failed" answer's `errors`.
fn fault(code: &str, field: &str, message: Option<&str>) -> serde_json::Value {
    let mut fault = serde_json::json!({ "resource": "CheckRun", "code": code, "field": field });
    if let Some(message) = message {
        fault["message"] = message.into();
    }
    fault
}

/// GitHub's `422` for what its own checks refuse, all of it at once.
fn validation_failed(faults: Vec<serde_json::Value>) -> Response<Body> {
    json_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        serde_json::json!({ "message": "Validation Failed", "errors": faults }),
    )
}

/// The name GitHub keeps: no more than 1,024 bytes of it, whole characters
/// only. A new check run's name loses the spaces around it too, unless
/// that would leave nothing.
fn kept_name(name: &str, new: bool) -> String {
    let mut end = name.len().min(MAX_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &name[..end];
    let trimmed = cut.trim_matches(['\t', '\n', '\x0b', '\x0c', '\r', ' ', '\0']);
    match new && !trimmed.is_empty() {
        true => trimmed.to_string(),
        false => cut.to_string(),
    }
}

/// What GitHub says of a `details_url` it refuses: one that is no URL at
/// all, or one whose scheme is neither `http` nor `https`. It asks nothing
/// more, so `http://` alone passes.
fn url_fault(url: &str) -> Option<&'static str> {
    let Some(scheme) = url_scheme(url) else {
        return Some("details_url is not a valid URL");
    };
    match scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        true => None,
        false => Some("details_url must use the http or https scheme"),
    }
}

/// The scheme of a URI reference as RFC 3986 writes one, which is empty
/// for a reference that has none, or `None` for text that is no URI
/// reference: one with a character the grammar has no place for, an
/// escape that is not two hex digits, a bracket outside a host, or a port
/// that is not a number.
fn url_scheme(url: &str) -> Option<&str> {
    let unreserved = |c: char| c.is_ascii_alphanumeric() || "-._~".contains(c);
    let sub_delim = |c: char| "!$&'()*+,;=".contains(c);
    // Every `%` begins an escape of two hex digits.
    let bytes = url.as_bytes();
    for (at, byte) in bytes.iter().enumerate() {
        let escapes = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_hexdigit);
        if *byte == b'%' && !(escapes(at + 1) && escapes(at + 2)) {
            return None;
        }
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, fragment),
        None => (url, ""),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((rest, query)) => (rest, query),
        None => (rest, ""),
    };
    let in_path = |c: char| unreserved(c) || sub_delim(c) || ":@/%".contains(c);
    let in_tail = |c: char| in_path(c) || c == '?';
    if !query.chars().all(in_tail) || !fragment.chars().all(in_tail) {
        return None;
    }
    // A scheme is what comes before the first colon, when no slash comes
    // before that.
    let (scheme, rest) = match rest.split_once(':') {
        Some((scheme, after)) if !scheme.contains('/') => {
            let mut letters = scheme.chars();
            let named = letters.next().is_some_and(|c| c.is_ascii_alphabetic())
                && letters.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
            if !named {
                return None;
            }
            (scheme, after)
        }
        _ => ("", rest),
    };
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let (authority, path) = match after.find('/') {
                Some(at) => after.split_at(at),
                None => (after, ""),
            };
            let host = match authority.rsplit_once('@') {
                Some((user, host)) => {
                    if !user
                        .chars()
                        .all(|c| unreserved(c) || sub_delim(c) || ":%".contains(c))
                    {
                        return None;
                    }
                    host
                }
                None => authority,
            };
            let (host, port) = match host.strip_prefix('[') {
                Some(literal) => {
                    let (address, after) = literal.split_once(']')?;
                    if !address
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() || ":.".contains(c))
                    {
                        return None;
                    }
                    (
                        "",
                        after.strip_prefix(':').or(after.is_empty().then_some(""))?,
                    )
                }
                None => match host.split_once(':') {
                    Some((host, port)) => (host, port),
                    None => (host, ""),
                },
            };
            let named = host
                .chars()
                .all(|c| unreserved(c) || sub_delim(c) || c == '%');
            if !named || !port.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            path
        }
        None => rest,
    };
    path.chars().all(in_path).then_some(scheme)
}

/// The commit a new check run's `head_sha` names, as GitHub resolves one:
/// whatever git reads as a commit the repository holds, which is a sha in
/// either case, an abbreviated one, a branch, or a revision such as
/// `main~1`. Of a run of more than 40 hex digits it reads the first 40. The
/// caller holds the gate.
async fn commit_named(
    face: &GithubFace,
    repo: &Repo,
    given: &str,
) -> Result<Option<String>, Box<Response<Body>>> {
    let overlong = given.len() > 40 && given.bytes().all(|b| b.is_ascii_hexdigit());
    let name = match overlong {
        true => &given[..40],
        false => given,
    };
    let _refs = face.git.read_lock(repo.id).await;
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    crate::reads::resolve_commit(face, repo, &bare, name).map_err(|e| Box::new(internal(e)))
}

/// A time a body's member names. `Ok(None)` for a member that was not
/// sent, and `Err` for a time that passed the schema and GitHub still
/// cannot read.
fn time_of(sent: &Sent, key: &str) -> Result<Option<i64>, ()> {
    match sent.at(key).as_str() {
        None => Ok(None),
        Some(text) => instant(text).map(Some).ok_or(()),
    }
}

/// `POST /repos/{o}/{r}/check-runs`. GitHub's checks run in this order,
/// each answering alone: the body's shape, the commit, the times, its own
/// rules for the fields, and last an annotation it fails on.
pub(crate) async fn create_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name)): UrlPath<(String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let actor = caller.actor;
    // The commit is read from the bare repository, which a restore replaces
    // under the gate.
    let _gate = face.git.gate().read_owned().await;
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
    let Ok(sent) = Sent::parse(&body) else {
        return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON");
    };
    if sent.member("actions").is_some() {
        return orrery_unsupported_field("CheckRun", "actions");
    }
    if let Some(refused) = NEW_CHECK_RUN.refuse(&sent, "links/0/schema") {
        return refused;
    }
    let given = sent.at("head_sha").as_str().unwrap_or_default();
    let head_sha = match commit_named(&face, &repo, given).await {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            return github_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("No commit found for SHA: {given}"),
            );
        }
        Err(resp) => return *resp,
    };
    let (Ok(started), Ok(completed)) =
        (time_of(&sent, "started_at"), time_of(&sent, "completed_at"))
    else {
        return crash();
    };
    let fields = Fields::read(&sent);
    let output = Output::read(sent.at("output"));
    let mut faults = Vec::new();
    // A new check run is refused for an annotation an update would leave
    // out.
    if output.left_out {
        faults.push(fault("invalid", "annotations", None));
    }
    if completed.is_some() && fields.conclusion.is_none() {
        faults.push(fault("missing_field", "conclusion", None));
    }
    faults.extend(fields.faults());
    if !faults.is_empty() {
        return validation_failed(faults);
    }
    if output.crashes {
        return crash();
    }
    let clock = match face.world.clock() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    // A conclusion completes the run, whatever status came with it.
    let status = match fields.conclusion {
        Some(_) => CheckStatus::Completed,
        None => fields.status.unwrap_or(CheckStatus::Queued),
    };
    let completed = completed.or((status == CheckStatus::Completed).then_some(clock));
    let name = kept_name(fields.name.unwrap_or_default(), true);
    let external_id = fields.kept_external_id().unwrap_or_default();
    let created = face.world.create_check_run(
        by,
        NewCheckRun {
            repo_id: repo.id,
            head_sha: &head_sha,
            name: &name,
            status,
            conclusion: fields.conclusion,
            details_url: fields.details_url,
            external_id: Some(&external_id),
            output: output.said.as_ref(),
            // GitHub stamps started_at at creation whatever the status.
            started_clock_unix: Some(started.unwrap_or(clock)),
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

/// `PATCH /repos/{o}/{r}/check-runs/{id}`. GitHub checks the body's shape,
/// then fails on an annotation it cannot keep, then applies its own rules
/// for the fields.
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
    let existing = match check_run_in(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
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
    let Ok(sent) = Sent::parse(&body) else {
        return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON");
    };
    if sent.member("actions").is_some() {
        return orrery_unsupported_field("CheckRun", "actions");
    }
    if let Some(refused) = CHECK_RUN_UPDATE.refuse(&sent, "links/1/schema") {
        return refused;
    }
    let output = Output::read(sent.at("output"));
    if output.crashes {
        return crash();
    }
    let fields = Fields::read(&sent);
    let clock = match face.world.clock() {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let was_complete = existing.status == CheckStatus::Completed;
    // On an update, a time GitHub cannot read is a time it does not keep,
    // and the field falls back to what it holds unset: the time of the
    // write for `started_at`, and for `completed_at` of a complete run.
    let started = sent
        .at("started_at")
        .as_str()
        .map(|text| instant(text).unwrap_or(clock));
    let completed_sent = sent.at("completed_at").as_str();
    let completed = completed_sent.and_then(instant);
    let mut faults = Vec::new();
    if completed.is_some() && fields.conclusion.is_none() && !was_complete {
        faults.push(fault("missing_field", "conclusion", None));
    }
    faults.extend(fields.faults());
    if !faults.is_empty() {
        return validation_failed(faults);
    }
    // A conclusion completes the run whatever status came with it, and a
    // complete run stays complete whatever status it is sent, which the
    // write holds to on the run as it finds it.
    let completes = fields.conclusion.is_some() || was_complete;
    let status = match fields.conclusion {
        Some(_) => Some(CheckStatus::Completed),
        None => fields.status,
    };
    let completed = match completed {
        Some(at) => Some(at),
        None if completes && (!was_complete || completed_sent.is_some()) => Some(clock),
        None => None,
    };
    let name = fields.name.map(|name| kept_name(name, false));
    let external_id = fields.kept_external_id();
    let updated = face.world.update_check_run(
        existing.id,
        by,
        CheckRunPatch {
            name: name.as_deref(),
            status,
            keep_completed: true,
            conclusion: fields.conclusion,
            details_url: fields.details_url,
            external_id: external_id.as_deref(),
            output: output.said.as_ref(),
            started_clock_unix: started,
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

/// What a request's `output` holds, once it has passed GitHub's schema.
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
    /// Read an `output` that passed [`OUTPUT`]. A null one, like one that
    /// was not sent, is no output.
    fn read(output: &Sent) -> Output {
        if !matches!(output, Sent::Object(_)) {
            return Output::default();
        }
        let mut read = Output {
            said: Some(serde_json::json!({
                "title": output.at("title").as_str(),
                "summary": output.at("summary").as_str(),
                "text": output.at("text").as_str(),
            })),
            ..Output::default()
        };
        let Sent::Array(items) = output.at("annotations") else {
            return read;
        };
        for item in items {
            match annotation(item) {
                Read::Kept(annotation) => read.annotations.push(annotation),
                Read::LeftOut => read.left_out = true,
                Read::Crashes => read.crashes = true,
            }
        }
        read
    }
}

/// A number GitHub reads from a line or a column: an integer, a number cut
/// to one, the digits a string starts with, which is 0 for a string that
/// starts with none, and 1 or 0 for true or false. `None` for anything
/// else.
fn whole(value: &Sent) -> Option<i64> {
    match value {
        Sent::Integer(_) => value.as_i64(),
        Sent::Float(n) => Some(*n as i64),
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
/// that is something other than a string. A character of four bytes leaves
/// the annotation out when it is in `raw_details`, and fails when it is in
/// the level.
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
    let wide = |text: &str| text.chars().any(|c| c.len_utf8() == 4);
    let raw_details = match item.at("raw_details") {
        Sent::Null => None,
        Sent::String(raw) if wide(raw) => return Read::LeftOut,
        Sent::String(raw) => Some(kept(raw)),
        Sent::Bool(true) => Some("t".to_string()),
        Sent::Bool(false) => Some("f".to_string()),
        other => Some(kept(&other.spelled())),
    };
    // GitHub fails on a type only in an annotation it would keep.
    let (Sent::String(path), Sent::String(message)) = (path, message) else {
        return Read::Crashes;
    };
    if wide(&level) {
        return Read::Crashes;
    }
    let title = match item.at("title") {
        Sent::Null => None,
        Sent::String(title) => Some(title.clone()),
        nothing if nothing.is_blank() => Some(nothing.spelled()),
        _ => return Read::Crashes,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each as github.com answered it as a check run's `details_url` on
    /// 2026-11-21.
    #[test]
    fn a_details_url_is_refused_as_github_refuses_it() {
        const NO_URL: Option<&str> = Some("details_url is not a valid URL");
        const SCHEME: Option<&str> = Some("details_url must use the http or https scheme");
        for (url, said) in [
            ("https://example.com/details", None),
            ("http://", None),
            ("https://", None),
            ("HTTP://EXAMPLE.COM/X", None),
            ("http:/x", None),
            ("https:example.com", None),
            ("https://example.com/x#frag", None),
            ("https://exa_mple.com/", None),
            ("https://user:pw@example.com:8080/x?y=1", None),
            ("", SCHEME),
            ("uuuu", SCHEME),
            ("ftp://example.com/x", SCHEME),
            ("//example.com/x", SCHEME),
            ("mailto:x@example.com", SCHEME),
            ("javascript:alert(1)", SCHEME),
            ("x:y", SCHEME),
            ("not a url", NO_URL),
            ("https://exa mple.com", NO_URL),
            ("https://example.com/\u{fc}", NO_URL),
            ("https://example.com/\u{1f600}", NO_URL),
            ("http://[::1", NO_URL),
            ("https://example.com/<x>", NO_URL),
            ("https://example.com/a b", NO_URL),
            (" https://example.com/x", NO_URL),
            ("https://example.com/x ", NO_URL),
            ("https://example.com/%zz", NO_URL),
            ("https://example.com/a|b", NO_URL),
            ("https://example.com/a\\b", NO_URL),
            ("https://example.com/a\"b", NO_URL),
            ("https://example.com/a{b}", NO_URL),
            ("https://example.com/a^b`c", NO_URL),
            ("https://example.com/a[b]", NO_URL),
            ("https://example.com/a%b", NO_URL),
            ("https://example.com:port/", NO_URL),
        ] {
            assert_eq!(url_fault(url), said, "{url}");
        }
    }

    /// Each as github.com kept it on 2026-11-21.
    #[test]
    fn a_name_is_kept_as_github_keeps_it() {
        assert_eq!(kept_name("  padded  ", true), "padded");
        assert_eq!(kept_name("\tname\n", true), "name");
        assert_eq!(kept_name("\u{a0}name\u{a0}", true), "\u{a0}name\u{a0}");
        assert_eq!(kept_name("   ", true), "   ");
        assert_eq!(kept_name("\t", true), "\t");
        assert_eq!(kept_name("", true), "");
        assert_eq!(kept_name("  again  ", false), "  again  ");
        assert_eq!(kept_name(&"n".repeat(70_000), true).len(), 1024);
        assert_eq!(kept_name(&"\u{e9}".repeat(2000), true).chars().count(), 512);
        assert_eq!(
            kept_name(&"\u{1f600}".repeat(2000), true).chars().count(),
            256
        );
        let mixed = format!("a{}", "\u{e9}".repeat(2000));
        assert_eq!(kept_name(&mixed, true).len(), 1023);
        let spaced = format!("   {}", "n".repeat(1030));
        assert_eq!(kept_name(&spaced, true).len(), 1021);
    }
}
