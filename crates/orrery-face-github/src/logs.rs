//! A workflow run's logs as GitHub serves them: each route redirects to a
//! link that answers for a while to whoever holds it, and the link answers
//! the log. The links are on the world's web host, under [`STORE_PATH`].

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Method, Response, StatusCode, header};
use axum::routing::get;
use orrery_world::{
    Caller, CheckRun, CheckStatus, Door, LogLine, Step, StepConclusion, StepStatus, WorkflowRun,
    WorldError,
};

use crate::actions::{attempt_at, job_at, not_found, run_at};
use crate::reads::query_param as param;
use crate::render::rfc3339;
use crate::{GithubFace, github_error, internal, media, resolve_repo};

/// Where the logs a link names are, on the web host. No login starts with
/// `_`, so the path is under no account's.
pub const STORE_PATH: &str = "/_actions-results";

/// How long a link to a job's or a step's log answers, which is what
/// GitHub's was seen to.
const TEXT_LINK_SECONDS: i64 = 600;
/// How long a link to an archive answers: the minute GitHub documents.
const ARCHIVE_LINK_SECONDS: i64 = 60;

/// GitHub's redirect to a link that answers `what` for `seconds`: `302`
/// with an empty HTML body, or `303` to a `HEAD`.
fn redirect(face: &GithubFace, method: &Method, what: &str, seconds: i64) -> Response<Body> {
    let link = || -> Result<String, WorldError> {
        let expires = face.world.clock()? + seconds;
        let signature = face.world.log_link_signature(what, expires)?;
        Ok(format!(
            "{}{STORE_PATH}/{what}?se={expires}&sig={signature}",
            face.layout.web()
        ))
    };
    let status = match *method == Method::HEAD {
        true => StatusCode::SEE_OTHER,
        false => StatusCode::FOUND,
    };
    match link() {
        Ok(link) => Response::builder()
            .status(status)
            .header(header::LOCATION, link)
            .header(header::CONTENT_TYPE, "text/html;charset=utf-8")
            .body(Body::empty())
            .expect("static response parts are valid"),
        Err(e) => internal(e),
    }
}

/// Whether the logs of the attempt `attempt` of `run` were deleted.
fn deleted(run: &WorkflowRun, attempt: i64) -> bool {
    run.logs_deleted_attempt
        .is_some_and(|through| attempt <= through)
}

/// The attempt a job is of.
fn attempt_of(job: &CheckRun) -> i64 {
    job.job.as_ref().map_or(0, |held| held.attempt)
}

/// `GET …/actions/jobs/{id}/logs`: a redirect for any job, whether or not
/// it has a log yet, unless its run's logs were deleted.
pub(crate) async fn job_log(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let (job, run) = match job_at(&face, &repo, &id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    if deleted(&run, attempt_of(&job)) {
        return not_found();
    }
    let what = format!("jobs/{}/job-logs.txt", job.id);
    redirect(&face, &method, &what, TEXT_LINK_SECONDS)
}

/// The step of this number that has a log: one that finished and was not
/// skipped.
fn logged_step(job: &CheckRun, n: i64) -> Option<&Step> {
    let steps = &job.job.as_ref()?.steps;
    let step = steps.get(usize::try_from(n.checked_sub(1)?).ok()?)?;
    (step.status == StepStatus::Completed && step.conclusion != Some(StepConclusion::Skipped))
        .then_some(step)
}

/// `GET …/actions/jobs/{id}/steps/{n}/logs`: a redirect for a step that
/// finished, as soon as it has, and `404` for any other.
pub(crate) async fn step_log(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id, n)): UrlPath<(String, String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let (job, _) = match job_at(&face, &repo, &id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    let Some(n) = n
        .parse::<i64>()
        .ok()
        .filter(|n| logged_step(&job, *n).is_some())
    else {
        return not_found();
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowJob") {
        return refused;
    }
    let what = format!("jobs/{}/steps/{n}.txt", job.id);
    redirect(&face, &method, &what, TEXT_LINK_SECONDS)
}

/// The redirect to an attempt's archive, or GitHub's `404` for one whose
/// logs were deleted and for a first attempt that has not finished. A later
/// attempt has an archive from its start.
fn archive(face: &GithubFace, method: &Method, run: &WorkflowRun) -> Response<Body> {
    if deleted(run, run.attempt) || (run.attempt == 1 && run.status != CheckStatus::Completed) {
        return not_found();
    }
    let what = format!("runs/{}/attempts/{}/logs.zip", run.id, run.attempt);
    redirect(face, method, &what, ARCHIVE_LINK_SECONDS)
}

/// `GET …/actions/runs/{id}/logs`: the archive of the run's latest attempt.
pub(crate) async fn run_log(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    archive(&face, &method, &run)
}

/// `GET …/actions/runs/{id}/attempts/{n}/logs`: the archive of one attempt.
pub(crate) async fn attempt_log(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id, n)): UrlPath<(String, String, String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match attempt_at(&face, &repo, &id, &n) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    archive(&face, &method, &run)
}

/// `DELETE …/actions/runs/{id}/logs`: the logs of a finished run are
/// deleted, and deleting them again is `204` too.
pub(crate) async fn delete_logs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    match face.world.delete_workflow_run_logs(run.id, by) {
        Ok(()) => crate::no_content(),
        Err(WorldError::WorkflowRunInProgress(_)) => github_error(
            StatusCode::FORBIDDEN,
            "Unable to delete logs while the workflow is running",
        ),
        Err(WorldError::NoSuchWorkflowRun(_)) => not_found(),
        Err(e) => internal(e),
    }
}

/// `DELETE …/actions/runs/{id}`: a finished run is deleted, with its jobs,
/// their check runs, and its suite.
pub(crate) async fn delete_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let run = match run_at(&face, &repo, &id) {
        Ok(run) => run,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "WorkflowRun") {
        return refused;
    }
    match face.world.delete_workflow_run(run.id, by) {
        Ok(()) => crate::no_content(),
        Err(WorldError::WorkflowRunInProgress(_)) => {
            github_error(StatusCode::FORBIDDEN, "Could not delete the workflow run")
        }
        Err(WorldError::NoSuchWorkflowRun(_)) => not_found(),
        Err(e) => internal(e),
    }
}

// ---- the store the links name ----

/// The routes the links name. A link carries its own authority, so these
/// take no token and sit outside the face's authentication.
pub fn store_router(face: GithubFace) -> Router {
    Router::new()
        .route(
            &format!("{STORE_PATH}/jobs/{{job}}/job-logs.txt"),
            get(stored_job_log),
        )
        .route(
            &format!("{STORE_PATH}/jobs/{{job}}/steps/{{file}}"),
            get(stored_step_log),
        )
        .route(
            &format!("{STORE_PATH}/runs/{{run}}/attempts/{{attempt}}/logs.zip"),
            get(stored_archive),
        )
        .with_state(face)
}

/// The store's answer in the shape of the blob store GitHub's links name:
/// an XML error under a byte-order mark, with its code in a header.
fn store_error(face: &GithubFace, status: StatusCode, code: &str, message: &str) -> Response<Body> {
    let time = face.world.clock().map(stamp_at_zero).unwrap_or_default();
    let body = format!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{code}</Code>\
         <Message>{message}\nRequestId:00000000-0000-0000-0000-000000000000\n\
         Time:{time}</Message></Error>"
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .header("x-ms-error-code", code)
        .body(Body::from(body))
        .expect("static response parts are valid")
}

fn blob_not_found(face: &GithubFace) -> Response<Body> {
    store_error(
        face,
        StatusCode::NOT_FOUND,
        "BlobNotFound",
        "The specified blob does not exist.",
    )
}

/// Whether the link's signature is the world's for `what`, and it has not
/// expired.
fn link_holds(face: &GithubFace, what: &str, query: &str) -> Result<bool, WorldError> {
    let (Some(expires), Some(signature)) = (
        param(query, "se").and_then(|se| se.parse::<i64>().ok()),
        param(query, "sig"),
    ) else {
        return Ok(false);
    };
    Ok(
        signature == face.world.log_link_signature(what, expires)?
            && face.world.clock()? <= expires,
    )
}

/// The refusal of a link to text that does not hold.
fn refuse_text_link(face: &GithubFace, what: &str, query: &str) -> Option<Response<Body>> {
    match link_holds(face, what, query) {
        Ok(true) => None,
        Ok(false) => Some(store_error(
            face,
            StatusCode::FORBIDDEN,
            "AuthenticationFailed",
            "Server failed to authenticate the request. Make sure the value of Authorization \
             header is formed correctly including the signature.",
        )),
        Err(e) => Some(internal(e)),
    }
}

/// A time as a log line's stamp: to seven places, which here count the
/// lines of the job's log, so each line's stamp is its own.
fn stamp(unix: i64, place: usize) -> String {
    let whole = rfc3339(unix);
    format!("{}.{:07}Z", whole.trim_end_matches('Z'), place % 10_000_000)
}

fn stamp_at_zero(unix: i64) -> String {
    stamp(unix, 0)
}

/// Whether a job has a log: it finished with every step it listed finished.
/// One that never ran lists none, and one cancelled by force is left with a
/// step unfinished.
fn has_log(job: &CheckRun) -> bool {
    let Some(held) = &job.job else {
        return false;
    };
    job.status == CheckStatus::Completed
        && !held.steps.is_empty()
        && held
            .steps
            .iter()
            .all(|step| step.status == StepStatus::Completed)
}

/// Log lines as a file: under a byte-order mark, each line after its stamp.
/// `lines` pairs each line with its place in the job's whole log.
fn text_file<'a>(lines: impl Iterator<Item = (usize, &'a LogLine)>) -> Vec<u8> {
    let mut out = String::from("\u{feff}");
    for (place, line) in lines {
        out.push_str(&stamp(line.clock_unix, place + 1));
        out.push(' ');
        out.push_str(&line.text);
        out.push('\n');
    }
    out.into_bytes()
}

fn text(body: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// The job a store path names with its run, when it is a job.
fn stored_job(face: &GithubFace, id: &str) -> Result<Option<(CheckRun, WorkflowRun)>, WorldError> {
    let Ok(id) = id.parse::<i64>() else {
        return Ok(None);
    };
    let Some(job) = face.world.check_run_by_id(id)? else {
        return Ok(None);
    };
    let Some(run_id) = job.job.as_ref().map(|held| held.run_id) else {
        return Ok(None);
    };
    Ok(face.world.workflow_run_by_id(run_id)?.map(|run| (job, run)))
}

async fn stored_job_log(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let what = format!("jobs/{id}/job-logs.txt");
    if let Some(refused) = refuse_text_link(&face, &what, query.as_deref().unwrap_or("")) {
        return refused;
    }
    let read = || -> Result<Option<Vec<u8>>, WorldError> {
        let Some((job, run)) = stored_job(&face, &id)? else {
            return Ok(None);
        };
        if !has_log(&job) || deleted(&run, attempt_of(&job)) {
            return Ok(None);
        }
        let lines = face.world.job_log(job.id)?;
        Ok(Some(text_file(lines.iter().enumerate())))
    };
    match read() {
        Ok(Some(body)) => text(body),
        Ok(None) => blob_not_found(&face),
        Err(e) => internal(e),
    }
}

async fn stored_step_log(
    State(face): State<GithubFace>,
    UrlPath((id, file)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let what = format!("jobs/{id}/steps/{file}");
    if let Some(refused) = refuse_text_link(&face, &what, query.as_deref().unwrap_or("")) {
        return refused;
    }
    let read = || -> Result<Option<Vec<u8>>, WorldError> {
        let Some((job, run)) = stored_job(&face, &id)? else {
            return Ok(None);
        };
        let n = file
            .strip_suffix(".txt")
            .and_then(|n| n.parse::<i64>().ok());
        let Some(n) = n.filter(|n| logged_step(&job, *n).is_some()) else {
            return Ok(None);
        };
        if deleted(&run, attempt_of(&job)) {
            return Ok(None);
        }
        let lines = face.world.job_log(job.id)?;
        let of_step = lines.iter().enumerate().filter(|(_, line)| line.step == n);
        Ok(Some(text_file(of_step)))
    };
    match read() {
        Ok(Some(body)) => text(body),
        Ok(None) => blob_not_found(&face),
        Err(e) => internal(e),
    }
}

/// An archive's files in order, each a path and its body.
type Files = Vec<(String, Vec<u8>)>;

/// A name as the archive spells it in a file's path, without the
/// characters a file name cannot hold.
fn file_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !matches!(
                c,
                '"' | ':' | '<' | '>' | '|' | '*' | '?' | '/' | '\\' | '\r' | '\n'
            )
        })
        .collect()
}

/// What the archive says of how a job came to run: no log line of the
/// job's, so it has no byte-order mark and its own stamps.
fn system_file(
    face: &GithubFace,
    run: &WorkflowRun,
    job: &CheckRun,
) -> Result<Vec<u8>, WorldError> {
    let workflow = face.world.workflow_by_id(run.workflow_id)?;
    let repo = face.world.repo_by_id(run.repo_id)?;
    let labels = workflow
        .as_ref()
        .and_then(|w| w.jobs.iter().find(|spec| spec.name == job.name))
        .map(|spec| spec.labels.join(", "))
        .unwrap_or_default();
    let defined = format!(
        "{}/{}@refs/heads/{}",
        repo.map(|r| r.full_name()).unwrap_or_default(),
        workflow.map(|w| w.path).unwrap_or_default(),
        run.head_branch.as_deref().unwrap_or_default()
    );
    let listed = stamp_at_zero(job.created_clock_unix);
    let taken = stamp_at_zero(job.started_clock_unix.unwrap_or(job.created_clock_unix));
    Ok([
        format!("{listed} Requested labels: {labels}"),
        format!("{listed} Job defined at: {defined}"),
        format!("{listed} Waiting for a runner to pick up this job..."),
        format!(
            "{taken} Job is about to start running on the hosted runner: {}",
            crate::actions::runner_name(job)
        ),
    ]
    .join("\n")
    .into_bytes())
}

/// The files of an attempt's archive: for each job that ran in the attempt
/// and has a log, the log, what the system says of the job, and a file for
/// each step that has one. A job carried over from an earlier attempt is in
/// that attempt's archive. Jobs are numbered newest first, and a job with
/// no log keeps its number.
fn archive_files(face: &GithubFace, run: &WorkflowRun) -> Result<Files, WorldError> {
    let mut jobs = face
        .world
        .list_workflow_jobs_of_attempt(run.id, run.attempt)?;
    jobs.retain(|job| {
        job.job
            .as_ref()
            .is_some_and(|held| held.carried_from.is_none())
    });
    jobs.reverse();
    let mut files = Vec::new();
    for (number, job) in jobs.iter().enumerate().filter(|(_, job)| has_log(job)) {
        let name = file_name(&job.name);
        let lines = face.world.job_log(job.id)?;
        files.push((
            format!("{number}_{name}.txt"),
            text_file(lines.iter().enumerate()),
        ));
        files.push((format!("{name}/system.txt"), system_file(face, run, job)?));
        let steps = job.job.as_ref().map_or(&[][..], |held| &held.steps);
        for (i, step) in steps.iter().enumerate() {
            let n = i as i64 + 1;
            if logged_step(job, n).is_none() {
                continue;
            }
            let of_step = lines.iter().enumerate().filter(|(_, line)| line.step == n);
            files.push((
                format!("{name}/{n}_{}.txt", file_name(&step.name)),
                text_file(of_step),
            ));
        }
    }
    Ok(files)
}

async fn stored_archive(
    State(face): State<GithubFace>,
    UrlPath((id, n)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response<Body> {
    let what = format!("runs/{id}/attempts/{n}/logs.zip");
    let empty = |status: StatusCode| {
        Response::builder()
            .status(status)
            .body(Body::empty())
            .expect("static response parts are valid")
    };
    // GitHub's archive host refuses a link that does not hold with an empty
    // body.
    match link_holds(&face, &what, query.as_deref().unwrap_or("")) {
        Ok(true) => {}
        Ok(false) => return empty(StatusCode::FORBIDDEN),
        Err(e) => return internal(e),
    }
    let read = || -> Result<Option<(WorkflowRun, Files)>, WorldError> {
        let (Ok(id), Ok(n)) = (id.parse::<i64>(), n.parse::<i64>()) else {
            return Ok(None);
        };
        let Some(run) = face.world.workflow_run_at_attempt(id, n)? else {
            return Ok(None);
        };
        if deleted(&run, run.attempt) {
            return Ok(None);
        }
        let files = archive_files(&face, &run)?;
        Ok(Some((run, files)))
    };
    match read() {
        Ok(Some((run, files))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/zip")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=logs_{}.zip", run.suite_id),
            )
            .body(Body::from(zip(&files)))
            .expect("static response parts are valid"),
        Ok(None) => empty(StatusCode::NOT_FOUND),
        Err(e) => internal(e),
    }
}

/// A zip of `files`, each stored as it is. Every entry is dated 1 January
/// 1980, the earliest date the format holds, as GitHub's entries are.
fn zip(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    // Version 2.0, names in UTF-8, no compression, midnight, 1980-01-01.
    const VERSION: u16 = 20;
    const UTF8_NAMES: u16 = 0x0800;
    const STORED: u16 = 0;
    const TIME: u16 = 0;
    const DATE: u16 = 0x0021;
    let mut out = Vec::new();
    let mut directory = Vec::new();
    let u16le = |n: u16| n.to_le_bytes();
    let u32le = |n: usize| (n as u32).to_le_bytes();
    for (name, body) in files {
        let offset = out.len();
        let crc = crc32(body).to_le_bytes();
        let mut common = Vec::new();
        common.extend(u16le(VERSION));
        common.extend(u16le(UTF8_NAMES));
        common.extend(u16le(STORED));
        common.extend(u16le(TIME));
        common.extend(u16le(DATE));
        common.extend(crc);
        common.extend(u32le(body.len()));
        common.extend(u32le(body.len()));
        common.extend(u16le(name.len() as u16));
        // No extra field.
        common.extend(u16le(0));
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(&common);
        out.extend(name.as_bytes());
        out.extend(body);
        directory.extend(0x0201_4b50u32.to_le_bytes());
        directory.extend(u16le(VERSION));
        directory.extend(&common);
        // No comment, disk 0, no attributes.
        directory.extend([0u8; 10]);
        directory.extend(u32le(offset));
        directory.extend(name.as_bytes());
    }
    let start = out.len();
    out.extend(&directory);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(u16le(files.len() as u16));
    out.extend(u16le(files.len() as u16));
    out.extend(u32le(directory.len()));
    out.extend(u32le(start));
    out.extend(u16le(0));
    out
}

/// The CRC-32 a zip records for each file.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_check_value_of_the_standard() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn an_empty_zip_is_its_end_record_alone() {
        assert_eq!(zip(&[]).len(), 22);
    }
}
