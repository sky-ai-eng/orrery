//! Drives the real `orrery` binary the way a supervisor does: generated
//! configs, Unix sockets for its listeners, and `--json` output.
//! Requests are hand-written HTTP/1.1, since no client in the tree speaks it
//! over a Unix socket.

// Each test target compiles this module separately, so a helper any one
// suite skips is "unused" in that target.
#![allow(dead_code)]

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

pub struct Served(pub Child);

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Write `dir/orrery.toml` for the instance `dir/instance`. `instance` and
/// `server` are extra lines for those tables.
pub fn write_config(dir: &Path, instance: &str, server: &str) -> PathBuf {
    let config = dir.join("orrery.toml");
    let instance_dir = dir.join("instance");
    std::fs::write(
        &config,
        format!("[instance]\ndir = {instance_dir:?}\n{instance}\n[server]\n{server}"),
    )
    .unwrap();
    config
}

pub fn orrery_serve(config: &Path, envs: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orrery"));
    cmd.args(["serve", "--config"])
        .arg(config)
        .envs(envs.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Start `orrery serve` and wait until every socket accepts connections.
pub fn serve(config: &Path, envs: &[(&str, &str)], sockets: &[&Path]) -> Served {
    let mut served = Served(orrery_serve(config, envs).spawn().unwrap());
    for _ in 0..200 {
        if sockets.iter().all(|s| UnixStream::connect(s).is_ok()) {
            return served;
        }
        if let Some(status) = served.0.try_wait().unwrap() {
            let mut stderr = String::new();
            served
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("orrery serve exited with {status}: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("orrery serve never listened on {sockets:?}");
}

/// Serve the instance `dir/instance` on one socket, `dir/faces.sock`, whose
/// `[instance]` table adds `instance`.
pub fn serve_instance(dir: &Path, instance: &str) -> (PathBuf, Served) {
    let socket = dir.join("faces.sock");
    let config = write_config(
        dir,
        instance,
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            socket.display()
        ),
    );
    let served = serve(&config, &[], &[&socket]);
    (socket, served)
}

/// One request over a Unix socket; returns the status and body.
pub fn request(
    socket: &Path,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).unwrap();
    let auth = token
        .map(|t| format!("authorization: token {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nhost: world.test\r\n{auth}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response[9..12].parse().unwrap();
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    (status, body.to_string())
}

/// A request whose answer must have `status`; returns its JSON body.
pub fn call(
    socket: &Path,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
    status: u16,
) -> serde_json::Value {
    let (got, text) = request(socket, method, path, token, body);
    assert_eq!(got, status, "{method} {path}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Mint `login` as a user and return its token.
pub fn mint(socket: &Path, login: &str) -> String {
    let created = call(
        socket,
        "POST",
        "/_orrery/actors",
        None,
        &format!(r#"{{"login":"{login}","kind":"user"}}"#),
        201,
    );
    created["token"].as_str().unwrap().to_string()
}

/// Run a one-shot `orrery` command that takes `--json` in `cwd`; returns its
/// exit code and the one JSON object it printed on stdout.
pub fn orrery_json(cwd: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_orrery"))
        .args(args)
        .arg("--json")
        .current_dir(cwd)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let parsed = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "orrery {args:?} printed no single JSON object ({e}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap(), parsed)
}
