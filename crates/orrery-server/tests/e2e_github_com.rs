//! End-to-end: a world served under github.com's layout. A request finds its
//! surface by the host it names, `api.github.com` or `github.com`, and every
//! URL the world returns names those hosts, whichever host the request came
//! through. Real git reaches the world as `github.com`, by name.

mod common;

use std::path::Path;

use common::{Instance, boot_github_com, control_post, face_answer, git, mint_actor};
use serde_json::{Value, json};

const API: &str = "api.github.com";
const WEB: &str = "github.com";

/// The world's listener, `127.0.0.1:<port>`.
fn listener(inst: &Instance) -> &str {
    inst.base.strip_prefix("http://").unwrap()
}

/// A call to `path` on the world's listener, naming `host`.
fn on(
    inst: &Instance,
    host: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (u16, Value) {
    face_answer(
        method,
        &format!("{}{path}", inst.base),
        token,
        &[("host", host)],
        body,
    )
}

/// `full_name`'s remote on github.com's web host, as git names it with a
/// token: `http://x-access-token:<token>@github.com:<port>/<full_name>.git`.
fn remote(inst: &Instance, token: &str, full_name: &str) -> String {
    let port = listener(inst).rsplit_once(':').unwrap().1;
    format!("http://x-access-token:{token}@{WEB}:{port}/{full_name}.git")
}

/// Real git with `github.com` resolved to the world's listener for this one
/// command, so a remote from [`remote`] reaches the world by that name.
fn git_on_github_com(inst: &Instance, cwd: &Path, args: &[&str]) -> String {
    let port = listener(inst).rsplit_once(':').unwrap().1;
    let resolve = format!("http.curloptResolve={WEB}:{port}:127.0.0.1");
    let mut all = vec!["-c", resolve.as_str()];
    all.extend_from_slice(args);
    git(cwd, inst.world.clock().unwrap(), &all)
}

/// Push `main`, then `feature` with one more commit, to `full_name`.
fn push_two_branches(inst: &Instance, token: &str, full_name: &str) {
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    let clock = inst.world.clock().unwrap();
    let remote = remote(inst, token, full_name);
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# readme\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);
    git_on_github_com(inst, src, &["push", "-q", &remote, "main"]);
    git(src, clock, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("change.txt"), "a change\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "a change"]);
    git_on_github_com(inst, src, &["push", "-q", &remote, "feature"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_graphql_and_git_answer_on_github_coms_hosts_with_its_urls() {
    let inst = boot_github_com().await;
    let alice = mint_actor(&inst.base, "alice", "user");

    let (status, repo) = on(
        &inst,
        API,
        "POST",
        "/user/repos",
        &alice,
        Some(json!({ "name": "webapp" })),
    );
    assert_eq!(status, 201, "{repo}");
    assert_eq!(repo["url"], "https://api.github.com/repos/alice/webapp");
    assert_eq!(repo["html_url"], "https://github.com/alice/webapp");
    assert_eq!(repo["clone_url"], "https://github.com/alice/webapp.git");
    assert_eq!(repo["ssh_url"], "git@github.com:alice/webapp.git");
    assert_eq!(repo["git_url"], "git://github.com/alice/webapp.git");

    push_two_branches(&inst, &alice, "alice/webapp");
    let work = tempfile::tempdir().unwrap();
    let refs = git_on_github_com(
        &inst,
        work.path(),
        &[
            "ls-remote",
            "--heads",
            &remote(&inst, &alice, "alice/webapp"),
        ],
    );
    assert!(refs.contains("refs/heads/feature"), "{refs}");

    let (status, pull) = on(
        &inst,
        API,
        "POST",
        "/repos/alice/webapp/pulls",
        &alice,
        Some(json!({ "title": "A change", "head": "feature", "base": "main" })),
    );
    assert_eq!(status, 201, "{pull}");
    assert_eq!(
        pull["url"],
        "https://api.github.com/repos/alice/webapp/pulls/1"
    );
    assert_eq!(pull["html_url"], "https://github.com/alice/webapp/pull/1");
    assert_eq!(
        pull["issue_url"],
        "https://api.github.com/repos/alice/webapp/issues/1"
    );

    let (status, answer) = on(
        &inst,
        API,
        "POST",
        "/graphql",
        &alice,
        Some(json!({
            "query": "query($id: ID!) { node(id: $id) { ... on PullRequest { url } } }",
            "variables": { "id": pull["node_id"] },
        })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["data"]["node"]["url"], "https://github.com/alice/webapp/pull/1",
        "{answer}"
    );

    // The world's own host keeps the GitHub Enterprise Server paths, for a
    // router or relay in front of it, and its URLs are still github.com's.
    let (status, own) = on(
        &inst,
        listener(&inst),
        "GET",
        "/api/v3/repos/alice/webapp",
        &alice,
        None,
    );
    assert_eq!(status, 200, "{own}");
    assert_eq!(own["url"], "https://api.github.com/repos/alice/webapp");
}

#[tokio::test(flavor = "multi_thread")]
async fn what_github_com_does_not_serve_is_not_mocked() {
    let inst = boot_github_com().await;
    let alice = mint_actor(&inst.base, "alice", "user");
    let own = listener(&inst).to_string();
    // Each answers the not-mocked 501 naming the path as it was sent, and
    // the host when the host is what decides.
    let cases: [(&str, &str, Option<&str>); 8] = [
        // github.com's `/meta` names no release, through any host.
        (API, "/meta", None),
        (&own, "/api/v3/meta", None),
        // An unmatched REST path is named as it was sent, not as served.
        (API, "/rate_limit", None),
        // REST, GraphQL, and the control plane are not on github.com's web
        // host, though the world's own host carries them.
        (WEB, "/api/v3/user", Some(WEB)),
        (WEB, "/api/graphql", Some(WEB)),
        (WEB, "/_orrery/healthz", Some(WEB)),
        // GitHub's other hosts are not served.
        (
            "codeload.github.com",
            "/alice/webapp/tar.gz/main",
            Some("codeload.github.com"),
        ),
        (
            "raw.githubusercontent.com",
            "/alice/webapp/main/README.md",
            Some("raw.githubusercontent.com"),
        ),
    ];
    for (host, path, named_host) in cases {
        let (status, answer) = on(&inst, host, "GET", path, &alice, None);
        assert_eq!(status, 501, "{host} {path}: {answer}");
        assert_eq!(answer["orrery"]["not_mocked"], true, "{host} {path}");
        assert_eq!(answer["orrery"]["path"], path, "{host} {path}");
        match named_host {
            Some(named) => assert_eq!(answer["orrery"]["host"], named, "{host} {path}"),
            None => assert!(answer["orrery"].get("host").is_none(), "{host} {path}"),
        }
    }

    // A refusal the host decides comes before any token is read, and names
    // the host by its name alone, in lower case.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    for (host, path, named) in [
        (
            "codeload.github.com",
            "/alice/webapp/tar.gz/main",
            "codeload.github.com",
        ),
        (
            "CodeLoad.GitHub.com.:443",
            "/alice/webapp/tar.gz/main",
            "codeload.github.com",
        ),
        ("github.com", "/_orrery/healthz", "github.com"),
        ("github.com.", "/api/v3/user", "github.com"),
    ] {
        let mut resp = agent
            .get(&format!("{}{path}", inst.base))
            .header("host", host)
            .call()
            .unwrap();
        let answer: Value = resp.body_mut().read_json().unwrap();
        assert_eq!(resp.status().as_u16(), 501, "{host} {path}: {answer}");
        assert_eq!(answer["orrery"]["host"], named, "{host} {path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_webhook_body_names_github_coms_hosts() {
    let inst = boot_github_com().await;
    let alice = mint_actor(&inst.base, "alice", "user");
    mint_actor(&inst.base, "acme", "org");
    let (status, repo) = on(
        &inst,
        API,
        "POST",
        "/orgs/acme/repos",
        &alice,
        Some(json!({ "name": "web" })),
    );
    assert_eq!(status, 201, "{repo}");
    control_post(
        &inst.base,
        "/_orrery/apps",
        json!({
            "slug": "helper",
            "name": "Helper",
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
            "permissions": { "contents": "write", "pull_requests": "write" },
            "events": ["push", "pull_request"],
            "webhook_url": "https://receiver.test/hook",
            "webhook_active": true,
        }),
    );
    control_post(
        &inst.base,
        "/_orrery/apps/helper/installations",
        json!({ "account": "acme" }),
    );
    push_two_branches(&inst, &alice, "acme/web");

    // The default options make deliveries and send none; each is in the log
    // with its body as it would be sent.
    let log = control_post(&inst.base, "/_orrery/log/list", json!({ "page_size": 200 }));
    let deliveries: Vec<(&Value, String)> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "delivery:created")
        .map(|e| {
            (
                &e["object"]["event"],
                e["object"]["body"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(deliveries.iter().any(|(event, _)| *event == "push"));
    for (event, body) in &deliveries {
        assert!(!body.contains(listener(&inst)), "{body}");
        if *event != "push" {
            continue;
        }
        let push: Value = serde_json::from_str(body).unwrap();
        assert_eq!(push["repository"]["url"], "https://github.com/acme/web");
        assert_eq!(
            push["repository"]["clone_url"],
            "https://github.com/acme/web.git"
        );
        assert!(
            push["compare"]
                .as_str()
                .unwrap()
                .starts_with("https://github.com/acme/web/compare/"),
            "{body}"
        );
    }
}
