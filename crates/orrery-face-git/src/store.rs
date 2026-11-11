//! On-disk bare-repo store. All server-side git subprocesses run hermetically:
//! nothing of the operator's git setup may reach world behavior. A
//! `credential.helper` or `url.insteadOf` in their config, a
//! `GIT_CONFIG_COUNT` or `GIT_EXTERNAL_DIFF` in the engine's environment, or
//! an attributes file under their home would each be a hidden input.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::GitFaceError;

/// All-zeros object id, the wire spelling for "ref did not exist / was deleted".
pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";

/// `git`, run in `cwd` with only the environment [`git_command`] gives it.
pub fn hermetic_git(cwd: &Path) -> Command {
    let mut cmd = git_command();
    cmd.current_dir(cwd);
    cmd
}

/// `git` with an environment of its own, not the engine's. The variables are
/// an allowlist because git reads more of them with each release, and any
/// one it inherits can change what a world serves: `GIT_CONFIG_COUNT` adds
/// config, `GIT_EXTERNAL_DIFF` replaces a diff, `HOME` locates a user
/// attributes file, and `LANG` translates the messages an error carries.
/// `PATH` is the one copied from the engine, since git finds the programs it
/// runs through it, `ssh-keygen` and a hook's commands among them.
/// `GIT_ATTR_NOSYSTEM` is set because git reads the host's system attributes
/// file with no config pointing at it.
pub fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
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
/// allowAnySHA1InWant, pushes over http-backend need http.receivepack,
/// receive.hideRefs refuses pushes to the pull request refs the engine moves,
/// and gc.auto=0 keeps object files append-only so snapshots can share them.
/// An empty `--template` keeps the host's template directory, and the hooks
/// it would install, out of the repository.
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
        run(hermetic_git(&dir).args([
            "init",
            "--bare",
            "--template=",
            "--initial-branch",
            default_branch,
            ".",
        ]))?;
    }
    for (key, value) in [
        ("uploadpack.allowFilter", "true"),
        ("uploadpack.allowAnySHA1InWant", "true"),
        ("http.receivepack", "true"),
        ("receive.hideRefs", "refs/pull"),
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

/// The ref `HEAD` names, such as `refs/heads/main`; `None` when it names no
/// ref, which `git symbolic-ref -q` reports with exit code 1.
pub fn head_symref(bare: &Path) -> Result<Option<String>, GitFaceError> {
    let (status, stdout, stderr) =
        run_raw(hermetic_git(bare).args(["symbolic-ref", "-q", "HEAD"]))?;
    match status.code() {
        Some(0) => Ok(Some(stdout.trim().to_string())),
        Some(1) => Ok(None),
        _ => Err(GitFaceError::Git {
            args: "symbolic-ref -q HEAD".to_string(),
            stderr,
        }),
    }
}

/// The lock files of a repository's refs, in path order: `HEAD.lock`,
/// `packed-refs.lock`, and `<ref>.lock` under `refs/`. git creates one while
/// it moves a ref and removes it once the move is done, so one is left only
/// by a git that was killed partway, and every later move of that ref fails
/// while it is there. git refuses a ref name ending in `.lock`, so every such
/// file under `refs/` is a lock file.
pub fn ref_lock_files(bare: &Path) -> Result<Vec<PathBuf>, GitFaceError> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "lock") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut locks: Vec<PathBuf> = ["HEAD.lock", "packed-refs.lock"]
        .into_iter()
        .map(|name| bare.join(name))
        .filter(|path| path.exists())
        .collect();
    walk(&bare.join("refs"), &mut locks)?;
    locks.sort();
    Ok(locks)
}

/// Resolve a revision to an oid; `None` when it doesn't resolve. Any failure
/// of `rev-parse --verify --quiet` reads as "doesn't resolve" — the command
/// distinguishes nothing further on its exit code. Callers pass names from
/// requests, and git reads a name with a leading `-` as an option, never as
/// a revision, so such a name resolves to nothing without running git. A
/// branch whose name starts with `-` still resolves by its `refs/heads/` name.
pub fn rev_parse(bare: &Path, rev: &str) -> Result<Option<String>, GitFaceError> {
    if rev.starts_with('-') {
        return Ok(None);
    }
    let (status, stdout, _) =
        run_raw(hermetic_git(bare).args(["rev-parse", "--verify", "--quiet", rev]))?;
    if !status.success() {
        return Ok(None);
    }
    Ok(Some(stdout.trim().to_string()))
}

/// Each of `revs` resolved as [`rev_parse`] resolves it, by one `git cat-file
/// --batch-check` for all of them.
pub fn rev_parse_many(bare: &Path, revs: &[&str]) -> Result<Vec<Option<String>>, GitFaceError> {
    // A rev that is empty, or that git would read as an option, resolves to
    // nothing, as rev_parse has it; one holding a newline would be two lines
    // of input, and names no ref.
    let askable = |rev: &str| !rev.is_empty() && !rev.starts_with('-') && !rev.contains('\n');
    let input: String = revs
        .iter()
        .filter(|rev| askable(rev))
        .map(|rev| format!("{rev}\n"))
        .collect();
    if input.is_empty() {
        return Ok(vec![None; revs.len()]);
    }
    let args = ["cat-file", "--batch-check=%(objectname)", "--buffer"];
    let out = run_with_input(hermetic_git(bare).args(args), input.as_bytes())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return Err(GitFaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let mut lines = stdout.lines();
    let mut oids = Vec::with_capacity(revs.len());
    for rev in revs {
        if !askable(rev) {
            oids.push(None);
            continue;
        }
        let line = lines.next().unwrap_or_default();
        if line == format!("{rev} missing") || line == format!("{rev} ambiguous") {
            oids.push(None);
        } else if is_oid(line) {
            oids.push(Some(line.to_string()));
        } else {
            return Err(GitFaceError::Cgi(format!(
                "cat-file --batch-check answered {line:?} for {rev:?}"
            )));
        }
    }
    Ok(oids)
}

/// Whether `s` is a full object id, SHA-1 or SHA-256, as git prints one.
pub fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Raw bytes of the blob at `spec` (e.g. `HEAD:README.md`); `None` when the
/// path doesn't resolve to a blob — a missing file, a directory, or an empty
/// repo all read as absent, which is the 404 the contents endpoint owes. A
/// `spec` with a leading `-` is absent too, for the reason [`rev_parse`]
/// gives.
pub fn cat_file_blob(bare: &Path, spec: &str) -> Result<Option<Vec<u8>>, GitFaceError> {
    if spec.starts_with('-') {
        return Ok(None);
    }
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

/// The commits reachable from `tip` and from none of `excluded`, oldest
/// first, and how many there are: at most `limit` of them, the newest. The
/// revisions go to `rev-list` on its stdin, so a repository with many refs
/// never overflows its argument list.
pub fn added_commits(
    bare: &Path,
    tip: &str,
    excluded: &[&str],
    limit: usize,
) -> Result<(Vec<String>, i64), GitFaceError> {
    use std::io::Write as _;
    let mut child = hermetic_git(bare)
        .args(["rev-list", "--reverse", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut revs = format!("{tip}\n");
    for oid in excluded {
        revs.push_str(&format!("^{oid}\n"));
    }
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(revs.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(GitFaceError::Git {
            args: format!("rev-list --reverse --stdin <<< {revs:?}"),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let all: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    let count = all.len() as i64;
    let newest = all[all.len().saturating_sub(limit)..].to_vec();
    Ok((newest, count))
}

/// Whether `ancestor` is reachable from `descendant`, a commit counting as its
/// own ancestor: `git merge-base --is-ancestor`, whose exit code 1 is "no".
/// An object that is not a commit, such as a tree a tag points at, is no
/// commit's ancestor and has none.
pub fn is_ancestor(bare: &Path, ancestor: &str, descendant: &str) -> Result<bool, GitFaceError> {
    let (status, _, stderr) =
        run_raw(hermetic_git(bare).args(["merge-base", "--is-ancestor", ancestor, descendant]))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        // git refuses a non-commit with the same code as any other failure,
        // so a refusal is "no" only when one side is not a commit.
        _ if !is_commit(bare, ancestor)? || !is_commit(bare, descendant)? => Ok(false),
        _ => Err(GitFaceError::Git {
            args: format!("merge-base --is-ancestor {ancestor} {descendant}"),
            stderr,
        }),
    }
}

/// Whether `oid` names a commit, or a tag that peels to one.
fn is_commit(bare: &Path, oid: &str) -> Result<bool, GitFaceError> {
    let (status, _, _) = run_raw(hermetic_git(bare).args([
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{oid}^{{commit}}"),
    ]))?;
    Ok(status.success())
}

/// Totals of `git diff --numstat -M base...head` — GitHub's additions /
/// deletions / changed-files triple, three-dot so the comparison base is the
/// merge base, and a rename counted once, as `changed_files` lists it. Binary
/// files report `-` counts, which contribute 0 lines but still count as a
/// changed file, matching GitHub.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
}

pub fn diff_stats(bare: &Path, base: &str, head: &str) -> Result<DiffStats, GitFaceError> {
    let out =
        run(hermetic_git(bare).args(["diff", "--numstat", "-M", &format!("{base}...{head}")]))?;
    let mut stats = DiffStats::default();
    for line in out.lines() {
        let mut parts = line.split('\t');
        let (Some(add), Some(del)) = (parts.next(), parts.next()) else {
            continue;
        };
        stats.changed_files += 1;
        stats.additions += add.parse::<i64>().unwrap_or(0);
        stats.deletions += del.parse::<i64>().unwrap_or(0);
    }
    Ok(stats)
}

/// How a file differs across a comparison, as GitHub's REST `status` spells
/// it. A type change, and any status git adds later, reads as `Changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
}

impl FileStatus {
    pub fn rest(self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Removed => "removed",
            FileStatus::Modified => "modified",
            FileStatus::Renamed => "renamed",
            FileStatus::Copied => "copied",
            FileStatus::Changed => "changed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub status: FileStatus,
    /// The path a renamed or copied file came from.
    pub previous: Option<String>,
}

/// The files `git diff base...head` changes, in path order, with renames
/// detected — three-dot so the comparison base is the merge base, as in
/// GitHub's compare and pull request files.
pub fn changed_files(
    bare: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<ChangedFile>, GitFaceError> {
    let out = run(hermetic_git(bare).args([
        "diff",
        "--name-status",
        "-z",
        "-M",
        &format!("{base}...{head}"),
    ]))?;
    let mut fields = out.split('\0');
    let mut files = Vec::new();
    while let Some(code) = fields.next() {
        if code.is_empty() {
            continue;
        }
        let status = file_status(code);
        let (previous, path) = match status {
            FileStatus::Renamed | FileStatus::Copied => {
                let (Some(from), Some(to)) = (fields.next(), fields.next()) else {
                    break;
                };
                (Some(from.to_string()), to)
            }
            _ => {
                let Some(path) = fields.next() else {
                    break;
                };
                (None, path)
            }
        };
        files.push(ChangedFile {
            path: path.to_string(),
            status,
            previous,
        });
    }
    Ok(files)
}

/// [`diff_stats`] for each `(base, head)`, from as few git processes as can
/// work them out together ([`three_dot_many`]).
pub fn diff_stats_many(
    bare: &Path,
    pairs: &[(&str, &str)],
) -> Vec<Result<DiffStats, GitFaceError>> {
    three_dot_many(bare, pairs, "--numstat", numstat_z, |base, head| {
        diff_stats(bare, base, head)
    })
}

/// [`changed_files`] for each `(base, head)`, from as few git processes as
/// can work them out together ([`three_dot_many`]).
pub fn changed_files_many(
    bare: &Path,
    pairs: &[(&str, &str)],
) -> Vec<Result<Vec<ChangedFile>, GitFaceError>> {
    three_dot_many(bare, pairs, "--name-status", name_status_z, |base, head| {
        changed_files(bare, base, head)
    })
}

/// Each `(base, head)`'s three-dot comparison, as `single` works one out:
/// one `git rev-parse` lists every pair's merge bases, and one `git diff-tree
/// --stdin`, printing `format`, compares the head of every pair with exactly
/// one to that merge base, which is the comparison `base...head` makes. A pair
/// with no merge base or with several, every pair when a batch fails, and a
/// lone pair, which a batch would take more processes for, go to `single`, so
/// each answer and each error is the one `single` gives.
fn three_dot_many<T>(
    bare: &Path,
    pairs: &[(&str, &str)],
    format: &str,
    parse: fn(&mut Fields<'_>) -> Option<T>,
    single: impl Fn(&str, &str) -> Result<T, GitFaceError>,
) -> Vec<Result<T, GitFaceError>> {
    let mut answers: Vec<Option<T>> = std::iter::repeat_with(|| None).take(pairs.len()).collect();
    if pairs.len() > 1
        && let Some(bases) = merge_bases(bare, pairs)
    {
        let (batched, comparisons): (Vec<usize>, Vec<(&str, &str)>) = pairs
            .iter()
            .zip(&bases)
            .enumerate()
            .filter_map(|(i, ((_, head), bases))| match &bases[..] {
                [base] => Some((i, (*head, base.as_str()))),
                _ => None,
            })
            .unzip();
        if let Some(found) = diff_tree_many(bare, &comparisons, format, parse) {
            for (i, answer) in batched.into_iter().zip(found) {
                answers[i] = Some(answer);
            }
        }
    }
    answers
        .into_iter()
        .zip(pairs)
        .map(|(answer, (base, head))| answer.map(Ok).unwrap_or_else(|| single(base, head)))
        .collect()
}

/// The merge bases of each `(base, head)`, from one `git rev-parse` per
/// [`MERGE_BASE_CHUNK`] of them; `None` when git fails, as it does when a
/// side names no commit, or prints what this does not expect.
fn merge_bases(bare: &Path, pairs: &[(&str, &str)]) -> Option<Vec<Vec<String>>> {
    let mut all = Vec::with_capacity(pairs.len());
    for chunk in pairs.chunks(MERGE_BASE_CHUNK) {
        let args: Vec<String> = chunk.iter().map(|(b, h)| format!("{b}...{h}")).collect();
        let (status, stdout, _) = run_raw(hermetic_git(bare).arg("rev-parse").args(&args)).ok()?;
        if !status.success() {
            return None;
        }
        // `rev-parse base...head` prints head, then base, then each merge
        // base with a `^` before it.
        let mut lines = stdout.lines().peekable();
        for (base, head) in chunk {
            if lines.next()? != *head || lines.next()? != *base {
                return None;
            }
            let mut bases = Vec::new();
            while let Some(line) = lines.next_if(|l| l.starts_with('^')) {
                bases.push(line[1..].to_string());
            }
            all.push(bases);
        }
        if lines.next().is_some() {
            return None;
        }
    }
    Some(all)
}

/// How many pairs one `git rev-parse` is given, which keeps its command line
/// far below any system's limit.
const MERGE_BASE_CHUNK: usize = 256;

type Fields<'a> = std::iter::Peekable<std::str::Split<'a, char>>;

/// `git diff-tree --stdin` printing `format` for each `(head, base)`,
/// comparing `base` to `head`, each comparison read by `parse`; `None` when
/// git fails or prints what this does not expect.
fn diff_tree_many<T>(
    bare: &Path,
    comparisons: &[(&str, &str)],
    format: &str,
    parse: fn(&mut Fields<'_>) -> Option<T>,
) -> Option<Vec<T>> {
    if comparisons.is_empty() {
        return Some(Vec::new());
    }
    // A line of a commit and others is the diff from the others, read as the
    // commit's parents, to the commit.
    let input: String = comparisons
        .iter()
        .map(|(head, base)| format!("{head} {base}\n"))
        .collect();
    let args = ["diff-tree", "--stdin", "--always", "-r", "-M", "-z", format];
    let out = run_with_input(hermetic_git(bare).args(args), input.as_bytes()).ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut fields = stdout.split('\0').peekable();
    let mut found = Vec::with_capacity(comparisons.len());
    for (head, _) in comparisons {
        // Each comparison starts with its commit's id, which `--always`
        // prints for an empty one too.
        if fields.next()? != *head {
            return None;
        }
        found.push(parse(&mut fields)?);
    }
    // All that is left is the empty field after the last NUL.
    match (fields.next(), fields.next()) {
        (Some(""), None) => Some(found),
        _ => None,
    }
}

/// One comparison's `--numstat -z` records, as [`diff_stats`] totals them. A
/// record is `<added>\t<deleted>\t<path>`, or, for a rename, the counts with
/// an empty path and then the two paths as fields of their own; the commit id
/// that starts the next comparison has no tab.
fn numstat_z(fields: &mut Fields<'_>) -> Option<DiffStats> {
    let mut stats = DiffStats::default();
    while let Some(record) = fields.next_if(|f| f.contains('\t')) {
        let mut parts = record.splitn(3, '\t');
        let (add, del, path) = (parts.next()?, parts.next()?, parts.next()?);
        if path.is_empty() {
            fields.next()?;
            fields.next()?;
        }
        stats.changed_files += 1;
        stats.additions += add.parse::<i64>().unwrap_or(0);
        stats.deletions += del.parse::<i64>().unwrap_or(0);
    }
    Some(stats)
}

/// One comparison's `--name-status -z` records, as [`changed_files`] lists
/// them. A record starts with its status, a capital letter and for a rename
/// or copy its score; the commit id that starts the next comparison does not.
fn name_status_z(fields: &mut Fields<'_>) -> Option<Vec<ChangedFile>> {
    let is_status = |f: &&str| {
        let mut chars = f.chars();
        chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_digit())
    };
    let mut files = Vec::new();
    while let Some(code) = fields.next_if(is_status) {
        let status = file_status(code);
        let previous = match status {
            FileStatus::Renamed | FileStatus::Copied => Some(fields.next()?.to_string()),
            _ => None,
        };
        files.push(ChangedFile {
            path: fields.next()?.to_string(),
            status,
            previous,
        });
    }
    Some(files)
}

/// The status a `--name-status` code gives a file.
fn file_status(code: &str) -> FileStatus {
    match code.chars().next() {
        Some('A') => FileStatus::Added,
        Some('D') => FileStatus::Removed,
        Some('M') => FileStatus::Modified,
        Some('R') => FileStatus::Renamed,
        Some('C') => FileStatus::Copied,
        _ => FileStatus::Changed,
    }
}

/// The shortest object id an `index` line of a diff abbreviates to. Git
/// otherwise picks the length from how many objects the repository's packs
/// hold, which a replayed world need not pack the same way, so the diff of
/// one comparison could change length between a world and its replay.
const DIFF_ABBREV: &str = "--abbrev=7";

/// The output of `git diff -M base...head`, as git prints it: the diff GitHub
/// serves as its diff media type.
pub fn diff(bare: &Path, base: &str, head: &str) -> Result<Vec<u8>, GitFaceError> {
    let args = ["diff", "-M", DIFF_ABBREV, &format!("{base}...{head}")];
    let out = hermetic_git(bare).args(args).output()?;
    if !out.status.success() {
        return Err(GitFaceError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(out.stdout)
}

/// One file's part of a comparison: the file as [`changed_files`] lists it,
/// with its line counts and its hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub file: ChangedFile,
    pub additions: i64,
    pub deletions: i64,
    /// The file's part of the diff from its first `@@` line, without the
    /// final newline, as GitHub's `patch`. `None` when it has no hunk: a
    /// binary file, a pure rename, a mode change, or an empty file.
    pub patch: Option<String>,
}

/// Every file `git diff -M base...head` changes, in path order, as GitHub
/// lists them, each with its part of [`diff`]. A type change prints as a
/// deletion and a creation of one path, two chunks under one header, and is
/// two files: the one removed, then the one added.
pub fn file_diffs(bare: &Path, base: &str, head: &str) -> Result<Vec<FileDiff>, GitFaceError> {
    let files = changed_files(bare, base, head)?;
    let diff = diff(bare, base, head)?;
    let mut sections: Vec<(&[u8], Vec<&[u8]>)> = Vec::new();
    for chunk in file_chunks(&diff) {
        let header = chunk.split(|&b| b == b'\n').next().unwrap_or_default();
        match sections.last_mut() {
            Some((last, chunks)) if *last == header => chunks.push(chunk),
            _ => sections.push((header, vec![chunk])),
        }
    }
    if sections.len() != files.len() {
        return Err(GitFaceError::Cgi(format!(
            "diff {base}...{head} prints {} files where its name-status lists {}",
            sections.len(),
            files.len()
        )));
    }
    let mut diffs = Vec::new();
    for (file, (_, chunks)) in files.into_iter().zip(sections) {
        match chunks[..] {
            [removed, added] if file.status == FileStatus::Changed => {
                let as_status = |status| ChangedFile {
                    status,
                    ..file.clone()
                };
                diffs.push(file_diff(as_status(FileStatus::Removed), &[removed]));
                diffs.push(file_diff(as_status(FileStatus::Added), &[added]));
            }
            _ => diffs.push(file_diff(file, &chunks)),
        }
    }
    Ok(diffs)
}

/// How many files a [`diff`] prints, as [`file_diffs`] lists them.
pub fn file_count(diff: &[u8]) -> usize {
    file_chunks(diff).len()
}

/// A file's counts and patch, from its chunks of a diff.
fn file_diff(file: ChangedFile, chunks: &[&[u8]]) -> FileDiff {
    let mut hunks = Vec::new();
    for chunk in chunks {
        if let Some(start) = hunk_start(chunk) {
            hunks.extend_from_slice(&chunk[start..]);
        }
    }
    let mut additions = 0;
    let mut deletions = 0;
    for line in hunks.split(|&b| b == b'\n') {
        match line.first() {
            Some(b'+') => additions += 1,
            Some(b'-') => deletions += 1,
            _ => {}
        }
    }
    let patch = (!hunks.is_empty()).then(|| {
        let text = hunks.strip_suffix(b"\n").unwrap_or(&hunks);
        String::from_utf8_lossy(text).into_owned()
    });
    FileDiff {
        file,
        additions,
        deletions,
        patch,
    }
}

/// A diff's chunks, each from a `diff --git` line up to the next one. A
/// line of a hunk starts with ` `, `+`, `-`, `\`, or `@`, so a line that
/// starts `diff --git ` is always a header.
fn file_chunks(diff: &[u8]) -> Vec<&[u8]> {
    const HEADER: &[u8] = b"diff --git ";
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    while at < diff.len() {
        if diff[at..].starts_with(HEADER) {
            starts.push(at);
        }
        at = match diff[at..].iter().position(|&b| b == b'\n') {
            Some(newline) => at + newline + 1,
            None => diff.len(),
        };
    }
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| &diff[start..starts.get(i + 1).copied().unwrap_or(diff.len())])
        .collect()
}

/// Where a chunk's first hunk starts: its first line that starts `@@`. No
/// header line does.
fn hunk_start(chunk: &[u8]) -> Option<usize> {
    let mut at = 0;
    while at < chunk.len() {
        if chunk[at..].starts_with(b"@@") {
            return Some(at);
        }
        at += chunk[at..].iter().position(|&b| b == b'\n')? + 1;
    }
    None
}

/// A commit and its parents' ids, first parent first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitParents {
    pub sha: String,
    pub parents: Vec<String>,
}

/// The commits reachable from `head` and not from `base`, oldest first and
/// parents before children — a compare's commit list.
pub fn commits_between(
    bare: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<CommitParents>, GitFaceError> {
    let out = run(hermetic_git(bare).args([
        "rev-list",
        "--parents",
        "--topo-order",
        "--reverse",
        &format!("{base}..{head}"),
    ]))?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let mut ids = line.split(' ');
            let sha = ids.next().filter(|s| !s.is_empty())?;
            Some(CommitParents {
                sha: sha.to_string(),
                parents: ids.map(str::to_string).collect(),
            })
        })
        .collect())
}

/// A commit's committer date as unix seconds; `None` when `sha` doesn't
/// resolve to a commit.
pub fn commit_date_unix(bare: &Path, sha: &str) -> Result<Option<i64>, GitFaceError> {
    let (status, stdout, _) = run_raw(hermetic_git(bare).args([
        "show",
        "-s",
        "--format=%ct",
        &format!("{sha}^{{commit}}"),
    ]))?;
    if !status.success() {
        return Ok(None);
    }
    Ok(stdout.trim().parse().ok())
}

/// [`commit_date_unix`] for each of `shas`, from one `git log --no-walk
/// --stdin` printing each one's `%ct`. A sha that `git log` would read as an
/// option, and every sha when the batch fails, as it does when one names no
/// commit, go to [`commit_date_unix`], so each answer and each error is the
/// one it gives.
pub fn commit_dates_unix(bare: &Path, shas: &[&str]) -> Vec<Result<Option<i64>, GitFaceError>> {
    let mut asked: Vec<&str> = Vec::new();
    for sha in shas {
        if !sha.is_empty() && !sha.starts_with('-') && !sha.contains('\n') && !asked.contains(sha) {
            asked.push(sha);
        }
    }
    let mut dates: BTreeMap<&str, i64> = BTreeMap::new();
    if asked.len() > 1 {
        let input: String = asked
            .iter()
            .map(|sha| format!("{sha}^{{commit}}\n"))
            .collect();
        let args = ["log", "--no-walk=unsorted", "--stdin", "--format=%ct"];
        if let Ok(out) = run_with_input(hermetic_git(bare).args(args), input.as_bytes())
            && out.status.success()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let printed: Vec<&str> = stdout.lines().collect();
            // `log` prints a commit once however many shas name it, so a
            // count that differs leaves the lines unmatched to the shas.
            if printed.len() == asked.len() {
                for (sha, line) in asked.iter().zip(printed) {
                    if let Ok(date) = line.parse() {
                        dates.insert(sha, date);
                    }
                }
            }
        }
    }
    shas.iter()
        .map(|sha| match dates.get(sha) {
            Some(date) => Ok(Some(*date)),
            None => commit_date_unix(bare, sha),
        })
        .collect()
}

/// Whether the merge of each `(base, head)` is clean, as [`merge_tree`] finds
/// it, from one `git merge-tree --stdin` for all of them. `--stdin` came in
/// git 2.40, and a pair git cannot merge at all, such as two unrelated
/// histories, stops the batch; in either case, and for a lone pair, each pair
/// is merged on its own, so each answer and each error is [`merge_tree`]'s.
/// The batch is asked only of full object ids: a line of `--stdin` reads a
/// name holding a space as more than one argument, and can then name a
/// different merge.
pub fn merges_clean(bare: &Path, pairs: &[(&str, &str)]) -> Vec<Result<bool, GitFaceError>> {
    if pairs.len() > 1
        && pairs
            .iter()
            .all(|(base, head)| is_oid(base) && is_oid(head))
        && let Some(clean) = merges_clean_batch(bare, pairs)
    {
        return clean.into_iter().map(Ok).collect();
    }
    pairs
        .iter()
        .map(|(base, head)| merge_tree(bare, base, head).map(|tree| tree.is_some()))
        .collect()
}

fn merges_clean_batch(bare: &Path, pairs: &[(&str, &str)]) -> Option<Vec<bool>> {
    let input: String = pairs.iter().map(|(b, h)| format!("{b} {h}\n")).collect();
    let args = [
        "merge-tree",
        "--write-tree",
        "--stdin",
        "--name-only",
        "--no-messages",
    ];
    let out = run_with_input(hermetic_git(bare).args(args), input.as_bytes()).ok()?;
    if !out.status.success() {
        return None;
    }
    // Each merge prints its status, 1 when clean and 0 when it conflicts,
    // then its tree, the paths that conflict, and an empty field.
    let mut fields = out.stdout.split(|&b| b == 0);
    let mut clean = Vec::with_capacity(pairs.len());
    for _ in pairs {
        let status = fields.next()?;
        fields.next()?;
        while !fields.next()?.is_empty() {}
        clean.push(match status {
            b"1" => true,
            b"0" => false,
            _ => return None,
        });
    }
    // All that is left is the empty field after the last NUL.
    match (fields.next(), fields.next()) {
        (Some([]), None) => Some(clean),
        _ => None,
    }
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

/// The ref GitHub keeps at a pull request's head, through which a client
/// fetches the pull request by number (`git fetch origin pull/7/head`).
pub fn pull_head_ref(number: i64) -> String {
    format!("refs/pull/{number}/head")
}

/// Point a ref at `new`, whatever it held, creating it if it is absent.
pub fn update_ref(bare: &Path, refname: &str, new: &str) -> Result<(), GitFaceError> {
    run(hermetic_git(bare).args(["update-ref", refname, new]))?;
    Ok(())
}

/// Move a ref with compare-and-swap on its old value, so a concurrent push
/// between mergeability check and ref move fails loudly instead of clobbering.
/// [`ZERO_OID`] spells an absent ref on either side: as `old`, the ref must
/// not exist; as `new`, the ref is deleted. A symbolic ref is moved or
/// deleted itself: without `--no-deref`, git moves the ref it points at, so
/// deleting `refs/heads/alias -> refs/heads/main` would delete `main`.
pub fn update_ref_cas(
    bare: &Path,
    refname: &str,
    new: &str,
    old: &str,
) -> Result<(), GitFaceError> {
    run(hermetic_git(bare).args(["update-ref", "--no-deref", refname, new, old]))?;
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

/// Run `cmd` with `input` on its stdin. The input is written from a thread of
/// its own: a git that prints as it reads fills its stdout pipe, and would
/// wait on it while this waited on its stdin.
fn run_with_input(cmd: &mut Command, input: &[u8]) -> Result<std::process::Output, GitFaceError> {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let out = std::thread::scope(|s| {
        // A git that stops early, on a bad line, closes its stdin and the
        // write fails; its exit status says what happened.
        s.spawn(move || {
            let _ = stdin.write_all(input);
        });
        child.wait_with_output()
    })?;
    Ok(out)
}

fn run_raw(cmd: &mut Command) -> Result<(std::process::ExitStatus, String, String), GitFaceError> {
    let out = cmd.output()?;
    Ok((
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::init_bare;

    /// A host's template directory would give every world repository the
    /// hooks, `description`, and `info/exclude` that host ships.
    #[test]
    fn a_new_repository_takes_nothing_from_the_host_template() {
        let dir = tempfile::tempdir().unwrap();
        let bare = init_bare(dir.path(), "alice", "webapp", "main").unwrap();
        assert!(bare.join("HEAD").is_file());
        for absent in ["hooks", "description", "info"] {
            assert!(!bare.join(absent).exists(), "the repository holds {absent}");
        }
    }
}
