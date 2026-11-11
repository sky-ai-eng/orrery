//! End-to-end: the `refs/pull/{n}/head` ref every pull request keeps
//! (spec/surface-git.md), fetched by number with real `git`. It is set when the
//! pull request opens, follows its head while it is open, stays where it is
//! once the pull request closes or its branch goes, takes its branch's tip
//! when the pull request reopens, refuses pushes, and follows from the log's
//! events (rule 5).

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::{Instance, boot, face, face_answer, face_ok, git, git_output, mint_actor};
use orrery_face_git::store;
use orrery_world::{Event, Signal, World};

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

/// The refs the log implies for one repository, rebuilt from its events
/// alone: a ref event sets or removes its ref, and every pull event that
/// carries the pull's head sets `refs/pull/{n}/head` to it.
fn refs_from_log(log: &[Event], repo: &str) -> BTreeMap<String, String> {
    let (owner, name) = repo.split_once('/').unwrap();
    let mut refs = BTreeMap::new();
    for e in log {
        let b = serde_json::to_value(&e.body).unwrap();
        let subject = &b["subject"];
        if subject["owner"]["login"] != owner || subject["repo"]["name"] != name {
            continue;
        }
        let sha = |v: &serde_json::Value| v.as_str().unwrap().to_string();
        match b["type"].as_str().unwrap() {
            "ref:created" => {
                refs.insert(sha(&subject["name"]), sha(&b["object"]["sha"]));
            }
            "ref:updated" => {
                refs.insert(sha(&subject["name"]), sha(&b["changes"]["sha"]["to"]));
            }
            "ref:deleted" => {
                refs.remove(subject["name"].as_str().unwrap());
            }
            kind if kind.starts_with("pull:") => {
                let head = match kind {
                    "pull:opened" => &b["object"]["head"],
                    _ => &b["changes"]["head"]["to"],
                };
                if let Some(head) = head["sha"].as_str() {
                    let number = subject["number"].as_i64().unwrap();
                    refs.insert(store::pull_head_ref(number), head.to_string());
                }
            }
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

    // The log alone rebuilds every ref the repository holds, the pull
    // requests' refs included, though none has an event of its own.
    let log = inst.world.log_after(0, 10_000).unwrap();
    assert!(
        log.iter().all(
            |e| !serde_json::to_value(&e.body).unwrap()["subject"]["name"]
                .as_str()
                .is_some_and(|r| r.starts_with("refs/pull/"))
        ),
        "a pull request's ref follows from its pull's head"
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

#[tokio::test(flavor = "multi_thread")]
async fn a_push_moves_its_refs_before_its_events_can_be_read() {
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
    let bare = store::bare_dir(&inst.world.repos_dir(), "alice", "webapp");

    // A listener runs as each write's events are appended, before any reader
    // can see them, so what it reads from the repository is the earliest view
    // a reader woken by them could have. It runs git with the world's lock
    // held, which only slows this test, and it records what git answered
    // rather than panicking there, which would poison the world's lock and
    // hide the cause.
    // At each write: where HEAD resolves, and where `refs/pull/1/head` points.
    type Refs = (
        Result<Option<String>, String>,
        Result<Option<String>, String>,
    );
    let seen: Arc<Mutex<Vec<Refs>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let watched = bare.clone();
    inst.world.listen(move |signal| {
        if signal == Signal::LogMayHaveGrown {
            let read = |rev: &str| store::rev_parse(&watched, rev).map_err(|e| e.to_string());
            let refs = (read("HEAD"), read(&store::pull_head_ref(1)));
            if let Ok(mut seen) = sink.lock() {
                seen.push(refs);
            }
        }
    });
    let during = |push: &dyn Fn()| {
        seen.lock().unwrap().clear();
        push();
        std::mem::take(&mut *seen.lock().unwrap())
    };

    // A first push of `trunk`: HEAD names it, and the repository holds no
    // `main`, so HEAD resolves only once it has moved.
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "trunk", "."]);
    let trunk = commit(&src, clock, "README.md", "# webapp\n");
    let first = during(&|| {
        git(&src, clock, &["push", &authed_url, "trunk"]);
    });
    assert!(!first.is_empty());
    for (head, _) in &first {
        assert_eq!(head, &Ok(Some(trunk.clone())), "{first:?}");
    }

    // A push to an open pull request's head branch: its ref has moved.
    git(&src, clock, &["checkout", "-b", "feature"]);
    commit(&src, clock, "feature.txt", "v1\n");
    git(&src, clock, &["push", &authed_url, "feature"]);
    face_ok(
        "POST",
        &format!("{base}/api/v3/repos/alice/webapp/pulls"),
        &token,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "trunk" }),
    );
    let feature_v2 = commit(&src, clock, "feature.txt", "v2\n");
    let sync = during(&|| {
        git(&src, clock, &["push", &authed_url, "feature"]);
    });
    assert!(!sync.is_empty());
    for (_, pull) in &sync {
        assert_eq!(pull, &Ok(Some(feature_v2.clone())), "{sync:?}");
    }
}

/// A repository where merging pull request #1 (`feature` into `main`) moves
/// the head of #2 (`main` into `release`) and of #3 (`main` into `staging`).
struct BaseOfOthers {
    inst: Instance,
    token: String,
    api: String,
    bare: PathBuf,
    /// `main`'s tip before the merge.
    main: String,
    _work: tempfile::TempDir,
}

async fn base_of_others() -> BaseOfOthers {
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

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    commit(&src, clock, "README.md", "# webapp\n");
    git(&src, clock, &["branch", "release"]);
    git(&src, clock, &["branch", "staging"]);
    git(&src, clock, &["checkout", "-b", "feature"]);
    commit(&src, clock, "feature.txt", "v1\n");
    git(&src, clock, &["checkout", "main"]);
    let main = commit(&src, clock, "main.txt", "v2\n");
    git(
        &src,
        clock,
        &["push", &authed_url, "main", "release", "staging", "feature"],
    );
    for (head, base) in [
        ("feature", "main"),
        ("main", "release"),
        ("main", "staging"),
    ] {
        face_ok(
            "POST",
            &format!("{api}/pulls"),
            &token,
            serde_json::json!({ "title": format!("{head} into {base}"), "head": head, "base": base }),
        );
    }
    BaseOfOthers {
        inst,
        token,
        api,
        bare,
        main,
        _work: work,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_moves_its_refs_before_its_events_can_be_read() {
    let s = base_of_others().await;

    // As in the push test above: a listener reads the repository as the
    // write's events are appended, which is the earliest view any reader
    // gets.
    type Refs = Vec<Result<Option<String>, String>>;
    let seen: Arc<Mutex<Vec<Refs>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let watched = s.bare.clone();
    s.inst.world.listen(move |signal| {
        if signal == Signal::LogMayHaveGrown {
            let refs = [
                "refs/heads/main".to_string(),
                store::pull_head_ref(2),
                store::pull_head_ref(3),
            ]
            .iter()
            .map(|r| store::rev_parse(&watched, r).map_err(|e| e.to_string()))
            .collect();
            if let Ok(mut seen) = sink.lock() {
                seen.push(refs);
            }
        }
    });

    let merged = face_ok(
        "PUT",
        &format!("{}/pulls/1/merge", s.api),
        &s.token,
        serde_json::json!({}),
    );
    let merge_sha = merged["sha"].as_str().unwrap().to_string();
    let seen = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(seen.len(), 1, "one signal for the merge's write: {seen:?}");
    for refs in &seen {
        assert!(
            refs.iter().all(|r| r == &Ok(Some(merge_sha.clone()))),
            "{seen:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_that_fails_puts_its_refs_back() {
    let s = base_of_others().await;
    let rev = |r: &str| store::rev_parse(&s.bare, r).unwrap();
    let pull = |n: i64| {
        face_ok(
            "GET",
            &format!("{}/pulls/{n}", s.api),
            &s.token,
            serde_json::json!({}),
        )
    };

    // A stale lock on #3's ref makes git refuse to move it, once the merge
    // has moved `main` and #2's ref.
    let lock = s.bare.join(format!("{}.lock", store::pull_head_ref(3)));
    std::fs::write(&lock, "").unwrap();
    let mark = s
        .inst
        .world
        .log_after(0, 10_000)
        .unwrap()
        .last()
        .unwrap()
        .seq;
    assert_eq!(
        face(
            "PUT",
            &format!("{}/pulls/1/merge", s.api),
            &s.token,
            serde_json::json!({}),
        ),
        Err(500)
    );

    // No entry, and every ref back where the world still has it.
    assert!(s.inst.world.log_after(mark, 10_000).unwrap().is_empty());
    assert_eq!(rev("refs/heads/main"), Some(s.main.clone()));
    assert_eq!(rev(&store::pull_head_ref(2)), Some(s.main.clone()));
    assert_eq!(rev(&store::pull_head_ref(3)), Some(s.main.clone()));
    let unmerged = pull(1);
    assert_eq!(unmerged["state"], "open");
    assert_eq!(unmerged["merged"], false);
    assert_eq!(pull(2)["head"]["sha"], s.main.as_str());

    // A retry merges from the same base: the merge commit's first parent is
    // the tip `main` had before either attempt.
    std::fs::remove_file(&lock).unwrap();
    let merged = face_ok(
        "PUT",
        &format!("{}/pulls/1/merge", s.api),
        &s.token,
        serde_json::json!({}),
    );
    let merge_sha = merged["sha"].as_str().unwrap();
    assert_eq!(rev(&format!("{merge_sha}^1")), Some(s.main.clone()));
    for r in [
        "refs/heads/main".to_string(),
        store::pull_head_ref(2),
        store::pull_head_ref(3),
    ] {
        assert_eq!(rev(&r).as_deref(), Some(merge_sha), "{r}");
    }
}

/// A repository whose `main`, `release`, and `stale` are at one commit and
/// whose `feature` is one commit ahead of them, with a pull request from
/// `feature` into `main` (#1) and one into `release` (#2), and a push, not
/// yet sent, that moves `feature`, creates `topic`, and deletes `stale`. The
/// push moves #1's and #2's refs once it is recorded.
struct PushThatFails {
    inst: Instance,
    token: String,
    api: String,
    bare: PathBuf,
    src: PathBuf,
    clock: i64,
    push: Vec<String>,
    feature_v1: String,
    feature_v2: String,
    _work: tempfile::TempDir,
}

async fn push_that_fails() -> PushThatFails {
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

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    commit(&src, clock, "README.md", "# webapp\n");
    git(&src, clock, &["branch", "release"]);
    git(&src, clock, &["branch", "stale"]);
    git(&src, clock, &["checkout", "-b", "feature"]);
    let feature_v1 = commit(&src, clock, "feature.txt", "v1\n");
    git(
        &src,
        clock,
        &["push", &authed_url, "main", "release", "stale", "feature"],
    );
    for into in ["main", "release"] {
        face_ok(
            "POST",
            &format!("{api}/pulls"),
            &token,
            serde_json::json!({ "title": format!("feature into {into}"), "head": "feature", "base": into }),
        );
    }

    let feature_v2 = commit(&src, clock, "feature.txt", "v2\n");
    git(&src, clock, &["branch", "topic"]);
    let push = ["push", &authed_url, "feature", "topic", ":stale"]
        .map(str::to_string)
        .to_vec();
    PushThatFails {
        inst,
        token,
        api,
        bare,
        src,
        clock,
        push,
        feature_v1,
        feature_v2,
        _work: work,
    }
}

impl PushThatFails {
    fn head_sha(&self, number: i64) -> String {
        face_ok(
            "GET",
            &format!("{}/pulls/{number}", self.api),
            &self.token,
            serde_json::json!(null),
        )["head"]["sha"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn args(&self) -> Vec<&str> {
        self.push.iter().map(String::as_str).collect()
    }

    /// Send the push, which the test has made fail, and check that it was
    /// answered `500` and left every ref where it was. Returns the newest
    /// `seq` before it, from which it should have appended nothing.
    fn send_and_fail(&self) -> i64 {
        let before = store::list_refs(&self.bare).unwrap();
        let mark = self
            .inst
            .world
            .log_after(0, 10_000)
            .unwrap()
            .last()
            .unwrap()
            .seq;
        let failed = git_output(&self.src, self.clock, &self.args());
        assert!(!failed.status.success());
        let stderr = String::from_utf8_lossy(&failed.stderr);
        assert!(stderr.contains("500"), "{stderr}");
        assert_eq!(store::list_refs(&self.bare).unwrap(), before);
        mark
    }

    /// Check that the failed push appended nothing after `mark` and left the
    /// pull requests on their heads, then retry it, now that it can land: it
    /// lands as if the failed push had not happened, its events starting
    /// from the refs the failed push found, so the log rebuilds the
    /// repository.
    fn moved_nothing_and_retry_lands(&self, mark: i64) {
        assert!(self.inst.world.log_after(mark, 10_000).unwrap().is_empty());
        assert_eq!(self.head_sha(1), self.feature_v1);
        assert_eq!(self.head_sha(2), self.feature_v1);

        git(&self.src, self.clock, &self.args());
        let refs = store::list_refs(&self.bare).unwrap();
        for r in [
            "refs/heads/feature".to_string(),
            "refs/heads/topic".to_string(),
            store::pull_head_ref(1),
            store::pull_head_ref(2),
        ] {
            assert_eq!(refs.get(&r), Some(&self.feature_v2), "{r}");
        }
        assert!(!refs.contains_key("refs/heads/stale"));
        assert_eq!(self.head_sha(2), self.feature_v2);
        let log = self.inst.world.log_after(0, 10_000).unwrap();
        let retried = log
            .iter()
            .find_map(|e| match &e.body {
                orrery_world::EventBody::RefUpdated(u)
                    if e.seq > mark && u.subject.name == "refs/heads/feature" =>
                {
                    Some(u)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            retried.changes.sha.as_ref().unwrap().from,
            self.feature_v1.as_str()
        );
        assert_eq!(refs_from_log(&log, "alice/webapp"), refs);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_that_fails_puts_its_refs_back() {
    let p = push_that_fails().await;
    // A stale lock on #2's ref makes git refuse to move it, once receive-pack
    // has moved the branches and the push has moved #1's ref.
    let lock = p.bare.join(format!("{}.lock", store::pull_head_ref(2)));
    std::fs::write(&lock, "").unwrap();
    let mark = p.send_and_fail();
    std::fs::remove_file(&lock).unwrap();
    p.moved_nothing_and_retry_lands(mark);
}

/// The push also creates `stale/b`, which takes the path `stale` needs, so
/// `stale` can be put back only once `stale/b` is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_that_fails_puts_back_a_branch_it_replaced_with_one_beneath_it() {
    let mut p = push_that_fails().await;
    p.push.push("feature:stale/b".to_string());
    let lock = p.bare.join(format!("{}.lock", store::pull_head_ref(2)));
    std::fs::write(&lock, "").unwrap();
    let mark = p.send_and_fail();
    std::fs::remove_file(&lock).unwrap();
    p.moved_nothing_and_retry_lands(mark);
    let refs = store::list_refs(&p.bare).unwrap();
    assert_eq!(refs.get("refs/heads/stale/b"), Some(&p.feature_v2));
}

/// A `post-receive` hook runs once receive-pack has moved the pushed refs.
/// This one kills receive-pack, its parent, so `git http-backend` fails
/// with the refs moved and the push is never observed.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_whose_http_backend_fails_puts_its_refs_back() {
    let p = push_that_fails().await;
    let hooks = p.bare.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("post-receive");
    std::fs::write(&hook, "#!/bin/sh\nkill -9 $PPID\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mark = p.send_and_fail();
    std::fs::remove_file(&hook).unwrap();
    p.moved_nothing_and_retry_lands(mark);
}

/// A listener that panics when the push's first entry is appended panics
/// the push's task once it has moved #1's and #2's refs, before its record
/// commits. The panic poisons the world's lock, so the world is read
/// afterwards through a connection of its own, and the push is not retried.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_that_panics_puts_its_refs_back() {
    let p = push_that_fails().await;
    p.inst.world.listen(|signal| {
        if signal == Signal::LogMayHaveGrown {
            panic!("the push's record panicked");
        }
    });
    let mark = p.send_and_fail();
    let world = World::open(p.inst.world.root()).unwrap();
    assert!(world.log_after(mark, 10_000).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reopened_pull_takes_its_head_branchs_tip() {
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
    let patch = |n: i64, body: serde_json::Value| {
        face_answer(
            "PATCH",
            &format!("{api}/pulls/{n}"),
            &token,
            &[],
            Some(body),
        )
    };
    let get = |n: i64| {
        face_ok(
            "GET",
            &format!("{api}/pulls/{n}"),
            &token,
            serde_json::json!(null),
        )
    };

    // Every ref a listener reads at a log entry is the earliest view a reader
    // woken by that entry could have (see the test above). At each entry:
    // where `refs/pull/1/head` points.
    type Read = Result<Option<String>, String>;
    let seen: Arc<Mutex<Vec<Read>>> = Arc::default();
    let sink = Arc::clone(&seen);
    let watched = bare.clone();
    inst.world.listen(move |signal| {
        if signal == Signal::LogMayHaveGrown {
            let read =
                store::rev_parse(&watched, &store::pull_head_ref(1)).map_err(|e| e.to_string());
            if let Ok(mut seen) = sink.lock() {
                seen.push(read);
            }
        }
    });

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    commit(&src, clock, "README.md", "# webapp\n");
    git(&src, clock, &["checkout", "-b", "feature"]);
    let feature_v1 = commit(&src, clock, "feature.txt", "v1\n");
    git(&src, clock, &["push", &authed_url, "main", "feature"]);
    face_ok(
        "POST",
        &format!("{api}/pulls"),
        &token,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "main" }),
    );

    // Closed, the pull request stays on its head while the branch moves on.
    let (status, _) = patch(1, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    let feature_v2 = commit(&src, clock, "feature.txt", "v2\n");
    git(&src, clock, &["push", &authed_url, "feature"]);
    assert_eq!(get(1)["head"]["sha"], feature_v1.as_str());
    assert_eq!(pull_ref(1).as_deref(), Some(feature_v1.as_str()));

    // Reopened, it takes the branch's tip, and its ref has moved by the time
    // a reader can see the reopen.
    seen.lock().unwrap().clear();
    let (status, reopened) = patch(1, serde_json::json!({ "state": "open" }));
    assert_eq!(status, 200, "{reopened}");
    assert_eq!(reopened["state"], "open");
    assert_eq!(reopened["head"]["sha"], feature_v2.as_str());
    assert_eq!(get(1)["head"]["sha"], feature_v2.as_str());
    assert_eq!(pull_ref(1).as_deref(), Some(feature_v2.as_str()));
    let during = std::mem::take(&mut *seen.lock().unwrap());
    assert!(!during.is_empty());
    for read in &during {
        assert_eq!(read, &Ok(Some(feature_v2.clone())), "{during:?}");
    }

    // A merge with no `sha` merges the tip the reopen took.
    let merged = face_ok(
        "PUT",
        &format!("{api}/pulls/1/merge"),
        &token,
        serde_json::json!({}),
    );
    let merge_sha = merged["sha"].as_str().unwrap();
    assert_eq!(
        store::rev_parse(&bare, &format!("{merge_sha}^2")).unwrap(),
        Some(feature_v2.clone())
    );

    // A second pull request, closed, then its branch force-pushed.
    git(&src, clock, &["checkout", "main"]);
    git(&src, clock, &["checkout", "-b", "rewrite"]);
    let rewrite_v1 = commit(&src, clock, "rewrite.txt", "v1\n");
    git(&src, clock, &["push", &authed_url, "rewrite"]);
    face_ok(
        "POST",
        &format!("{api}/pulls"),
        &token,
        serde_json::json!({ "title": "Rewrite", "head": "rewrite", "base": "main" }),
    );
    let (status, _) = patch(2, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    git(&src, clock, &["commit", "--amend", "-m", "rewritten"]);
    git(&src, clock, &["push", "--force", &authed_url, "rewrite"]);

    // GitHub refuses to reopen a pull request whose branch no longer
    // contains its head, and one whose branch is gone; the refusal writes
    // nothing, the title beside it included.
    let refusal = |message: &str| {
        serde_json::json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "PullRequest",
                "code": "custom",
                "field": "state",
                "message": message,
            }],
        })
    };
    let reopen_entries = || {
        inst.world
            .log_after(0, 10_000)
            .unwrap()
            .iter()
            .filter(|e| e.body.type_name() == "pull:reopened")
            .count()
    };
    let reopens = reopen_entries();

    // A merged pull request is refused before its branch is looked at:
    // GitHub answers the same once the merged head branch is deleted.
    let merged = (
        422,
        refusal("state cannot be changed. The pull request cannot be reopened."),
    );
    assert_eq!(patch(1, serde_json::json!({ "state": "open" })), merged);
    git(&src, clock, &["push", &authed_url, ":feature"]);
    assert_eq!(patch(1, serde_json::json!({ "state": "open" })), merged);

    let reopen_and_retitle = serde_json::json!({ "state": "open", "title": "Retitled" });
    assert_eq!(
        patch(2, reopen_and_retitle.clone()),
        (
            422,
            refusal("state cannot be changed. The rewrite branch was force-pushed or recreated.")
        )
    );
    git(&src, clock, &["push", &authed_url, ":rewrite"]);
    assert_eq!(
        patch(2, reopen_and_retitle.clone()),
        (
            422,
            refusal("state cannot be changed. The rewrite branch has been deleted.")
        )
    );
    let kept = get(2);
    assert_eq!(kept["state"], "closed");
    assert_eq!(kept["title"], "Rewrite");
    assert_eq!(kept["head"]["sha"], rewrite_v1.as_str());
    assert_eq!(pull_ref(2).as_deref(), Some(rewrite_v1.as_str()));
    assert_eq!(reopen_entries(), reopens);

    // Restoring the branch at the pull request's head lets it reopen.
    git(
        &src,
        clock,
        &[
            "push",
            &authed_url,
            &format!("{rewrite_v1}:refs/heads/rewrite"),
        ],
    );
    let (status, reopened) = patch(2, reopen_and_retitle);
    assert_eq!(status, 200, "{reopened}");
    assert_eq!(reopened["title"], "Retitled");
    assert_eq!(reopened["head"]["sha"], rewrite_v1.as_str());

    // The log alone rebuilds the refs, the reopens' included.
    let log = inst.world.log_after(0, 10_000).unwrap();
    assert_eq!(
        refs_from_log(&log, "alice/webapp"),
        store::list_refs(&bare).unwrap()
    );
}
