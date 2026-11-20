//! Commit statuses as GitHub serves them: giving a commit a status, every
//! status of a commit, and what they come to.

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_face_git::store;
use orrery_world::{
    Actor, Caller, CommitStatus, Door, NewCommitStatus, Repo, StatusState, WorldError,
    combined_state, newest_of_each_context,
};

use crate::actions::{page, repo_by_id, with_page_links};
use crate::render::{repo_json, rfc3339, user_json};
use crate::{GithubFace, github_error, internal, json_response, media, resolve_repo};

/// The longest description GitHub takes.
const MAX_DESCRIPTION: usize = 140;

/// A status as GitHub's status object, with who gave it when `creator` is
/// set: the list names them, and the combined status does not.
pub(crate) fn status_json(
    face: &GithubFace,
    repo: &Repo,
    status: &CommitStatus,
    creator: bool,
) -> serde_json::Value {
    let api = face.layout.api();
    let created = rfc3339(status.created_clock_unix);
    let mut json = serde_json::json!({
        "url": format!("{api}/repos/{}/statuses/{}", repo.full_name(), status.sha),
        "avatar_url": serde_json::Value::Null,
        "id": status.id,
        "node_id": crate::nodeid::status(status.id),
        "state": status.state.as_str(),
        "description": status.description,
        "target_url": status.target_url,
        "context": status.context,
        "created_at": created,
        "updated_at": created,
    });
    if creator {
        json["creator"] = user_json(face, &status.creator_login);
    }
    json
}

/// GitHub's `422` for a status whose fields it refuses: every fault in one
/// string, where its other validation errors are a list.
fn validation_failed(faults: &[&str]) -> Response<Body> {
    json_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        serde_json::json!({
            "message": "Validation Failed",
            "errors": format!("Validation failed: {}", faults.join(", ")),
        }),
    )
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `POST /repos/{o}/{r}/statuses/{sha}`: the commit gets a status. One of
/// a context it has a status of already is a new status.
pub(crate) async fn create(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, given)): UrlPath<(String, String, String)>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let by = caller.via(Door::Rest);
    // The commit is read from the bare repository, which a restore replaces
    // under the gate.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CommitStatus") {
        return refused;
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    // A field of the wrong type is refused by name, before anything is
    // looked for. `null` is a field left out.
    let mut text: [Option<&str>; 4] = [None; 4];
    for (at, field) in ["context", "state", "description", "target_url"]
        .iter()
        .enumerate()
    {
        match parsed.get(*field) {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::String(given)) => text[at] = Some(given),
            Some(_) => {
                return github_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &format!("Invalid value for '{field}'. Must be a string."),
                );
            }
        }
    }
    let [context, state, description, target_url] = text;
    // The commit is looked for by whatever names one: a sha in either
    // case, or a branch. Only a full sha in lower case is then valid.
    let sha = match crate::suites::commit_of(&face, &repo, &given).await {
        Ok(sha) => sha,
        Err(resp) => return *resp,
    };
    let held = match &sha {
        Some(sha) => {
            let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
            match store::rev_parse(&bare, &format!("{sha}^{{commit}}")) {
                Ok(held) => held,
                Err(e) => return internal(e),
            }
        }
        None => None,
    };
    let Some(sha) = held else {
        return github_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("No commit found for SHA: {given}"),
        );
    };
    let context = context.unwrap_or("default");
    let state = state.and_then(StatusState::parse);
    let mut faults = Vec::new();
    if given != sha {
        faults.push("Sha must be a valid hex object ID");
    }
    if context.is_empty() {
        faults.push("Context can't be blank");
    }
    if state.is_none() {
        faults.push("State is not included in the list");
    }
    if description.is_some_and(|d| d.chars().count() > MAX_DESCRIPTION) {
        faults.push("Description is too long (maximum is 140 characters)");
    }
    if target_url.is_some_and(|url| !url.starts_with("http://") && !url.starts_with("https://")) {
        faults.push("Target url must use http(s) scheme");
    }
    let Some(state) = state.filter(|_| faults.is_empty()) else {
        return validation_failed(&faults);
    };
    let new = NewCommitStatus {
        repo_id: repo.id,
        sha: &sha,
        context,
        state,
        description,
        target_url,
    };
    match face.world.create_commit_status(by, new) {
        Ok(status) => json_response(
            StatusCode::CREATED,
            status_json(&face, &repo, &status, true),
        ),
        Err(WorldError::TooManyStatuses { .. }) => {
            validation_failed(&["This SHA and context has reached the maximum number of statuses."])
        }
        Err(e) => internal(e),
    }
}

/// The commit a read's `{ref}` names: a full sha, whether or not the
/// repository holds it, or a branch's tip. GitHub answers for a sha no
/// commit has as for a commit with no statuses.
async fn read_sha(
    face: &GithubFace,
    repo: &Repo,
    git_ref: &str,
) -> Result<Option<String>, Box<Response<Body>>> {
    if is_sha(git_ref) {
        return Ok(Some(git_ref.to_ascii_lowercase()));
    }
    crate::suites::commit_of(face, repo, git_ref).await
}

/// Every status of the commit, newest first, as an array with page links.
fn list_page(face: &GithubFace, repo: &Repo, sha: &str, query: &str) -> Response<Body> {
    let statuses = match face.world.list_commit_statuses(repo.id, sha) {
        Ok(statuses) => statuses,
        Err(e) => return internal(e),
    };
    let total = statuses.len();
    let items: Vec<serde_json::Value> = page(statuses, query)
        .iter()
        .map(|status| status_json(face, repo, status, true))
        .collect();
    let resp = json_response(StatusCode::OK, serde_json::Value::Array(items));
    with_page_links(resp, face, repo, &format!("statuses/{sha}"), query, total)
}

/// `GET /repos/{o}/{r}/commits/{ref}/statuses`, and the same list at
/// `GET /repos/{o}/{r}/statuses/{ref}`.
pub(crate) async fn list_for_ref(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, git_ref)): UrlPath<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let sha = match read_sha(&face, &repo, &git_ref).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CommitStatus") {
        return refused;
    }
    list_page(&face, &repo, &sha, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/statuses/{sha}`: the same list, at the path its
/// own `Link` header names.
pub(crate) async fn list_by_id(
    State(face): State<GithubFace>,
    UrlPath((repo_id, sha)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, _) = match repo_by_id(&face, &repo_id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if !is_sha(&sha) {
        return github_error(StatusCode::NOT_FOUND, "Not Found");
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "CommitStatus") {
        return refused;
    }
    list_page(
        &face,
        &repo,
        &sha.to_ascii_lowercase(),
        query.as_deref().unwrap_or(""),
    )
}

/// What the commit's statuses come to: the newest of each context, a page
/// of them, and the state they make together.
fn combined_page(
    face: &GithubFace,
    repo: &Repo,
    owner: &Actor,
    sha: &str,
    query: &str,
) -> Response<Body> {
    let statuses = match face.world.list_commit_statuses(repo.id, sha) {
        Ok(statuses) => statuses,
        Err(e) => return internal(e),
    };
    let newest = newest_of_each_context(&statuses);
    let total = newest.len();
    let state = combined_state(newest.iter().copied());
    let items: Vec<serde_json::Value> = page(newest, query)
        .into_iter()
        .map(|status| status_json(face, repo, status, false))
        .collect();
    let commit = format!(
        "{}/repos/{}/commits/{sha}",
        face.layout.api(),
        repo.full_name()
    );
    let resp = json_response(
        StatusCode::OK,
        serde_json::json!({
            "state": state.as_str(),
            "statuses": items,
            "sha": sha,
            "total_count": total,
            "repository": repo_json(face, repo, owner),
            "commit_url": commit,
            "url": format!("{commit}/status"),
        }),
    );
    with_page_links(resp, face, repo, &format!("status/{sha}"), query, total)
}

/// `GET /repos/{o}/{r}/commits/{ref}/status`: the combined status.
pub(crate) async fn combined_for_ref(
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
    let sha = match read_sha(&face, &repo, &git_ref).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return github_error(StatusCode::NOT_FOUND, "Ref not found"),
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "CommitStatus") {
        return refused;
    }
    combined_page(&face, &repo, &owner, &sha, query.as_deref().unwrap_or(""))
}

/// `GET /repositories/{id}/status/{sha}`: the combined status, at the path
/// its own `Link` header names.
pub(crate) async fn combined_by_id(
    State(face): State<GithubFace>,
    UrlPath((repo_id, sha)): UrlPath<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (repo, owner) = match repo_by_id(&face, &repo_id) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    if !is_sha(&sha) {
        return github_error(StatusCode::NOT_FOUND, "Ref not found");
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "CommitStatus") {
        return refused;
    }
    let sha = sha.to_ascii_lowercase();
    combined_page(&face, &repo, &owner, &sha, query.as_deref().unwrap_or(""))
}
