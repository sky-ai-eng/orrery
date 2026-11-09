//! `git`, run as the e2e tests run it: hermetically, with a fixed identity
//! and date, so the operator's configuration never changes what a bench does.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

pub fn command(cwd: &Path) -> Command {
    let date = "1577836800 +0000";
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "alice")
        .env("GIT_AUTHOR_EMAIL", "alice@orrery.invalid")
        .env("GIT_COMMITTER_NAME", "alice")
        .env("GIT_COMMITTER_EMAIL", "alice@orrery.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null());
    cmd
}

/// Run `git args` in `cwd`; its trimmed stdout.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String> {
    stdout(command(cwd).args(args))
}

/// Run a prepared command; its trimmed stdout.
pub fn stdout(cmd: &mut Command) -> Result<String> {
    let out = output(cmd)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a prepared command to completion, failing with its stderr.
pub fn output(cmd: &mut Command) -> Result<Output> {
    let out = cmd.output().with_context(|| format!("spawning {cmd:?}"))?;
    if !out.status.success() {
        bail!(
            "{cmd:?} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// Write `contents` into the object store as a blob; its id.
pub fn hash_blob(repo: &Path, contents: &[u8]) -> Result<String> {
    use std::io::Write;
    let mut child = command(repo)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git hash-object")?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(contents)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "git hash-object: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
