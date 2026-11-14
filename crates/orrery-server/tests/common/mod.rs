//! Shared E2E harness: boot a served world in-process on an ephemeral port,
//! and drive it the way real consumers do — real `git` subprocesses and real
//! HTTP, no simulated requests (rule 2).

// Each test target compiles this module separately, so a helper any one
// suite skips is "unused" in that target.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use orrery_world::{DEFAULT_T0_UNIX, World, WorldSecret};

pub struct Instance {
    /// The instance directory: `world/` live, `snapshots/` beside it.
    pub dir: tempfile::TempDir,
    pub world: Arc<World>,
    /// The faces' origin.
    pub base: String,
    /// The control plane's origin: `base`, unless the instance was booted
    /// with [`boot_split`].
    pub control: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Boot a local world: the well-known secret, as `orrery serve` uses.
pub async fn boot() -> Instance {
    boot_with(orrery_server::Options::default()).await
}

pub async fn boot_with(options: orrery_server::Options) -> Instance {
    let (dir, world) = create(&WorldSecret::LOCAL);
    serve(dir, world, options).await
}

pub async fn boot_keyed(secret: &WorldSecret) -> Instance {
    let (dir, world) = create(secret);
    serve(dir, world, orrery_server::Options::default()).await
}

fn create(secret: &WorldSecret) -> (tempfile::TempDir, Arc<World>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let world = Arc::new(
        World::create(&dir.path().join("world"), DEFAULT_T0_UNIX, secret).expect("create world"),
    );
    (dir, world)
}

/// Boot a server over an existing world directory — how a forked instance
/// comes up.
pub async fn boot_at(dir: tempfile::TempDir, world_root: &Path) -> Instance {
    let world = Arc::new(World::open(world_root).expect("open world"));
    serve(dir, world, orrery_server::Options::default()).await
}

/// Boot a local world with the control plane on a listener of its own, as
/// `orrery serve` does when `server.control_listen` is set.
pub async fn boot_split() -> Instance {
    let (dir, world) = create(&WorldSecret::LOCAL);
    let (faces, base) = bind().await;
    let (control, control_base) = bind().await;
    let (faces_app, control_app) = orrery_server::routers(
        world.clone(),
        base.clone(),
        dir.path().join("snapshots"),
        orrery_server::Options::default(),
    )
    .split();
    Instance {
        dir,
        world,
        base,
        control: control_base,
        tasks: vec![
            spawn_server(faces, faces_app),
            spawn_server(control, control_app),
        ],
    }
}

async fn serve(
    dir: tempfile::TempDir,
    world: Arc<World>,
    options: orrery_server::Options,
) -> Instance {
    let (listener, base) = bind().await;
    let app = orrery_server::routers(
        world.clone(),
        base.clone(),
        dir.path().join("snapshots"),
        options,
    )
    .combined();
    Instance {
        dir,
        world,
        control: base.clone(),
        base,
        tasks: vec![spawn_server(listener, app)],
    }
}

async fn bind() -> (tokio::net::TcpListener, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    (listener, base)
}

fn spawn_server(
    listener: tokio::net::TcpListener,
    app: axum::Router,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        orrery_server::run(listener, app, std::future::pending())
            .await
            .expect("server run");
    })
}

/// Run git hermetically, the way an authoring script would: identity pinned,
/// commit dates from the world clock, no operator config.
pub fn git(cwd: &Path, clock_unix: i64, args: &[&str]) -> String {
    let out = git_output(cwd, clock_unix, args);
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// [`git`] for a run whose failure is the point.
pub fn git_output(cwd: &Path, clock_unix: i64, args: &[&str]) -> Output {
    let date = format!("{clock_unix} +0000");
    Command::new("git")
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "alice")
        .env("GIT_AUTHOR_EMAIL", "alice@orrery.invalid")
        .env("GIT_COMMITTER_NAME", "alice")
        .env("GIT_COMMITTER_EMAIL", "alice@orrery.invalid")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .args(args)
        .output()
        .expect("spawn git")
}

pub fn control_post(base: &str, path: &str, body: serde_json::Value) -> serde_json::Value {
    ureq::post(format!("{base}{path}"))
        .send_json(body)
        .expect("control plane call")
        .body_mut()
        .read_json()
        .expect("control plane json")
}

/// Mint an actor on the control plane; returns its token.
pub fn mint_actor(base: &str, login: &str, kind: &str) -> String {
    let created = control_post(
        base,
        "/_orrery/actors",
        serde_json::json!({ "login": login, "kind": kind }),
    );
    created["token"]
        .as_str()
        .expect("token in response")
        .to_string()
}

/// Snapshot the world as `name`; returns the snapshot's newest `seq`.
pub fn snapshot(base: &str, name: &str) -> i64 {
    control_post(
        base,
        "/_orrery/snapshots",
        serde_json::json!({ "name": name }),
    )["last_seq"]
        .as_i64()
        .expect("last_seq in response")
}

/// Restore the snapshot `name` over the live world.
pub fn restore(base: &str, name: &str) {
    control_post(
        base,
        &format!("/_orrery/snapshots/{name}/restore"),
        serde_json::json!({}),
    );
}

/// A face call that must succeed; panics with the status on failure.
pub fn face_ok(method: &str, url: &str, token: &str, body: serde_json::Value) -> serde_json::Value {
    match face(method, url, token, body) {
        Ok(v) => v,
        Err(code) => panic!("{method} {url} unexpectedly failed with {code}"),
    }
}

/// A face call whose interesting outcome is the response itself: `Ok(json)`
/// on 2xx, `Err(status)` otherwise.
pub fn face(
    method: &str,
    url: &str,
    token: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, u16> {
    let auth = format!("token {token}");
    let result = match method {
        "GET" => ureq::get(url).header("authorization", &auth).call(),
        "POST" => ureq::post(url)
            .header("authorization", &auth)
            .send_json(body),
        "PATCH" => ureq::patch(url)
            .header("authorization", &auth)
            .send_json(body),
        "PUT" => ureq::put(url)
            .header("authorization", &auth)
            .send_json(body),
        "DELETE" => ureq::delete(url).header("authorization", &auth).call(),
        other => panic!("unsupported method {other}"),
    };
    match result {
        Ok(mut resp) => Ok(resp.body_mut().read_json().expect("face json")),
        Err(ureq::Error::StatusCode(code)) => Err(code),
        Err(other) => panic!("{method} {url} transport error: {other:?}"),
    }
}

/// A face call answered with its status and JSON body, whatever the status,
/// sending `headers` beside the token.
pub fn face_answer(
    method: &str,
    url: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let auth = format!("token {token}");
    let mut resp = match (method, body) {
        ("GET", None) => headers
            .iter()
            .fold(
                agent.get(url).header("authorization", &auth),
                |r, (k, v)| r.header(*k, *v),
            )
            .call(),
        (method, Some(body)) => {
            let request = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                "PATCH" => agent.patch(url),
                other => panic!("unsupported method {other} with a body"),
            };
            headers
                .iter()
                .fold(request.header("authorization", &auth), |r, (k, v)| {
                    r.header(*k, *v)
                })
                .send_json(body)
        }
        (other, None) => panic!("unsupported method {other} without a body"),
    }
    .unwrap_or_else(|e| panic!("{method} {url} transport error: {e:?}"));
    let status = resp.status().as_u16();
    (status, resp.body_mut().read_json().expect("face json"))
}

/// Each control-plane error's `(reason, field)`, in the order the answer
/// lists them.
pub fn faults(answer: &serde_json::Value) -> Vec<(String, Option<String>)> {
    answer["errors"]
        .as_array()
        .unwrap_or_else(|| panic!("no errors list in {answer}"))
        .iter()
        .map(|e| {
            (
                e["reason"].as_str().unwrap().to_string(),
                e["field"].as_str().map(str::to_string),
            )
        })
        .collect()
}

pub fn fault(reason: &str, field: Option<&str>) -> (String, Option<String>) {
    (reason.to_string(), field.map(str::to_string))
}

/// An RSA key pair as a GitHub App holds one: the private half signs the
/// App's JWTs, and the public half is what a world is given.
pub struct AppKey {
    private: ring::signature::RsaKeyPair,
    /// The public half as a PEM `PUBLIC KEY` block.
    pub public_pem: String,
}

/// Make a 2048-bit RSA key pair with `ssh-keygen`.
pub fn app_key() -> AppKey {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("key");
    let run = |args: &[&str]| {
        let out = Command::new("ssh-keygen")
            .args(args)
            .output()
            .expect("spawn ssh-keygen");
        assert!(
            out.status.success(),
            "ssh-keygen {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("ssh-keygen output is text")
    };
    let path_str = path.to_str().expect("utf-8 path");
    run(&[
        "-q", "-t", "rsa", "-b", "2048", "-m", "PEM", "-N", "", "-f", path_str,
    ]);
    let public_pem = run(&["-e", "-m", "PKCS8", "-f", &format!("{path_str}.pub")]);
    let private_pem = std::fs::read_to_string(&path).expect("read the private key");
    let body: String = private_pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, body)
        .expect("the private key's PEM body is base64");
    AppKey {
        private: ring::signature::RsaKeyPair::from_der(&der).expect("a PKCS#1 RSA private key"),
        public_pem,
    }
}

impl AppKey {
    /// A JWT with `header` and `claims`, signed RS256 with this key.
    pub fn sign(&self, header: serde_json::Value, claims: serde_json::Value) -> String {
        let encode = |bytes: &[u8]| {
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
        };
        let signed = format!(
            "{}.{}",
            encode(header.to_string().as_bytes()),
            encode(claims.to_string().as_bytes())
        );
        let mut signature = vec![0; self.private.public().modulus_len()];
        self.private
            .sign(
                &ring::signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .expect("sign the JWT");
        format!("{signed}.{}", encode(&signature))
    }

    /// The JWT a GitHub App sends: RS256, issued a minute before `now`, and
    /// expiring nine minutes after it.
    pub fn jwt(&self, iss: serde_json::Value, now: i64) -> String {
        self.sign(
            serde_json::json!({ "alg": "RS256", "typ": "JWT" }),
            serde_json::json!({ "iss": iss, "iat": now - 60, "exp": now + 540 }),
        )
    }
}
