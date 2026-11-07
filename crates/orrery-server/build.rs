//! Resolves the commit this build is from, as `ORRERY_BUILD_COMMIT` for
//! `orrery_server::BUILD_COMMIT`: the variable of that name when the build
//! sets it, else `git rev-parse HEAD`, else `unknown`.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORRERY_BUILD_COMMIT");
    let given = std::env::var("ORRERY_BUILD_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    let commit = match given {
        Some(commit) => commit,
        None => {
            watch_head();
            git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
        }
    };
    println!("cargo:rustc-env=ORRERY_BUILD_COMMIT={commit}");
}

/// Rerun when HEAD moves: a checkout rewrites `HEAD`, and a commit writes the
/// branch's loose ref under `refs/heads`, even when the ref was packed
/// before. Cargo reruns a build script on every build while a path it
/// watches is missing, so only paths that exist are watched.
fn watch_head() {
    for name in ["HEAD", "refs/heads", "packed-refs"] {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}
