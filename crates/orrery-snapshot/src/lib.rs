//! Snapshots: a snapshot is a world, paused — a quiesced copy of the whole
//! world directory, nothing more (spec/usage.md). That one fact carries every
//! operation here: create is a copy, restore is the reverse copy, fork is a
//! copy to a new instance, list reads each copy's own database, and delete is
//! `rm -rf` (rule 10). No sidecar metadata files exist to disagree with the
//! copy.
//!
//! Layout: `<instance>/world/` live, `<instance>/snapshots/<name>/` each a
//! full world directory. Every credential in a world is derived and fake
//! (rule 12), but a copy holds the world secret and the stored tokens, so
//! nothing here returns a snapshot's raw contents.

use std::path::{Path, PathBuf};
use std::process::Command;

use orrery_world::{World, WorldReader, WorldSecret, WorldView};

pub mod diff;

pub use diff::{DiffSide, WorldDiff};

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("invalid snapshot name {0:?}: 1-64 chars of [A-Za-z0-9._-], not '.' or '..'")]
    InvalidName(String),
    #[error("no snapshot named {0:?}")]
    NotFound(String),
    #[error("a snapshot named {0:?} already exists")]
    AlreadyExists(String),
    #[error("copy failed: {0}")]
    Copy(String),
    #[error(transparent)]
    World(#[from] orrery_world::WorldError),
    #[error(transparent)]
    Git(#[from] orrery_face_git::GitFaceError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SnapshotInfo {
    pub name: String,
    /// The paused world's own clock — read from the copy, not recorded beside
    /// it, so it cannot disagree.
    pub clock_unix: i64,
    pub last_seq: i64,
}

/// Snapshot the live world into `snapshots_dir/name`. The caller must already
/// hold whatever gate keeps git traffic out (the server's world write gate);
/// this function quiesces the database itself. Blocking: run off the async
/// runtime.
pub fn create(world: &World, snapshots_dir: &Path, name: &str) -> Result<SnapshotInfo> {
    validate_name(name)?;
    let dest = snapshots_dir.join(name);
    if dest.exists() {
        return Err(SnapshotError::AlreadyExists(name.to_string()));
    }
    std::fs::create_dir_all(snapshots_dir)?;
    // Copy into a temp dir first, then rename: a crash mid-copy leaves
    // `.tmp-*` junk (removed on the next attempt), never a half snapshot
    // that lists as real.
    let tmp = snapshots_dir.join(format!(".tmp-{name}"));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    world.quiesce(|| copy_dir(world.root(), &tmp))??;
    std::fs::rename(&tmp, &dest)?;
    info_of(name, &dest)
}

pub fn info(snapshots_dir: &Path, name: &str) -> Result<Option<SnapshotInfo>> {
    validate_name(name)?;
    let dir = snapshots_dir.join(name);
    if !dir.exists() {
        return Ok(None);
    }
    info_of(name, &dir).map(Some)
}

/// All snapshots, sorted by name (the listing's total order).
pub fn list(snapshots_dir: &Path) -> Result<Vec<SnapshotInfo>> {
    let mut names = Vec::new();
    let entries = match std::fs::read_dir(snapshots_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // `.tmp-*` is an in-flight or crashed copy, not a snapshot.
        if !entry.file_type()?.is_dir() || name.starts_with('.') {
            continue;
        }
        names.push(name);
    }
    names.sort();
    names
        .iter()
        .map(|n| info_of(n, &snapshots_dir.join(n)))
        .collect()
}

pub fn delete(snapshots_dir: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let dir = snapshots_dir.join(name);
    if !dir.exists() {
        return Err(SnapshotError::NotFound(name.to_string()));
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Replace the live world with a snapshot's state. The caller must hold the
/// world write gate exclusively; the kernel's `swap_state` closes and reopens
/// the database around the directory swap. Blocking: run off the async
/// runtime.
///
/// No re-keying: the world returns to its own earlier state, and the secret
/// in that state is its own.
///
/// The old live state moves aside before the copy and is removed only after
/// it succeeds; a failed copy renames it back, so the world is never left
/// half-restored.
pub fn restore(world: &World, snapshots_dir: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let src = snapshots_dir.join(name);
    if !src.exists() {
        return Err(SnapshotError::NotFound(name.to_string()));
    }
    let root = world.root().to_path_buf();
    let trash = sibling(&root, ".world-trash")?;
    world.swap_state(|| {
        if trash.exists() {
            std::fs::remove_dir_all(&trash)?;
        }
        std::fs::rename(&root, &trash)?;
        match copy_dir(&src, &root) {
            Ok(()) => {
                std::fs::remove_dir_all(&trash)?;
                Ok(())
            }
            Err(copy_err) => {
                let restored = std::fs::rename(&trash, &root);
                if let Err(rename_err) = restored {
                    tracing::error!(
                        error = %rename_err,
                        "moving the old world back after a failed restore also failed; \
                         the previous state is in {}",
                        trash.display(),
                    );
                }
                Err(std::io::Error::other(copy_err.to_string()))
            }
        }
    })?;
    Ok(())
}

/// Copy a snapshot into a fresh instance directory as its live world —
/// a fork — and re-key it under `secret`. A fork is a new world, so it must
/// accept none of its source's credentials. Snapshots are static, so this is
/// safe while the source instance serves traffic.
pub fn fork(
    snapshots_dir: &Path,
    name: &str,
    dest_instance_dir: &Path,
    secret: &WorldSecret,
) -> Result<PathBuf> {
    validate_name(name)?;
    let src = snapshots_dir.join(name);
    if !src.exists() {
        return Err(SnapshotError::NotFound(name.to_string()));
    }
    let dest_world = dest_instance_dir.join("world");
    if dest_world.exists() {
        return Err(SnapshotError::Copy(format!(
            "{} already contains a world",
            dest_instance_dir.display()
        )));
    }
    std::fs::create_dir_all(dest_instance_dir)?;
    copy_dir(&src, &dest_world)?;
    if let Err(e) = World::open_rekeyed(&dest_world, secret) {
        // A copy left un-re-keyed would accept its source's credentials.
        if let Err(cleanup) = std::fs::remove_dir_all(&dest_world) {
            tracing::error!(
                error = %cleanup,
                "removing a fork that failed to re-key also failed; {} still holds \
                 its source's credentials",
                dest_world.display(),
            );
        }
        return Err(e.into());
    }
    Ok(dest_world)
}

fn info_of(name: &str, dir: &Path) -> Result<SnapshotInfo> {
    let meta = WorldReader::open(dir)?.view_meta()?;
    Ok(SnapshotInfo {
        name: name.to_string(),
        clock_unix: meta.clock_unix,
        last_seq: meta.last_seq,
    })
}

fn sibling(root: &Path, name: &str) -> Result<PathBuf> {
    root.parent()
        .map(|p| p.join(name))
        .ok_or_else(|| SnapshotError::Copy(format!("{} has no parent directory", root.display())))
}

/// Recursive copy via `cp -a`, reflinking when the filesystem supports it —
/// bare repos with `gc.auto=0` are append-only object stores, so reflinked
/// snapshots share almost everything. The plain `cp -a` retry covers cp
/// builds without `--reflink` (macOS); same bytes, just not shared.
fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    let reflink = Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(src)
        .arg(dest)
        .output()?;
    if reflink.status.success() {
        return Ok(());
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let plain = Command::new("cp").arg("-a").arg(src).arg(dest).output()?;
    if plain.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "cp -a {} {} failed: {}",
        src.display(),
        dest.display(),
        String::from_utf8_lossy(&plain.stderr),
    )))
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(SnapshotError::InvalidName(name.to_string()))
    }
}
