//! End-to-end: `orrery relay` in front of a world that another process,
//! `orrery serve` under the github.com layout, serves on its own address.
//! Real git and real gh reach the world through the relay as
//! `https://github.com` and `https://api.github.com`, trusting the relay's
//! test authority alone, and nothing reaches the network.

mod common;

use std::io::{BufRead, BufReader, Read, Write as _};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::{
    Ready, Served, certificate_files, git_command, github_certificate, mint, refused, serve,
    start_reading, tls_client, write_config,
};
use serde_json::Value;

/// The line `orrery relay` prints on stdout once it is ready.
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct RelayReady {
    world: String,
    #[serde(default)]
    proxy_listen: Option<String>,
    #[serde(default)]
    tls_listen: Option<String>,
}

/// The variables that would send a client somewhere else, or trust another
/// authority, which the operator's environment may hold.
const CLIENT_NETWORK_VARS: [&str; 15] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "GIT_SSL_CAINFO",
    "GIT_SSL_CAPATH",
    "GIT_SSL_NO_VERIFY",
    "GIT_SSL_CERT",
    "GIT_SSL_KEY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// A world under the github.com layout in `dir`, its faces and control plane
/// on one plain listener, with `server` added to its `[server]` table.
fn world(dir: &Path, server: &str) -> (Served, Ready) {
    std::fs::create_dir_all(dir).unwrap();
    let config = write_config(
        dir,
        "",
        &format!("listen = \"127.0.0.1:0\"\nlayout = \"github.com\"\n{server}"),
    );
    serve(&config, &[])
}

fn orrery_relay(args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orrery"));
    cmd.arg("relay")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// The relay's certificate, for GitHub's two hosts and the loopback address,
/// from an authority of its own in `dir`; the flags that name it.
fn relay_certificate(dir: &Path) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    let (cert, key) = certificate_files(dir, &["github.com", "api.github.com", "127.0.0.1"]);
    vec![
        "--tls-cert".to_string(),
        cert.display().to_string(),
        "--tls-key".to_string(),
        key.display().to_string(),
    ]
}

/// Start a relay to `world_url` with its certificate in `dir` and `args`
/// beside it, and read its ready line.
fn relay(dir: &Path, world_url: &str, args: &[&str]) -> (Served, RelayReady) {
    let mut all = vec!["--world".to_string(), world_url.to_string()];
    all.extend(relay_certificate(dir));
    all.extend(args.iter().map(|a| a.to_string()));
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    start_reading(orrery_relay(&all))
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

/// Real git in `cwd` through the relay's proxy, trusting the authority `ca`
/// alone.
fn git_relayed(cwd: &Path, proxy: &str, ca: &Path, args: &[&str]) -> Output {
    let mut all = vec![
        "-c".to_string(),
        format!("http.proxy=http://{proxy}"),
        "-c".to_string(),
        format!("http.sslCAInfo={}", ca.display()),
    ];
    all.extend(args.iter().map(|a| a.to_string()));
    let all: Vec<&str> = all.iter().map(String::as_str).collect();
    let mut cmd = git_command(cwd, &all);
    for var in CLIENT_NETWORK_VARS {
        cmd.env_remove(var);
    }
    cmd.output().expect("spawn git")
}

/// Real gh through the relay's proxy as `token`, trusting the authority `ca`
/// alone.
fn gh(proxy: &str, ca: &Path, token: &str, args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    Command::new("gh")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("GH_CONFIG_DIR", home.path().join("gh"))
        .env("GH_TOKEN", token)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("HTTPS_PROXY", format!("http://{proxy}"))
        .env("SSL_CERT_FILE", ca)
        .args(args)
        .output()
        .expect("spawn gh: the relay tests need gh on PATH")
}

fn gh_json(proxy: &str, ca: &Path, token: &str, args: &[&str]) -> Value {
    let out = ok(gh(proxy, ca, token, args), &format!("gh {args:?}"));
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("gh {args:?} answered {out:?}: {e}"))
}

/// Bytes no compression shrinks, the same every run.
fn noise(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

#[test]
fn git_and_gh_reach_a_world_served_elsewhere_through_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let (_world, ready) = world(&dir.path().join("world"), "");
    let relay_dir = dir.path().join("relay");
    let (mut relayed, relay_ready) = relay(
        &relay_dir,
        &ready.base_url,
        &["--proxy-listen", "127.0.0.1:0"],
    );
    assert_eq!(relay_ready.world, ready.base_url);
    assert_eq!(relay_ready.tls_listen, None);
    let proxy = relay_ready.proxy_listen.clone().unwrap();
    assert!(
        proxy.starts_with("127.0.0.1:") && !proxy.ends_with(":0"),
        "{relay_ready:?}"
    );
    let ca = relay_dir.join("ca.pem");
    let alice = mint(&ready.control, "alice");

    // A write through the REST face, which names github.com's hosts.
    let repo = gh_json(
        &proxy,
        &ca,
        &alice,
        &["api", "-X", "POST", "/user/repos", "-f", "name=webapp"],
    );
    assert_eq!(repo["clone_url"], "https://github.com/alice/webapp.git");

    // A push bigger than git sends in one buffer, so its body streams
    // through the relay in chunks, then a clone that streams it back.
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    let big = noise(8 * 1024 * 1024);
    let remote = format!("https://x-access-token:{alice}@github.com/alice/webapp.git");
    let run = |cwd: &Path, args: &[&str]| {
        ok(
            git_relayed(cwd, &proxy, &ca, args),
            &format!("git {args:?}"),
        )
    };
    run(&src, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("noise.bin"), &big).unwrap();
    run(&src, &["add", "."]);
    run(&src, &["commit", "-q", "-m", "init"]);
    run(&src, &["push", "-q", &remote, "main"]);
    run(&src, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("change.txt"), "a change\n").unwrap();
    run(&src, &["add", "."]);
    run(&src, &["commit", "-q", "-m", "a change"]);
    run(&src, &["push", "-q", &remote, "feature"]);
    run(work.path(), &["clone", "-q", &remote, "clone"]);
    let cloned = std::fs::read(work.path().join("clone/noise.bin")).unwrap();
    assert!(
        cloned == big,
        "the clone's noise.bin differs from the one pushed"
    );

    let user = gh_json(&proxy, &ca, &alice, &["api", "/user"]);
    assert_eq!(user["login"], "alice");
    assert_eq!(user["url"], "https://api.github.com/users/alice");
    let pull = gh_json(
        &proxy,
        &ca,
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
    );
    assert_eq!(pull["html_url"], "https://github.com/alice/webapp/pull/1");
    let node = pull["node_id"].as_str().unwrap();
    let answer = gh_json(
        &proxy,
        &ca,
        &alice,
        &[
            "api",
            "graphql",
            "-f",
            "query=query($id: ID!) { node(id: $id) { ... on PullRequest { url } } }",
            "-F",
            &format!("id={node}"),
        ],
    );
    assert_eq!(
        answer["data"]["node"]["url"], "https://github.com/alice/webapp/pull/1",
        "{answer}"
    );

    // SIGTERM stops the relay, which exits 0.
    relayed.signal("TERM");
    let (status, stdout, stderr) = relayed.exited(Duration::from_secs(10));
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(stdout, "", "nothing follows the ready line on stdout");
}

#[test]
fn git_reaches_the_relays_tls_listener_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let (_world, ready) = world(&dir.path().join("world"), "");
    let relay_dir = dir.path().join("relay");
    let (_relayed, relay_ready) = relay(
        &relay_dir,
        &ready.base_url,
        &["--tls-listen", "127.0.0.1:0"],
    );
    assert_eq!(relay_ready.proxy_listen, None);
    let tls_listen = relay_ready.tls_listen.unwrap();
    let port = tls_listen.rsplit_once(':').unwrap().1;
    let alice = mint(&ready.control, "alice");
    let (status, body) = common::request(
        &ready.listen,
        "POST",
        "/api/v3/user/repos",
        Some(&alice),
        r#"{"name":"webapp"}"#,
    );
    assert_eq!(status, 201, "{body}");

    let work = tempfile::tempdir().unwrap();
    let ca = relay_dir.join("ca.pem");
    let mut cmd = git_command(
        work.path(),
        &[
            "-c",
            &format!("http.curloptResolve=github.com:{port}:127.0.0.1"),
            "-c",
            &format!("http.sslCAInfo={}", ca.display()),
            "ls-remote",
            &format!("https://x-access-token:{alice}@github.com:{port}/alice/webapp.git"),
        ],
    );
    for var in CLIENT_NETWORK_VARS {
        cmd.env_remove(var);
    }
    // An empty repository has no refs, so git lists none and succeeds.
    assert_eq!(ok(cmd.output().unwrap(), "git ls-remote"), "");
}

#[test]
fn the_relay_reaches_a_world_served_over_tls_by_the_authorities_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let world_dir = dir.path().join("world");
    std::fs::create_dir_all(&world_dir).unwrap();
    let world_cert = github_certificate(&world_dir);
    let (_world, ready) = world(
        &world_dir,
        &format!("{world_cert}tls_listen = \"127.0.0.1:0\"\n"),
    );
    let world_tls = format!("https://{}", ready.tls_listen.clone().unwrap());
    let alice = mint(&ready.control, "alice");

    // The world's authority is not one the binary ships, so without it the
    // relay refuses the world's certificate and says so.
    let untrusting = dir.path().join("untrusting");
    let (_relayed, relay_ready) =
        relay(&untrusting, &world_tls, &["--proxy-listen", "127.0.0.1:0"]);
    let client = tls_client(&untrusting, relay_ready.proxy_listen.as_deref());
    let mut answer = client
        .get("https://api.github.com/user")
        .header("authorization", format!("token {alice}"))
        .call()
        .unwrap();
    assert_eq!(answer.status(), 502);
    let body: Value = answer.body_mut().read_json().unwrap();
    assert_eq!(body["orrery"]["relay_failed"], true, "{body}");
    assert_eq!(body["orrery"]["world"], world_tls.as_str(), "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.contains("GET https://127.0.0.1:") && message.contains("/api/v3/user"),
        "{message}"
    );

    let world_ca = world_dir.join("ca.pem").display().to_string();
    let trusting = dir.path().join("trusting");
    let (_relayed, relay_ready) = relay(
        &trusting,
        &world_tls,
        &[
            "--proxy-listen",
            "127.0.0.1:0",
            "--extra-ca-certs",
            &world_ca,
        ],
    );
    let user = gh_json(
        relay_ready.proxy_listen.as_deref().unwrap(),
        &trusting.join("ca.pem"),
        &alice,
        &["api", "/user"],
    );
    assert_eq!(user["login"], "alice");
}

#[test]
fn what_the_world_does_not_serve_is_answered_by_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let (_world, ready) = world(&dir.path().join("world"), "");
    let relay_dir = dir.path().join("relay");
    let (_relayed, relay_ready) = relay(
        &relay_dir,
        &ready.base_url,
        &[
            "--proxy-listen",
            "127.0.0.1:0",
            "--tls-listen",
            "127.0.0.1:0",
        ],
    );
    let proxied = tls_client(&relay_dir, relay_ready.proxy_listen.as_deref());
    let read = |client: &ureq::Agent, url: &str| -> (u16, Value) {
        let mut answer = client.get(url).call().unwrap();
        let status = answer.status().as_u16();
        (status, answer.body_mut().read_json().unwrap())
    };

    // The control plane is never github.com's, and github.com serves no
    // REST: the world's own 501, which the relay answers without the world.
    for (url, path) in [
        ("https://github.com/_orrery/healthz", "/_orrery/healthz"),
        ("https://github.com/api/v3/user", "/api/v3/user"),
    ] {
        let (status, body) = read(&proxied, url);
        assert_eq!(status, 501, "{url}: {body}");
        assert_eq!(body["orrery"]["not_mocked"], true, "{body}");
        assert_eq!(body["orrery"]["host"], "github.com", "{body}");
        assert_eq!(body["orrery"]["path"], path, "{body}");
    }

    // Surface the world does not mock is the world's 501, naming the path
    // the client sent, as the world does when reached as api.github.com.
    let alice = mint(&ready.control, "alice");
    let mut answer = proxied
        .get("https://api.github.com/nosuch/thing")
        .header("authorization", format!("token {alice}"))
        .call()
        .unwrap();
    assert_eq!(answer.status(), 501);
    let body: Value = answer.body_mut().read_json().unwrap();
    assert_eq!(
        body["message"], "orrery: GET /nosuch/thing is not mocked",
        "{body}"
    );
    assert_eq!(body["orrery"]["path"], "/nosuch/thing", "{body}");
    assert!(body["orrery"].get("host").is_none(), "{body}");

    // A host that is not GitHub's, on the TLS listener, is not forwarded.
    let direct = tls_client(&relay_dir, None);
    let tls_listen = relay_ready.tls_listen.unwrap();
    let (status, body) = read(&direct, &format!("https://{tls_listen}/api/v3/user"));
    assert_eq!(status, 421, "{body}");
    assert_eq!(body["orrery"]["relay_refused"], true, "{body}");
    assert_eq!(body["orrery"]["host"], "127.0.0.1", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains(&ready.base_url),
        "{body}"
    );
}

#[test]
fn a_world_the_relay_cannot_reach_is_a_502_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    // A port bound and released, so nothing listens on it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let world_url = format!("http://127.0.0.1:{port}");
    let (_relayed, relay_ready) = relay(dir.path(), &world_url, &["--proxy-listen", "127.0.0.1:0"]);
    let client = tls_client(dir.path(), relay_ready.proxy_listen.as_deref());
    let mut answer = client.get("https://api.github.com/user").call().unwrap();
    assert_eq!(answer.status(), 502);
    let body: Value = answer.body_mut().read_json().unwrap();
    assert_eq!(body["orrery"]["relay_failed"], true, "{body}");
    assert_eq!(body["orrery"]["world"], world_url.as_str(), "{body}");
}

#[test]
fn a_relay_that_cannot_serve_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let flags = relay_certificate(dir.path());
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    let (one_host_cert, one_host_key) = {
        let only = dir.path().join("only-github");
        std::fs::create_dir_all(&only).unwrap();
        let (cert, key) = certificate_files(&only, &["github.com"]);
        (cert.display().to_string(), key.display().to_string())
    };
    let socket = dir.path().join("relay.sock").display().to_string();
    let unix = format!("unix:{socket}");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            joined(
                &flags,
                &[
                    "--world",
                    "https://api.github.com",
                    "--proxy-listen",
                    "127.0.0.1:0",
                ],
            ),
            "--world \"https://api.github.com\" names one of GitHub's own hosts",
        ),
        (
            joined(
                &flags,
                &[
                    "--world",
                    "http://127.0.0.1:99999",
                    "--proxy-listen",
                    "127.0.0.1:0",
                ],
            ),
            "--world \"http://127.0.0.1:99999\" has a port that is not 1 to 65535",
        ),
        (
            joined(
                &flags,
                &[
                    "--world",
                    "https://world.example",
                    "--proxy-listen",
                    "127.0.0.1:47999",
                    "--tls-listen",
                    "127.0.0.1:47999",
                ],
            ),
            "--proxy-listen and --tls-listen are both 127.0.0.1:47999",
        ),
        (
            joined(
                &flags,
                &[
                    "--world",
                    "https://world.example/?x=1",
                    "--proxy-listen",
                    "127.0.0.1:0",
                ],
            ),
            "--world \"https://world.example/?x=1\" has a query: it is the URL of the world's own host",
        ),
        (
            joined(
                &flags,
                &["--world", "world.example", "--proxy-listen", "127.0.0.1:0"],
            ),
            "--world \"world.example\" is not an http or https URL",
        ),
        (
            vec![
                "--world",
                "https://world.example",
                "--tls-cert",
                &one_host_cert,
                "--tls-key",
                &one_host_key,
                "--proxy-listen",
                "127.0.0.1:0",
            ],
            "--tls-cert does not name api.github.com",
        ),
        (
            joined(
                &flags,
                &[
                    "--world",
                    "https://world.example",
                    "--proxy-listen",
                    &unix,
                    "--tls-listen",
                    &unix,
                ],
            ),
            "--proxy-listen and --tls-listen are both unix:",
        ),
        (
            joined(
                &flags,
                &[
                    "--world",
                    "https://world.example",
                    "--proxy-listen",
                    "127.0.0.1:0",
                    "--extra-ca-certs",
                    "/nonexistent/ca.pem",
                ],
            ),
            "/nonexistent/ca.pem",
        ),
    ];
    for (args, expected) in cases {
        let out = refused(orrery_relay(&args));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }

    // With no listener at all it is a usage error.
    let mut no_listener = vec!["--world", "https://world.example"];
    no_listener.extend(&flags);
    let out = refused(orrery_relay(&no_listener));
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--proxy-listen"), "{stderr}");
}

/// `flags` followed by `args`.
fn joined<'a>(flags: &[&'a str], args: &[&'a str]) -> Vec<&'a str> {
    flags.iter().chain(args).copied().collect()
}

/// How long a side of the streaming test waits for the other.
const STEP: Duration = Duration::from_secs(20);

/// One chunk of a chunked body; empty at its end, after which its empty
/// trailer is read.
fn read_chunk(reader: &mut impl BufRead) -> Vec<u8> {
    let mut size = String::new();
    reader.read_line(&mut size).unwrap();
    let size = usize::from_str_radix(size.trim(), 16).unwrap();
    let mut chunk = vec![0; size];
    reader.read_exact(&mut chunk).unwrap();
    let mut end = String::new();
    reader.read_line(&mut end).unwrap();
    chunk
}

/// A request body that sends its first part, then waits until the world has
/// read it before sending its second: a relay that read the whole body
/// before forwarding any would wait forever.
struct Halting {
    sent: usize,
    world_read_first: mpsc::Receiver<()>,
}

impl Read for Halting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let part: &[u8] = match self.sent {
            0 => b"first part",
            1 => {
                self.world_read_first.recv_timeout(STEP).map_err(|_| {
                    std::io::Error::other("the world never read the body's first part")
                })?;
                b"second part"
            }
            _ => return Ok(0),
        };
        self.sent += 1;
        buf[..part.len()].copy_from_slice(part);
        Ok(part.len())
    }
}

/// A world written by hand answers one request, so the test sees what the
/// relay sends it, and when: the world's own host in `Host`, the REST path,
/// none of the client's hop-by-hop headers, and each body streamed through a
/// part at a time, in both directions.
#[test]
fn the_relay_streams_each_body_to_the_world_and_back() {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let world_addr = listener.local_addr().unwrap();
    let (world_read_first, client_waits) = mpsc::channel();
    let (client_read_first, world_waits) = mpsc::channel::<()>();
    let world = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(STEP)).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer = stream;
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        let mut body = Vec::new();
        while body.len() < b"first part".len() {
            body.extend(read_chunk(&mut reader));
        }
        world_read_first.send(()).unwrap();
        loop {
            let chunk = read_chunk(&mut reader);
            if chunk.is_empty() {
                break;
            }
            body.extend(chunk);
        }
        writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\nconnection: close, x-world-hop\r\nx-world-hop: 1\r\n\r\nc\r\nfirst answer\r\n")
            .unwrap();
        writer.flush().unwrap();
        world_waits
            .recv_timeout(STEP)
            .expect("the client never read the answer's first part");
        writer
            .write_all(b"d\r\nsecond answer\r\n0\r\n\r\n")
            .unwrap();
        writer.flush().unwrap();
        (head, body)
    });

    let (_relayed, relay_ready) = relay(
        dir.path(),
        &format!("http://{world_addr}"),
        &["--proxy-listen", "127.0.0.1:0"],
    );
    let client = tls_client(dir.path(), relay_ready.proxy_listen.as_deref());
    let mut halting = Halting {
        sent: 0,
        world_read_first: client_waits,
    };
    let mut answer = client
        .post("https://api.github.com/stream?x=1")
        .header("connection", "x-client-hop")
        .header("x-client-hop", "1")
        .send(ureq::SendBody::from_reader(&mut halting))
        .unwrap();
    assert_eq!(answer.status(), 200);
    assert!(answer.headers().get("x-world-hop").is_none());
    let mut reader = answer.body_mut().as_reader();
    let mut first = [0; 12];
    reader.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"first answer");
    client_read_first.send(()).unwrap();
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "second answer");

    let (head, body) = world.join().unwrap();
    assert!(
        head.starts_with("post /api/v3/stream?x=1 http/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&format!("\r\nhost: {world_addr}\r\n")),
        "{head}"
    );
    assert!(!head.contains("x-client-hop"), "{head}");
    assert_eq!(body, b"first partsecond part");
}
