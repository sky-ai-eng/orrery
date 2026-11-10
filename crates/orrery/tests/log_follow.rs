//! `orrery log follow` against a served world: the lines it prints, and how
//! each way the stream ends tells the caller where to resume.

mod common;

use std::io::{BufRead as _, BufReader, Read as _};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use common::{Served, call, serve, wait_until, write_config};

/// How long a test waits for the next line, or for the command to exit.
const WAIT: Duration = Duration::from_secs(20);

/// A world served on a free TCP port; returns it and its control URL.
fn serve_tcp(dir: &std::path::Path) -> (Served, String, String) {
    let config = write_config(dir, "", "listen = \"127.0.0.1:0\"\n");
    let (served, ready) = serve(&config, &[]);
    let url = format!("http://{}", ready.control);
    (served, ready.control, url)
}

/// A running `orrery log follow`, its stdout read line by line on a thread
/// of its own so the test can change the world while it reads.
struct Following {
    child: Child,
    lines: mpsc::Receiver<String>,
    /// Reads stderr as it is written, so the logs never fill the pipe.
    stderr: Option<JoinHandle<String>>,
}

impl Drop for Following {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn follow(control_url: &str, after_seq: i64) -> Following {
    let mut child = Command::new(env!("CARGO_BIN_EXE_orrery"))
        .args(["log", "follow", "--after-seq", &after_seq.to_string()])
        .args(["--control-url", control_url])
        .env_remove("ORRERY_CONTROL_URL")
        // Logging at its most verbose, so a log line that reached stdout
        // would fail the line-by-line asserts.
        .env("RUST_LOG", "trace")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut all = String::new();
        stderr.read_to_string(&mut all).unwrap();
        all
    });
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in stdout.lines() {
            if tx.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    Following {
        child,
        lines,
        stderr: Some(stderr),
    }
}

impl Following {
    /// Read lines until the entry with `last`.
    fn entries_through(&self, last: i64) -> Vec<serde_json::Value> {
        let mut got: Vec<serde_json::Value> = Vec::new();
        while got.last().and_then(|e| e["seq"].as_i64()) != Some(last) {
            let line = self.lines.recv_timeout(WAIT).expect("the next line");
            got.push(serde_json::from_str(&line).expect("a line of JSON"));
        }
        got
    }

    /// Wait for the command to exit; returns its status, what else it printed
    /// on stdout, and its stderr.
    fn exited(mut self) -> (ExitStatus, Vec<String>, String) {
        wait_until(WAIT, "orrery log follow exiting", || {
            self.child.try_wait().unwrap().is_some()
        });
        let status = self.child.wait().unwrap();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        let rest = self.lines.try_iter().collect();
        (status, rest, stderr)
    }
}

/// One more entry in the log.
fn append(control: &str) {
    call(
        control,
        "POST",
        "/_orrery/clock/advance",
        None,
        r#"{"seconds":1}"#,
        200,
    );
}

/// The whole log, as `POST /_orrery/log/list` answers it.
fn whole_log(control: &str) -> Vec<serde_json::Value> {
    let mut log = Vec::new();
    let mut body = serde_json::json!({ "page_size": 500 });
    loop {
        let page = call(
            control,
            "POST",
            "/_orrery/log/list",
            None,
            &body.to_string(),
            200,
        );
        log.extend(page["items"].as_array().unwrap().iter().cloned());
        match page.get("next_page_token") {
            Some(token) => body["page_token"] = token.clone(),
            None => return log,
        }
    }
}

/// The newest position in `log`: its last entry's `seq`, or 0.
fn newest_seq(log: &[serde_json::Value]) -> i64 {
    log.last().map_or(0, |e| e["seq"].as_i64().unwrap())
}

#[test]
fn it_prints_the_log_then_each_entry_as_it_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let (_served, control, url) = serve_tcp(dir.path());
    for _ in 0..3 {
        append(&control);
    }
    let existing = whole_log(&control);

    let following = follow(&url, 0);
    assert_eq!(following.entries_through(newest_seq(&existing)), existing);
    append(&control);
    let log = whole_log(&control);
    assert_eq!(
        following.entries_through(newest_seq(&log)),
        log[existing.len()..]
    );
}

#[test]
fn a_restore_ends_it_with_status_3_naming_the_restored_logs_newest_seq() {
    let dir = tempfile::tempdir().unwrap();
    let (_served, control, url) = serve_tcp(dir.path());
    append(&control);
    call(
        &control,
        "POST",
        "/_orrery/snapshots",
        None,
        r#"{"name":"before"}"#,
        201,
    );
    append(&control);
    let log = whole_log(&control);

    let following = follow(&url, 0);
    following.entries_through(newest_seq(&log));
    let restored = call(
        &control,
        "POST",
        "/_orrery/snapshots/before/restore",
        None,
        "{}",
        200,
    );
    let (status, rest, stderr) = following.exited();
    assert_eq!(status.code(), Some(3), "{stderr}");
    assert!(rest.is_empty(), "{rest:?}");
    let restored_seq = restored["last_seq"].as_i64().unwrap();
    assert!(
        stderr.contains(&format!("the restored log's newest seq is {restored_seq}")),
        "{stderr}"
    );
}

#[test]
fn a_position_that_names_no_entry_fails_with_the_worlds_message() {
    let dir = tempfile::tempdir().unwrap();
    let (_served, control, url) = serve_tcp(dir.path());
    let newest = newest_seq(&whole_log(&control));

    let (status, rest, stderr) = follow(&url, newest + 1000).exited();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(rest.is_empty(), "{rest:?}");
    assert!(
        stderr.contains(&format!(
            "after_seq names seq {}, which is not in the log",
            newest + 1000
        )),
        "{stderr}"
    );
}

#[test]
fn stopping_the_world_fails_it_naming_the_last_seq_it_printed() {
    let dir = tempfile::tempdir().unwrap();
    let (served, control, url) = serve_tcp(dir.path());
    append(&control);
    let last = newest_seq(&whole_log(&control));

    let following = follow(&url, 0);
    following.entries_through(last);
    served.signal("TERM");
    let (status, rest, stderr) = following.exited();
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(rest.is_empty(), "{rest:?}");
    assert!(
        stderr.contains(&format!(
            "stopped after seq {last}; resume with --after-seq {last}"
        )),
        "{stderr}"
    );
}
