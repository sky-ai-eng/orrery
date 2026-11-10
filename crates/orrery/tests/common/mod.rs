//! Drives the real `orrery` binary the way a supervisor does: generated
//! configs, the ready line `serve` prints on stdout to say where it listens,
//! and the `--json` output of the commands that make worlds.
//! Requests are hand-written HTTP/1.1, since no client in the tree speaks it
//! over a Unix socket.

// Each test target compiles this module separately, so a helper any one
// suite skips is "unused" in that target.
#![allow(dead_code)]

use std::io::{BufRead as _, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::RawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

/// The line `orrery serve` prints on stdout once it is ready.
#[derive(serde::Deserialize, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub base_url: String,
    pub listen: String,
    pub control: String,
}

pub struct Served {
    pub child: Child,
    /// Reads stdout after the ready line until the process exits.
    rest_of_stdout: Option<JoinHandle<String>>,
    /// Reads stderr as it is written: a pipe nobody drains fills with logs
    /// and blocks the world's next write.
    stderr: Option<JoinHandle<String>>,
}

impl Served {
    /// Kill the process; return what it wrote to stdout after the ready
    /// line, and all it wrote to stderr.
    pub fn stop(&mut self) -> (String, String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let stdout = self.rest_of_stdout.take().unwrap().join().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (stdout, stderr)
    }
}

impl Served {
    /// Send the process `signal`, named as `kill` takes it, such as `TERM`.
    pub fn signal(&self, signal: &str) {
        let sent = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(sent.success(), "kill -{signal} failed");
    }

    pub fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Wait up to `within` for the process to exit on its own; return its
    /// status, what it wrote to stdout after the ready line, and all it wrote
    /// to stderr.
    pub fn exited(&mut self, within: Duration) -> (std::process::ExitStatus, String, String) {
        wait_until(within, "orrery serve exiting", || !self.running());
        let status = self.child.wait().unwrap();
        let stdout = self.rest_of_stdout.take().unwrap().join().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (status, stdout, stderr)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// The most sockets [`pass_sockets`] passes: its `pre_exec` runs between
/// fork and exec, where it must not allocate.
const MAX_PASSED: usize = 8;

/// Pass `sockets` to the process `cmd` starts as a supervisor does under
/// `sd_listen_fds(3)`: at descriptors 3 onward, with `LISTEN_FDS` their
/// count and `LISTEN_FDNAMES` `names`. `LISTEN_PID` is the caller's to set.
pub fn pass_sockets(cmd: &mut Command, sockets: &[RawFd], names: &str) {
    assert!(sockets.len() <= MAX_PASSED);
    let n = sockets.len();
    let mut fds = [-1; MAX_PASSED];
    fds[..n].copy_from_slice(sockets);
    let first_free = 3 + RawFd::try_from(n).unwrap();
    cmd.env("LISTEN_FDS", n.to_string())
        .env("LISTEN_FDNAMES", names);
    let ok = |got: libc::c_int| {
        if got == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(got)
        }
    };
    // SAFETY: the closure calls only fcntl and dup2, which are
    // async-signal-safe, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            // Each socket is copied above every target before any is moved,
            // so no dup2 overwrites a socket a later one still needs. The
            // copies are close-on-exec and the descriptors dup2 makes are
            // not, so only the targets reach the new process, even one whose
            // socket already sat at its target.
            let mut copies = [-1; MAX_PASSED];
            for i in 0..n {
                copies[i] = ok(libc::fcntl(fds[i], libc::F_DUPFD_CLOEXEC, first_free))?;
            }
            for (target, &copy) in (3..).zip(&copies[..n]) {
                ok(libc::dup2(copy, target))?;
            }
            Ok(())
        });
    }
}

/// `orrery serve` started as a supervisor that passes it `sockets`, named
/// `names`, starts it: through a shell that sets `LISTEN_PID` to its own pid
/// and then execs the binary, which keeps that pid.
pub fn orrery_serve_passing(config: &Path, sockets: &[RawFd], names: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        r#"export LISTEN_PID=$$; exec "$0" serve --config "$1""#,
    ])
    .arg(env!("CARGO_BIN_EXE_orrery"))
    .arg(config)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    pass_sockets(&mut cmd, sockets, names);
    cmd
}

/// Start `orrery serve` and read its ready line. Every listener is bound by
/// then, so nothing needs to poll for it.
pub fn serve(config: &Path, envs: &[(&str, &str)]) -> (Served, Ready) {
    start(orrery_serve(config, envs))
}

/// Run `cmd`, which starts `orrery serve`, and read its ready line.
pub fn start(mut cmd: Command) -> (Served, Ready) {
    let mut child = cmd.spawn().unwrap();
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

/// Serve the instance `dir/instance`, whose `[instance]` table adds
/// `instance`, with its faces and control plane on one Unix socket; returns
/// that socket's address in the ready line's syntax.
pub fn serve_instance(dir: &Path, instance: &str) -> (String, Served) {
    let config = write_config(
        dir,
        instance,
        &format!(
            "listen = \"unix:{}\"\nbase_url = \"http://world.test\"\n",
            dir.join("faces.sock").display()
        ),
    );
    let (served, ready) = serve(&config, &[]);
    (ready.listen, served)
}

/// One request to an address in the ready line's syntax, `host:port` or
/// `unix:/path`; returns the status and body.
pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
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

/// A request whose answer must have `status`; returns its JSON body.
pub fn call(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
    status: u16,
) -> serde_json::Value {
    let (got, text) = request(addr, method, path, token, body);
    assert_eq!(got, status, "{method} {path}: {text}");
    serde_json::from_str(&text).unwrap()
}

/// Mint `login` as a user through the control plane at `addr`; returns its
/// token.
pub fn mint(addr: &str, login: &str) -> String {
    let created = call(
        addr,
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

/// Run git in `cwd` with no operator config and fixed commit dates; returns
/// its stdout.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = git_command(cwd, args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The command [`git`] runs, for a caller that starts it and carries on.
pub fn git_command(cwd: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "alice")
        .env("GIT_AUTHOR_EMAIL", "alice@orrery.invalid")
        .env("GIT_COMMITTER_NAME", "alice")
        .env("GIT_COMMITTER_EMAIL", "alice@orrery.invalid")
        .env("GIT_AUTHOR_DATE", "@1577836800 +0000")
        .env("GIT_COMMITTER_DATE", "@1577836800 +0000")
        .args(args);
    cmd
}

/// Check `done` every 50ms until it holds, for up to `within`, then fail
/// naming `what`.
pub fn wait_until(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let tick = Duration::from_millis(50);
    for _ in 0..=within.as_millis() / tick.as_millis() {
        if done() {
            return;
        }
        std::thread::sleep(tick);
    }
    panic!("{what} did not happen within {within:?}");
}
