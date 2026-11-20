//! End-to-end: a world served as github.com's hosts over TLS, which real git
//! and real gh reach unmodified. Through the HTTPS proxy they name
//! `https://github.com` and `https://api.github.com` themselves; on the TLS
//! listener, git resolves `github.com` to it. Each trusts the test authority
//! alone, and nothing reaches the network.

mod common;

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Output};

use common::{GithubTls, Instance, boot_github_com_tls, face_answer, git_command, mint_actor};
use serde_json::{Value, json};

/// Real git, trusting the test authority alone, with `config` beside it.
fn git_tls(
    inst: &Instance,
    tls: &GithubTls,
    cwd: &Path,
    config: &[String],
    args: &[&str],
) -> Output {
    let mut command = git_command(cwd, inst.world.clock().unwrap());
    command
        .arg("-c")
        .arg(format!("http.sslCAInfo={}", tls.ca_pem.display()));
    for c in config {
        command.arg("-c").arg(c);
    }
    command.args(args).output().expect("spawn git")
}

fn ok(out: Output, what: &str) -> String {
    assert!(
        out.status.success(),
        "{what} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Real gh through the proxy, trusting the test authority alone, as `token`.
fn gh(tls: &GithubTls, token: &str, args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    Command::new("gh")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("GH_CONFIG_DIR", home.path().join("gh"))
        .env("GH_TOKEN", token)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("HTTPS_PROXY", format!("http://{}", tls.proxy))
        .env("SSL_CERT_FILE", &tls.ca_pem)
        .args(args)
        .output()
        .expect("spawn gh: the TLS tests need gh on PATH")
}

/// Create `alice/webapp` with `main` and `feature`, pushed through the
/// proxy; answer alice's token.
fn seeded(inst: &Instance, tls: &GithubTls) -> String {
    let alice = mint_actor(&inst.base, "alice", "user");
    let (status, repo) = face_answer(
        "POST",
        &format!("{}/user/repos", inst.base),
        &alice,
        &[("host", "api.github.com")],
        Some(json!({ "name": "webapp" })),
    );
    assert_eq!(status, 201, "{repo}");
    assert_eq!(repo["clone_url"], "https://github.com/alice/webapp.git");

    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    let proxy = vec![format!("http.proxy=http://{}", tls.proxy)];
    let remote = format!("https://x-access-token:{alice}@github.com/alice/webapp.git");
    let run = |args: &[&str]| {
        ok(
            git_tls(inst, tls, src, &proxy, args),
            &format!("git {args:?}"),
        )
    };
    run(&["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# readme\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
    run(&["push", "-q", &remote, "main"]);
    run(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("change.txt"), "a change\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "a change"]);
    run(&["push", "-q", &remote, "feature"]);
    alice
}

#[tokio::test(flavor = "multi_thread")]
async fn git_pushes_and_clones_through_the_proxy() {
    let (inst, tls) = boot_github_com_tls().await;
    let alice = seeded(&inst, &tls);

    let work = tempfile::tempdir().unwrap();
    let proxy = vec![format!("http.proxy=http://{}", tls.proxy)];
    let remote = format!("https://x-access-token:{alice}@github.com/alice/webapp.git");
    ok(
        git_tls(
            &inst,
            &tls,
            work.path(),
            &proxy,
            &["clone", "-q", &remote, "webapp"],
        ),
        "git clone",
    );
    let log = ok(
        git_tls(
            &inst,
            &tls,
            &work.path().join("webapp"),
            &[],
            &["log", "--format=%s", "origin/feature"],
        ),
        "git log",
    );
    assert_eq!(log, "a change\ninit\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn git_reaches_the_tls_listener_by_name() {
    let (inst, tls) = boot_github_com_tls().await;
    let alice = seeded(&inst, &tls);

    let port = tls.direct.rsplit_once(':').unwrap().1;
    let resolve = vec![format!("http.curloptResolve=github.com:{port}:127.0.0.1")];
    let remote = format!("https://x-access-token:{alice}@github.com:{port}/alice/webapp.git");
    let work = tempfile::tempdir().unwrap();
    let refs = ok(
        git_tls(
            &inst,
            &tls,
            work.path(),
            &resolve,
            &["ls-remote", "--heads", &remote],
        ),
        "git ls-remote",
    );
    assert!(refs.contains("refs/heads/main"), "{refs}");
    assert!(refs.contains("refs/heads/feature"), "{refs}");
}

#[tokio::test(flavor = "multi_thread")]
async fn gh_calls_rest_and_graphql_through_the_proxy() {
    let (inst, tls) = boot_github_com_tls().await;
    let alice = seeded(&inst, &tls);

    let user: Value =
        serde_json::from_str(&ok(gh(&tls, &alice, &["api", "/user"]), "gh api /user")).unwrap();
    assert_eq!(user["login"], "alice");
    assert_eq!(user["url"], "https://api.github.com/users/alice");

    let pull: Value = serde_json::from_str(&ok(
        gh(
            &tls,
            &alice,
            &[
                "api",
                "/repos/alice/webapp/pulls",
                "-f",
                "title=A change",
                "-f",
                "head=feature",
                "-f",
                "base=main",
            ],
        ),
        "gh api pulls",
    ))
    .unwrap();
    assert_eq!(pull["html_url"], "https://github.com/alice/webapp/pull/1");

    let node = pull["node_id"].as_str().unwrap();
    let answer: Value = serde_json::from_str(&ok(
        gh(
            &tls,
            &alice,
            &[
                "api",
                "graphql",
                "-f",
                "query=query($id: ID!) { node(id: $id) { ... on PullRequest { url } } }",
                "-F",
                &format!("id={node}"),
            ],
        ),
        "gh api graphql",
    ))
    .unwrap();
    assert_eq!(
        answer["data"]["node"]["url"], "https://github.com/alice/webapp/pull/1",
        "{answer}"
    );
}

/// The raw answer to `request` sent to the proxy.
fn ask_proxy(tls: &GithubTls, request: &str) -> (String, Value) {
    let mut stream = TcpStream::connect(&tls.proxy).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let status_line = head.lines().next().unwrap().to_string();
    if status_line.contains(" 405 ") {
        assert!(head.lines().any(|l| l == "allow: CONNECT"), "{head}");
    }
    (status_line, serde_json::from_str(body).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_proxy_refuses_what_is_not_a_tunnel_to_github() {
    let (_inst, tls) = boot_github_com_tls().await;
    for target in ["example.com:443", "github.com:22", "notgithub.com:443"] {
        let (status, body) = ask_proxy(
            &tls,
            &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
        );
        assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}: {body}");
        assert_eq!(body["orrery"]["proxy_refused"], true, "{target}");
        assert_eq!(body["orrery"]["target"], target);
        assert!(
            body["message"].as_str().unwrap().contains("NO_PROXY"),
            "{body}"
        );
    }
    // A GitHub host the world's certificate does not name would fail TLS in
    // the tunnel, so the proxy refuses it and says why.
    let (status, body) = ask_proxy(
        &tls,
        "CONNECT objects.githubusercontent.com:443 HTTP/1.1\r\nHost: objects.githubusercontent.com:443\r\n\r\n",
    );
    assert_eq!(status, "HTTP/1.1 403 Forbidden", "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("does not name objects.githubusercontent.com"),
        "{body}"
    );
    let (status, body) = ask_proxy(
        &tls,
        "GET http://api.github.com/user HTTP/1.1\r\nHost: api.github.com\r\n\r\n",
    );
    assert_eq!(status, "HTTP/1.1 405 Method Not Allowed", "{body}");
    assert_eq!(body["orrery"]["proxy_refused"], true);
}
