//! Stopping `orrery serve` with a signal to its process alone, as a
//! supervisor stops a process: no git process the world started outlives it,
//! and a push in flight is either recorded or leaves no ref moved, so the
//! world's next start finds nothing to put back. Linux only: the processes a
//! world started are found through `/proc`.
#![cfg(target_os = "linux")]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use common::{Ready, Served, git, git_command, serve, wait_until, write_config};

/// Every line start-up writes when it puts a ref back (spec/surface-git.md,
/// "Starting a world").
const REPAIRS: [&str; 3] = [
    "moved a ref the log does not describe",
    "removed a stale ref lock file",
    "pointed HEAD back",
];

/// A served world whose `alice/webapp` holds `main` at one commit, pushed
/// from a clone at `src`.
struct World {
    dir: tempfile::TempDir,
    config: PathBuf,
    served: Served,
    ready: Ready,
    remote: String,
    src: PathBuf,
    bare: PathBuf,
    first: String,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
        let (served, ready) = serve(&config, &[]);
        let token = common::mint(&ready.control, "alice");
        common::call(
            &ready.listen,
            "POST",
            "/api/v3/user/repos",
            Some(&token),
            r#"{"name":"webapp"}"#,
            201,
        );
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main", "."]);
        git(&src, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let first = git(&src, &["rev-parse", "HEAD"]);
        let remote = format!(
            "http://x-access-token:{token}@{}/alice/webapp.git",
            ready.listen
        );
        git(&src, &["push", "-q", &remote, "main"]);
        World {
            bare: dir.path().join("instance/world/repos/alice/webapp.git"),
            dir,
            config,
            served,
            ready,
            remote,
            src,
            first,
        }
    }

    fn repos(&self) -> PathBuf {
        self.dir.path().join("instance/world/repos")
    }

    /// Start a push of a new commit on `main` whose receive-pack holds in
    /// its post-receive hook until `release` exists, and return once it
    /// holds there, with `main` moved and the push not yet recorded.
    fn push_held(&self, release: &Path) -> (std::process::Child, String) {
        let started = self.dir.path().join("post-receive-started");
        let hook = self.bare.join("hooks/post-receive");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\n",
                started.display(),
                release.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &self.src,
            &["commit", "-q", "--allow-empty", "-m", "second"],
        );
        let second = git(&self.src, &["rev-parse", "HEAD"]);
        let push = git_command(&self.src, &["push", "-q", &self.remote, "main"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_until(
            Duration::from_secs(10),
            "the post-receive hook starting",
            || started.exists(),
        );
        assert_eq!(git(&self.bare, &["rev-parse", "refs/heads/main"]), second);
        assert!(
            !world_git_processes(&self.repos()).is_empty(),
            "the held push has no git process to look for"
        );
        (push, second)
    }

    /// Serve the stopped world again, check that start-up put nothing back,
    /// and stop it; return its log's `ref:*` events, oldest first.
    fn restart(&self) -> Vec<serde_json::Value> {
        let (mut served, ready) = serve(&self.config, &[]);
        let log = common::call(
            &ready.control,
            "POST",
            "/_orrery/log/list",
            None,
            r#"{"sort_key":"seq","sort_dir":"asc"}"#,
            200,
        );
        let (_, stderr) = served.stop();
        for repair in REPAIRS {
            assert!(
                !stderr.contains(repair),
                "start-up repaired the world:\n{stderr}"
            );
        }
        log["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"].as_str().unwrap().starts_with("ref:"))
            .cloned()
            .collect()
    }

    fn main(&self) -> String {
        git(&self.bare, &["rev-parse", "refs/heads/main"])
    }
}

/// Every running process a git request of the world whose repositories are
/// `repos` started: `git http-backend` passes `GIT_PROJECT_ROOT` to the
/// receive-pack it starts, and that to its hooks. Each is `<pid> <command>`.
fn world_git_processes(repos: &Path) -> Vec<String> {
    let mark = format!("GIT_PROJECT_ROOT={}", repos.display());
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap() {
        let proc = entry.unwrap().path();
        let Some(pid) = proc.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        // A process that has exited since the listing, or another user's,
        // can't be read, and neither is one of the world's.
        let Ok(environ) = std::fs::read(proc.join("environ")) else {
            continue;
        };
        if environ.split(|&b| b == 0).any(|var| var == mark.as_bytes()) {
            let cmdline = std::fs::read(proc.join("cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            found.push(format!("{pid} {}", cmdline.trim()));
        }
    }
    found
}

/// Fails naming every git process of the world, its repositories `repos`,
/// that outlives it. One killed with SIGKILL runs no code once the signal
/// lands, but stays listed until it has finished exiting, so each gets a
/// moment to.
fn assert_none_outlive(repos: &Path) {
    let mut left = Vec::new();
    for _ in 0..40 {
        left = world_git_processes(repos);
        if left.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("git processes outlived the world: {left:?}");
}

/// The push finishes within the grace period, so the world waits for it,
/// records it, answers it, and exits. Until then it accepts no connection.
#[test]
fn a_push_in_flight_when_the_world_is_stopped_is_recorded() {
    let mut world = World::new();
    let release = world.dir.path().join("release");
    let (push, second) = world.push_held(&release);

    world.served.signal("TERM");
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        world.served.running(),
        "orrery serve exited with a push in flight"
    );
    assert!(
        TcpStream::connect(&world.ready.listen).is_err(),
        "a stopping world accepted a connection"
    );
    std::fs::write(&release, "").unwrap();

    let pushed = push.wait_with_output().unwrap();
    assert!(
        pushed.status.success(),
        "the push failed: {}",
        String::from_utf8_lossy(&pushed.stderr)
    );
    let (status, _, stderr) = world.served.exited(Duration::from_secs(10));
    assert_none_outlive(&world.repos());
    assert!(status.success(), "{status}: {stderr}");

    assert_eq!(world.main(), second);
    let refs = world.restart();
    let moved = refs.last().unwrap();
    assert_eq!(moved["type"], "ref:updated", "{refs:?}");
    assert_eq!(moved["subject"]["name"], "refs/heads/main");
    assert_eq!(moved["changes"]["sha"]["from"], world.first.as_str());
    assert_eq!(moved["changes"]["sha"]["to"], second.as_str());
}

/// The push is still held when the grace period ends, so the world kills its
/// git processes, the hook among them, moves `main` back, and exits. The
/// push fails, and nothing of it is in the log.
#[test]
fn a_push_still_running_when_the_grace_period_ends_is_killed_and_moved_back() {
    let mut world = World::new();
    let (push, _) = world.push_held(&world.dir.path().join("never"));

    world.served.signal("TERM");
    let (status, _, stderr) = world.served.exited(Duration::from_secs(15));
    assert_none_outlive(&world.repos());
    assert!(status.success(), "{status}: {stderr}");
    assert!(stderr.contains("killing them"), "{stderr}");
    assert!(!push.wait_with_output().unwrap().status.success());

    assert_eq!(world.main(), world.first);
    let refs = world.restart();
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert_eq!(refs[0]["type"], "ref:created");
    assert_eq!(refs[0]["object"]["sha"], world.first.as_str());
}

/// With nothing in flight the world exits at once, well inside the grace
/// period, though a log stream is open: the stream ends, rather than being
/// cut off, as the world stops.
#[test]
fn a_world_with_nothing_in_flight_stops_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "", "listen = \"127.0.0.1:0\"\n");
    let (mut served, ready) = serve(&config, &[]);
    let mut stream = TcpStream::connect(&ready.control).unwrap();
    write!(
        stream,
        "GET /_orrery/log/stream?after_seq=0 HTTP/1.1\r\nhost: world.test\r\n\r\n"
    )
    .unwrap();
    let mut stream = BufReader::new(stream);
    let mut status = String::new();
    stream.read_line(&mut status).unwrap();
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");

    served.signal("INT");
    let (exit, _, stderr) = served.exited(Duration::from_secs(2));
    assert!(exit.success(), "{exit}: {stderr}");
    let mut rest = String::new();
    stream.read_to_string(&mut rest).unwrap();
    assert!(
        rest.ends_with("\r\n0\r\n\r\n"),
        "the stream was cut off: {rest:?}"
    );
}
