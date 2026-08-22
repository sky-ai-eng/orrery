//! On-disk bare-repo store. All server-side git subprocesses run hermetically:
//! the operator's global/system git config must never leak into world behavior
//! (a `credential.helper` or `url.insteadOf` there would be a hidden input).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::GitFaceError;

/// All-zeros object id, the wire spelling for "ref did not exist / was deleted".
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";

pub fn hermetic_git(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

/// The path of a repo's bare directory, from the row's canonical casing —
/// never from URL casing, so case-variant lookups hit one directory.
pub fn bare_dir(repos_dir: &Path, owner_login: &str, name: &str) -> PathBuf {
    repos_dir.join(owner_login).join(format!("{name}.git"))
}

/// Initialize a bare repo with the serving recipe from spec/surface-git.md:
/// blobless clones need allowFilter, promisor by-OID fetches need
/// allowAnySHA1InWant, pushes over http-backend need http.receivepack, and
/// gc.auto=0 keeps object files append-only so snapshots can share them.
///
/// Idempotent: re-applying to an existing bare repo only re-asserts the
/// recipe, so a crash between the kernel row insert and disk init heals on
/// the next attempt.
pub fn init_bare(
    repos_dir: &Path,
    owner_login: &str,
    name: &str,
    default_branch: &str,
) -> Result<PathBuf, GitFaceError> {
    let dir = bare_dir(repos_dir, owner_login, name);
    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir)?;
        run(hermetic_git(&dir).args(["init", "--bare", "--initial-branch", default_branch, "."]))?;
    }
    for (key, value) in [
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
        ("http.receivepack", "true"),
        ("gc.auto", "0"),
    ] {
        run(hermetic_git(&dir).args(["config", key, value]))?;
    }
    Ok(dir)
}

/// Every ref in the bare repo, sorted by name (BTreeMap), as `ref -> oid`.
pub fn list_refs(bare: &Path) -> Result<BTreeMap<String, String>, GitFaceError> {
    let out =
        run(hermetic_git(bare).args(["for-each-ref", "--format=%(refname)%00%(objectname)"]))?;
    let mut refs = BTreeMap::new();
    for line in out.lines() {
        if let Some((name, oid)) = line.split_once('\0') {
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    Ok(refs)
}

pub fn set_head_symref(bare: &Path, branch: &str) -> Result<(), GitFaceError> {
    run(hermetic_git(bare).args(["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]))?;
    Ok(())
}

/// Resolve a revision to an oid; `None` when it doesn't resolve. Any failure
/// of `rev-parse --verify --quiet` reads as "doesn't resolve" — the command
/// distinguishes nothing further on its exit code.
pub fn rev_parse(bare: &Path, rev: &str) -> Result<Option<String>, GitFaceError> {
    let (status, stdout, _) =
        run_raw(hermetic_git(bare).args(["rev-parse", "--verify", "--quiet", rev]))?;
    if !status.success() {
        return Ok(None);
    }
    Ok(Some(stdout.trim().to_string()))
}

/// Raw bytes of the blob at `spec` (e.g. `HEAD:README.md`); `None` when the
/// path doesn't resolve to a blob — a missing file, a directory, or an empty
/// repo all read as absent, which is the 404 the contents endpoint owes.
pub fn cat_file_blob(bare: &Path, spec: &str) -> Result<Option<Vec<u8>>, GitFaceError> {
    let out = hermetic_git(bare)
        .args(["cat-file", "blob", spec])
        .output()?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(out.stdout))
}

/// Commits reachable from `to` and not from `from` — zero means "nothing to
/// pull-request", GitHub's "No commits between" case.
pub fn rev_list_count(bare: &Path, from: &str, to: &str) -> Result<u64, GitFaceError> {
    let out = run(hermetic_git(bare).args(["rev-list", "--count", &format!("{from}..{to}")]))?;
    out.trim()
        .parse()
        .map_err(|_| GitFaceError::Cgi(format!("rev-list --count returned {out:?}")))
}

/// Compute the merge of two commits without a worktree. `Ok(Some(tree))` on a
/// clean merge, `Ok(None)` on content conflicts — git's exit code 1.
pub fn merge_tree(bare: &Path, base: &str, head: &str) -> Result<Option<String>, GitFaceError> {
    let (status, stdout, stderr) =
        run_raw(hermetic_git(bare).args(["merge-tree", "--write-tree", base, head]))?;
    match status.code() {
        Some(0) => Ok(stdout
            .lines()
            .next()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())),
        Some(1) => Ok(None),
        _ => Err(GitFaceError::Git {
            args: format!("merge-tree {base} {head}"),
            stderr,
        }),
    }
}

/// Create a commit object for `tree` with the given parents, authored and
/// committed by `login` at the world clock — commit dates from the clock are
/// the determinism rule for every server-side commit (spec/usage.md).
pub fn commit_tree(
    bare: &Path,
    tree: &str,
    parents: &[&str],
    message: &str,
    login: &str,
    clock_unix: i64,
) -> Result<String, GitFaceError> {
    let date = format!("{clock_unix} +0000");
    let email = format!("{login}@orrery.invalid");
    let mut cmd = hermetic_git(bare);
    cmd.env("GIT_AUTHOR_NAME", login)
        .env("GIT_AUTHOR_EMAIL", &email)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_NAME", login)
        .env("GIT_COMMITTER_EMAIL", &email)
        .env("GIT_COMMITTER_DATE", &date)
        .args(["commit-tree", tree, "-m", message]);
    for p in parents {
        cmd.args(["-p", p]);
    }
    Ok(run(&mut cmd)?.trim().to_string())
}

/// Move a ref with compare-and-swap on its old value, so a concurrent push
/// between mergeability check and ref move fails loudly instead of clobbering.
pub fn update_ref_cas(
    bare: &Path,
    refname: &str,
    new: &str,
    old: &str,
) -> Result<(), GitFaceError> {
    run(hermetic_git(bare).args(["update-ref", refname, new, old]))?;
    Ok(())
}

fn run(cmd: &mut Command) -> Result<String, GitFaceError> {
    let (status, stdout, stderr) = run_raw(cmd)?;
    if !status.success() {
        return Err(GitFaceError::Git {
            args: format!("{:?}", cmd.get_args().collect::<Vec<_>>()),
            stderr,
        });
    }
    Ok(stdout)
}

fn run_raw(cmd: &mut Command) -> Result<(std::process::ExitStatus, String, String), GitFaceError> {
    let out = cmd.output()?;
    Ok((
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}
