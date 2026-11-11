//! Each batched git read answers exactly as the one-process read it batches,
//! answer for answer and error for error, over repositories shaped to trip a
//! parser: renames, binary files, type and mode changes, a path with a tab,
//! empty diffs, two merge bases, unrelated histories, tags, and names that
//! resolve to nothing.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use orrery_face_git::GitFaceError;
use orrery_face_git::store;

/// A repository whose branches each change `main`'s first commit one way,
/// and whose `main` then moves on.
struct Repo {
    _dir: tempfile::TempDir,
    work: PathBuf,
    /// The committer date of the next commit, a second after the last, so
    /// each commit's date tells it apart.
    clock: std::cell::Cell<i64>,
}

impl Repo {
    fn git(&self, args: &[&str]) -> String {
        let out = store::hermetic_git(&self.work)
            .env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env(
                "GIT_COMMITTER_DATE",
                format!("{} +0000", self.clock.replace(self.clock.get() + 1)),
            )
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn write(&self, path: &str, body: &[u8]) {
        let path = self.work.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn branch(&self, name: &str, from: &str) {
        self.git(&["checkout", "-q", "-B", name, from]);
    }

    /// The repository's git directory, which git reads as it reads a bare
    /// repository.
    fn bare(&self) -> PathBuf {
        self.work.join(".git")
    }
}

fn lines(n: usize, tag: &str) -> Vec<u8> {
    (0..n)
        .map(|i| format!("{tag} {i}\n"))
        .collect::<String>()
        .into_bytes()
}

/// `main`'s first commit, branches off it, and `main` moved on, as `(name,
/// tip)`, with the first commit as `first`.
fn repo() -> (Repo, String, Vec<(String, String)>) {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let r = Repo {
        _dir: dir,
        work,
        clock: std::cell::Cell::new(1700000000),
    };
    r.git(&["init", "-q", "-b", "main", "."]);
    r.write("a.txt", &lines(20, "a"));
    r.write("r.txt", &lines(50, "r"));
    r.write("b.bin", b"\0\x01\x02binary\0");
    r.write("dir/x.txt", &lines(5, "x"));
    r.write("e.txt", b"");
    r.write("run.sh", b"echo hi\n");
    std::os::unix::fs::symlink("a.txt", r.work.join("link")).unwrap();
    let first = r.commit("first");
    let mut tips = Vec::new();
    let mut add = |name: &str, change: &dyn Fn(&Repo)| {
        r.branch(name, &first);
        change(&r);
        tips.push((name.to_string(), r.commit(name)));
    };
    add("modify", &|r| {
        let mut body = lines(20, "a");
        body.extend_from_slice(b"appended\n");
        r.write("a.txt", &body);
    });
    add("rename", &|r| {
        std::fs::remove_file(r.work.join("r.txt")).unwrap();
        let mut body = lines(50, "r");
        body.extend_from_slice(b"one more\n");
        r.write("moved/r2.txt", &body);
    });
    add("binary", &|r| r.write("b.bin", b"\0\x03changed\0"));
    add("typechange", &|r| {
        std::fs::remove_file(r.work.join("link")).unwrap();
        r.write("link", b"now a file\n");
    });
    add("mode", &|r| {
        let run = r.work.join("run.sh");
        std::fs::set_permissions(run, std::fs::Permissions::from_mode(0o755)).unwrap();
    });
    add("delete", &|r| {
        std::fs::remove_file(r.work.join("dir/x.txt")).unwrap();
        r.write("new.txt", b"new\n");
    });
    add("tab", &|r| r.write("a\tb c.txt", b"tab\n"));
    add("empty", &|_| {});
    add("conflict", &|r| {
        let body = String::from_utf8(lines(20, "a")).unwrap();
        r.write("a.txt", body.replace("a 2\n", "branch 2\n").as_bytes());
    });
    // Two merge bases: x and y each merge the other's first commit.
    r.branch("x", &first);
    r.write("x.txt", b"x\n");
    let x1 = r.commit("x1");
    r.branch("y", &first);
    r.write("y.txt", b"y\n");
    let y1 = r.commit("y1");
    r.git(&["merge", "-q", "--no-edit", &x1]);
    let y2 = r.git(&["rev-parse", "HEAD"]);
    r.branch("x", &x1);
    r.git(&["merge", "-q", "--no-edit", &y1]);
    r.write("x.txt", b"x again\n");
    let x2 = r.commit("x2");
    tips.push(("criss-x".to_string(), x2));
    tips.push(("criss-y".to_string(), y2));
    r.git(&["checkout", "-q", "--orphan", "unrelated"]);
    r.git(&["rm", "-q", "-r", "-f", "."]);
    r.write("other.txt", b"other\n");
    tips.push(("unrelated".to_string(), r.commit("unrelated")));
    r.branch("main", &first);
    let body = String::from_utf8(lines(20, "a")).unwrap();
    r.write("a.txt", body.replace("a 2\n", "main 2\n").as_bytes());
    tips.push(("main".to_string(), r.commit("main moves")));
    (r, first, tips)
}

/// Every pair of ends a test compares: each tip against `main`'s first
/// commit and its tip, both ways, and the two-merge-base pair.
fn pairs(first: &str, tips: &[(String, String)]) -> Vec<(String, String)> {
    let main = &tips.iter().find(|(n, _)| n == "main").unwrap().1;
    let mut pairs = Vec::new();
    for (_, tip) in tips {
        for base in [first, main.as_str()] {
            pairs.push((base.to_string(), tip.clone()));
            pairs.push((tip.clone(), base.to_string()));
        }
    }
    let tip = |name: &str| tips.iter().find(|(n, _)| n == name).unwrap().1.clone();
    pairs.push((tip("criss-x"), tip("criss-y")));
    pairs.push((tip("criss-y"), tip("criss-x")));
    pairs
}

/// An answer or an error, comparable.
fn shown<T: std::fmt::Debug>(r: &Result<T, GitFaceError>) -> String {
    match r {
        Ok(v) => format!("ok {v:?}"),
        Err(e) => format!("error {e}"),
    }
}

fn assert_same<T: std::fmt::Debug>(
    what: &str,
    asked: &[(String, String)],
    batched: Vec<Result<T, GitFaceError>>,
    single: impl Fn(&str, &str) -> Result<T, GitFaceError>,
) {
    assert_eq!(batched.len(), asked.len(), "{what}: one answer per pair");
    for ((base, head), batched) in asked.iter().zip(&batched) {
        assert_eq!(
            shown(batched),
            shown(&single(base, head)),
            "{what} of {base}...{head}"
        );
    }
}

fn borrowed(pairs: &[(String, String)]) -> Vec<(&str, &str)> {
    pairs
        .iter()
        .map(|(b, h)| (b.as_str(), h.as_str()))
        .collect()
}

#[test]
fn three_dot_reads_answer_as_one_process_each_does() {
    let (r, first, tips) = repo();
    let bare = r.bare();
    let asked = pairs(&first, &tips);
    let batched = store::diff_stats_many(&bare, &borrowed(&asked));
    assert_same("diff_stats", &asked, batched, |b, h| {
        store::diff_stats(&bare, b, h)
    });
    let batched = store::changed_files_many(&bare, &borrowed(&asked));
    assert_same("changed_files", &asked, batched, |b, h| {
        store::changed_files(&bare, b, h)
    });
    // The repository is shaped as it says: these are what the batch must get
    // right, not cases it happens to skip.
    let stats = |name: &str| {
        let tip = &tips.iter().find(|(n, _)| n == name).unwrap().1;
        store::changed_files(&bare, &first, tip).unwrap()
    };
    assert_eq!(stats("rename")[0].status, store::FileStatus::Renamed);
    assert_eq!(stats("typechange")[0].status, store::FileStatus::Changed);
    assert_eq!(stats("tab")[0].path, "a\tb c.txt");
    assert!(stats("empty").is_empty());
    assert!(store::diff_stats(&bare, &first, &tips.last().unwrap().1).is_ok());
    let unrelated = &tips.iter().find(|(n, _)| n == "unrelated").unwrap().1;
    assert!(store::diff_stats(&bare, &first, unrelated).is_err());
}

#[test]
fn a_comparison_git_refuses_fails_alone() {
    let (r, first, tips) = repo();
    let bare = r.bare();
    let missing = "0123456789012345678901234567890123456789".to_string();
    let mut asked = pairs(&first, &tips);
    asked.insert(1, (first.clone(), missing.clone()));
    asked.push((missing, first));
    assert_same(
        "diff_stats",
        &asked,
        store::diff_stats_many(&bare, &borrowed(&asked)),
        |b, h| store::diff_stats(&bare, b, h),
    );
    assert_same(
        "changed_files",
        &asked,
        store::changed_files_many(&bare, &borrowed(&asked)),
        |b, h| store::changed_files(&bare, b, h),
    );
}

#[test]
fn test_merges_answer_as_one_process_each_does() {
    let (r, first, tips) = repo();
    let bare = r.bare();
    let single = |b: &str, h: &str| store::merge_tree(&bare, b, h).map(|t| t.is_some());
    // Without the unrelated history, the batch answers every pair itself.
    let unrelated = &tips.iter().find(|(n, _)| n == "unrelated").unwrap().1;
    let related: Vec<(String, String)> = pairs(&first, &tips)
        .into_iter()
        .filter(|(b, h)| b != unrelated && h != unrelated)
        .collect();
    let batched = store::merges_clean(&bare, &borrowed(&related));
    assert_same("merges_clean", &related, batched, single);
    let main = &tips.last().unwrap().1;
    let conflict = &tips.iter().find(|(n, _)| n == "conflict").unwrap().1;
    assert_eq!(store::merge_tree(&bare, main, conflict).unwrap(), None);
    // An unrelated history stops git's batch; every pair is still answered,
    // and that one fails as it does alone.
    let all = pairs(&first, &tips);
    let batched = store::merges_clean(&bare, &borrowed(&all));
    assert_same("merges_clean", &all, batched, single);
    // A line of `merge-tree --stdin` would read this name as a merge base and
    // a side, making a clean merge of the name's commits that the name alone
    // does not ask for; it is answered as it is alone.
    let mut named = related.clone();
    named.push((format!("{first} -- {first}"), main.clone()));
    let batched = store::merges_clean(&bare, &borrowed(&named));
    assert_same("merges_clean", &named, batched, single);
}

#[test]
fn commit_dates_answer_as_one_process_each_does() {
    let (r, _, tips) = repo();
    let bare = r.bare();
    r.git(&["tag", "-a", "-m", "a tag", "v1", &tips[0].1]);
    let tag = r.git(&["rev-parse", "v1"]);
    let tree = r.git(&["rev-parse", "main^{tree}"]);
    let mut shas: Vec<String> = tips.iter().map(|(_, tip)| tip.clone()).collect();
    let check = |shas: &[String]| {
        let asked: Vec<&str> = shas.iter().map(String::as_str).collect();
        let batched = store::commit_dates_unix(&bare, &asked);
        assert_eq!(batched.len(), asked.len());
        for (sha, batched) in asked.iter().zip(&batched) {
            assert_eq!(
                shown(batched),
                shown(&store::commit_date_unix(&bare, sha)),
                "date of {sha}"
            );
        }
    };
    check(&shas);
    let dates: std::collections::BTreeSet<Option<i64>> = shas
        .iter()
        .map(|sha| store::commit_date_unix(&bare, sha).unwrap())
        .collect();
    assert_eq!(dates.len(), shas.len(), "each commit has a date of its own");
    // A sha listed twice, and a tag naming a listed commit, are answered as
    // the commit is. A sha naming no commit stops git's batch; every other
    // is still answered.
    shas.push(tips[0].1.clone());
    shas.push(tag);
    check(&shas);
    shas.push(tree);
    shas.push("0123456789012345678901234567890123456789".to_string());
    shas.push("-n1".to_string());
    check(&shas);
}

#[test]
fn ref_reads_answer_as_one_process_each_does() {
    let (r, _, tips) = repo();
    let bare = r.bare();
    r.git(&["symbolic-ref", "refs/heads/alias", "refs/heads/main"]);
    r.git(&["tag", "-a", "-m", "a tag", "v1", &tips[0].1]);
    let mut revs: Vec<String> = tips
        .iter()
        .map(|(name, _)| format!("refs/heads/{name}"))
        .collect();
    revs.extend(
        [
            "refs/heads/alias",
            "refs/heads/gone",
            "refs/tags/v1",
            "main",
            "",
            "-q",
            "refs/heads/a\nb",
            "main~1",
        ]
        .map(String::from),
    );
    let asked: Vec<&str> = revs.iter().map(String::as_str).collect();
    let batched = store::rev_parse_many(&bare, &asked).unwrap();
    assert_eq!(batched.len(), asked.len());
    for (rev, batched) in asked.iter().zip(batched) {
        assert_eq!(batched, store::rev_parse(&bare, rev).unwrap(), "{rev:?}");
    }
}

#[test]
fn a_batch_of_nothing_runs_nothing() {
    let missing = Path::new("/nonexistent/repository");
    assert!(store::diff_stats_many(missing, &[]).is_empty());
    assert!(store::changed_files_many(missing, &[]).is_empty());
    assert!(store::merges_clean(missing, &[]).is_empty());
    assert!(store::commit_dates_unix(missing, &[]).is_empty());
    assert!(store::rev_parse_many(missing, &[]).unwrap().is_empty());
    // Every rev is one git is never asked about.
    assert_eq!(
        store::rev_parse_many(missing, &["", "-x"]).unwrap(),
        vec![None, None]
    );
}
