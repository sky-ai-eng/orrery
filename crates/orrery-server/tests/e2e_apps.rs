//! End-to-end for GitHub Apps (spec/surface-github.md §1.11): registering
//! and installing one on the control plane, the routes an App calls with a
//! JWT it signs, the installation tokens it issues, and what such a token
//! reaches over REST, GraphQL, and git.

mod common;

use common::{
    AppKey, app_key, boot, control_post, face_ok, fault, faults, git, git_output, mint_actor,
};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// A call with `authorization` as its `Authorization` header: the status,
/// the `Link` header, and the JSON body.
fn call(
    method: &str,
    url: &str,
    authorization: &str,
    body: Option<serde_json::Value>,
) -> (u16, Option<String>, serde_json::Value) {
    let http = agent();
    let mut resp = match (method, body) {
        ("GET", None) => http.get(url).header("authorization", authorization).call(),
        ("POST", None) => http
            .post(url)
            .header("authorization", authorization)
            .send_empty(),
        ("POST", Some(body)) => http
            .post(url)
            .header("authorization", authorization)
            .send_json(body),
        (other, _) => panic!("unsupported call {other}"),
    }
    .unwrap_or_else(|e| panic!("{method} {url} transport error: {e:?}"));
    let link = resp
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string());
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().expect("a body");
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, link, json)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// A control-plane call that may be refused: the status and the JSON body.
fn control(method: &str, url: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let http = agent();
    let mut resp = match method {
        "GET" => http.get(url).call(),
        "POST" => http.post(url).send_json(body),
        other => panic!("unsupported call {other}"),
    }
    .unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
}

/// A world with a user and an organization that each own a public and a
/// private repository, and an App owned by the organization, with a key,
/// installed on the organization (every repository) and on the user (`own`
/// alone).
struct Setup {
    inst: common::Instance,
    base: String,
    api: String,
    alice: String,
    key: AppKey,
    app_id: i64,
    client_id: String,
    client_secret: String,
    on_acme: i64,
    on_alice: i64,
}

async fn setup() -> Setup {
    let inst = boot().await;
    let base = inst.base.clone();
    let api = format!("{base}/api/v3");
    let alice = mint_actor(&base, "alice", "user");
    mint_actor(&base, "acme", "org");
    for (path, name, private) in [
        ("orgs/acme/repos", "web", false),
        ("orgs/acme/repos", "secret", true),
        ("user/repos", "own", true),
        ("user/repos", "pub", false),
    ] {
        face_ok(
            "POST",
            &format!("{api}/{path}"),
            &alice,
            serde_json::json!({ "name": name, "private": private }),
        );
    }
    let app = control_post(
        &base,
        "/_orrery/apps",
        serde_json::json!({
            "slug": "helper",
            "name": "Helper",
            "owner": "acme",
            "callback_urls": ["https://app.test/callback", "https://app.test/connect"],
            "permissions": { "contents": "write", "metadata": "read", "members": "read" },
            "events": ["push", "pull_request"],
        }),
    );
    let key = app_key();
    control_post(
        &base,
        "/_orrery/apps/helper/keys",
        serde_json::json!({ "public_key": key.public_pem }),
    );
    let on_acme = control_post(
        &base,
        "/_orrery/apps/helper/installations",
        serde_json::json!({ "account": "acme" }),
    );
    let on_alice = control_post(
        &base,
        "/_orrery/apps/helper/installations",
        serde_json::json!({ "account": "alice", "repositories": ["own"] }),
    );
    Setup {
        app_id: app["id"].as_i64().unwrap(),
        client_id: app["client_id"].as_str().unwrap().to_string(),
        client_secret: app["client_secret"].as_str().unwrap().to_string(),
        on_acme: on_acme["id"].as_i64().unwrap(),
        on_alice: on_alice["id"].as_i64().unwrap(),
        inst,
        base,
        api,
        alice,
        key,
    }
}

impl Setup {
    fn clock(&self) -> i64 {
        self.inst.world.clock().unwrap()
    }

    /// The JWT the App sends now, with its numeric id as the issuer.
    fn jwt(&self) -> String {
        bearer(&self.key.jwt(serde_json::json!(self.app_id), self.clock()))
    }

    /// Issue a token for `installation` with `body`, which must succeed.
    fn token(&self, installation: i64, body: Option<serde_json::Value>) -> serde_json::Value {
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.api
        );
        let (status, _, issued) = call("POST", &url, &self.jwt(), body);
        assert_eq!(status, 201, "{issued}");
        issued
    }
}

fn names(repositories: &serde_json::Value) -> Vec<&str> {
    repositories
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["full_name"].as_str().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_reads_itself_and_its_installations() {
    let s = setup().await;
    let api = &s.api;

    let (status, _, app) = call("GET", &format!("{api}/app"), &s.jwt(), None);
    assert_eq!(status, 200, "{app}");
    assert_eq!(app["id"], s.app_id);
    assert_eq!(app["slug"], "helper");
    assert_eq!(app["name"], "Helper");
    assert_eq!(app["client_id"], s.client_id.as_str());
    assert_eq!(app["owner"]["login"], "acme");
    assert_eq!(app["owner"]["type"], "Organization");
    assert_eq!(
        app["permissions"],
        serde_json::json!({ "contents": "write", "members": "read", "metadata": "read" })
    );
    assert_eq!(app["events"], serde_json::json!(["push", "pull_request"]));
    assert_eq!(app["html_url"], format!("{}/apps/helper", s.base));
    assert_eq!(app["installations_count"], 2);
    assert!(app.get("client_secret").is_none());

    // The issuer may be the id as a string, or the client id.
    for iss in [
        serde_json::json!(s.app_id.to_string()),
        serde_json::json!(s.client_id),
    ] {
        let jwt = bearer(&s.key.jwt(iss.clone(), s.clock()));
        let (status, _, app) = call("GET", &format!("{api}/app"), &jwt, None);
        assert_eq!((status, &app["slug"]), (200, &"helper".into()), "iss {iss}");
    }

    let (status, link, listed) = call("GET", &format!("{api}/app/installations"), &s.jwt(), None);
    assert_eq!(status, 200, "{listed}");
    assert_eq!(link, None, "one page has no Link");
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 2);
    let acme = &listed[0];
    assert_eq!(acme["id"], s.on_acme);
    assert_eq!(acme["account"]["login"], "acme");
    assert_eq!(acme["account"]["type"], "Organization");
    assert_eq!(acme["target_type"], "Organization");
    assert_eq!(acme["target_id"], acme["account"]["id"]);
    assert_eq!(acme["app_id"], s.app_id);
    assert_eq!(acme["app_slug"], "helper");
    assert_eq!(acme["repository_selection"], "all");
    assert_eq!(acme["permissions"]["contents"], "write");
    assert_eq!(acme["events"], serde_json::json!(["push", "pull_request"]));
    assert_eq!(acme["created_at"], "2020-01-01T00:00:00Z");
    assert!(acme["suspended_at"].is_null());
    assert!(acme["suspended_by"].is_null());
    assert_eq!(
        acme["access_tokens_url"],
        format!("{api}/app/installations/{}/access_tokens", s.on_acme)
    );
    assert_eq!(listed[1]["id"], s.on_alice);
    assert_eq!(listed[1]["repository_selection"], "selected");
    assert_eq!(listed[1]["target_type"], "User");

    // Paged, each page names the ones around it on this origin.
    let (_, link, first) = call(
        "GET",
        &format!("{api}/app/installations?per_page=1"),
        &s.jwt(),
        None,
    );
    assert_eq!(first.as_array().unwrap().len(), 1);
    let page = |n: u8| format!("<{api}/app/installations?per_page=1&page={n}>");
    assert_eq!(
        link.unwrap(),
        format!("{}; rel=\"next\", {}; rel=\"last\"", page(2), page(2))
    );
    let (_, link, second) = call(
        "GET",
        &format!("{api}/app/installations?per_page=1&page=2"),
        &s.jwt(),
        None,
    );
    assert_eq!(second[0]["id"], s.on_alice);
    assert_eq!(
        link.unwrap(),
        format!("{}; rel=\"prev\", {}; rel=\"first\"", page(1), page(1))
    );

    let one = format!("{api}/app/installations/{}", s.on_alice);
    let (status, _, read) = call("GET", &one, &s.jwt(), None);
    assert_eq!((status, &read), (200, &listed[1]));
    for missing in ["999", "nope"] {
        let (status, _, body) = call(
            "GET",
            &format!("{api}/app/installations/{missing}"),
            &s.jwt(),
            None,
        );
        assert_eq!((status, &body["message"]), (404, &"Not Found".into()));
    }

    // A path under /app that Orrery does not serve says so, to an App alone.
    let hook = format!("{api}/app/hook/config");
    let (status, _, body) = call("GET", &hook, &s.jwt(), None);
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["orrery"]["path"], "/api/v3/app/hook/config");
    let (status, _, _) = call("GET", &hook, &bearer(&s.alice), None);
    assert_eq!(status, 401);
    let (status, _, body) = call("POST", &format!("{api}/app"), &s.jwt(), None);
    assert_eq!(status, 501, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jwt_is_held_to_githubs_rules() {
    let s = setup().await;
    let url = format!("{}/app", s.api);
    let now = s.clock();
    let rs256 = || serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let claims =
        |iat: i64, exp: i64| serde_json::json!({ "iss": s.app_id, "iat": iat, "exp": exp });
    let refused = |jwt: &str| {
        let (status, _, body) = call("GET", &url, &bearer(jwt), None);
        assert_eq!(status, 401, "{body}");
        body["message"].as_str().unwrap().to_string()
    };
    const NOT_DECODED: &str = "A JSON web token could not be decoded";

    // What is not a JWT one of the App's keys signed.
    assert_eq!(refused("not-a-jwt"), NOT_DECODED);
    assert_eq!(refused(&s.alice), NOT_DECODED);
    let other = app_key();
    assert_eq!(
        refused(&other.jwt(serde_json::json!(s.app_id), now)),
        NOT_DECODED
    );
    assert_eq!(
        refused(&s.key.jwt(serde_json::json!(999), now)),
        NOT_DECODED
    );
    assert_eq!(
        refused(&s.key.jwt(serde_json::json!("nobody"), now)),
        NOT_DECODED
    );
    assert_eq!(
        refused(&s.key.sign(
            serde_json::json!({ "alg": "none" }),
            claims(now - 60, now + 540)
        )),
        NOT_DECODED
    );
    let good = s.key.jwt(serde_json::json!(s.app_id), now);
    let tampered = format!("{}x", good);
    assert_eq!(refused(&tampered), NOT_DECODED);
    let (status, _, _) = call("GET", &url, "", None);
    assert_eq!(status, 401);

    // The claims.
    let signed = |claims: serde_json::Value| s.key.sign(rs256(), claims);
    assert!(refused(&signed(serde_json::json!({ "iss": s.app_id, "iat": now }))).contains("'exp'"));
    assert!(
        refused(&signed(
            serde_json::json!({ "iss": s.app_id, "exp": now + 60 })
        ))
        .contains("'iat'")
    );
    assert_eq!(
        refused(&signed(claims(now, now + 601))),
        "'Expiration time' claim ('exp') is too far in the future"
    );
    let (status, _, _) = call("GET", &url, &bearer(&signed(claims(now, now + 600))), None);
    assert_eq!(status, 200);

    // A manual world's clock is not the signer's, so a JWT dated years
    // from it is accepted.
    let later = now + 86_400 * 365;
    let (status, _, _) = call(
        "GET",
        &url,
        &bearer(&s.key.jwt(serde_json::json!(s.app_id), later)),
        None,
    );
    assert_eq!(status, 200);

    // A real world holds both claims to its clock.
    let host = now + 1000;
    s.inst.world.set_host_time(host);
    let http = agent();
    let patched = http
        .patch(format!("{}/_orrery/clock", s.base))
        .send_json(serde_json::json!({ "mode": "real" }))
        .unwrap();
    assert_eq!(patched.status(), 200);
    let (status, _, _) = call(
        "GET",
        &url,
        &bearer(&s.key.jwt(serde_json::json!(s.app_id), host)),
        None,
    );
    assert_eq!(status, 200);
    assert!(refused(&signed(claims(host - 600, host))).contains("'exp'"));
    assert!(refused(&signed(claims(host + 1, host + 300))).contains("'iat'"));
    // Time passes, and a JWT that was good expires.
    let jwt = s.key.jwt(serde_json::json!(s.app_id), host);
    s.inst.world.set_host_time(host + 540);
    assert!(refused(&jwt).contains("'exp'"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installation_token_reaches_what_its_installation_covers() {
    let s = setup().await;
    let api = &s.api;
    let t0 = s.clock();

    // With no body, the token reaches the whole installation.
    let issued = s.token(s.on_acme, None);
    let token = issued["token"].as_str().unwrap();
    assert!(token.starts_with("orrfake_"));
    assert_eq!(issued["expires_at"], "2020-01-01T01:00:00Z");
    assert_eq!(issued["repository_selection"], "all");
    assert_eq!(issued["permissions"]["contents"], "write");
    assert!(issued.get("repositories").is_none());
    let auth = bearer(token);

    let (status, _, repos) = call(
        "GET",
        &format!("{api}/installation/repositories"),
        &auth,
        None,
    );
    assert_eq!(status, 200, "{repos}");
    assert_eq!(repos["total_count"], 2);
    assert_eq!(repos["repository_selection"], "all");
    assert_eq!(names(&repos["repositories"]), ["acme/web", "acme/secret"]);
    assert_eq!(repos["repositories"][1]["private"], true);
    assert_eq!(repos["repositories"][0]["owner"]["login"], "acme");
    let (_, _, paged) = call(
        "GET",
        &format!("{api}/installation/repositories?per_page=1&page=2"),
        &auth,
        None,
    );
    assert_eq!(paged["total_count"], 2);
    assert_eq!(names(&paged["repositories"]), ["acme/secret"]);

    // It acts as the App's bot.
    let (status, _, me) = call("GET", &format!("{api}/user"), &auth, None);
    assert_eq!(status, 403);
    assert_eq!(me["message"], "Resource not accessible by integration");
    let (_, _, bot) = call("GET", &format!("{api}/users/helper[bot]"), &auth, None);
    assert_eq!(bot["type"], "Bot");

    // A repository the installation covers, a private one it does not, and
    // a public one it does not, which anyone reads and it may not write.
    let read = |auth: &str, repo: &str| call("GET", &format!("{api}/repos/{repo}"), auth, None).0;
    assert_eq!(read(&auth, "acme/secret"), 200);
    assert_eq!(read(&auth, "alice/own"), 404);
    assert_eq!(read(&auth, "alice/pub"), 200);
    assert_eq!(read(&auth, "alice/nothing"), 404);
    let open = serde_json::json!({ "title": "t", "head": "f", "base": "main" });
    let (status, _, body) = call(
        "POST",
        &format!("{api}/repos/alice/pub/pulls"),
        &auth,
        Some(open.clone()),
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["message"], "Resource not accessible by integration");
    let (status, _, _) = call(
        "POST",
        &format!("{api}/repos/alice/own/pulls"),
        &auth,
        Some(open.clone()),
    );
    assert_eq!(status, 404);
    // A covered repository takes the write as far as its own checks.
    let (status, _, body) = call(
        "POST",
        &format!("{api}/repos/acme/web/pulls"),
        &auth,
        Some(open),
    );
    assert_eq!(status, 422, "{body}");
    let own_id = face_ok(
        "GET",
        &format!("{api}/repos/alice/own"),
        &s.alice,
        serde_json::json!(null),
    )["id"]
        .as_i64()
        .unwrap();
    let (status, _, _) = call("GET", &format!("{api}/repositories/{own_id}"), &auth, None);
    assert_eq!(status, 404);

    // GraphQL sees the same repositories.
    let graphql = |auth: &str, repo: (&str, &str)| {
        let (status, _, body) = call(
            "POST",
            &format!("{}/api/graphql", s.base),
            auth,
            Some(serde_json::json!({
                "query": "query($o: String!, $n: String!) { repository(owner: $o, name: $n) { nameWithOwner } }",
                "variables": { "o": repo.0, "n": repo.1 },
            })),
        );
        assert_eq!(status, 200, "{body}");
        body["data"]["repository"].clone()
    };
    assert_eq!(
        graphql(&auth, ("acme", "secret"))["nameWithOwner"],
        "acme/secret"
    );
    assert_eq!(
        graphql(&auth, ("alice", "pub"))["nameWithOwner"],
        "alice/pub"
    );
    assert!(graphql(&auth, ("alice", "own")).is_null());
    assert_eq!(
        graphql(&bearer(&s.alice), ("alice", "own"))["nameWithOwner"],
        "alice/own"
    );

    // A selected installation lists its repositories alone.
    let selected = s.token(s.on_alice, None);
    assert_eq!(selected["repository_selection"], "selected");
    let auth = bearer(selected["token"].as_str().unwrap());
    let (_, _, repos) = call(
        "GET",
        &format!("{api}/installation/repositories"),
        &auth,
        None,
    );
    assert_eq!(repos["repository_selection"], "selected");
    assert_eq!(names(&repos["repositories"]), ["alice/own"]);
    assert_eq!(read(&auth, "alice/own"), 200);
    assert_eq!(read(&auth, "acme/secret"), 404);

    // Narrowed, a token reaches the repositories it names.
    let narrowed = s.token(
        s.on_acme,
        Some(serde_json::json!({ "repositories": ["web"] })),
    );
    assert_eq!(narrowed["repository_selection"], "selected");
    assert_eq!(names(&narrowed["repositories"]), ["acme/web"]);
    let auth = bearer(narrowed["token"].as_str().unwrap());
    assert_eq!(read(&auth, "acme/web"), 200);
    assert_eq!(read(&auth, "acme/secret"), 404);
    let (_, _, repos) = call(
        "GET",
        &format!("{api}/installation/repositories"),
        &auth,
        None,
    );
    assert_eq!(names(&repos["repositories"]), ["acme/web"]);

    // And holds the permissions it asks for, none above the installation's.
    let lesser = s.token(
        s.on_acme,
        Some(serde_json::json!({ "permissions": { "contents": "read" } })),
    );
    assert_eq!(
        lesser["permissions"],
        serde_json::json!({ "contents": "read" })
    );
    let tokens = format!("{api}/app/installations/{}/access_tokens", s.on_acme);
    for (body, message) in [
        (
            serde_json::json!({ "repositories": ["own"] }),
            "There is at least one repository that does not exist or is not accessible to the parent installation.",
        ),
        (
            serde_json::json!({ "permissions": { "contents": "admin" } }),
            "The permissions requested are not granted to this installation.",
        ),
        (
            serde_json::json!({ "permissions": { "issues": "read" } }),
            "The permissions requested are not granted to this installation.",
        ),
    ] {
        let (status, _, refused) = call("POST", &tokens, &s.jwt(), Some(body));
        assert_eq!((status, &refused["message"]), (422, &message.into()));
    }
    let (status, _, refused) = call(
        "POST",
        &tokens,
        &s.jwt(),
        Some(serde_json::json!({ "repository_ids": [1] })),
    );
    assert_eq!(status, 422);
    assert_eq!(refused["orrery"]["field"], "repository_ids");
    let (status, _, _) = call(
        "POST",
        &format!("{api}/app/installations/999/access_tokens"),
        &s.jwt(),
        None,
    );
    assert_eq!(status, 404);

    // Any other token lists no installation's repositories.
    let (status, _, body) = call(
        "GET",
        &format!("{api}/installation/repositories"),
        &bearer(&s.alice),
        None,
    );
    assert_eq!(status, 403, "{body}");

    // Each issue is the bot's write, through the App.
    let log = control_post(
        &s.base,
        "/_orrery/log/list",
        serde_json::json!({ "sort_key": "seq", "sort_dir": "desc", "page_size": 1 }),
    );
    let last = &log["items"][0];
    assert_eq!(last["type"], "installation_token:created");
    assert_eq!(last["actor"]["login"], "helper");
    assert_eq!(last["door"], "rest");
    assert_eq!(last["app"]["client_id"], s.client_id.as_str());
    assert_eq!(last["object"]["expires_unix"], t0 + 3600);
    assert!(
        !last.to_string().contains(lesser["token"].as_str().unwrap()),
        "an event carries no token"
    );

    // A token expires an hour later on the world's clock.
    let token = bearer(token);
    control_post(
        &s.base,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 3599 }),
    );
    assert_eq!(read(&token, "acme/web"), 200);
    control_post(
        &s.base,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 1 }),
    );
    let (status, _, body) = call("GET", &format!("{api}/repos/acme/web"), &token, None);
    assert_eq!((status, &body["message"]), (401, &"Bad credentials".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installation_token_fetches_and_pushes_what_it_reaches() {
    let s = setup().await;
    let clock = s.clock();
    let token = s.token(s.on_acme, None)["token"]
        .as_str()
        .unwrap()
        .to_string();
    let remote = |repo: &str| {
        format!("{}/{repo}.git", s.base)
            .replace("http://", &format!("http://x-access-token:{token}@"))
    };

    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# readme\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);

    // It pushes to and fetches a private repository its installation covers.
    git(src, clock, &["push", "-q", &remote("acme/secret"), "main"]);
    let listed = git(src, clock, &["ls-remote", &remote("acme/secret")]);
    assert!(listed.contains("refs/heads/main"), "{listed}");
    let log = control_post(
        &s.base,
        "/_orrery/log/list",
        serde_json::json!({ "sort_key": "seq", "sort_dir": "desc", "page_size": 2 }),
    );
    let pushed = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "repo:pushed")
        .expect("the push is logged");
    assert_eq!(pushed["actor"]["login"], "helper");
    assert_eq!(pushed["app"]["client_id"], s.client_id.as_str());

    // A private repository outside it does not exist.
    let out = git_output(src, clock, &["ls-remote", &remote("alice/own")]);
    assert!(!out.status.success());
    let out = git_output(src, clock, &["push", "-q", &remote("alice/own"), "main"]);
    assert!(!out.status.success());

    // A public one is fetched, as anyone fetches it, and not pushed to.
    git(src, clock, &["ls-remote", &remote("alice/pub")]);
    let out = git_output(src, clock, &["push", "-q", &remote("alice/pub"), "main"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("403"), "{stderr}");
    let branches = face_ok(
        "GET",
        &format!("{}/repos/alice/pub/branches", s.api),
        &s.alice,
        serde_json::json!(null),
    );
    assert_eq!(branches, serde_json::json!([]), "nothing was pushed");
}

#[tokio::test(flavor = "multi_thread")]
async fn graphql_shows_an_installation_token_what_it_reaches() {
    let s = setup().await;
    let clock = s.clock();
    let token = s.token(s.on_acme, None)["token"]
        .as_str()
        .unwrap()
        .to_string();

    // Alice opens a pull request in her private repository and in her
    // public one, neither of which acme's installation covers.
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# readme\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);
    git(src, clock, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("README.md"), "# changed\n").unwrap();
    git(src, clock, &["commit", "-q", "-a", "-m", "change"]);
    let mut ids = Vec::new();
    for repo in ["own", "pub"] {
        let remote = format!("{}/alice/{repo}.git", s.base)
            .replace("http://", &format!("http://x-access-token:{}@", s.alice));
        git(src, clock, &["push", "-q", &remote, "main", "feature"]);
        let pull = face_ok(
            "POST",
            &format!("{}/repos/alice/{repo}/pulls", s.api),
            &s.alice,
            serde_json::json!({ "title": repo, "head": "feature", "base": "main" }),
        );
        ids.push(pull["node_id"].as_str().unwrap().to_string());
    }
    let (private_id, public_id) = (&ids[0], &ids[1]);

    let graphql = |auth: &str, query: &str, variables: serde_json::Value| {
        let (status, _, body) = call(
            "POST",
            &format!("{}/api/graphql", s.base),
            &bearer(auth),
            Some(serde_json::json!({ "query": query, "variables": variables })),
        );
        assert_eq!(status, 200, "{body}");
        body
    };

    // A node in a private repository the token does not reach is null.
    let nodes = "query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest { title } } }";
    let variables = serde_json::json!({ "ids": [private_id, public_id] });
    let seen = graphql(&token, nodes, variables.clone());
    assert_eq!(
        seen["data"]["nodes"],
        serde_json::json!([null, { "title": "pub" }])
    );
    let seen = graphql(&s.alice, nodes, variables);
    assert_eq!(
        seen["data"]["nodes"],
        serde_json::json!([{ "title": "own" }, { "title": "pub" }])
    );

    // A search leaves it out.
    let search = "query { search(query: \"is:pr author:alice\", type: ISSUE, first: 10) { nodes { ... on PullRequest { title } } } }";
    let found = graphql(&token, search, serde_json::json!({}));
    assert_eq!(
        found["data"]["search"]["nodes"],
        serde_json::json!([{ "title": "pub" }])
    );
    let found = graphql(&s.alice, search, serde_json::json!({}));
    assert_eq!(
        found["data"]["search"]["nodes"].as_array().unwrap().len(),
        2
    );

    // And a mutation changes neither: the private one does not exist for
    // it, and the public one it may only read.
    let to_draft = "mutation($id: ID!) { convertPullRequestToDraft(input: {pullRequestId: $id}) { pullRequest { isDraft } } }";
    let refused = graphql(&token, to_draft, serde_json::json!({ "id": private_id }));
    assert!(
        refused["errors"][0]["message"]
            .as_str()
            .unwrap()
            .starts_with("Could not resolve to a node"),
        "{refused}"
    );
    let refused = graphql(&token, to_draft, serde_json::json!({ "id": public_id }));
    assert_eq!(
        refused["errors"][0]["message"],
        "Resource not accessible by integration"
    );
    let done = graphql(&s.alice, to_draft, serde_json::json!({ "id": private_id }));
    assert_eq!(
        done["data"]["convertPullRequestToDraft"]["pullRequest"]["isDraft"],
        true
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_is_an_oauth_client() {
    let s = setup().await;
    let http = agent();

    // GitHub's check of a client's credentials.
    let check = |id: &str, secret: &str| {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"));
        call(
            "POST",
            &format!("{}/applications/{}/token", s.api, s.client_id),
            &format!("Basic {basic}"),
            Some(serde_json::json!({ "access_token": "made-up" })),
        )
    };
    let (status, _, body) = check(&s.client_id, "wrong");
    assert_eq!((status, &body["message"]), (401, &"Bad credentials".into()));
    let (status, _, _) = check("orrfake_client_nobody", &s.client_secret);
    assert_eq!(status, 401);
    let (status, _, body) = check(&s.client_id, &s.client_secret);
    assert_eq!((status, &body["message"]), (404, &"Not Found".into()));
    // A secret issued since registration is the App's too.
    let issued = control_post(
        &s.base,
        &format!("/_orrery/oauth-apps/{}/secrets", s.client_id),
        serde_json::json!({}),
    );
    let (status, _, _) = check(&s.client_id, issued["client_secret"].as_str().unwrap());
    assert_eq!(status, 404);

    // The OAuth web flow takes the App's client id, and redirects to any of
    // its callback URLs: the first when the request names none.
    let authorize = |redirect_uri: Option<&str>| {
        let mut form = vec![("client_id", s.client_id.as_str()), ("login", "alice")];
        if let Some(uri) = redirect_uri {
            form.push(("redirect_uri", uri));
        }
        let granted = http
            .post(format!("{}/login/oauth/authorize", s.base))
            .send_form(form)
            .unwrap();
        assert_eq!(granted.status(), 302);
        granted.headers()["location"].to_str().unwrap().to_string()
    };
    assert!(authorize(None).starts_with("https://app.test/callback?code="));
    let back = authorize(Some("https://app.test/connect"));
    assert!(back.starts_with("https://app.test/connect?code="), "{back}");
    let back = authorize(Some("https://elsewhere.test/connect"));
    assert!(
        back.starts_with("https://app.test/callback?error=redirect_uri_mismatch"),
        "{back}"
    );

    // The code is traded for a token acting as the user, through the App.
    let code = authorize(Some("https://app.test/connect"));
    let code = code
        .split_once("code=")
        .unwrap()
        .1
        .split('&')
        .next()
        .unwrap();
    let mut traded = http
        .post(format!("{}/login/oauth/access_token", s.base))
        .header("accept", "application/json")
        .send_form([
            ("client_id", s.client_id.as_str()),
            ("client_secret", s.client_secret.as_str()),
            ("code", code),
        ])
        .unwrap();
    let traded: serde_json::Value = traded.body_mut().read_json().unwrap();
    let user_token = traded["access_token"].as_str().expect("a token");
    let me = face_ok(
        "GET",
        &format!("{}/user", s.api),
        user_token,
        serde_json::json!(null),
    );
    assert_eq!(me["login"], "alice");
    // The check of a token the App did issue answers the authorization it
    // stands for, which Orrery does not render.
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", s.client_id, s.client_secret));
    let (status, _, _) = call(
        "POST",
        &format!("{}/applications/{}/token", s.api, s.client_id),
        &format!("Basic {basic}"),
        Some(serde_json::json!({ "access_token": user_token })),
    );
    assert_eq!(status, 501);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_plane_registers_and_installs_apps() {
    let s = setup().await;
    let base = &s.base;

    // An App is read as it was registered, with its keys and no secret.
    let (status, app) = control(
        "GET",
        &format!("{base}/_orrery/apps/helper"),
        serde_json::json!(null),
    );
    assert_eq!(status, 200);
    assert_eq!(app["id"], s.app_id);
    assert_eq!(app["slug"], "helper");
    assert_eq!(app["owner"], "acme");
    assert_eq!(
        app["callback_urls"],
        serde_json::json!(["https://app.test/callback", "https://app.test/connect"])
    );
    assert_eq!(app["keys"].as_array().unwrap().len(), 1);
    assert!(!app.to_string().contains("orrfake_secret_"));
    let listed = control_post(base, "/_orrery/apps/list", serde_json::json!({}));
    assert_eq!(listed["total_count"], 1);
    assert_eq!(listed["items"][0], app);
    let (status, _) = control(
        "GET",
        &format!("{base}/_orrery/apps/nobody"),
        serde_json::json!(null),
    );
    assert_eq!(status, 404);

    // Its OAuth client is an OAuth app like any other.
    let oauth = control_post(base, "/_orrery/oauth-apps/list", serde_json::json!({}));
    assert_eq!(oauth["items"][0]["client_id"], s.client_id.as_str());

    // Its bot is an actor, made with it.
    let (status, bot) = control(
        "GET",
        &format!("{base}/_orrery/actors/helper"),
        serde_json::json!(null),
    );
    assert_eq!((status, &bot["kind"]), (200, &"bot".into()));

    // Installations are read as they were made.
    let on_alice = format!("{base}/_orrery/apps/helper/installations/{}", s.on_alice);
    let (status, installation) = control("GET", &on_alice, serde_json::json!(null));
    assert_eq!(status, 200);
    assert_eq!(
        installation,
        serde_json::json!({
            "id": s.on_alice,
            "app": "helper",
            "account": "alice",
            "repository_selection": "selected",
            "repositories": ["own"],
            "permissions": { "contents": "write", "members": "read", "metadata": "read" },
            "events": ["push", "pull_request"],
        })
    );
    let listed = control_post(
        base,
        "/_orrery/apps/helper/installations/list",
        serde_json::json!({ "page_size": 1 }),
    );
    assert_eq!(listed["total_count"], 2);
    assert_eq!(listed["items"][0]["account"], "acme");
    let next = control_post(
        base,
        "/_orrery/apps/helper/installations/list",
        serde_json::json!({ "page_size": 1, "page_token": listed["next_page_token"] }),
    );
    assert_eq!(next["items"][0], installation);
    assert!(next.get("next_page_token").is_none());
    for missing in ["999", "nope"] {
        let url = format!("{base}/_orrery/apps/helper/installations/{missing}");
        assert_eq!(control("GET", &url, serde_json::json!(null)).0, 404);
    }

    // A registration reports every field at fault.
    let (status, refused) = control(
        "POST",
        &format!("{base}/_orrery/apps"),
        serde_json::json!({
            "slug": "-bad-",
            "name": " ",
            "callback_urls": [],
            "permissions": { "Contents": "write", "metadata": "own" },
            "events": ["push", "push"],
            "extra": true,
        }),
    );
    assert_eq!(status, 400);
    let fields: Vec<_> = faults(&refused)
        .into_iter()
        .map(|(_, field)| field.unwrap())
        .collect();
    assert_eq!(
        fields,
        [
            "slug",
            "name",
            "owner",
            "callback_urls",
            "permissions",
            "events",
            "extra"
        ]
    );

    let register = |slug: &str, owner: &str| {
        control(
            "POST",
            &format!("{base}/_orrery/apps"),
            serde_json::json!({
                "slug": slug, "name": "Another", "owner": owner,
                "callback_urls": ["https://other.test/cb"],
            }),
        )
    };
    let (status, refused) = register("helper", "acme");
    assert_eq!(status, 409);
    assert_eq!(faults(&refused), [fault("ALREADY_EXISTS", Some("slug"))]);
    let (status, refused) = register("alice", "acme");
    assert_eq!(status, 409);
    assert_eq!(faults(&refused), [fault("ALREADY_EXISTS", Some("slug"))]);
    let (status, refused) = register("other", "helper");
    assert_eq!(status, 400);
    assert_eq!(faults(&refused), [fault("INVALID_ARGUMENT", Some("owner"))]);

    // A bot that has no App becomes the App registered under its login.
    let ci_token = mint_actor(base, "ci", "bot");
    let (status, ci) = register("ci", "alice");
    assert_eq!(status, 201, "{ci}");
    assert_eq!(ci["permissions"], serde_json::json!({}));
    assert_eq!(ci["events"], serde_json::json!([]));
    assert!(
        ci["client_secret"]
            .as_str()
            .unwrap()
            .starts_with("orrfake_secret_")
    );
    let bot = face_ok(
        "GET",
        &format!("{}/users/ci[bot]", s.api),
        &ci_token,
        serde_json::json!(null),
    );
    assert_eq!(bot["type"], "Bot");

    // Keys: the public half alone, once.
    let keys = format!("{base}/_orrery/apps/ci/keys");
    let key = app_key();
    let (status, added) = control(
        "POST",
        &keys,
        serde_json::json!({ "public_key": key.public_pem }),
    );
    assert_eq!(status, 201, "{added}");
    let (status, refused) = control(
        "POST",
        &keys,
        serde_json::json!({ "public_key": key.public_pem }),
    );
    assert_eq!(status, 409);
    assert_eq!(
        faults(&refused),
        [fault("ALREADY_EXISTS", Some("public_key"))]
    );
    let private = "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n";
    let (status, refused) = control("POST", &keys, serde_json::json!({ "public_key": private }));
    assert_eq!(status, 400);
    assert_eq!(
        faults(&refused),
        [fault("INVALID_ARGUMENT", Some("public_key"))]
    );
    assert!(
        refused["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("private key")
    );
    let nobody = format!("{base}/_orrery/apps/nobody/keys");
    assert_eq!(
        control(
            "POST",
            &nobody,
            serde_json::json!({ "public_key": key.public_pem })
        )
        .0,
        404
    );
    // A key signs for its own App alone.
    let ci_id = ci["id"].as_i64().unwrap();
    let (status, _, _) = call(
        "GET",
        &format!("{}/app", s.api),
        &bearer(&key.jwt(serde_json::json!(s.app_id), s.clock())),
        None,
    );
    assert_eq!(status, 401);
    let (status, _, read) = call(
        "GET",
        &format!("{}/app", s.api),
        &bearer(&key.jwt(serde_json::json!(ci_id), s.clock())),
        None,
    );
    assert_eq!((status, &read["slug"]), (200, &"ci".into()));

    // Installing.
    let install = |body: serde_json::Value| {
        control(
            "POST",
            &format!("{base}/_orrery/apps/ci/installations"),
            body,
        )
    };
    let (status, refused) = install(serde_json::json!({ "account": "nobody" }));
    assert_eq!(status, 400);
    assert_eq!(
        faults(&refused),
        [fault("INVALID_ARGUMENT", Some("account"))]
    );
    let (status, refused) = install(serde_json::json!({ "account": "ci" }));
    assert_eq!(status, 400);
    assert_eq!(
        faults(&refused),
        [fault("INVALID_ARGUMENT", Some("account"))]
    );
    for repositories in [
        serde_json::json!([]),
        serde_json::json!(["web"]),
        serde_json::json!(["own", "own"]),
    ] {
        let (status, refused) =
            install(serde_json::json!({ "account": "alice", "repositories": repositories }));
        assert_eq!(status, 400, "{repositories}");
        assert_eq!(
            faults(&refused),
            [fault("INVALID_ARGUMENT", Some("repositories"))]
        );
    }
    let (status, made) =
        install(serde_json::json!({ "account": "alice", "repositories": ["pub", "own"] }));
    assert_eq!(status, 201, "{made}");
    assert_eq!(made["repositories"], serde_json::json!(["own", "pub"]));
    let (status, refused) = install(serde_json::json!({ "account": "alice" }));
    assert_eq!(status, 409);
    assert_eq!(faults(&refused), [fault("ALREADY_EXISTS", Some("account"))]);
    let nobody = format!("{base}/_orrery/apps/nobody/installations");
    assert_eq!(
        control("POST", &nobody, serde_json::json!({ "account": "alice" })).0,
        404
    );

    // Another App's installation is not this one's.
    let (status, _, _) = call(
        "GET",
        &format!("{}/app/installations/{}", s.api, made["id"]),
        &s.jwt(),
        None,
    );
    assert_eq!(status, 404);
}
