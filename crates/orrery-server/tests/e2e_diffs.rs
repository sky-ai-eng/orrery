//! End-to-end for a comparison's diff (spec/surface-github.md §1.4 and §1.8):
//! the diff media type on a pull request and on a compare, GitHub's 406 for a
//! pull request's diff past its file limit, and the line counts and patches a
//! changed file carries, which a consumer refused the diff rebuilds it from.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use common::{boot, face_answer, face_ok, git, mint_actor};

struct Seeded {
    /// Serves the world for as long as the test holds it.
    _inst: common::Instance,
    token: String,
    _work: tempfile::TempDir,
    src: PathBuf,
    authed_url: String,
    api: String,
    clock: i64,
}

/// Boot, mint alice, create alice/webapp, and push a `main` holding `files`.
async fn seed(files: &[(&str, &[u8])]) -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let token = mint_actor(&base, "alice", "user");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        serde_json::json!({ "name": "webapp" }),
    );
    let authed_url = repo["clone_url"]
        .as_str()
        .unwrap()
        .replace("http://", &format!("http://x-access-token:{token}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    for (path, body) in files {
        write(&src, path, body);
    }
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["push", &authed_url, "main"]);
    Seeded {
        _inst: inst,
        token,
        _work: work,
        src,
        authed_url,
        api: format!("{base}/api/v3/repos/alice/webapp"),
        clock,
    }
}

impl Seeded {
    fn git(&self, args: &[&str]) -> String {
        git(&self.src, self.clock, args)
    }

    /// Commit everything in the work tree to `branch` and push it.
    fn push(&self, branch: &str, message: &str) {
        self.git(&["add", "-A", "."]);
        self.git(&["commit", "-m", message]);
        self.git(&["push", &self.authed_url, branch]);
    }

    fn open_pull(&self, head: &str) -> u64 {
        let pull = face_ok(
            "POST",
            &format!("{}/pulls", self.api),
            &self.token,
            serde_json::json!({ "title": head, "head": head, "base": "main" }),
        );
        pull["number"].as_u64().unwrap()
    }

    /// `path` read with `accept`: its status, content type, and body.
    fn read(&self, path: &str, accept: &str) -> (u16, String, Vec<u8>) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut resp = agent
            .get(format!("{}{path}", self.api))
            .header("authorization", &format!("token {}", self.token))
            .header("accept", accept)
            .call()
            .expect("transport");
        let content_type = resp.headers()["content-type"].to_str().unwrap().to_string();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_to_vec()
            .unwrap();
        (resp.status().as_u16(), content_type, body)
    }

    /// Every changed file a pull request's files list, page by page.
    fn pull_files(&self, number: u64) -> Vec<serde_json::Value> {
        self.pull_files_paged(number, 100)
    }

    /// [`Self::pull_files`], `per_page` to a page.
    fn pull_files_paged(&self, number: u64, per_page: usize) -> Vec<serde_json::Value> {
        let mut files = Vec::new();
        for page in 1.. {
            let listed = face_ok(
                "GET",
                &format!(
                    "{}/pulls/{number}/files?per_page={per_page}&page={page}",
                    self.api
                ),
                &self.token,
                serde_json::json!(null),
            );
            let listed = listed.as_array().unwrap().clone();
            let short = listed.len() < per_page;
            files.extend(listed);
            if short {
                return files;
            }
        }
        unreachable!()
    }
}

fn write(src: &Path, path: &str, body: &[u8]) {
    let path = src.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// What a consumer reads off a diff to anchor a review comment: each file's
/// hunks, as the new side's line ranges, keyed by the file's path.
type Hunks = BTreeMap<String, Vec<(u64, u64)>>;

/// The new-side ranges of one file's hunks. A hunk that adds no line to the
/// new side has none.
fn patch_hunks(patch: &str) -> Vec<(u64, u64)> {
    let mut hunks = Vec::new();
    let mut open: Option<(u64, u64)> = None;
    let mut close = |open: &mut Option<(u64, u64)>| {
        if let Some((start, next)) = open.take()
            && next > start
        {
            hunks.push((start, next - 1));
        }
    };
    for line in patch.split('\n') {
        if line.starts_with("@@") {
            close(&mut open);
            let new = line.split(' ').nth(2).unwrap();
            let start: u64 = new[1..].split(',').next().unwrap().parse().unwrap();
            open = (start > 0).then_some((start, start));
        } else if let Some((_, next)) = open.as_mut()
            && (line.starts_with('+') || line.starts_with(' '))
        {
            *next += 1;
        }
    }
    close(&mut open);
    hunks
}

/// The hunks of a whole diff, each file keyed by its `b/` path.
fn diff_hunks(diff: &str) -> Hunks {
    let mut hunks = Hunks::new();
    let mut file: Option<String> = None;
    let mut patch = String::new();
    let mut flush = |file: &Option<String>, patch: &mut String| {
        if let Some(file) = file {
            hunks
                .entry(file.clone())
                .or_default()
                .extend(patch_hunks(patch));
        }
        patch.clear();
    };
    for line in diff.split('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            flush(&file, &mut patch);
            file = Some(header.split_once(" b/").unwrap().1.to_string());
        } else {
            patch.push_str(line);
            patch.push('\n');
        }
    }
    flush(&file, &mut patch);
    hunks
}

/// The hunks a consumer rebuilds from a files list's patches.
fn files_hunks(files: &[serde_json::Value]) -> Hunks {
    let mut hunks = Hunks::new();
    for f in files {
        let patch = f["patch"].as_str().unwrap_or("");
        hunks
            .entry(f["filename"].as_str().unwrap().to_string())
            .or_default()
            .extend(patch_hunks(patch));
    }
    hunks
}

/// A pull request and a compare answer the diff media type with the diff git
/// prints for the comparison, renames detected, and each changed file
/// carries its line counts and its part of that diff, so the hunks a
/// consumer reads off either are the same.
#[tokio::test(flavor = "multi_thread")]
async fn the_diff_media_type_and_the_files_carry_one_diff() {
    let renamed: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    let s = seed(&[
        ("old.txt", renamed.as_bytes()),
        ("data.bin", b"\x00\x01\x02 a binary file\n"),
        ("run.sh", b"#!/bin/sh\necho run\n"),
        ("tail.txt", b"one\ntwo"),
        ("gone.txt", b"going\naway\n"),
        ("link", b"a file that becomes a link\n"),
    ])
    .await;
    s.git(&["checkout", "-b", "feature"]);
    s.git(&["mv", "old.txt", "new.txt"]);
    write(
        &s.src,
        "new.txt",
        renamed.replace("line 10\n", "line ten\n").as_bytes(),
    );
    write(&s.src, "data.bin", b"\x00\x01\x03 a changed binary file\n");
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(s.src.join("run.sh"), mode).unwrap();
    }
    write(&s.src, "tail.txt", b"one\nthree");
    std::fs::remove_file(s.src.join("gone.txt")).unwrap();
    write(&s.src, "added.txt", b"new\nfile\n");
    write(&s.src, "empty", b"");
    std::fs::remove_file(s.src.join("link")).unwrap();
    std::os::unix::fs::symlink("new.txt", s.src.join("link")).unwrap();
    s.push("feature", "change everything");
    let number = s.open_pull("feature");

    let expected = s.git(&["diff", "-M", "--abbrev=7", "main...feature"]);
    let pull = format!("/pulls/{number}");
    for media in [
        "application/vnd.github.v3.diff",
        "application/vnd.github.diff",
    ] {
        let (status, content_type, body) = s.read(&pull, media);
        assert_eq!(status, 200, "{media}: {}", String::from_utf8_lossy(&body));
        assert_eq!(content_type, format!("{media}; charset=utf-8"));
        assert_eq!(String::from_utf8(body).unwrap(), expected, "{media}");
    }
    let (status, _, body) = s.read("/compare/main...feature", "application/vnd.github.v3.diff");
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8(body).unwrap(), expected);
    for shape in [
        "\nrename from old.txt\nrename to new.txt\n",
        "Binary files a/data.bin and b/data.bin differ\n",
        "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n",
        "-two\n\\ No newline at end of file\n+three\n\\ No newline at end of file\n",
    ] {
        assert!(expected.contains(shape), "{shape:?} is not in\n{expected}");
    }

    let files = s.pull_files(number);
    let by_name = |name: &str| {
        let file = files.iter().find(|f| f["filename"] == name).unwrap();
        (
            file["status"].as_str().unwrap(),
            file["additions"].as_i64().unwrap(),
            file["deletions"].as_i64().unwrap(),
            file["changes"].as_i64().unwrap(),
            file.get("patch").and_then(|p| p.as_str()),
        )
    };
    assert_eq!(
        by_name("new.txt"),
        (
            "renamed",
            1,
            1,
            2,
            Some(
                "@@ -7,7 +7,7 @@ line 6\n line 7\n line 8\n line 9\n-line 10\n+line ten\n line 11\n line 12\n line 13"
            )
        )
    );
    assert_eq!(by_name("data.bin"), ("modified", 0, 0, 0, None));
    assert_eq!(by_name("run.sh"), ("modified", 0, 0, 0, None));
    assert_eq!(by_name("empty"), ("added", 0, 0, 0, None));
    assert_eq!(
        by_name("tail.txt"),
        (
            "modified",
            1,
            1,
            2,
            Some(
                "@@ -1,2 +1,2 @@\n one\n-two\n\\ No newline at end of file\n+three\n\\ No newline at end of file"
            )
        )
    );
    assert_eq!(
        by_name("gone.txt"),
        ("removed", 0, 2, 2, Some("@@ -1,2 +0,0 @@\n-going\n-away"))
    );
    assert_eq!(
        by_name("added.txt"),
        ("added", 2, 0, 2, Some("@@ -0,0 +1,2 @@\n+new\n+file"))
    );
    // A type change is two files of one name: the one removed, then the one
    // added.
    let link: Vec<(&str, i64, i64, Option<&str>)> = files
        .iter()
        .filter(|f| f["filename"] == "link")
        .map(|f| {
            (
                f["status"].as_str().unwrap(),
                f["additions"].as_i64().unwrap(),
                f["deletions"].as_i64().unwrap(),
                f.get("patch").and_then(|p| p.as_str()),
            )
        })
        .collect();
    assert_eq!(
        link,
        vec![
            (
                "removed",
                0,
                1,
                Some("@@ -1 +0,0 @@\n-a file that becomes a link")
            ),
            (
                "added",
                1,
                0,
                Some("@@ -0,0 +1 @@\n+new.txt\n\\ No newline at end of file")
            ),
        ]
    );
    assert_eq!(
        files.iter().find(|f| f["filename"] == "new.txt").unwrap()["previous_filename"],
        "old.txt"
    );

    // Read either way, the comparison has the same hunks.
    assert_eq!(files_hunks(&files), diff_hunks(&expected));
    let compare = face_ok(
        "GET",
        &format!("{}/compare/main...feature", s.api),
        &s.token,
        serde_json::json!(null),
    );
    assert_eq!(compare["files"].as_array().unwrap(), &files);

    // The diff media type wins over any other `Accept` lists beside it.
    for accept in [
        "application/vnd.github+json, application/vnd.github.v3.diff",
        "application/vnd.github.v3.patch, application/vnd.github.v3.diff",
        "application/vnd.github.v3.full+json, application/vnd.github.diff",
        "application/vnd.github.v3.diff+json",
    ] {
        let (status, _, body) = s.read(&pull, accept);
        assert_eq!(
            (status, String::from_utf8(body).unwrap()),
            (200, expected.clone()),
            "{accept}"
        );
    }
    // The patch media type is still refused, and the files list is JSON alone.
    for (path, accept, resource) in [
        (
            pull.clone(),
            "application/vnd.github.v3.patch",
            "PullRequest",
        ),
        (
            "/compare/main...feature".to_string(),
            "application/vnd.github.patch",
            "Comparison",
        ),
        (
            format!("{pull}/files"),
            "application/vnd.github.v3.diff",
            "PullRequest",
        ),
    ] {
        let (status, refused) = face_answer(
            "GET",
            &format!("{}{path}", s.api),
            &s.token,
            &[("accept", accept)],
            None,
        );
        assert_eq!(status, 422, "{path} {accept}: {refused}");
        assert_eq!(refused["orrery"]["not_mocked"], true, "{refused}");
        assert_eq!(refused["errors"][0]["resource"], resource, "{refused}");
    }

    // Something that doesn't exist is GitHub's 404, whatever the request
    // accepts.
    let (status, _, _) = s.read("/pulls/9", "application/vnd.github.v3.diff");
    assert_eq!(status, 404);
    let (status, _, _) = s.read("/compare/main...ghost", "application/vnd.github.v3.diff");
    assert_eq!(status, 404);
    // An identical comparison's diff is empty.
    let (status, _, body) = s.read("/compare/main...main", "application/vnd.github.v3.diff");
    assert_eq!((status, body.len()), (200, 0));
}

/// A pull request's diff renders at most 300 files and answers GitHub's 406
/// past them, while its files list them all; a compare's diff has no limit.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_diff_past_300_files_is_not_acceptable() {
    let s = seed(&[]).await;
    s.git(&["checkout", "-b", "wide"]);
    for n in 0..300 {
        write(&s.src, &format!("files/{n:03}.txt"), b"one line\n");
    }
    s.push("wide", "300 files");
    let number = s.open_pull("wide");
    let pull = format!("/pulls/{number}");
    let (status, _, body) = s.read(&pull, "application/vnd.github.v3.diff");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        String::from_utf8(body)
            .unwrap()
            .matches("diff --git ")
            .count(),
        300
    );

    write(&s.src, "files/300.txt", b"one line\n");
    s.push("wide", "301 files");
    let (status, too_large) = face_answer(
        "GET",
        &format!("{}{pull}", s.api),
        &s.token,
        &[("accept", "application/vnd.github.v3.diff")],
        None,
    );
    assert_eq!(status, 406, "{too_large}");
    assert_eq!(
        too_large,
        serde_json::json!({
            "message": "Sorry, the diff exceeded the maximum number of files (300). Consider using 'List pull requests files' API or locally cloning the repository instead.",
            "errors": [{ "resource": "PullRequest", "field": "diff", "code": "too_large" }],
        })
    );
    let files = s.pull_files(number);
    assert_eq!(files.len(), 301);
    assert!(
        files
            .iter()
            .all(|f| f["patch"] == "@@ -0,0 +1 @@\n+one line")
    );

    let (status, _, body) = s.read("/compare/main...wide", "application/vnd.github.v3.diff");
    assert_eq!(status, 200);
    let diff = String::from_utf8(body).unwrap();
    assert_eq!(diff.matches("diff --git ").count(), 301);
    assert_eq!(files_hunks(&files), diff_hunks(&diff));
}

/// The content of an added file whose patch is exactly `bytes` long, at
/// least 51,968: 1,000 lines, the last one sized to fit.
fn added_with_patch_of(bytes: usize) -> String {
    let header = "@@ -0,0 +1,1000 @@\n".len();
    let line = "x".repeat(50);
    let last = bytes - header - 999 * (1 + line.len() + 1) - 1;
    let mut body: String = (0..999).map(|_| format!("{line}\n")).collect();
    body.push_str(&"y".repeat(last));
    body.push('\n');
    body
}

/// An added file of `lines` numbered lines.
fn numbered(lines: usize) -> String {
    (1..=lines).map(|n| format!("{n}\n")).collect()
}

/// Each file's name, additions, and patch length, in the order listed.
fn summary(files: &[serde_json::Value]) -> Vec<(String, i64, Option<usize>)> {
    files
        .iter()
        .map(|f| {
            (
                f["filename"].as_str().unwrap().to_string(),
                f["additions"].as_i64().unwrap(),
                f.get("patch").map(|p| p.as_str().unwrap().len()),
            )
        })
        .collect()
}

/// A patch of 100 KiB or more is left out, the file keeping its line counts.
/// A response's files carry patches until the next one would take them past
/// 20,000 lines, and from that file on carry neither patches nor counts. The
/// limit is per response, so a page of a pull request's files holds its own.
#[tokio::test(flavor = "multi_thread")]
async fn patches_are_left_out_past_githubs_line_limits() {
    let s = seed(&[]).await;
    s.git(&["checkout", "-b", "big"]);
    write(
        &s.src,
        "a/below-limit.txt",
        added_with_patch_of(100 * 1024 - 1).as_bytes(),
    );
    write(
        &s.src,
        "a/at-limit.txt",
        added_with_patch_of(100 * 1024).as_bytes(),
    );
    // 5,001 patch lines each: the fourth takes a response past 20,000.
    for n in 1..=4 {
        write(&s.src, &format!("b/{n}.txt"), numbered(5_000).as_bytes());
    }
    write(&s.src, "c/small.txt", b"small\n");
    s.push("big", "big files");
    let number = s.open_pull("big");

    let five = "@@ -0,0 +1,5000 @@\n".len() + 5_000 + numbered(5_000).len() - 1;
    let capped = vec![
        ("a/at-limit.txt".to_string(), 1000, None),
        ("a/below-limit.txt".to_string(), 1000, Some(100 * 1024 - 1)),
        ("b/1.txt".to_string(), 5000, Some(five)),
        ("b/2.txt".to_string(), 5000, Some(five)),
        ("b/3.txt".to_string(), 5000, Some(five)),
        ("b/4.txt".to_string(), 0, None),
        ("c/small.txt".to_string(), 0, None),
    ];
    let compare = face_ok(
        "GET",
        &format!("{}/compare/main...big", s.api),
        &s.token,
        serde_json::json!(null),
    );
    assert_eq!(summary(compare["files"].as_array().unwrap()), capped);
    assert_eq!(compare["files"][5]["changes"], 0);
    assert_eq!(summary(&s.pull_files(number)), capped);

    // Four files to a page, neither page reaches the limit.
    let files = s.pull_files_paged(number, 4);
    assert_eq!(
        summary(&files),
        vec![
            ("a/at-limit.txt".to_string(), 1000, None),
            ("a/below-limit.txt".to_string(), 1000, Some(100 * 1024 - 1)),
            ("b/1.txt".to_string(), 5000, Some(five)),
            ("b/2.txt".to_string(), 5000, Some(five)),
            ("b/3.txt".to_string(), 5000, Some(five)),
            ("b/4.txt".to_string(), 5000, Some(five)),
            (
                "c/small.txt".to_string(),
                1,
                Some("@@ -0,0 +1 @@\n+small".len())
            ),
        ]
    );
    // The diff media type has every file's hunks, the left-out patch's too.
    let (status, _, body) = s.read(
        &format!("/pulls/{number}"),
        "application/vnd.github.v3.diff",
    );
    assert_eq!(status, 200);
    let diff = diff_hunks(&String::from_utf8(body).unwrap());
    assert_eq!(diff["a/at-limit.txt"], vec![(1, 1000)]);
    let mut rebuilt = files_hunks(&files);
    rebuilt.insert("a/at-limit.txt".to_string(), vec![(1, 1000)]);
    assert_eq!(rebuilt, diff);
}

/// A response's files carry patches until the next one would take them past
/// 1 MiB, each patch counting one byte more than its length, and from that
/// file on carry neither patches nor counts.
#[tokio::test(flavor = "multi_thread")]
async fn patches_are_left_out_past_githubs_byte_limit() {
    let s = seed(&[]).await;
    // Ten patches of 95,000 bytes, then one that brings the total to exactly
    // 1 MiB on `full` and one byte past it on `over`, then a small one.
    let last_fits = (1 << 20) - 10 * 95_001 - 1;
    for (branch, last) in [("full", last_fits), ("over", last_fits + 1)] {
        s.git(&["checkout", "-q", "-b", branch, "main"]);
        for n in 1..=10 {
            write(
                &s.src,
                &format!("h{n:02}.txt"),
                added_with_patch_of(95_000).as_bytes(),
            );
        }
        write(&s.src, "h11.txt", added_with_patch_of(last).as_bytes());
        write(&s.src, "h12.txt", b"small\n");
        s.push(branch, branch);
    }
    let ten = || (1..=10).map(|n| (format!("h{n:02}.txt"), 1000, Some(95_000)));
    let compare = |branch: &str| {
        let compare = face_ok(
            "GET",
            &format!("{}/compare/main...{branch}", s.api),
            &s.token,
            serde_json::json!(null),
        );
        summary(compare["files"].as_array().unwrap())
    };
    let full: Vec<_> = ten()
        .chain([
            ("h11.txt".to_string(), 1000, Some(last_fits)),
            ("h12.txt".to_string(), 0, None),
        ])
        .collect();
    assert_eq!(compare("full"), full);
    let over: Vec<_> = ten()
        .chain([
            ("h11.txt".to_string(), 0, None),
            ("h12.txt".to_string(), 0, None),
        ])
        .collect();
    assert_eq!(compare("over"), over);
    let number = s.open_pull("full");
    assert_eq!(summary(&s.pull_files(number)), full);
}
