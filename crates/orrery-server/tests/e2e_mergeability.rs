//! End-to-end for a pull request's mergeability: REST `mergeable`,
//! `mergeable_state`, and `rebaseable` on a single read and on the create and
//! edit answers (spec/surface-github.md §1.0 and §1.4), and GraphQL
//! `mergeable` (§3.3), both from one test merge of real commits; the unknown
//! answer of the read or write that starts a test merge; and their absence
//! from a list.

mod common;

use common::{Instance, boot, control_post, face_ok, git, mint_actor};

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "query": query, "variables": variables }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json")
}

type Rest = (serde_json::Value, serde_json::Value, serde_json::Value);

/// A served world with one repository, `alice/webapp`, and a clone to push
/// from.
struct Webapp {
    inst: Instance,
    token: String,
    url: String,
    api: String,
    work: tempfile::TempDir,
    clock: i64,
}

impl Webapp {
    async fn boot() -> Webapp {
        let inst = boot().await;
        let token = mint_actor(&inst.base, "alice", "user");
        face_ok(
            "POST",
            &format!("{}/api/v3/user/repos", inst.base),
            &token,
            serde_json::json!({ "name": "webapp" }),
        );
        let url = format!("{}/alice/webapp.git", inst.base)
            .replace("http://", &format!("http://x-access-token:{token}@"));
        let api = format!("{}/api/v3/repos/alice/webapp", inst.base);
        let work = tempfile::tempdir().unwrap();
        let clock = inst.world.clock().unwrap();
        let app = Webapp {
            inst,
            token,
            url,
            api,
            work,
            clock,
        };
        app.git(&["init", "-q", "-b", "main", "."]);
        std::fs::write(app.work.path().join("README.md"), "# webapp\n").unwrap();
        app.git(&["add", "."]);
        app.git(&["commit", "-q", "-m", "init"]);
        app
    }

    fn git(&self, args: &[&str]) {
        git(self.work.path(), self.clock, args);
    }

    /// A commit on `branch`, started from `from`, that writes `file`.
    fn commit(&self, branch: &str, from: &str, file: &str, text: &str) {
        self.git(&["checkout", "-q", "-B", branch, from]);
        std::fs::write(self.work.path().join(file), text).unwrap();
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", file]);
    }

    fn push(&self, refs: &[&str]) {
        let mut args = vec!["push", "-q", self.url.as_str()];
        args.extend(refs);
        self.git(&args);
    }

    fn open(&self, head: &str, base: &str, draft: bool) -> serde_json::Value {
        face_ok(
            "POST",
            &format!("{}/pulls", self.api),
            &self.token,
            serde_json::json!({ "title": head, "head": head, "base": base, "draft": draft }),
        )
    }

    fn patch(&self, n: i64, body: serde_json::Value) -> serde_json::Value {
        face_ok(
            "PATCH",
            &format!("{}/pulls/{n}", self.api),
            &self.token,
            body,
        )
    }

    fn get(&self, n: i64) -> serde_json::Value {
        face_ok(
            "GET",
            &format!("{}/pulls/{n}", self.api),
            &self.token,
            serde_json::json!(null),
        )
    }

    fn list(&self) -> serde_json::Value {
        face_ok(
            "GET",
            &format!("{}/pulls?state=all", self.api),
            &self.token,
            serde_json::json!(null),
        )
    }

    /// GraphQL `mergeable` of each pull, by number, from one query.
    fn graphql(&self, numbers: &[i64]) -> Vec<serde_json::Value> {
        let listed = self.list();
        let ids: Vec<&serde_json::Value> = numbers
            .iter()
            .map(|n| {
                let pull = listed
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["number"] == *n);
                &pull.expect("a pull with that number")["node_id"]
            })
            .collect();
        let resp = gql(
            &self.inst.base,
            &self.token,
            "query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest { mergeable } } }",
            serde_json::json!({ "ids": ids }),
        );
        assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
        resp["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["mergeable"].clone())
            .collect()
    }

    /// The test merges reads have started: each `pull:test_merge_started` in
    /// the log, as the pull's number and the door the read came through.
    fn started(&self) -> Vec<(i64, String)> {
        let log = control_post(
            &self.inst.control,
            "/_orrery/log/list",
            serde_json::json!({ "page_size": 500 }),
        );
        assert!(
            log.get("next_page_token").is_none(),
            "one page holds the log"
        );
        log["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "pull:test_merge_started")
            .map(|event| {
                assert_eq!(event["actor"]["login"], "alice", "{event}");
                assert_eq!(
                    event["changes"],
                    serde_json::json!({ "test_merge_stale": { "from": true, "to": false } }),
                );
                (
                    event["subject"]["number"].as_i64().unwrap(),
                    event["door"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

fn rest(pull: &serde_json::Value) -> Rest {
    (
        pull["mergeable"].clone(),
        pull["mergeable_state"].clone(),
        pull["rebaseable"].clone(),
    )
}

fn clean_open() -> Rest {
    (true.into(), "clean".into(), true.into())
}

fn clean_closed() -> Rest {
    (true.into(), "clean".into(), false.into())
}

fn dirty() -> Rest {
    (false.into(), "dirty".into(), false.into())
}

fn unknown() -> Rest {
    (
        serde_json::Value::Null,
        "unknown".into(),
        serde_json::Value::Null,
    )
}

/// Each row of §1.0 once its test merge has run, over REST and over GraphQL.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_reports_its_mergeability_as_github_does() {
    let app = Webapp::boot().await;
    app.commit("feature", "main", "feature.txt", "feature\n");
    app.commit("wip", "main", "wip.txt", "wip\n");
    app.commit("clash", "main", "README.md", "# clash\n");
    app.commit("wip-clash", "main", "README.md", "# wip clash\n");
    app.commit("shut", "main", "shut.txt", "shut\n");
    app.commit("spike", "main", "spike.txt", "spike\n");
    app.commit("stale", "main", "stale.txt", "stale\n");
    app.commit("read", "main", "read.txt", "read\n");
    app.commit("release", "main", "release.txt", "release\n");
    app.commit("docs", "release", "docs.txt", "docs\n");
    app.commit("main", "main", "README.md", "# webapp, again\n");
    app.push(&[
        "main",
        "feature",
        "wip",
        "clash",
        "wip-clash",
        "shut",
        "spike",
        "stale",
        "read",
        "release",
        "docs",
    ]);
    let check = |n: i64, expected: Rest, graph: &str| {
        assert_eq!(rest(&app.get(n)), expected, "pull {n} over REST");
        assert_eq!(app.graphql(&[n]), [graph], "pull {n} over GraphQL");
    };

    // Open: a clean merge, a draft, a conflict, and a conflicting draft. A
    // draft reads as any other pull request. Opening one starts its test
    // merge, so the create answer has none to report and the next read does.
    assert_eq!(rest(&app.open("feature", "main", false)), unknown());
    check(1, clean_open(), "MERGEABLE");
    app.open("wip", "main", true);
    check(2, clean_open(), "MERGEABLE");
    assert_eq!(rest(&app.open("clash", "main", false)), unknown());
    check(3, dirty(), "CONFLICTING");
    app.open("wip-clash", "main", true);
    check(4, dirty(), "CONFLICTING");

    // An edit that starts no test merge answers as a read does.
    let edited = app.patch(2, serde_json::json!({ "title": "Still in progress" }));
    assert_eq!(rest(&edited), clean_open());

    // Closed with both branches in place, a draft among them: the test merge
    // stands, and nothing is rebaseable.
    app.open("shut", "main", false);
    assert_eq!(
        rest(&app.patch(5, serde_json::json!({ "state": "closed" }))),
        clean_closed()
    );
    check(5, clean_closed(), "MERGEABLE");
    app.patch(2, serde_json::json!({ "state": "closed" }));
    check(2, clean_closed(), "MERGEABLE");

    // Closed by a deleted head while the base has not moved, and by a
    // deleted base.
    app.open("spike", "main", false);
    app.open("docs", "release", false);
    app.open("stale", "main", false);
    app.open("read", "main", false);
    app.push(&[":spike", ":release"]);
    check(6, clean_closed(), "MERGEABLE");
    check(7, dirty(), "CONFLICTING");

    // Merged, and then with its head deleted.
    face_ok(
        "PUT",
        &format!("{}/pulls/1/merge", app.api),
        &app.token,
        serde_json::json!({}),
    );
    check(1, unknown(), "UNKNOWN");

    // The merge moved main. A read after it starts a test merge, so a head
    // deleted after that read leaves a closed pull request that still
    // reports it. A head deleted with no read since the move leaves one that
    // cannot make a test merge, however often it is read.
    assert_eq!(rest(&app.get(9)), unknown());
    app.push(&[":stale", ":feature", ":read"]);
    check(8, unknown(), "UNKNOWN");
    check(8, unknown(), "UNKNOWN");
    check(9, clean_closed(), "MERGEABLE");
    check(1, unknown(), "UNKNOWN");
    assert_eq!(app.started(), [(9, "rest".to_string())]);

    // A list carries no test merge.
    for pull in app.list().as_array().unwrap() {
        assert!(pull["mergeable"].is_null(), "{pull}");
        assert!(pull.get("mergeable_state").is_none(), "{pull}");
        assert!(pull.get("rebaseable").is_none(), "{pull}");
    }
}

/// A move of the base branch or of the head leaves the test merge stale, and
/// the next read that carries mergeability starts one: that read has no
/// answer, and the one after it does.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_after_a_branch_moves_starts_a_test_merge() {
    let app = Webapp::boot().await;
    app.commit("one", "main", "one.txt", "one\n");
    app.commit("two", "main", "two.txt", "two\n");
    app.commit("three", "main", "README.md", "# three\n");
    app.push(&["main", "one", "two", "three"]);
    for head in ["one", "two", "three"] {
        app.open(head, "main", false);
    }
    assert_eq!(rest(&app.get(1)), clean_open());
    assert_eq!(app.graphql(&[2, 3]), ["MERGEABLE", "MERGEABLE"]);
    assert_eq!(app.started(), []);

    // A push to the base, which conflicts with the third. A list starts no
    // test merge, however often it is read.
    app.commit("main", "main", "README.md", "# webapp, moved\n");
    app.push(&["main"]);
    app.list();
    app.list();
    assert_eq!(app.started(), []);
    assert_eq!(rest(&app.get(1)), unknown());
    assert_eq!(rest(&app.get(1)), clean_open());
    assert_eq!(rest(&app.get(1)), clean_open());
    // One GraphQL query starts one for each pull it reads.
    assert_eq!(app.graphql(&[1, 2, 3]), ["MERGEABLE", "UNKNOWN", "UNKNOWN"]);
    assert_eq!(
        app.graphql(&[1, 2, 3]),
        ["MERGEABLE", "MERGEABLE", "CONFLICTING"]
    );
    assert_eq!(rest(&app.get(3)), dirty());
    let rest_door = |n: i64| (n, "rest".to_string());
    let graphql_door = |n: i64| (n, "graphql".to_string());
    let mut started = vec![rest_door(1), graphql_door(2), graphql_door(3)];
    assert_eq!(app.started(), started);

    // A push to a head leaves only that pull stale, and the push resolves
    // its conflict.
    app.commit("three", "three", "README.md", "# webapp, moved\n");
    app.push(&["three"]);
    assert_eq!(
        app.graphql(&[1, 2, 3]),
        ["MERGEABLE", "MERGEABLE", "UNKNOWN"]
    );
    assert_eq!(rest(&app.get(3)), clean_open());
    started.push(graphql_door(3));
    assert_eq!(app.started(), started);

    // An edit's answer is a read too: the edit of a stale pull starts its
    // test merge.
    app.commit("main", "main", "more.txt", "more\n");
    app.push(&["main"]);
    let edited = app.patch(1, serde_json::json!({ "title": "One, again" }));
    assert_eq!(rest(&edited), unknown());
    assert_eq!(rest(&app.get(1)), clean_open());
    started.push(rest_door(1));
    assert_eq!(app.started(), started);

    // A merge moves the base as a push does.
    face_ok(
        "PUT",
        &format!("{}/pulls/1/merge", app.api),
        &app.token,
        serde_json::json!({}),
    );
    assert_eq!(rest(&app.get(2)), unknown());
    assert_eq!(rest(&app.get(2)), clean_open());
    started.push(rest_door(2));
    assert_eq!(app.started(), started);
}

/// Reopening a pull request and moving it to another base start a test merge
/// in the write, so the edit's answer has none to report, the next read
/// does, and no read starts one.
#[tokio::test(flavor = "multi_thread")]
async fn a_reopen_and_a_base_change_start_a_test_merge() {
    let app = Webapp::boot().await;
    app.commit("feature", "main", "feature.txt", "feature\n");
    app.commit("release", "main", "README.md", "# release\n");
    app.push(&["main", "feature", "release"]);
    app.open("feature", "main", false);
    app.patch(1, serde_json::json!({ "state": "closed" }));

    // Both branches move while it is closed, so its test merge is stale when
    // the reopen starts another.
    app.commit("main", "main", "README.md", "# webapp, moved\n");
    app.commit("feature", "feature", "feature.txt", "feature, again\n");
    app.push(&["main", "feature"]);
    let reopened = app.patch(1, serde_json::json!({ "state": "open" }));
    assert_eq!(rest(&reopened), unknown());
    assert_eq!(rest(&app.get(1)), clean_open());

    app.commit("release", "release", "release.txt", "release\n");
    app.push(&["release"]);
    let moved = app.patch(1, serde_json::json!({ "base": "release" }));
    assert_eq!(rest(&moved), unknown());
    assert_eq!(app.graphql(&[1]), ["MERGEABLE"]);

    // Naming the base it is already on changes nothing and starts nothing.
    let same = app.patch(1, serde_json::json!({ "base": "release" }));
    assert_eq!(rest(&same), clean_open());
    assert_eq!(app.started(), []);
}
