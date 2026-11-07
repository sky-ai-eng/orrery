//! The commit a binary was built from, as `--version` and the control plane
//! report it. CI also runs this target with `ORRERY_BUILD_COMMIT` set, to
//! prove a build that names its commit reports that one.

mod common;

use std::process::Command;

use common::{call, serve_instance};

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
fn version_and_healthz_report_the_build_commit() {
    let out = Command::new(env!("CARGO_BIN_EXE_orrery"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let version = String::from_utf8(out.stdout).unwrap();
    let commit = version
        .trim_end()
        .strip_prefix(concat!("orrery ", env!("CARGO_PKG_VERSION"), " ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected --version output: {version:?}"));
    assert_eq!(commit, expected());

    let dir = tempfile::tempdir().unwrap();
    let (socket, _served) = serve_instance(dir.path(), "");
    let health = call(&socket, "GET", "/_orrery/healthz", None, "", 200);
    assert_eq!(health["build"], commit, "{health}");
}
