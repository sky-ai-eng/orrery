//! Shared E2E harness: boot a served world in-process on an ephemeral port,
//! and drive it the way real consumers do — real `git` subprocesses and real
//! HTTP, no simulated requests (rule 2).

// Each test target compiles this module separately, so a helper any one
// suite skips is "unused" in that target.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use orrery_world::{DEFAULT_T0_UNIX, World};

pub struct Instance {
    /// The instance directory: `world/` live, `snapshots/` beside it.
    pub dir: tempfile::TempDir,
    pub world: Arc<World>,
    pub base: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn boot() -> Instance {
    let dir = tempfile::tempdir().expect("tempdir");
    let world =
        Arc::new(World::create(&dir.path().join("world"), DEFAULT_T0_UNIX).expect("create world"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = orrery_server::app(world.clone(), base.clone(), dir.path().join("snapshots"));
    let task = tokio::spawn(async move {
        orrery_server::run(listener, app).await.expect("server run");
    });
    Instance {
        dir,
        world,
        base,
        task,
    }
}

/// Boot a server over an existing world directory — how a forked instance
/// comes up.
pub async fn boot_at(dir: tempfile::TempDir, world_root: &Path) -> Instance {
    let world = Arc::new(World::open(world_root).expect("open world"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = orrery_server::app(world.clone(), base.clone(), dir.path().join("snapshots"));
    let task = tokio::spawn(async move {
        orrery_server::run(listener, app).await.expect("server run");
    });
    Instance {
        dir,
        world,
        base,
        task,
    }
}

/// Run git hermetically, the way an authoring script would: identity pinned,
/// commit dates from the world clock, no operator config.
pub fn git(cwd: &Path, clock_unix: i64, args: &[&str]) -> String {
    let date = format!("{clock_unix} +0000");
    let out = Command::new("git")
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
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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
