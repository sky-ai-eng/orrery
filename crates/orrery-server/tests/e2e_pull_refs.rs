//! End-to-end: the `refs/pull/{n}/head` ref every pull request keeps
//! (spec/surface-git.md), fetched by number with real `git`. It is set when the
//! pull request opens, follows its head while it is open, stays where it is
//! once the pull request closes or its branch goes, refuses pushes, and is
//! rebuilt by replaying the action log (rule 5).

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{boot, face_ok, git, git_output, mint_actor};
use orrery_face_git::store;
use orrery_world::LogEntry;

/// Commit `file` with `contents` on the current branch and return the sha.
fn commit(src: &Path, clock: i64, file: &str, contents: &str) -> String {
    std::fs::write(src.join(file), contents).unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-m", file]);
    git(src, clock, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Fetch a pull request by number, the way a client that knows only its
/// number does, and return the commit fetched.
fn fetch_pull(client: &Path, clock: i64, number: i64) -> String {
    git(
        client,
        clock,
        &["fetch", "origin", &format!("pull/{number}/head")],
    );
    git(client, clock, &["rev-parse", "FETCH_HEAD"])
        .trim()
        .to_string()
}

/// The refs the action log implies for one repository, rebuilt from its
/// entries alone: a `ref.update` moves its ref, a `pull.open` pins
/// `refs/pull/{n}/head` at its head branch's tip, and an open pull request's
/// ref follows every later move of that branch.
fn refs_from_log(log: &[LogEntry], repo: &str) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    let mut heads: BTreeMap<i64, String> = BTreeMap::new();
    let mut open: Vec<i64> = Vec::new();
    for entry in log.iter().filter(|e| e.details["repo"] == repo) {
        let d = &entry.details;
        let number = d["number"].as_i64();
        match entry.action.as_str() {
            "ref.update" => {
                let name = d["ref"].as_str().unwrap();
                let new = d["new"].as_str().unwrap();
                if new == store::ZERO_OID {
                    refs.remove(name);
                    continue;
                }
                refs.insert(name.to_string(), new.to_string());
                if let Some(branch) = name.strip_prefix("refs/heads/") {
                    for n in open.iter().filter(|n| heads[n] == branch) {
                        refs.insert(store::pull_head_ref(*n), new.to_string());
                    }
                }
            }
            "pull.open" => {
                let n = number.unwrap();
                let head = d["head"].as_str().unwrap().to_string();
                let tip = refs[&format!("refs/heads/{head}")].clone();
                refs.insert(store::pull_head_ref(n), tip);
                heads.insert(n, head);
                open.push(n);
            }
            "pull.close" | "pull.merge" => open.retain(|n| Some(*n) != number),
            "pull.reopen" => open.push(number.unwrap()),
            _ => {}
        }
    }
    refs
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_head_refs_follow_the_head_and_refuse_pushes() {
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
    let api = format!("{base}/api/v3/repos/alice/webapp");
    let bare = store::bare_dir(&inst.world.repos_dir(), "alice", "webapp");
    let pull_ref = |n: i64| store::rev_parse(&bare, &store::pull_head_ref(n)).unwrap();

    // main and release at one commit, feature one ahead of them.
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    commit(&src, clock, "README.md", "# webapp\n");
    git(&src, clock, &["branch", "release"]);
    git(&src, clock, &["checkout", "-b", "feature"]);
    let feature_v1 = commit(&src, clock, "feature.txt", "v1\n");
    git(
        &src,
        clock,
        &["push", &authed_url, "main", "release", "feature"],
    );

    // Opening the pull request sets its ref, which a client that never saw
    // the head branch fetches by number.
    let pr = face_ok(
        "POST",
        &format!("{api}/pulls"),
        &token,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "main" }),
    );
    assert_eq!(pr["number"], 1);
    let client = work.path().join("client");
    git(
        work.path(),
        clock,
        &[
            "clone",
            "--single-branch",
            "--branch",
            "main",
            &authed_url,
            client.to_str().unwrap(),
        ],
    );
    assert_eq!(fetch_pull(&client, clock, 1), feature_v1);
    let advertised = git(&client, clock, &["ls-remote", "origin", "refs/pull/*"]);
    assert_eq!(advertised, format!("{feature_v1}\trefs/pull/1/head\n"));

    // A push to the head branch moves the ref with the pull request's head.
    let feature_v2 = commit(&src, clock, "feature.txt", "v2\n");
    git(&src, clock, &["push", &authed_url, "feature"]);
    assert_eq!(fetch_pull(&client, clock, 1), feature_v2);

    // A second pull request whose head is the base of the first: merging the
    // first moves main, so this one's head and ref take the merge commit.
    git(&src, clock, &["checkout", "main"]);
    let main_v2 = commit(&src, clock, "main.txt", "v2\n");
    git(&src, clock, &["push", &authed_url, "main"]);
    let release = face_ok(
        "POST",
        &format!("{api}/pulls"),
        &token,
        serde_json::json!({ "title": "Release", "head": "main", "base": "release" }),
    );
    assert_eq!(release["number"], 2);
    assert_eq!(pull_ref(2).as_deref(), Some(main_v2.as_str()));

    let merged = face_ok(
        "PUT",
        &format!("{api}/pulls/1/merge"),
        &token,
        serde_json::json!({}),
    );
    let merge_sha = merged["sha"].as_str().unwrap().to_string();
    assert_eq!(
        fetch_pull(&client, clock, 1),
        feature_v2,
        "a merge leaves the merged pull request's ref at its head"
    );
    assert_eq!(fetch_pull(&client, clock, 2), merge_sha);
    let release = face_ok(
        "GET",
        &format!("{api}/pulls/2"),
        &token,
        serde_json::json!({}),
    );
    assert_eq!(release["head"]["sha"], merge_sha.as_str());

    // Closing a pull request leaves its ref where it is, even when its head
    // branch moves on, and so does deleting the head branch.
    face_ok(
        "PATCH",
        &format!("{api}/pulls/2"),
        &token,
        serde_json::json!({ "state": "closed" }),
    );
    git(&src, clock, &["pull", "--ff-only", &authed_url, "main"]);
    commit(&src, clock, "main.txt", "v3\n");
    git(&src, clock, &["push", &authed_url, "main"]);
    assert_eq!(fetch_pull(&client, clock, 2), merge_sha);
    git(&src, clock, &["push", &authed_url, ":feature"]);
    assert_eq!(store::rev_parse(&bare, "refs/heads/feature").unwrap(), None);
    assert_eq!(fetch_pull(&client, clock, 1), feature_v2);

    // A push to a pull request's ref is refused as GitHub refuses it, whether
    // the ref exists or not, and moves nothing.
    let log_before = inst.world.log_after(0, 10_000).unwrap().len();
    let refused = git_output(
        &client,
        clock,
        &[
            "push",
            "origin",
            "HEAD:refs/pull/1/head",
            "HEAD:refs/pull/9/head",
        ],
    );
    assert!(!refused.status.success(), "a push to refs/pull/* must fail");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(
        stderr.matches("deny updating a hidden ref").count(),
        2,
        "{stderr}"
    );
    assert_eq!(pull_ref(1).as_deref(), Some(feature_v2.as_str()));
    assert_eq!(pull_ref(9), None);
    assert_eq!(inst.world.log_after(0, 10_000).unwrap().len(), log_before);

    // Replay: the log alone rebuilds every ref the repository holds, the
    // pull requests' refs included, though none has an entry of its own.
    let log = inst.world.log_after(0, 10_000).unwrap();
    assert!(
        log.iter().all(|e| !e.details["ref"]
            .as_str()
            .is_some_and(|r| r.starts_with("refs/pull/"))),
        "a pull request's ref follows from its own entries"
    );
    let refs = store::list_refs(&bare).unwrap();
    assert_eq!(
        refs.keys()
            .filter(|r| r.starts_with("refs/pull/"))
            .collect::<Vec<_>>(),
        ["refs/pull/1/head", "refs/pull/2/head"]
    );
    assert_eq!(refs_from_log(&log, "alice/webapp"), refs);
}
