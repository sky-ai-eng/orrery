//! End-to-end: every URL the face renders starts at the root its kind names.
//! A world is seeded so that each place the face renders a URL answers at
//! least once: REST answers, `Link` headers, GraphQL, and stored webhook
//! bodies. Then every URL in them is checked by the key it is under: an
//! `html_url`, `clone_url`, `diff_url`, `patch_url`, or `external_url` is a
//! page, at the web root, as are `_links.html`, GraphQL's `url`, and a push's
//! repository and commits; any other URL starts at the REST root.

mod common;

use std::collections::BTreeSet;

use common::{Instance, app_key, boot_github_com_with, boot_with, control_post, git, mint_actor};
use serde_json::{Value, json};

/// Where URLs start: the REST root, the web root, and the web host.
struct Roots {
    api: String,
    web: String,
    host: String,
}

/// One answer: its status, its `Link` header, and its body.
struct Answer {
    status: u16,
    link: Option<String>,
    body: Value,
}

fn call(method: &str, url: &str, auth: &str, body: Option<Value>) -> Answer {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let request = ureq::http::Request::builder()
        .method(method)
        .uri(url)
        .header("authorization", auth)
        .header("content-type", "application/json");
    let mut resp = agent
        .run(
            request
                .body(body.map(|b| b.to_string()).unwrap_or_default())
                .unwrap(),
        )
        .unwrap_or_else(|e| panic!("{method} {url}: {e}"));
    let link = resp
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string());
    let text = resp.body_mut().read_to_string().unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{method} {url}: {e}: {text}"))
    };
    Answer {
        status: resp.status().as_u16(),
        link,
        body,
    }
}

/// What the seeded world rendered, labeled by where it came from.
#[derive(Default)]
struct Rendered {
    rest: Vec<(String, Value)>,
    links: Vec<(String, String)>,
    graphql: Vec<(String, Value)>,
}

impl Rendered {
    /// Keep an answer, which must have `status`.
    fn keep(&mut self, what: &str, status: u16, answer: Answer) -> Value {
        assert_eq!(answer.status, status, "{what}: {}", answer.body);
        if let Some(link) = answer.link {
            self.links.push((what.to_string(), link));
        }
        self.rest.push((what.to_string(), answer.body.clone()));
        answer.body
    }
}

/// Seed a world that renders every kind of URL, through the REST paths at
/// `rest` and the GraphQL endpoint at `graphql`, and gather what it rendered.
fn render_everything(inst: &Instance, rest: &str, graphql: &str) -> Rendered {
    let base = &inst.base;
    let mut r = Rendered::default();
    let alice = mint_actor(base, "alice", "user");
    let bob = mint_actor(base, "bob", "user");
    mint_actor(base, "acme", "org");
    let as_alice = format!("token {alice}");
    let get = |path: &str, auth: &str| call("GET", &format!("{rest}{path}"), auth, None);
    let send = |method: &str, path: &str, auth: &str, body: Value| {
        call(method, &format!("{rest}{path}"), auth, Some(body))
    };

    r.keep(
        "PUT org membership",
        200,
        send(
            "PUT",
            "/orgs/acme/memberships/alice",
            &as_alice,
            json!({ "role": "admin" }),
        ),
    );
    let repo = r.keep(
        "POST org repo",
        201,
        send(
            "POST",
            "/orgs/acme/repos",
            &as_alice,
            json!({ "name": "web" }),
        ),
    );

    // The App that receives the deliveries, installed before the push so it
    // is owed one for it.
    let app = control_post(
        base,
        "/_orrery/apps",
        json!({
            "slug": "helper",
            "name": "Helper",
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
            "permissions": {
                "checks": "write", "contents": "write", "pull_requests": "write",
                "vulnerability_alerts": "read",
            },
            "events": ["push", "pull_request", "pull_request_review", "issue_comment", "check_run", "check_suite", "dependabot_alert"],
            "webhook_url": RECEIVER,
            "webhook_active": true,
        }),
    );
    let key = app_key();
    control_post(
        base,
        "/_orrery/apps/helper/keys",
        json!({ "public_key": key.public_pem }),
    );
    let installation = control_post(
        base,
        "/_orrery/apps/helper/installations",
        json!({ "account": "acme" }),
    )["id"]
        .as_i64()
        .unwrap();
    // A second installation and a second repository, so that each list
    // pages and sends a `Link`.
    control_post(
        base,
        "/_orrery/apps/helper/installations",
        json!({ "account": "alice" }),
    );
    r.keep(
        "POST second org repo",
        201,
        send(
            "POST",
            "/orgs/acme/repos",
            &as_alice,
            json!({ "name": "api" }),
        ),
    );
    let jwt = || {
        let now = inst.world.clock().unwrap();
        format!("Bearer {}", key.jwt(json!(app["id"]), now))
    };
    let issued = r.keep(
        "POST installation token",
        201,
        call(
            "POST",
            &format!("{rest}/app/installations/{installation}/access_tokens"),
            &jwt(),
            None,
        ),
    );
    let as_app = format!("token {}", issued["token"].as_str().unwrap());

    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    let clock = inst.world.clock().unwrap();
    // Through the world's own host, which serves git under either layout.
    let remote = authed(&format!("{base}/acme/web.git"), &alice);
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# web\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);
    git(src, clock, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("change.txt"), "a change\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "a change"]);
    git(src, clock, &["push", "-q", &remote, "main", "feature"]);
    let head = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();

    let pull = r.keep(
        "POST pull",
        201,
        send(
            "POST",
            "/repos/acme/web/pulls",
            &as_alice,
            json!({ "title": "A change", "head": "feature", "base": "main" }),
        ),
    );
    r.keep(
        "POST review",
        200,
        send(
            "POST",
            "/repos/acme/web/pulls/1/reviews",
            &as_alice,
            json!({ "event": "COMMENT", "body": "looks fine" }),
        ),
    );
    r.keep(
        "POST comment",
        201,
        send(
            "POST",
            "/repos/acme/web/issues/1/comments",
            &as_alice,
            json!({ "body": "a comment" }),
        ),
    );
    r.keep(
        "POST check run",
        201,
        send(
            "POST",
            "/repos/acme/web/check-runs",
            &as_app,
            json!({ "name": "build", "head_sha": head, "status": "completed", "conclusion": "success" }),
        ),
    );
    r.keep(
        "POST team",
        201,
        send(
            "POST",
            "/orgs/acme/teams",
            &as_alice,
            json!({ "name": "Platform" }),
        ),
    );
    r.keep(
        "PUT org membership for bob",
        200,
        send("PUT", "/orgs/acme/memberships/bob", &as_alice, json!({})),
    );
    r.keep(
        "PUT team membership",
        200,
        send(
            "PUT",
            "/orgs/acme/teams/platform/memberships/bob",
            &as_alice,
            json!({}),
        ),
    );
    r.keep(
        "PUT vulnerability alerts",
        204,
        send(
            "PUT",
            "/repos/acme/web/vulnerability-alerts",
            &as_alice,
            json!({}),
        ),
    );
    for package in ["lodash", "minimist"] {
        control_post(
            base,
            "/_orrery/repos/acme/web/dependabot-alerts",
            json!({
                "ecosystem": "npm", "package": package, "manifest_path": "package-lock.json",
                "ghsa_id": format!("GHSA-{package}"), "summary": "a flaw",
                "severity": "high", "vulnerable_version_range": "< 9",
            }),
        );
    }

    // Deliveries are listed once they are attempted.
    for _ in 0..100 {
        let listed = get("/app/hook/deliveries", &jwt());
        if listed.body.as_array().is_some_and(|d| d.len() >= 2) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let repo_id = repo["id"].as_i64().unwrap();
    for (what, path, auth) in [
        ("GET repo", "/repos/acme/web".to_string(), as_alice.clone()),
        (
            "GET repo by id",
            format!("/repositories/{repo_id}"),
            as_alice.clone(),
        ),
        (
            "GET pulls",
            "/repos/acme/web/pulls".to_string(),
            as_alice.clone(),
        ),
        (
            "GET pull",
            "/repos/acme/web/pulls/1".to_string(),
            as_alice.clone(),
        ),
        (
            "GET reviews",
            "/repos/acme/web/pulls/1/reviews".to_string(),
            as_alice.clone(),
        ),
        (
            "GET comments",
            "/repos/acme/web/issues/1/comments".to_string(),
            as_alice.clone(),
        ),
        (
            "GET check runs",
            format!("/repos/acme/web/commits/{head}/check-runs"),
            as_alice.clone(),
        ),
        (
            "GET check suites",
            format!("/repos/acme/web/commits/{head}/check-suites"),
            as_alice.clone(),
        ),
        (
            "GET commit",
            format!("/repos/acme/web/commits/{head}"),
            as_alice.clone(),
        ),
        (
            "GET ref",
            "/repos/acme/web/git/ref/heads/main".to_string(),
            as_alice.clone(),
        ),
        (
            "GET contents",
            "/repos/acme/web/contents/README.md".to_string(),
            as_alice.clone(),
        ),
        (
            "GET alerts",
            "/repos/acme/web/dependabot/alerts?per_page=1".to_string(),
            as_alice.clone(),
        ),
        ("GET org", "/orgs/acme".to_string(), as_alice.clone()),
        (
            "GET org teams",
            "/orgs/acme/teams".to_string(),
            as_alice.clone(),
        ),
        (
            "GET org membership",
            "/orgs/acme/memberships/bob".to_string(),
            as_alice.clone(),
        ),
        ("GET user", "/user".to_string(), as_alice.clone()),
        (
            "GET user by login",
            "/users/bob".to_string(),
            format!("token {bob}"),
        ),
        ("GET app", "/app".to_string(), jwt()),
        (
            "GET app installations",
            "/app/installations?per_page=1".to_string(),
            jwt(),
        ),
        (
            "GET installation repositories",
            "/installation/repositories?per_page=1".to_string(),
            as_app.clone(),
        ),
        (
            "GET deliveries",
            "/app/hook/deliveries?per_page=1".to_string(),
            jwt(),
        ),
    ] {
        let answer = get(&path, &auth);
        r.keep(what, 200, answer);
    }

    let node = pull["node_id"].as_str().unwrap();
    let answer = call(
        "POST",
        graphql,
        &as_alice,
        Some(json!({
            "query": "query($id: ID!) { node(id: $id) { ... on PullRequest { url } } }",
            "variables": { "id": node },
        })),
    );
    assert_eq!(answer.status, 200, "{}", answer.body);
    r.graphql.push(("GraphQL pull".to_string(), answer.body));

    let log = control_post(base, "/_orrery/log/list", json!({ "page_size": 500 }));
    for event in log["items"].as_array().unwrap() {
        if event["type"] == "delivery:created" {
            let what = format!("delivery {}", event["object"]["event"]);
            let body = serde_json::from_str(event["object"]["body"].as_str().unwrap()).unwrap();
            r.rest.push((what, body));
        }
    }
    r
}

/// `url` with `token` as git's `x-access-token` password.
fn authed(url: &str, token: &str) -> String {
    url.replacen("://", &format!("://x-access-token:{token}@"), 1)
}

/// Every URL in `value`, as `(key, url)`, under the keys it sits at.
fn urls(value: &Value, key: &str, path: &str, out: &mut Vec<(String, String, String)>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                urls(v, k, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                urls(v, key, &format!("{path}[{i}]"), out);
            }
        }
        Value::String(s)
            if s.starts_with("http://")
                || s.starts_with("https://")
                || s.starts_with("git://")
                || s.starts_with("git@") =>
        {
            out.push((key.to_string(), path.to_string(), s.clone()));
        }
        _ => {}
    }
}

/// Where the App's webhook points: a port nothing listens on, so each
/// delivery is attempted, fails, and is listed.
const RECEIVER: &str = "http://127.0.0.1:1/hook";

/// URLs a test chose and the world only stores.
fn chosen(url: &str) -> bool {
    url.starts_with("https://app.test/") || url == RECEIVER
}

/// Check every URL in `rendered` against `roots`, and that the world
/// rendered each kind this test means to reach.
fn check(rendered: &Rendered, roots: &Roots) {
    let at = |root: &str, url: &str| url == root || url.starts_with(&format!("{root}/"));
    // Under GitHub Enterprise Server's layout the REST root is below the web
    // root, so a page is at the web root and not at the REST root.
    let page = |url: &str| at(&roots.web, url) && !at(&roots.api, url);
    let mut keys = BTreeSet::new();
    for (what, body) in &rendered.rest {
        let mut found = Vec::new();
        urls(body, "", "", &mut found);
        for (key, path, url) in found {
            if chosen(&url) {
                continue;
            }
            // A push payload names its repository and its commits by their
            // pages, as GitHub's does.
            if what == "delivery \"push\""
                && (path == ".repository.url"
                    || path == ".compare"
                    || path == ".head_commit.url"
                    || (path.starts_with(".commits[") && path.ends_with("].url")))
            {
                assert!(page(&url), "{what} {path}: {url}");
                continue;
            }
            // `_links.html` is a page; every other link is a REST URL.
            if path.ends_with("._links.html.href") {
                assert!(page(&url), "{what} {path}: {url}");
                continue;
            }
            keys.insert(key.clone());
            match key.as_str() {
                // An App's `external_url` is its page, as the world has no
                // site of the App's own to name.
                "html_url" | "clone_url" | "external_url" | "diff_url" | "patch_url" => {
                    assert!(page(&url), "{what} {path}: {url}")
                }
                // Avatars are never served, at a host that can't resolve.
                "avatar_url" => assert!(
                    url.starts_with("https://orrery.invalid/avatars/"),
                    "{what} {path}: {url}"
                ),
                "git_url" => assert!(
                    url.starts_with(&format!("git://{}/", roots.host)),
                    "{what} {path}: {url}"
                ),
                "ssh_url" => assert!(
                    url.starts_with(&format!("git@{}:", roots.host)),
                    "{what} {path}: {url}"
                ),
                _ => assert!(at(&roots.api, &url), "{what} {path}: {url}"),
            }
        }
    }
    for (what, link) in &rendered.links {
        for target in link.split(',').filter_map(|l| {
            let l = l.trim();
            l.strip_prefix('<')
                .and_then(|l| l.split_once('>'))
                .map(|(u, _)| u)
        }) {
            assert!(at(&roots.api, target), "{what} Link: {target}");
        }
    }
    for (what, body) in &rendered.graphql {
        let mut found = Vec::new();
        urls(body, "", "", &mut found);
        assert!(!found.is_empty(), "{what}: {body}");
        for (_, path, url) in found {
            assert!(page(&url), "{what} {path}: {url}");
        }
    }
    for key in [
        "url",
        "html_url",
        "clone_url",
        "git_url",
        "ssh_url",
        "issue_url",
        "pull_request_url",
        "repos_url",
        "members_url",
        "organization_url",
        "access_tokens_url",
        "repositories_url",
        "check_runs_url",
        "hooks_url",
    ] {
        assert!(keys.contains(key), "no {key} was rendered: {keys:?}");
    }
    let linked: BTreeSet<&str> = rendered.links.iter().map(|(w, _)| w.as_str()).collect();
    for what in [
        "GET alerts",
        "GET app installations",
        "GET installation repositories",
        "GET deliveries",
    ] {
        assert!(linked.contains(what), "{what} sent no Link: {linked:?}");
    }
}

fn sending() -> orrery_server::Options {
    orrery_server::Options {
        outbound: Some(
            orrery_outbound::Client::new(orrery_outbound::Egress::Direct, None).unwrap(),
        ),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn enterprise_server_urls_start_at_base_url() {
    let inst = boot_with(sending()).await;
    let rest = format!("{}/api/v3", inst.base);
    let graphql = format!("{}/api/graphql", inst.base);
    let rendered = render_everything(&inst, &rest, &graphql);
    let host = inst
        .base
        .strip_prefix("http://")
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .0
        .to_string();
    check(
        &rendered,
        &Roots {
            api: rest.clone(),
            web: inst.base.clone(),
            host,
        },
    );
}

/// A page's form posts below the base URL's path, where every URL the world
/// returns starts, so a world reached through a proxy that serves it under a
/// path gets each form back.
#[tokio::test(flavor = "multi_thread")]
async fn a_pages_form_posts_below_the_base_urls_path() {
    let dir = tempfile::tempdir().unwrap();
    let world = std::sync::Arc::new(
        orrery_world::World::create(
            &dir.path().join("world"),
            orrery_world::DEFAULT_T0_UNIX,
            &orrery_world::WorldSecret::LOCAL,
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = orrery_server::routers(
        world,
        orrery_server::Layout::enterprise_server(&format!("{origin}/prefix")),
        dir.path().join("snapshots"),
        orrery_server::Options::default(),
    )
    .combined();
    tokio::spawn(orrery_server::run(listener, app, std::future::pending()));

    mint_actor(&origin, "acme", "org");
    let oauth = control_post(
        &origin,
        "/_orrery/oauth-apps",
        json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
    );
    control_post(
        &origin,
        "/_orrery/apps",
        json!({
            "slug": "helper",
            "name": "Helper",
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
            "permissions": { "contents": "read" },
            "events": [],
        }),
    );
    let page = |path: &str| {
        let mut resp = ureq::get(&format!("{origin}{path}")).call().unwrap();
        resp.body_mut().read_to_string().unwrap()
    };
    let authorize = page(&format!(
        "/login/oauth/authorize?client_id={}",
        oauth["client_id"].as_str().unwrap()
    ));
    assert!(
        authorize.contains(r#"action="/prefix/login/oauth/authorize""#),
        "{authorize}"
    );
    let install = page("/apps/helper/installations/new");
    assert!(
        install.contains(r#"action="/prefix/apps/helper/installations/new""#),
        "{install}"
    );
}

/// Under github.com's layout every URL names github.com's hosts, though each
/// request came through the world's own host at the GitHub Enterprise Server
/// paths, as a relay in front of the world sends it.
#[tokio::test(flavor = "multi_thread")]
async fn github_com_urls_name_github_coms_hosts() {
    let inst = boot_github_com_with(sending()).await;
    let rest = format!("{}/api/v3", inst.base);
    let graphql = format!("{}/api/graphql", inst.base);
    let rendered = render_everything(&inst, &rest, &graphql);
    check(
        &rendered,
        &Roots {
            api: "https://api.github.com".to_string(),
            web: "https://github.com".to_string(),
            host: "github.com".to_string(),
        },
    );
}
