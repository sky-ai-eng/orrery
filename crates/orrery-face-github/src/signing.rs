//! Commit signing on the REST face: an account's SSH signing keys, and a
//! commit with the verdict on its signature (spec/surface-github.md §1.9).

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as UrlPath, RawQuery, State};
use axum::http::{HeaderMap, Response, StatusCode};
use orrery_face_git::signature::{self, Commit};
use orrery_face_git::store;
use orrery_world::{Actor, ActorKind, Caller, Door, Repo, SigningKey, Verdict, WorldError};

use crate::reads::{page_params, paginate, refuse_integration, resolve_commit};
use crate::render::rfc3339;
use crate::{
    GithubFace, github_error, internal, json_response, media, resolve_repo, validation_missing,
};

const KEY_RESOURCE: &str = "SshSigningKey";

fn key_json(key: &SigningKey) -> serde_json::Value {
    serde_json::json!({
        "id": key.id,
        "key": key.key,
        "title": key.title,
        "created_at": rfc3339(key.created_clock_unix),
    })
}

fn keys_page(face: &GithubFace, owner: &Actor, query: Option<String>) -> Response<Body> {
    let keys = match face.world.signing_keys(owner.id) {
        Ok(keys) => keys,
        Err(e) => return internal(e),
    };
    let (per_page, page) = page_params(&query.unwrap_or_default());
    let items: Vec<serde_json::Value> = paginate(keys, per_page, page)
        .iter()
        .map(key_json)
        .collect();
    json_response(StatusCode::OK, serde_json::Value::Array(items))
}

/// `GET /users/{username}/ssh_signing_keys`: the account's signing keys,
/// oldest first, paged. The account is found as `GET /users/{username}`
/// finds it, so a login nobody holds is GitHub's 404.
pub(crate) async fn list_user_keys(
    State(face): State<GithubFace>,
    UrlPath(username): UrlPath<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    let (login, bot) = match username.strip_suffix("[bot]") {
        Some(slug) => (slug, true),
        None => (username.as_str(), false),
    };
    let account = match face.world.actor_by_login(login) {
        Ok(Some(a)) if (a.kind == ActorKind::Bot) == bot && a.kind != ActorKind::System => a,
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "SshSigningKey") {
        return refused;
    }
    keys_page(&face, &account, query)
}

/// `GET /user/ssh_signing_keys`: the caller's signing keys, as the public
/// listing answers them.
pub(crate) async fn list_own_keys(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "SshSigningKey") {
        return refused;
    }
    keys_page(&face, &actor, query)
}

#[derive(serde::Deserialize, Default)]
struct CreateKeyBody {
    title: Option<String>,
    key: Option<String>,
}

/// GitHub's 422 for a `key` it refuses, with its prose.
fn key_refused(message: &str) -> Response<Body> {
    json_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "resource": KEY_RESOURCE, "code": "custom", "field": "key",
                         "message": message }],
        }),
    )
}

/// `POST /user/ssh_signing_keys`: `{title?, key}`, 201 with the key.
pub(crate) async fn create_key(
    State(face): State<GithubFace>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&caller.actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "SshSigningKey") {
        return refused;
    }
    let parsed: CreateKeyBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    let Some(key) = parsed.key.as_deref().filter(|k| !k.trim().is_empty()) else {
        return validation_missing(KEY_RESOURCE, "key");
    };
    match face
        .world
        .add_signing_key(caller.via(Door::Rest), key, parsed.title.as_deref())
    {
        Ok(key) => json_response(StatusCode::CREATED, key_json(&key)),
        Err(WorldError::InvalidSigningKey(_)) => {
            key_refused("key is invalid. You must supply a key in OpenSSH public key format")
        }
        Err(WorldError::SigningKeyExists) => key_refused("key is already in use"),
        Err(e) => internal(e),
    }
}

/// `DELETE /user/ssh_signing_keys/{id}`: 204. A key the caller does not
/// hold is GitHub's 404, whoever holds it.
pub(crate) async fn delete_key(
    State(face): State<GithubFace>,
    UrlPath(id): UrlPath<String>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response<Body> {
    if let Some(refused) = refuse_integration(&caller.actor) {
        return refused;
    }
    let Ok(id) = id.parse::<i64>() else {
        return github_error(StatusCode::NOT_FOUND, "Not Found");
    };
    match face.world.signing_keys(caller.actor.id) {
        Ok(keys) if keys.iter().any(|k| k.id == id) => {}
        Ok(_) => return github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => return internal(e),
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "SshSigningKey") {
        return refused;
    }
    match face.world.delete_signing_key(caller.via(Door::Rest), id) {
        Ok(()) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .expect("static response parts are valid"),
        Err(WorldError::NoSuchSigningKey(_)) => github_error(StatusCode::NOT_FOUND, "Not Found"),
        Err(e) => internal(e),
    }
}

/// GitHub's name for each verdict, as `commit.verification.reason`.
fn reason(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Unsigned => "unsigned",
        Verdict::UnknownKey => "unknown_key",
        Verdict::Malformed => "malformed_signature",
        Verdict::BadSignature => "invalid",
        Verdict::Verified => "valid",
        Verdict::UnverifiedAddress => "unverified_email",
        Verdict::OtherAccountsAddress => "bad_email",
        Verdict::NoAccount => "no_user",
    }
}

/// `GET /repos/{owner}/{repo}/commits/{ref}`: the commit `ref` names, with
/// the verdict on its signature. `ref` is anything `git rev-parse` resolves
/// to a commit, as a compare's ends are.
pub(crate) async fn get_commit(
    State(face): State<GithubFace>,
    UrlPath((owner, repo_name, rev)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
) -> Response<Body> {
    // A restore replaces the bare repository under the gate, so a read of
    // its objects holds the gate throughout.
    let _gate = face.git.gate().read_owned().await;
    let (repo, _) = match resolve_repo(&face, &owner, &repo_name) {
        Ok(x) => x,
        Err(resp) => return *resp,
    };
    let bare = store::bare_dir(&face.world.repos_dir(), &repo.owner_login, &repo.name);
    // The ref resolves, and a push's stored verdict is read, against one
    // state of the repository: a push records its verdicts in the commit
    // that records its refs.
    let refs = face.git.read_lock(repo.id).await;
    let commit = match resolve_commit(&bare, &rev) {
        Ok(Some(sha)) => signature::read_commit(&bare, &sha),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let commit = match commit {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            return github_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("No commit found for SHA: {rev}"),
            );
        }
        Err(e) => return internal(e),
    };
    let stored = match face.world.stored_commit_verification(repo.id, &commit.sha) {
        Ok(stored) => stored,
        Err(e) => return internal(e),
    };
    drop(refs);
    if let Some(refused) = media::refuse_unrendered(&headers, "Commit") {
        return refused;
    }
    let (verdict, verified_at) = match stored {
        Some(at) => (Verdict::Verified, Some(at)),
        None => match verdict_now(&face, &repo, &bare, &commit) {
            Ok(v) => v,
            Err(resp) => return *resp,
        },
    };
    json_response(
        StatusCode::OK,
        commit_json(&face, &repo, &commit, verdict, verified_at),
    )
}

/// The verdict on a commit the push that brought it in stored none for,
/// reached against the world as it is now.
fn verdict_now(
    face: &GithubFace,
    repo: &Repo,
    bare: &std::path::Path,
    commit: &Commit,
) -> Result<(Verdict, Option<i64>), Box<Response<Body>>> {
    let facts = signature::commit_facts(bare, commit).map_err(|e| Box::new(internal(e)))?;
    match face.world.commit_verification(repo.id, &facts) {
        Ok(v) => Ok((v.verdict, v.stored_clock_unix)),
        Err(e) => Err(Box::new(internal(e))),
    }
}

fn commit_json(
    face: &GithubFace,
    repo: &Repo,
    commit: &Commit,
    verdict: Verdict,
    verified_at: Option<i64>,
) -> serde_json::Value {
    let base = face.base_url.trim_end_matches('/');
    let full_name = repo.full_name();
    let api = format!("{base}/api/v3/repos/{full_name}");
    let ident = |i: &signature::Ident| serde_json::json!({ "name": i.name, "email": i.email, "date": rfc3339(i.unix) });
    let parents: Vec<serde_json::Value> = commit
        .parents
        .iter()
        .map(|p| {
            serde_json::json!({
                "sha": p,
                "url": format!("{api}/commits/{p}"),
                "html_url": format!("{base}/{full_name}/commit/{p}"),
            })
        })
        .collect();
    let signed = commit.signature.is_some();
    serde_json::json!({
        "sha": commit.sha,
        "url": format!("{api}/commits/{}", commit.sha),
        "html_url": format!("{base}/{full_name}/commit/{}", commit.sha),
        "commit": {
            "url": format!("{api}/git/commits/{}", commit.sha),
            "author": ident(&commit.author),
            "committer": ident(&commit.committer),
            "message": commit.message.trim_end_matches('\n'),
            "tree": { "sha": commit.tree, "url": format!("{api}/git/trees/{}", commit.tree) },
            "verification": {
                "verified": verdict == Verdict::Verified,
                "reason": reason(verdict),
                "signature": commit.signature,
                "payload": signed.then_some(&commit.payload),
                "verified_at": verified_at.map(rfc3339),
            },
        },
        "parents": parents,
    })
}
