//! `orrery serve` the way a supervisor runs it: the real binary, a generated
//! config, and its ready line read from stdout to learn where it listens.
//! Requests are hand-written HTTP/1.1, since no client in the tree speaks it
//! over a Unix socket.

use std::io::{BufRead as _, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

/// The line `orrery serve` prints on stdout once it is ready.
#[derive(serde::Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct Ready {
    base_url: String,
    listen: String,
    control: String,
}

struct Served {
    child: Child,
    /// Reads stdout after the ready line until the process exits.
    rest_of_stdout: Option<JoinHandle<String>>,
    /// Reads stderr as it is written: a pipe nobody drains fills with logs
    /// and blocks the world's next write.
    stderr: Option<JoinHandle<String>>,
}

impl Served {
    /// Kill the process; return what it wrote to stdout after the ready
    /// line, and all it wrote to stderr.
    fn stop(&mut self) -> (String, String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let stdout = self.rest_of_stdout.take().unwrap().join().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (stdout, stderr)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `instance` and `server` are extra lines for those tables.
fn write_config(dir: &Path, instance: &str, server: &str) -> PathBuf {
    let config = dir.join("orrery.toml");
    let instance_dir = dir.join("instance");
    std::fs::write(
        &config,
        format!("[instance]\ndir = {instance_dir:?}\n{instance}\n[server]\n{server}"),
    )
    .unwrap();
    config
}

fn orrery_serve(config: &Path, envs: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orrery"));
    cmd.args(["serve", "--config"])
        .arg(config)
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Start `orrery serve` and read its ready line. Every listener is bound by
/// then, so nothing needs to poll for it.
fn serve(config: &Path, envs: &[(&str, &str)]) -> (Served, Ready) {
    let mut child = orrery_serve(config, envs).spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut all = String::new();
        stderr.read_to_string(&mut all).unwrap();
        all
    });
    let (first_line, first_line_rx) = std::sync::mpsc::channel();
    let rest_of_stdout = std::thread::spawn(move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let _ = first_line.send(line);
        let mut rest = String::new();
        stdout.read_to_string(&mut rest).unwrap();
        rest
    });
    let mut served = Served {
        child,
        rest_of_stdout: Some(rest_of_stdout),
        stderr: Some(stderr),
    };
    let line = first_line_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("orrery serve printed no ready line");
    if line.is_empty() {
        let (_, stderr) = served.stop();
        panic!("orrery serve exited before its ready line: {stderr}");
    }
    let ready = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("{line:?} is not the ready line: {e}"));
    (served, ready)
}

/// One request to an address in the ready line's syntax, `host:port` or
/// `unix:/path`; returns the status and body.
fn request(addr: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    match addr.strip_prefix("unix:") {
        Some(socket) => exchange(
            UnixStream::connect(socket).unwrap(),
            method,
            path,
            token,
            body,
        ),
        None => exchange(TcpStream::connect(addr).unwrap(), method, path, token, body),
    }
}

fn exchange(
    mut stream: impl Read + Write,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
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

#[test]
fn split_listeners_serve_over_unix_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let faces = dir.path().join("faces.sock");
    let control = dir.path().join("control.sock");
    // A killed world process leaves its socket file behind.
    drop(UnixListener::bind(&faces).unwrap());
    assert!(faces.exists());
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\ncontrol_listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            faces.display(),
            control.display()
        ),
    );
    let (_served, ready) = serve(&config, &[]);
    assert_eq!(
        ready,
        Ready {
            base_url: "http://world.test".to_string(),
            listen: format!("unix:{}", faces.display()),
            control: format!("unix:{}", control.display()),
        }
    );
    let (faces, control) = (&ready.listen, &ready.control);

    let (status, body) = request(control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(faces, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
    assert!(body.contains("\"not_mocked\":true"), "{body}");

    let (status, body) = request(
        control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = created["token"].as_str().unwrap();
    let (status, body) = request(faces, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"login\":\"alice\""), "{body}");
    let (status, body) = request(control, "GET", "/api/v3/user", Some(token), "");
    assert_eq!(status, 501, "{body}");
}

/// Port 0 lets a caller start many worlds without picking free ports
/// itself; the ready line is how it learns the ports they got.
#[test]
fn the_ready_line_reports_the_port_bound_for_port_zero() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (_served, ready) = serve(&config, &[]);
    let port: u16 = ready
        .listen
        .strip_prefix("127.0.0.1:")
        .unwrap_or_else(|| panic!("{ready:?}"))
        .parse()
        .unwrap();
    assert_ne!(port, 0);
    assert_eq!(ready.base_url, format!("http://127.0.0.1:{port}"));
    assert_eq!(ready.control, ready.listen);

    let host = ready.base_url.strip_prefix("http://").unwrap();
    let (status, body) = request(host, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
}

#[test]
fn the_ready_line_reports_a_control_socket() {
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"127.0.0.1:0\"\ncontrol_listen = \"unix:{}\"\n",
            control.display()
        ),
    );
    let (_served, ready) = serve(&config, &[]);
    assert_eq!(ready.control, format!("unix:{}", control.display()));
    assert_eq!(ready.base_url, format!("http://{}", ready.listen));

    let (status, body) = request(&ready.control, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200, "{body}");
    let (status, body) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 501, "{body}");
}

#[test]
fn nothing_but_the_ready_line_reaches_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (mut served, ready) = serve(&config, &[("RUST_LOG", "debug")]);
    // Traffic that logs: a control-plane write and unmocked surface.
    let (status, body) = request(
        &ready.control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = request(&ready.listen, "GET", "/not/mocked", None, "");
    assert_eq!(status, 501, "{body}");

    let (stdout, stderr) = served.stop();
    assert_eq!(stdout, "", "stdout after the ready line");
    assert!(stderr.contains("orrery serving"), "{stderr}");
    assert!(stderr.contains("NOT MOCKED"), "{stderr}");
}

/// A caller that reads a ready line may send traffic to every address in it,
/// so a listener that fails to bind must stop the world before it prints one.
#[test]
fn no_ready_line_until_every_listener_is_bound() {
    let dir = tempfile::tempdir().unwrap();
    let control = dir.path().join("control.sock");
    std::fs::write(&control, "not a socket").unwrap();
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"127.0.0.1:0\"\ncontrol_listen = \"unix:{}\"\n",
            control.display()
        ),
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exists and is not a socket"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn a_live_socket_is_not_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("faces.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            socket.display()
        ),
    );
    let (_first, ready) = serve(&config, &[]);
    let second = orrery_serve(&config, &[]).output().unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("in use by another process"), "{stderr}");
    assert_eq!(String::from_utf8_lossy(&second.stdout), "");
    let (status, _) = request(&ready.listen, "GET", "/_orrery/healthz", None, "");
    assert_eq!(status, 200);
}

#[test]
fn a_socket_listener_needs_an_explicit_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\n",
            dir.path().join("faces.sock").display()
        ),
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("server.base_url must be set"), "{stderr}");
}

/// A supervisor running many worlds starts each with
/// `TOKIO_WORKER_THREADS=2`. `#[tokio::main]` reads it with no code of
/// ours, which is exactly what a change to the runtime setup could drop.
#[cfg(target_os = "linux")]
#[test]
fn tokio_worker_threads_sizes_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("faces.sock");
    let config = write_config(
        dir.path(),
        "",
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            socket.display()
        ),
    );
    let (served, _) = serve(&config, &[("TOKIO_WORKER_THREADS", "2")]);
    let threads = std::fs::read_dir(format!("/proc/{}/task", served.child.id()))
        .unwrap()
        .count();
    assert_eq!(threads, 3, "the main thread and two workers");
}

/// Serve a fresh instance whose `[instance]` table adds `instance`, and
/// return the token it mints for alice and the running server.
fn alice_token(dir: &Path, instance: &str) -> (String, Served) {
    let config = write_config(
        dir,
        instance,
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            dir.join("faces.sock").display()
        ),
    );
    let (served, ready) = serve(&config, &[]);
    let (status, body) = request(
        &ready.control,
        "POST",
        "/_orrery/actors",
        None,
        r#"{"login":"alice","kind":"user"}"#,
    );
    assert_eq!(status, 201, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = created["token"].as_str().unwrap().to_string();
    (token, served)
}

#[test]
fn a_random_secret_makes_each_new_worlds_tokens_unguessable() {
    let dirs: Vec<_> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (default, _) = alice_token(dirs[0].path(), "");
    let (local, _) = alice_token(dirs[1].path(), "secret = \"local\"\n");
    let (random, served) = alice_token(dirs[2].path(), "secret = \"random\"\n");
    let (other_random, _) = alice_token(dirs[3].path(), "secret = \"random\"\n");

    // Local worlds share the well-known secret, so they mint the same token.
    assert_eq!(default, local);
    assert_ne!(random, local);
    assert_ne!(random, other_random);

    // The secret is the world's, not the config's: a restart keeps it, so
    // the token alice already holds still works.
    drop(served);
    let config = dirs[2].path().join("orrery.toml");
    let (_served, ready) = serve(&config, &[]);
    let (status, body) = request(&ready.listen, "GET", "/api/v3/user", Some(&random), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"login\":\"alice\""), "{body}");
}

#[test]
fn an_unknown_secret_choice_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(
        dir.path(),
        "secret = \"sometimes\"\n",
        "listen = \"127.0.0.1:0\"\n",
    );
    let out = orrery_serve(&config, &[]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown variant `sometimes`"), "{stderr}");
    assert!(
        !dir.path().join("instance").exists(),
        "no world was created"
    );
}
