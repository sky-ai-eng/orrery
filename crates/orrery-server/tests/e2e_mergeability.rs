//! End-to-end for a pull request's mergeability: REST `mergeable`,
//! `mergeable_state`, and `rebaseable` on a single read and on the create and
//! edit answers (spec/surface-github.md §1.0 and §1.4), and GraphQL
//! `mergeable` (§3.3), both from one test merge of real commits, and their
//! absence from a list.

mod common;

use common::{boot, face_ok, git, mint_actor};

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "query": query, "variables": variables }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json")
}

const MERGEABLE_QUERY: &str =
    "query($id: ID!) { node(id: $id) { ... on PullRequest { mergeable } } }";

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_reports_its_mergeability_as_github_does() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        serde_json::json!({ "name": "webapp" }),
    );
    let url = format!("{base}/alice/webapp.git")
        .replace("http://", &format!("http://x-access-token:{alice}@"));
    let api = format!("{base}/api/v3/repos/alice/webapp");

    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    let clock = inst.world.clock().unwrap();
    let commit = |branch: &str, from: &str, file: &str, text: &str| {
        git(src, clock, &["checkout", "-q", "-B", branch, from]);
        std::fs::write(src.join(file), text).unwrap();
        git(src, clock, &["add", "."]);
        git(src, clock, &["commit", "-q", "-m", file]);
    };
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);
    commit("feature", "main", "feature.txt", "feature\n");
    commit("wip", "main", "wip.txt", "wip\n");
    commit("clash", "main", "README.md", "# clash\n");
    commit("wip-clash", "main", "README.md", "# wip clash\n");
    commit("shut", "main", "shut.txt", "shut\n");
    commit("spike", "main", "spike.txt", "spike\n");
    commit("stale", "main", "stale.txt", "stale\n");
    commit("release", "main", "release.txt", "release\n");
    commit("docs", "release", "docs.txt", "docs\n");
    commit("main", "main", "README.md", "# webapp, again\n");
    git(
        src,
        clock,
        &[
            "push",
            "-q",
            &url,
            "main",
            "feature",
            "wip",
            "clash",
            "wip-clash",
            "shut",
            "spike",
            "stale",
            "release",
            "docs",
        ],
    );

    let open = |head: &str, base: &str, draft: bool| {
        face_ok(
            "POST",
            &format!("{api}/pulls"),
            &alice,
            serde_json::json!({ "title": head, "head": head, "base": base, "draft": draft }),
        )
    };
    let close = |n: i64| {
        face_ok(
            "PATCH",
            &format!("{api}/pulls/{n}"),
            &alice,
            serde_json::json!({ "state": "closed" }),
        )
    };
    let get = |n: i64| {
        face_ok(
            "GET",
            &format!("{api}/pulls/{n}"),
            &alice,
            serde_json::json!(null),
        )
    };
    let rest = |pull: &serde_json::Value| {
        (
            pull["mergeable"].clone(),
            pull["mergeable_state"].clone(),
            pull["rebaseable"].clone(),
        )
    };
    let graphql = |pull: &serde_json::Value| {
        let resp = gql(
            &base,
            &alice,
            MERGEABLE_QUERY,
            serde_json::json!({ "id": pull["node_id"] }),
        );
        assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
        resp["data"]["node"]["mergeable"].clone()
    };
    let (yes, no, null) = (
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::Value::Null,
    );
    let clean_open = (yes.clone(), serde_json::json!("clean"), yes.clone());
    let clean_closed = (yes.clone(), serde_json::json!("clean"), no.clone());
    let dirty = (no.clone(), serde_json::json!("dirty"), no.clone());
    let unknown = (null.clone(), serde_json::json!("unknown"), null.clone());
    // Each row of §1.0, read over REST and over GraphQL.
    let check = |n: i64,
                 expected: &(serde_json::Value, serde_json::Value, serde_json::Value),
                 graph: &str| {
        let pull = get(n);
        assert_eq!(&rest(&pull), expected, "pull {n} over REST");
        assert_eq!(graphql(&pull), graph, "pull {n} over GraphQL");
    };

    // Open: a clean merge, a draft, a conflict, and a conflicting draft. A
    // draft reads as any other pull request. The create answer carries the
    // same as a read.
    let created = open("feature", "main", false);
    assert_eq!(rest(&created), clean_open);
    assert_eq!(rest(&get(1)), rest(&created));
    check(1, &clean_open, "MERGEABLE");
    open("wip", "main", true);
    check(2, &clean_open, "MERGEABLE");
    let clash = open("clash", "main", false);
    assert_eq!(rest(&clash), dirty);
    check(3, &dirty, "CONFLICTING");
    open("wip-clash", "main", true);
    check(4, &dirty, "CONFLICTING");

    // An edit answers with it too.
    let edited = face_ok(
        "PATCH",
        &format!("{api}/pulls/2"),
        &alice,
        serde_json::json!({ "title": "Still in progress" }),
    );
    assert_eq!(rest(&edited), clean_open);

    // Closed with both branches in place, a draft among them: the test merge
    // stands, and nothing is rebaseable.
    open("shut", "main", false);
    assert_eq!(rest(&close(5)), clean_closed);
    check(5, &clean_closed, "MERGEABLE");
    close(2);
    check(2, &clean_closed, "MERGEABLE");

    // Closed by a deleted head while the base has not moved, and by a
    // deleted base.
    open("spike", "main", false);
    open("docs", "release", false);
    open("stale", "main", false);
    git(src, clock, &["push", "-q", &url, ":spike", ":release"]);
    check(6, &clean_closed, "MERGEABLE");
    check(7, &dirty, "CONFLICTING");

    // Merged, and then with its head deleted.
    face_ok(
        "PUT",
        &format!("{api}/pulls/1/merge"),
        &alice,
        serde_json::json!({}),
    );
    check(1, &unknown, "UNKNOWN");

    // The merge moved main, so a head deleted now leaves a closed pull
    // request with no test merge to report.
    git(src, clock, &["push", "-q", &url, ":stale", ":feature"]);
    check(8, &unknown, "UNKNOWN");
    check(1, &unknown, "UNKNOWN");

    // A list carries no test merge.
    let listed = face_ok(
        "GET",
        &format!("{api}/pulls?state=all"),
        &alice,
        serde_json::json!(null),
    );
    for pull in listed.as_array().unwrap() {
        assert!(pull["mergeable"].is_null(), "{pull}");
        assert!(pull.get("mergeable_state").is_none(), "{pull}");
        assert!(pull.get("rebaseable").is_none(), "{pull}");
    }
}
