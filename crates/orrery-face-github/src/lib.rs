//! The GitHub REST face. Grows strictly on demand (spec/README.md rule 1):
//! every endpoint here exists because a consumer needs it, and is listed in
//! `spec/surface-github.md`. Currently the authoring surface: repo
//! creation, org membership and teams, pull requests (open/edit/merge),
//! review requests, reviews, issue comments, labels, and check runs — the
//! writes a scenario driver performs (spec/usage.md).
//!
//! Face endpoints mimic real GitHub, including its lax body decoding: unknown
//! request fields are ignored, exactly as GitHub ignores them. The exception
//! is a known-but-unsupported *behavioral* field (`auto_init`, ...): silently
//! ignoring one would make the caller's world silently wrong, so those are
//! rejected loudly with the field named (the NOT-MOCKED principle applied at
//! field granularity).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::routing::{delete, get, patch, post, put};
use orrery_face_git::GitFace;
use orrery_world::{Actor, ActorKind, Repo, World, WorldError};

mod checks;
pub mod graphql;
mod media;
pub mod nodeid;
pub mod oauth;
mod orgs;
mod pulls;
mod reads;
mod render;

pub use render::repo_json;

#[derive(Clone)]
pub struct GithubFace {
    pub world: Arc<World>,
    pub git: Arc<GitFace>,
    /// External base URL of this instance, e.g. `http://127.0.0.1:4180` —
    /// what `html_url`/`clone_url` are rendered against.
    pub base_url: String,
}

pub fn router(face: GithubFace) -> Router {
    Router::new()
        .route("/orgs/{org}", get(reads::get_org))
        .route("/orgs/{org}/repos", post(create_org_repo))
        .route(
            "/orgs/{org}/teams",
            get(reads::list_org_teams).post(orgs::create_team),
        )
        .route("/orgs/{org}/teams/{team_slug}", delete(orgs::delete_team))
        .route(
            "/orgs/{org}/teams/{team_slug}/memberships/{username}",
            put(orgs::set_team_membership).delete(orgs::remove_team_membership),
        )
        .route(
            "/orgs/{org}/memberships/{username}",
            put(orgs::set_org_membership),
        )
        .route("/users/{username}", get(reads::get_user_by_login))
        .route("/user", get(reads::get_user))
        .route("/user/emails", get(reads::get_user_emails))
        .route("/user/teams", get(reads::list_user_teams))
        .route(
            "/user/repos",
            get(reads::list_user_repos).post(create_user_repo),
        )
        .route("/repos/{owner}/{repo}", get(reads::get_repo))
        .route(
            "/repos/{owner}/{repo}/pulls",
            get(reads::list_pulls).post(pulls::create_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}",
            get(reads::get_pull).patch(pulls::patch_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/merge",
            put(pulls::merge_pull),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/files",
            get(reads::list_pull_files),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/reviews",
            get(reads::list_reviews).post(pulls::create_review),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/requested_reviewers",
            post(pulls::request_reviewers).delete(pulls::remove_review_requests),
        )
        .route(
            "/repos/{owner}/{repo}/pulls/{number}/comments",
            get(reads::list_review_comments),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            get(reads::list_issue_comments).post(pulls::create_comment),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels",
            post(pulls::add_labels),
        )
        .route(
            "/repos/{owner}/{repo}/issues/{number}/labels/{name}",
            delete(pulls::remove_label),
        )
        .route(
            "/repos/{owner}/{repo}/commits/{sha}/check-runs",
            get(reads::list_check_runs_for_ref),
        )
        .route(
            "/repos/{owner}/{repo}/contents/{*path}",
            get(reads::get_contents),
        )
        .route("/repos/{owner}/{repo}/branches", get(reads::list_branches))
        .route(
            "/repos/{owner}/{repo}/git/ref/heads/{*branch}",
            get(reads::get_branch_ref),
        )
        .route(
            "/repos/{owner}/{repo}/compare/{basehead}",
            get(reads::compare),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs",
            post(checks::create_check_run),
        )
        .route(
            "/repos/{owner}/{repo}/check-runs/{id}",
            patch(checks::patch_check_run),
        )
        .with_state(face)
}

/// Fields GitHub's create-repo accepts that Orrery deliberately does not
/// implement. Sending one is a loud 422, never a silent ignore.
const UNSUPPORTED_BEHAVIORAL_FIELDS: &[&str] = &[
    "auto_init",
    "gitignore_template",
    "license_template",
    "is_template",
    "team_id",
];

#[derive(serde::Deserialize)]
struct CreateRepoBody {
    name: Option<String>,
    description: Option<String>,
    private: Option<bool>,
    visibility: Option<String>,
    // GitHub ignores unknown fields; we capture the remainder only to detect
    // known-behavioral ones we refuse to fake.
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, serde_json::Value>,
}

async fn create_org_repo(
    State(face): State<GithubFace>,
    UrlPath(org): UrlPath<String>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let owner = match resolve_org(&face, &org) {
        Ok(org) => org,
        Err(resp) => return *resp,
    };
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    create_repo(face, owner, actor, &body).await
}

async fn create_user_repo(
    State(face): State<GithubFace>,
    Extension(actor): Extension<Actor>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    if let Some(refused) = reads::refuse_integration(&actor) {
        return refused;
    }
    if let Some(refused) = media::refuse_unrendered(&headers, "Repository") {
        return refused;
    }
    let owner = actor.clone();
    create_repo(face, owner, actor, &body).await
}

async fn create_repo(
    face: GithubFace,
    owner: Actor,
    creator: Actor,
    body: &[u8],
) -> Response<Body> {
    let parsed: CreateRepoBody = match serde_json::from_slice(body) {
        Ok(p) => p,
        Err(_) => return github_error(StatusCode::BAD_REQUEST, "Problems parsing JSON"),
    };
    for field in UNSUPPORTED_BEHAVIORAL_FIELDS {
        if parsed.rest.contains_key(*field) {
            return orrery_unsupported_field("Repository", field);
        }
    }
    let Some(name) = parsed
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return creation_failed("missing_field", "name", "name is required");
    };
    let private = match parsed.visibility.as_deref() {
        Some("private") => true,
        Some("public") => false,
        Some(other) => {
            return orrery_unsupported_field("Repository", &format!("visibility={other}"));
        }
        None => parsed.private.unwrap_or(false),
    };

    let created = face.world.create_repo(
        owner.id,
        name,
        parsed.description.as_deref(),
        private,
        "main",
        creator.id,
    );
    let repo = match created {
        Ok(r) => r,
        Err(WorldError::RepoExists(_)) => {
            return creation_failed("custom", "name", "name already exists on this account");
        }
        // GitHub slugifies invalid names; Orrery rejects them instead —
        // divergence accepted loudly until a consumer needs slugification.
        Err(WorldError::InvalidRepoName(_)) => {
            return creation_failed(
                "custom",
                "name",
                "name is invalid (orrery does not slugify)",
            );
        }
        Err(e) => return internal(e),
    };
    if let Err(e) = face.git.init_repo_dir(&repo) {
        return internal(e);
    }

    let json = render::repo_json(&face, &repo, &owner);
    json_response(StatusCode::CREATED, json)
}

/// Resolve `{owner}/{repo}` to the stored row and its owner actor, or the
/// vendor-faithful 404 (a matched route with missing data — never a 501).
pub(crate) fn resolve_repo(
    face: &GithubFace,
    owner: &str,
    name: &str,
) -> Result<(Repo, Actor), Box<Response<Body>>> {
    let repo = match face.world.repo_by_owner_name(owner, name) {
        Ok(Some(r)) => r,
        Ok(None) => return Err(Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"))),
        Err(e) => return Err(Box::new(internal(e))),
    };
    let owner_actor = match face.world.actor_by_login(&repo.owner_login) {
        Ok(Some(a)) => a,
        Ok(None) => return Err(Box::new(internal("repo owner actor missing"))),
        Err(e) => return Err(Box::new(internal(e))),
    };
    Ok((repo, owner_actor))
}

/// Resolve `{org}` to the organization actor, or GitHub's 404 for a login that
/// names no organization.
pub(crate) fn resolve_org(face: &GithubFace, org: &str) -> Result<Actor, Box<Response<Body>>> {
    match face.world.actor_by_login(org) {
        Ok(Some(a)) if a.kind == ActorKind::Org => Ok(a),
        Ok(_) => Err(Box::new(github_error(StatusCode::NOT_FOUND, "Not Found"))),
        Err(e) => Err(Box::new(internal(e))),
    }
}

/// A query string or an `application/x-www-form-urlencoded` body as its
/// pairs, each key and value decoded as a URL query is (`%XX`, and `+` as a
/// space). A repeated key keeps its first value.
pub(crate) fn parse_form(raw: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in form_urlencoded::parse(raw) {
        out.entry(k.into_owned()).or_insert_with(|| v.into_owned());
    }
    out
}

/// The GitHub 422 creation-failure envelope, as consumers parse it
/// (spec/grammar.md: `message` + `errors[].{code,field,message}`).
pub(crate) fn creation_failed(code: &str, field: &str, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Repository creation failed.",
        "errors": [{ "resource": "Repository", "code": code, "field": field, "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's generic 422 for an invalid field value on `resource`.
pub(crate) fn validation_invalid(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "invalid", "field": field }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's 422 for an invalid `field` on `resource`, with its prose — the
/// shape of "Cannot change the base branch of a closed pull request."
pub(crate) fn validation_invalid_message(
    resource: &str,
    field: &str,
    message: &str,
) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "invalid", "field": field, "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's generic 422 for a required field absent on `resource`.
pub(crate) fn validation_missing(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "missing_field", "field": field }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's 422 with a prose `custom` error — the shape of "No commits
/// between x and y" and "A pull request already exists".
pub(crate) fn validation_custom(resource: &str, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "custom", "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// GitHub's 422 with a prose `custom` error naming `field` — the shape of a
/// refused reopen ("state cannot be changed. …").
pub(crate) fn validation_custom_field(
    resource: &str,
    field: &str,
    message: &str,
) -> Response<Body> {
    let body = serde_json::json!({
        "message": "Validation Failed",
        "errors": [{ "resource": resource, "code": "custom", "field": field, "message": message }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

/// Loud refusal for a real-GitHub field Orrery chooses not to fake, on the
/// `resource` the request is about. Shaped like a GitHub 422 so strict
/// clients parse it, but self-describing so a human reads exactly what to do
/// (NOT-MOCKED at field granularity).
pub(crate) fn orrery_unsupported_field(resource: &str, field: &str) -> Response<Body> {
    let body = serde_json::json!({
        "message": format!("orrery: request field {field:?} is not mocked"),
        "orrery": {
            "not_mocked": true,
            "field": field,
            "hint": "If a consumer needs this behavior, add it to the surface inventory (spec/surface-github.md) and implement it.",
        },
        "errors": [{ "resource": resource, "code": "custom", "field": field,
                     "message": "not mocked by orrery" }],
    });
    json_response(StatusCode::UNPROCESSABLE_ENTITY, body)
}

pub(crate) fn github_error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, serde_json::json!({ "message": message }))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response<Body> {
    tracing::error!(error = %e, "github face internal error");
    github_error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
}

/// A header's value, lowercased because the values matched against it (media
/// types, mostly) compare ignoring case; empty when absent or not ASCII.
pub(crate) fn header_str(headers: &HeaderMap, name: header::HeaderName) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub(crate) fn json_response(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .expect("static response parts are valid")
}

#[cfg(test)]
mod ref_read_tests {
    use std::future::Future;
    use std::io::Write;
    use std::path::Path;
    use std::pin::Pin;
    use std::process::Stdio;
    use std::task::{Context, Poll, Waker};

    use axum::body::Bytes;
    use axum::extract::RawQuery;
    use orrery_face_git::store;
    use orrery_world::{DEFAULT_T0_UNIX, NewPull, Pull, PullState, RefMove, WorldSecret};

    use super::*;

    type Read<'a> = Pin<Box<dyn Future<Output = Response<Body>> + Send + 'a>>;

    /// A world with one public repository, `alice/widgets`: `main` holds a
    /// README, and pull request #1 proposes `feature`, which adds a file.
    struct Fixture {
        face: GithubFace,
        schema: graphql::GithubSchema,
        alice: Actor,
        repo: Repo,
        bare: std::path::PathBuf,
        main: String,
        feature: String,
        pull: Pull,
        pull_node: String,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let world = Arc::new(
            World::create(
                &dir.path().join("world"),
                DEFAULT_T0_UNIX,
                &WorldSecret::LOCAL,
            )
            .unwrap(),
        );
        let face = GithubFace {
            world: world.clone(),
            git: GitFace::new(world.clone()),
            base_url: "http://world.test".to_string(),
        };
        let (alice, _) = world.create_actor("alice", ActorKind::User).unwrap();
        let repo = world
            .create_repo(alice.id, "widgets", None, false, "main", alice.id)
            .unwrap();
        face.git.init_repo_dir(&repo).unwrap();
        let bare = store::bare_dir(&world.repos_dir(), "alice", "widgets");
        let main = commit(&bare, &[], &[("README.md", "# widgets\n")]);
        let feature = commit(
            &bare,
            &[&main],
            &[("README.md", "# widgets\n"), ("feature.txt", "v1\n")],
        );
        store::update_ref(&bare, "refs/heads/main", &main).unwrap();
        store::update_ref(&bare, "refs/heads/feature", &feature).unwrap();
        let pull = world
            .create_pull(NewPull {
                repo_id: repo.id,
                author_id: alice.id,
                title: "Add feature",
                body: None,
                head_ref: "feature",
                head_sha: &feature,
                base_ref: "main",
                base_sha: &main,
                draft: false,
            })
            .unwrap();
        Fixture {
            schema: graphql::schema(face.clone()),
            face,
            alice,
            repo,
            bare,
            main,
            feature,
            pull_node: nodeid::pull(pull.id),
            pull,
            _dir: dir,
        }
    }

    /// A commit on `parents` whose tree holds `files`, all at the top level.
    fn commit(bare: &Path, parents: &[&str], files: &[(&str, &str)]) -> String {
        let git = |args: &[&str], input: &str| {
            let mut child = store::hermetic_git(bare)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let entries: String = files
            .iter()
            .map(|(name, contents)| {
                let blob = git(&["hash-object", "-w", "--stdin"], contents);
                format!("100644 blob {blob}\t{name}\n")
            })
            .collect();
        let tree = git(&["mktree"], &entries);
        store::commit_tree(bare, &tree, parents, "commit", "alice", DEFAULT_T0_UNIX).unwrap()
    }

    /// `read`, boxed and polled outside tokio's cooperative budget, so a lock
    /// that is free is taken on its first poll however much the test polled
    /// before it.
    fn unbudgeted<'a>(read: impl Future<Output = Response<Body>> + Send + 'a) -> Read<'a> {
        Box::pin(tokio::task::unconstrained(read))
    }

    /// Every REST read that reads the repository's refs.
    fn rest_reads(f: &Fixture) -> Vec<(&'static str, Read<'_>)> {
        let path = || ("alice".to_string(), "widgets".to_string());
        let path3 = |third: &str| {
            let (owner, repo) = path();
            UrlPath((owner, repo, third.to_string()))
        };
        let face = || State(f.face.clone());
        vec![
            (
                "GET branches",
                unbudgeted(reads::list_branches(
                    face(),
                    UrlPath(path()),
                    RawQuery(None),
                    HeaderMap::new(),
                )),
            ),
            (
                "GET git/ref/heads/main",
                unbudgeted(reads::get_branch_ref(
                    face(),
                    path3("main"),
                    HeaderMap::new(),
                )),
            ),
            (
                "GET compare",
                unbudgeted(reads::compare(
                    face(),
                    path3("main...feature"),
                    HeaderMap::new(),
                )),
            ),
            (
                "GET contents",
                unbudgeted(reads::get_contents(
                    face(),
                    path3("README.md"),
                    RawQuery(None),
                    HeaderMap::new(),
                )),
            ),
            (
                "GET check-runs for a branch",
                unbudgeted(reads::list_check_runs_for_ref(
                    face(),
                    path3("main"),
                    RawQuery(None),
                    HeaderMap::new(),
                )),
            ),
            (
                "GET pull files",
                unbudgeted(reads::list_pull_files(
                    face(),
                    path3("1"),
                    RawQuery(None),
                    HeaderMap::new(),
                )),
            ),
        ]
    }

    /// One GraphQL query, as alice.
    fn graphql(f: &Fixture, query: String) -> Read<'_> {
        let body = serde_json::to_vec(&serde_json::json!({ "query": query })).unwrap();
        unbudgeted(async move { graphql::handle(&f.schema, f.alice.clone(), &body).await })
    }

    /// A query selecting `selection` on pull request #1.
    fn pull_query<'a>(f: &'a Fixture, selection: &str) -> Read<'a> {
        graphql(
            f,
            format!(
                r#"{{ node(id: "{}") {{ ... on PullRequest {{ {selection} }} }} }}"#,
                f.pull_node
            ),
        )
    }

    /// Every GraphQL field that reads the repository's refs, one to a query.
    fn graphql_reads(f: &Fixture) -> Vec<(&'static str, Read<'_>)> {
        vec![
            (
                "Repository.ref",
                graphql(
                    f,
                    r#"{ repository(owner: "alice", name: "widgets") { ref(qualifiedName: "refs/heads/main") { id } } }"#
                        .to_string(),
                ),
            ),
            ("PullRequest.mergeable", pull_query(f, "mergeable")),
            (
                "PullRequest.additions",
                pull_query(f, "additions deletions changedFiles"),
            ),
            (
                "PullRequest.files",
                pull_query(f, "files(first: 10) { nodes { path } }"),
            ),
        ]
    }

    fn ref_reads(f: &Fixture) -> Vec<(&'static str, Read<'_>)> {
        let mut reads = rest_reads(f);
        reads.extend(graphql_reads(f));
        reads
    }

    /// Poll once, with a waker that does nothing: a read that is waiting
    /// stays where it is until it is polled again.
    fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
        fut.poll(&mut Context::from_waker(Waker::noop()))
    }

    async fn answer(what: &str, resp: Response<Body>) -> serde_json::Value {
        assert_eq!(resp.status(), StatusCode::OK, "{what}");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(body.get("errors").is_none(), "{what}: {body}");
        body
    }

    /// The test holds the repository's lock for write, as a merge does. Each
    /// read's git calls run inside its first poll unless it waits for the
    /// lock, so one still pending after that poll had not read the refs yet.
    #[tokio::test]
    async fn ref_reads_wait_out_a_write() {
        let f = fixture();
        let lock = f.face.git.repo_lock(f.repo.id);
        for (what, mut read) in ref_reads(&f) {
            let write = lock.clone().write_owned().await;
            assert!(
                poll_once(read.as_mut()).is_pending(),
                "{what}: read the refs during a write"
            );
            drop(write);
            answer(what, read.await).await;
        }
    }

    /// The test holds the repository's lock for read, as a fetch in progress
    /// does. Each read is answered on its first poll.
    #[tokio::test]
    async fn ref_reads_do_not_wait_on_each_other() {
        let f = fixture();
        let _read = f.face.git.read_lock(f.repo.id).await;
        for (what, mut read) in ref_reads(&f) {
            match poll_once(read.as_mut()) {
                Poll::Ready(resp) => drop(answer(what, resp).await),
                Poll::Pending => panic!("{what}: waited for another read"),
            }
        }
    }

    /// The test holds the world write gate, as a snapshot restore does while
    /// it replaces the bare repositories. Each REST read waits for it. The
    /// GraphQL handler holds the gate for a whole query, outside this crate.
    #[tokio::test]
    async fn rest_ref_reads_wait_out_a_restore() {
        let f = fixture();
        for (what, mut read) in rest_reads(&f) {
            let restore = f.face.git.gate().write_owned().await;
            assert!(
                poll_once(read.as_mut()).is_pending(),
                "{what}: read the repository during a restore"
            );
            drop(restore);
            answer(what, read.await).await;
        }
    }

    /// A query loads pull request #1 while it is open, and its diff fields
    /// then wait for a merge of it that holds the lock. The fields compute
    /// from the merged pull, against its pinned base, rather than comparing
    /// the open row's head with the commit that merged it, which would answer
    /// an open pull request with nothing to merge.
    #[tokio::test]
    async fn pull_fields_read_after_a_merge_see_the_merged_pull() {
        let f = fixture();
        let lock = f.face.git.repo_lock(f.repo.id);
        let mut read = pull_query(
            &f,
            "additions changedFiles mergeable files(first: 10) { nodes { path } }",
        );

        let merging = lock.clone().write_owned().await;
        assert!(poll_once(read.as_mut()).is_pending());
        let merge = commit(
            &f.bare,
            &[&f.main, &f.feature],
            &[("README.md", "# widgets\n"), ("feature.txt", "v1\n")],
        );
        store::update_ref_cas(&f.bare, "refs/heads/main", &merge, &f.main).unwrap();
        f.face
            .world
            .record_merge(
                f.pull.id,
                f.alice.id,
                &RefMove {
                    name: "refs/heads/main",
                    old: &f.main,
                    new: &merge,
                    repins: Vec::new(),
                },
            )
            .unwrap();
        drop(merging);

        let body = answer("the pull's diff fields", read.await).await;
        assert_eq!(
            body["data"]["node"],
            serde_json::json!({
                "additions": 1,
                "changedFiles": 1,
                "mergeable": "UNKNOWN",
                "files": { "nodes": [{ "path": "feature.txt" }] },
            })
        );
    }

    /// `PATCH …/pulls/1` with `body`, as alice.
    fn edit<'a>(f: &'a Fixture, body: &'static str) -> Read<'a> {
        unbudgeted(pulls::patch_pull(
            State(f.face.clone()),
            UrlPath((
                "alice".to_string(),
                "widgets".to_string(),
                f.pull.number.to_string(),
            )),
            Extension(f.alice.clone()),
            HeaderMap::new(),
            Bytes::from_static(body.as_bytes()),
        ))
    }

    /// The test holds the repository's lock for read, as a fetch in progress
    /// does. An edit that sets neither `state` nor `base` is answered on its
    /// first poll.
    #[tokio::test]
    async fn a_title_or_body_edit_does_not_wait_for_fetches() {
        let f = fixture();
        let _fetch = f.face.git.read_lock(f.repo.id).await;
        for body in [
            r#"{"title": "Renamed"}"#,
            r#"{"body": "Details"}"#,
            r#"{"title": "Renamed again", "body": null}"#,
            r#"{"state": "merged"}"#,
            "{}",
        ] {
            let mut edit = edit(&f, body);
            match poll_once(edit.as_mut()) {
                Poll::Ready(resp) => drop(answer(body, resp).await),
                Poll::Pending => panic!("{body}: waited for a fetch"),
            }
        }
        let pull = f.face.world.pull_by_number(f.repo.id, 1).unwrap().unwrap();
        assert_eq!(
            (pull.title.as_str(), pull.body, pull.state),
            ("Renamed again", None, PullState::Open)
        );
    }

    /// The test holds the repository's lock for write, as a merge does while
    /// it builds its commit message from the pull's title. An edit that sets
    /// neither `state` nor `base` waits for it, and lands once it ends.
    #[tokio::test]
    async fn a_title_or_body_edit_waits_out_a_write() {
        let f = fixture();
        let lock = f.face.git.repo_lock(f.repo.id);
        for body in [r#"{"title": "Renamed"}"#, r#"{"body": "Details"}"#] {
            let merge = lock.clone().write_owned().await;
            let mut edit = edit(&f, body);
            assert!(
                poll_once(edit.as_mut()).is_pending(),
                "{body}: landed during a write"
            );
            drop(merge);
            answer(body, edit.await).await;
        }
    }

    /// An edit waits for the gate, held as a restore holds it, and the pull
    /// is closed while it waits. The edit answers the pull closed: a title
    /// edit lands on it, as an edit of a closed pull's title does, and an
    /// edit that writes nothing answers the pull as the handler re-read it.
    #[tokio::test]
    async fn an_edit_answers_a_close_that_landed_while_it_waited() {
        let f = fixture();
        for (body, title) in [(r#"{"title": "Renamed"}"#, "Renamed"), ("{}", "Renamed")] {
            let restore = f.face.git.gate().write_owned().await;
            let mut edit = edit(&f, body);
            assert!(poll_once(edit.as_mut()).is_pending());
            f.face.world.close_pull(f.pull.id, f.alice.id).unwrap();
            drop(restore);

            let answered = answer(body, edit.await).await;
            assert_eq!(
                (&answered["title"], &answered["state"]),
                (&serde_json::json!(title), &serde_json::json!("closed")),
                "{body}"
            );
            f.face
                .world
                .reopen_pull(f.pull.id, f.alice.id, &f.feature)
                .unwrap();
        }
    }

    /// The test holds the repository's lock for read, as a fetch in progress
    /// does. An edit that sets `state` or `base`, alone or beside other
    /// fields, waits for it, and a fetch request sent after the edit waits
    /// for the edit. A `state` the pull is already in waits too.
    #[tokio::test]
    async fn a_state_or_base_edit_waits_for_fetches_and_holds_off_later_ones() {
        let f = fixture();
        store::update_ref(&f.bare, "refs/heads/develop", &f.main).unwrap();
        let lock = f.face.git.repo_lock(f.repo.id);
        for body in [
            r#"{"state": "closed"}"#,
            r#"{"title": "Reopened", "state": "open"}"#,
            r#"{"state": "open"}"#,
            r#"{"base": "develop"}"#,
            r#"{"body": "Back to main", "base": "main"}"#,
        ] {
            let fetch = f.face.git.read_lock(f.repo.id).await;
            let mut edit = edit(&f, body);
            assert!(
                poll_once(edit.as_mut()).is_pending(),
                "{body}: landed during a fetch"
            );
            let mut later = Box::pin(tokio::task::unconstrained(f.face.git.read_lock(f.repo.id)));
            assert!(
                poll_once(later.as_mut()).is_pending(),
                "{body}: a later fetch request was let in ahead of the edit"
            );

            drop(fetch);
            assert!(
                lock.try_read().is_err(),
                "{body}: the edit was not waiting for the repository's lock"
            );
            answer(body, edit.await).await;
            assert!(
                lock.try_write().is_err(),
                "{body}: the later fetch request was not waiting for the edit"
            );
            drop(later.await);
        }
        let pull = f.face.world.pull_by_number(f.repo.id, 1).unwrap().unwrap();
        assert_eq!(
            (pull.title.as_str(), pull.state, pull.base_ref.as_str()),
            ("Reopened", PullState::Open, "main")
        );
    }
}
