//! The commit a binary was built from and the world schema it writes, as
//! `--version` and the control plane report them. The expected commit is
//! read the way the build reads it, so the same test holds for a build that
//! names its commit with `ORRERY_BUILD_COMMIT` and for one that falls back to
//! the checkout.

mod common;

use std::process::Command;

use common::{call, serve_instance};
use orrery_world::SCHEMA_VERSION;

/// What the build resolves: `ORRERY_BUILD_COMMIT` when set, else the
/// checkout's `HEAD`, else `unknown`.
fn expected() -> String {
    if let Some(commit) = option_env!("ORRERY_BUILD_COMMIT").map(str::trim)
        && !commit.is_empty()
    {
        return commit.to_string();
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8(out.stdout).unwrap().trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[test]
fn version_and_healthz_report_the_build_commit_and_world_schema() {
    let out = Command::new(env!("CARGO_BIN_EXE_orrery"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let version = String::from_utf8(out.stdout).unwrap();
    let (first, second) = version
        .trim_end()
        .split_once('\n')
        .unwrap_or_else(|| panic!("unexpected --version output: {version:?}"));
    assert_eq!(second, format!("world schema {SCHEMA_VERSION}"));
    let commit = first
        .strip_prefix(concat!("orrery ", env!("CARGO_PKG_VERSION"), " ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected --version output: {version:?}"));
    assert_eq!(commit, expected());

    let dir = tempfile::tempdir().unwrap();
    let (socket, _served) = serve_instance(dir.path(), "");
    let health = call(&socket, "GET", "/_orrery/healthz", None, "", 200);
    assert_eq!(health["build"], commit, "{health}");
    assert_eq!(health["world_schema"], SCHEMA_VERSION, "{health}");
}
