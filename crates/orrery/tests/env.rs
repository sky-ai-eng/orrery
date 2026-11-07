//! `orrery env` against a world served over TCP, the way a seed script
//! reaches one.

use std::io::Read as _;
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

const ORRERY: &str = env!("CARGO_BIN_EXE_orrery");
/// `t0` in the served config.
const T0_UNIX: i64 = 1_767_225_600;

struct Served {
    child: Child,
    control_url: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Held by every test that takes a port, so two tests are never handed the
/// same free one: a world could otherwise answer another test's requests.
static PORTS: Mutex<()> = Mutex::new(());

fn ports() -> MutexGuard<'static, ()> {
    PORTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A port nothing listens on once this returns.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn serve(dir: &Path) -> Served {
    let port = free_port();
    let config = dir.join("orrery.toml");
    std::fs::write(
        &config,
        format!(
            "[instance]\ndir = {:?}\nt0 = \"2026-01-01T00:00:00Z\"\n[server]\nlisten = \"127.0.0.1:{port}\"\n",
            dir.join("instance")
        ),
    )
    .unwrap();
    let child = Command::new(ORRERY)
        .args(["serve", "--config"])
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut served = Served {
        child,
        control_url: format!("http://127.0.0.1:{port}"),
    };
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return served;
        }
        if let Some(status) = served.child.try_wait().unwrap() {
            let mut stderr = String::new();
            let mut pipe = served.child.stderr.take().unwrap();
            pipe.read_to_string(&mut stderr).unwrap();
            panic!("orrery serve exited with {status}: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("orrery serve never listened on port {port}");
}

impl Served {
    fn clock(&self) -> i64 {
        let body: serde_json::Value = ureq::get(format!("{}/_orrery/clock", self.control_url))
            .call()
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        body["clock_unix"].as_i64().unwrap()
    }

    fn advance(&self, seconds: i64) {
        ureq::post(format!("{}/_orrery/clock/advance", self.control_url))
            .send_json(serde_json::json!({ "seconds": seconds }))
            .unwrap();
    }
}

/// Logging at its most verbose, since a shell evaluates whatever reaches
/// stdout: the exact-output asserts fail if a log line lands there.
fn orrery_env(envs: &[(&str, &str)], args: &[&str]) -> Output {
    Command::new(ORRERY)
        .arg("env")
        .args(args)
        .env_remove("ORRERY_CONTROL_URL")
        .env("RUST_LOG", "trace")
        .envs(envs.iter().copied())
        .output()
        .unwrap()
}

fn exports(clock: i64) -> String {
    format!("export GIT_AUTHOR_DATE='@{clock} +0000'\nexport GIT_COMMITTER_DATE='@{clock} +0000'\n")
}

#[test]
fn the_dates_are_the_world_clock_and_move_with_it() {
    let _ports = ports();
    let dir = tempfile::tempdir().unwrap();
    let served = serve(dir.path());
    assert_eq!(served.clock(), T0_UNIX);

    let out = orrery_env(&[], &["--control-url", &served.control_url]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), exports(T0_UNIX));

    served.advance(600);
    let out = orrery_env(&[("ORRERY_CONTROL_URL", &served.control_url)], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        exports(T0_UNIX + 600)
    );
}

#[test]
fn a_commit_after_eval_carries_the_world_clock() {
    let _ports = ports();
    let dir = tempfile::tempdir().unwrap();
    let served = serve(dir.path());
    served.advance(3600);
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let out = Command::new("sh")
        .arg("-c")
        .arg(
            r#"set -e
git init -q
eval "$("$ORRERY" env)"
git -c user.name=seed -c user.email=seed@example.com commit -q --allow-empty -m seed
git log -1 --format='%at %ct'"#,
        )
        .current_dir(&repo)
        .env("ORRERY", ORRERY)
        .env("ORRERY_CONTROL_URL", &served.control_url)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_AUTHOR_DATE")
        .env_remove("GIT_COMMITTER_DATE")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let clock = served.clock();
    assert_eq!(clock, T0_UNIX + 3600);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!("{clock} {clock}\n")
    );
}

#[test]
fn an_unreachable_world_fails_naming_its_url() {
    let _ports = ports();
    let url = format!("http://127.0.0.1:{}", free_port());
    let out = orrery_env(&[], &["--control-url", &url]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&url), "{stderr}");
}

#[test]
fn a_unix_socket_is_refused() {
    let out = orrery_env(
        &[("ORRERY_CONTROL_URL", "unix:/run/orrery/control.sock")],
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("expected an http or https origin"),
        "{stderr}"
    );
}
