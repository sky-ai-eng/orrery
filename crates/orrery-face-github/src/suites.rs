//! Check suites, and the reads of check runs by suite and by id.

use axum::body::Body;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_face_git::store;
use orrery_world::{Actor, CheckRun, CheckSuite, PullState, Repo, WorldError};

use crate::reads::{page_params, paginate};
use crate::render::{check_run_json, repo_json, rfc3339};
use crate::{GithubFace, github_error, internal, json_response, media, resolve_repo};

/// The open pull requests whose head is `sha`, as a check run and a check
/// suite list them.
pub(crate) fn pulls_at(
    face: &GithubFace,
    repo: &Repo,
    sha: &str,
) -> Result<serde_json::Value, WorldError> {
    let base = face.base_url.trim_end_matches('/');
    let url = format!("{base}/api/v3/repos/{}", repo.full_name());
    let end = |name: &str, sha: &str| {
        serde_json::json!({
            "ref": name,
            "sha": sha,
            "repo": { "id": repo.id, "url": url, "name": repo.name },
        })
    };
    Ok(face
        .world
        .list_pulls_for_repo(repo.id, Some(PullState::Open))?
        .iter()
        .filter(|pull| pull.head_sha == sha)
        .map(|pull| {
            serde_json::json!({
                "url": format!("{url}/pulls/{}", pull.number),
                "id": pull.id,
                "number": pull.number,
                "head": end(&pull.head_ref, &pull.head_sha),
                "base": end(&pull.base_ref, &pull.base_sha),
            })
        })
        .collect())
}

/// A suite as a check run's webhook payload carries it.
pub(crate) fn suite_brief(
    face: &GithubFace,
    repo: &Repo,
    suite: &CheckSuite,
) -> Result<serde_json::Value, WorldError> {
    let base = face.base_url.trim_end_matches('/');
    // A suite a push made is that push's: what the branch pointed at, and
    // the commit it was brought to.
    let after = suite.before_sha.as_ref().map(|_| suite.head_sha.as_str());
    Ok(serde_json::json!({
        "id": suite.id,
        "node_id": crate::nodeid::check_suite(suite.id),
        "head_branch": suite.head_branch,
        "head_sha": suite.head_sha,
        "status": suite.status.as_str(),
        "conclusion": suite.conclusion.map(|c| c.as_str()),
        "url": format!("{base}/api/v3/repos/{}/check-suites/{}", repo.full_name(), suite.id),
        "before": suite.before_sha,
        "after": after,
        "pull_requests": pulls_at(face, repo, &suite.head_sha)?,
        "app": crate::render::check_run_app(face, &suite.creator_login)?,
        "created_at": rfc3339(suite.created_clock_unix),
        "updated_at": rfc3339(suite.updated_clock_unix),
    }))
}

/// A suite as REST answers it.
pub(crate) fn suite_json(
    face: &GithubFace,
    repo: &Repo,
    owner: &Actor,
    suite: &CheckSuite,
) -> Result<serde_json::Value, WorldError> {
    let base = face.base_url.trim_end_matches('/');
    let latest = newest(face.world.list_check_runs_for_suite(suite.id)?).len();
    let mut json = suite_brief(face, repo, suite)?;
    let more = serde_json::json!({
        "rerequestable": true,
        "runs_rerequestable": true,
        "latest_check_runs_count": latest,
        "check_runs_url": format!(
            "{base}/api/v3/repos/{}/check-suites/{}/check-runs",
            repo.full_name(),
            suite.id
        ),
        "repository": repo_json(face, repo, owner),
    });
    for (key, value) in more.as_object().into_iter().flatten() {
        json[key] = value.clone();
    }
    Ok(json)
}

/// The newest run of each name within its suite, which is what GitHub
/// counts and lists unless asked for every run.
pub(crate) fn newest(runs: Vec<CheckRun>) -> Vec<CheckRun> {
    let keep: Vec<bool> = runs
        .iter()
        .map(|run| {
            !runs.iter().any(|other| {
                other.suite_id == run.suite_id && other.name == run.name && other.id > run.id
            })
        })
        .collect();
    runs.into_iter()
        .zip(keep)
        .filter_map(|(run, keep)| keep.then_some(run))
        .collect()
}

/// A query's value for `name`, percent-decoded.
fn param(query: &str, name: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// The check runs a list answers: GitHub's filters applied to `runs`, which
/// are oldest first. `filter=all` keeps every run, and any other value the
/// newest of each name. `check_name`, `status`, and `app_id` each keep the
/// runs that match, and a value that matches none keeps none.
pub(crate) fn filtered(
    face: &GithubFace,
    runs: Vec<CheckRun>,
    query: &str,
) -> Result<Vec<CheckRun>, WorldError> {
    let mut runs = match param(query, "filter").as_deref() {
        Some("all") => runs,
        _ => newest(runs),
    };
    if let Some(name) = param(query, "check_name") {
        runs.retain(|run| run.name == name);
    }
    if let Some(status) = param(query, "status") {
        runs.retain(|run| run.status.as_str() == status);
    }
    if let Some(app_id) = param(query, "app_id") {
        let mut kept = Vec::new();
        for run in runs {
            let app = face.world.app_by_slug(&run.creator_login)?;
            if app.is_some_and(|app| app.id.to_string() == app_id) {
                kept.push(run);
            }
        }
        runs = kept;
    }
    Ok(runs)
}

/// `{total_count, check_runs}` for one page of `runs`.
pub(crate) fn runs_page(
    face: &GithubFace,
    repo: &Repo,
    runs: Vec<CheckRun>,
    query: &str,
) -> Response<Body> {
    let (per_page, page) = page_params(query);
    let total = runs.len();
    let items: Result<Vec<serde_json::Value>, _> = paginate(runs, per_page, page)
        .iter()
        .map(|run| check_run_json(face, repo, run))
        .collect();
    match items {
        Ok(items) => json_response(
            StatusCode::OK,
            serde_json::json!({ "total_count": total, "check_runs": items }),
        ),
        Err(e) => internal(e),
    }
}

fn not_found() -> Response<Body> {
    github_error(StatusCode::NOT_FOUND, "Not Found")
}

/// The repository's suite with this id, or GitHub's 404 for one that is
/// another repository's, or none.
fn suite_at(face: &GithubFace, repo: &Repo, id: &str) -> Result<CheckSuite, Box<Response<Body>>> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(Box::new(not_found()));
    };
    match face.world.check_suite_by_id(id) {
        Ok(Some(suite)) if suite.repo_id == repo.id => Ok(suite),
        Ok(_) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(internal(e))),
    }
}

pub(crate) async fn get_suite(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckSuite") {
        return refused;
    }
    let suite = match suite_at(&face, &repo, &id) {
        Ok(suite) => suite,
        Err(resp) => return *resp,
    };
    match suite_json(&face, &repo, &owner, &suite) {
        Ok(json) => json_response(StatusCode::OK, json),
        Err(e) => internal(e),
    }
}

/// A suite's check runs. GitHub lists the newest of each name oldest
/// first, and every run newest first.
pub(crate) async fn list_suite_runs(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckRun") {
        return refused;
    }
    let suite = match suite_at(&face, &repo, &id) {
        Ok(suite) => suite,
        Err(resp) => return *resp,
    };
    let query = query.unwrap_or_default();
    let runs = face
        .world
        .list_check_runs_for_suite(suite.id)
        .and_then(|runs| filtered(&face, runs, &query));
    match runs {
        Ok(mut runs) => {
            if param(&query, "filter").as_deref() == Some("all") {
                runs.reverse();
            }
            runs_page(&face, &repo, runs, &query)
        }
        Err(e) => internal(e),
    }
}

/// The suites on a commit, oldest first. `app_id` keeps one app's. With
/// `check_name`, GitHub answers a suite once for each of its check runs of
/// that name, and so does a world.
pub(crate) async fn list_suites_for_ref(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, git_ref)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, owner) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckSuite") {
        return refused;
    }
    let sha = match commit_of(&face, &repo, &git_ref).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return not_found(),
        Err(resp) => return *resp,
    };
    let query = query.unwrap_or_default();
    let listed = || -> Result<Vec<CheckSuite>, WorldError> {
        let mut suites = face.world.list_check_suites_for_sha(repo.id, &sha)?;
        if let Some(app_id) = param(&query, "app_id") {
            let mut kept = Vec::new();
            for suite in suites {
                let app = face.world.app_by_slug(&suite.creator_login)?;
                if app.is_some_and(|app| app.id.to_string() == app_id) {
                    kept.push(suite);
                }
            }
            suites = kept;
        }
        if let Some(name) = param(&query, "check_name") {
            let mut repeated = Vec::new();
            for suite in suites {
                let runs = face.world.list_check_runs_for_suite(suite.id)?;
                let named = runs.iter().filter(|run| run.name == name).count();
                repeated.extend(std::iter::repeat_n(suite, named));
            }
            suites = repeated;
        }
        Ok(suites)
    };
    let suites = match listed() {
        Ok(suites) => suites,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query);
    let total = suites.len();
    let items: Result<Vec<serde_json::Value>, _> = paginate(suites, per_page, page)
        .iter()
        .map(|suite| suite_json(&face, &repo, &owner, suite))
        .collect();
    match items {
        Ok(items) => json_response(
            StatusCode::OK,
            serde_json::json!({ "total_count": total, "check_suites": items }),
        ),
        Err(e) => internal(e),
    }
}

pub(crate) async fn get_check_run(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, id)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CheckRun") {
        return refused;
    }
    let Ok(id) = id.parse::<i64>() else {
        return not_found();
    };
    let run = match face.world.check_run_by_id(id) {
        Ok(Some(run)) if run.repo_id == repo.id => run,
        Ok(_) => return not_found(),
        Err(e) => return internal(e),
    };
    match check_run_json(&face, &repo, &run) {
        Ok(json) => json_response(StatusCode::OK, json),
        Err(e) => internal(e),
    }
}

/// The commit a `{ref}` path segment names: a full sha as it is, or the tip
/// of a branch, or none. The caller holds the gate.
pub(crate) async fn commit_of(
    face: &GithubFace,
    repo: &Repo,
    git_ref: &str,
) -> Result<Option<String>, Box<Response<Body>>> {
    if git_ref.len() == 40 && git_ref.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Some(git_ref.to_ascii_lowercase()));
    }
    let _refs = face.git.read_lock(repo.id).await;
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    store::rev_parse(&bare, &format!("refs/heads/{git_ref}")).map_err(|e| Box::new(internal(e)))
}
