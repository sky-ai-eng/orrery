//! End-to-end for a REST pull request's mergeability (spec/surface-github.md
//! §1.0 and §1.4): `mergeable`, `mergeable_state`, and `rebaseable` on a
//! single read and on the create and edit answers, from a test merge of real
//! commits, and their absence from a list.

mod common;

use common::{boot, face_ok, git, mint_actor};

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
    commit("spike", "main", "spike.txt", "spike\n");
    commit("release", "main", "release.txt", "release\n");
    commit("docs", "release", "docs.txt", "docs\n");
    commit("main", "main", "README.md", "# webapp, again\n");
    git(
        src,
        clock,
        &[
            "push", "-q", &url, "main", "feature", "wip", "clash", "spike", "release", "docs",
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
    let get = |n: i64| {
        face_ok(
            "GET",
            &format!("{api}/pulls/{n}"),
            &alice,
            serde_json::json!(null),
        )
    };
    let mergeability = |pull: &serde_json::Value| {
        (
            pull["mergeable"].clone(),
            pull["mergeable_state"].clone(),
            pull["rebaseable"].clone(),
        )
    };
    let expect = |mergeable: serde_json::Value, state: &str, rebaseable: serde_json::Value| {
        (mergeable, serde_json::json!(state), rebaseable)
    };
    let (yes, no, null) = (
        serde_json::json!(true),
        serde_json::json!(false),
        serde_json::Value::Null,
    );

    // Open: a clean merge, a draft, and a conflict. The create answer carries
    // the same as a read.
    let created = open("feature", "main", false);
    assert_eq!(
        mergeability(&created),
        expect(yes.clone(), "clean", yes.clone())
    );
    assert_eq!(mergeability(&get(1)), mergeability(&created));
    open("wip", "main", true);
    assert_eq!(
        mergeability(&get(2)),
        expect(yes.clone(), "draft", yes.clone())
    );
    let clash = open("clash", "main", false);
    assert_eq!(
        mergeability(&clash),
        expect(no.clone(), "dirty", no.clone())
    );

    // An edit answers with it too.
    let edited = face_ok(
        "PATCH",
        &format!("{api}/pulls/2"),
        &alice,
        serde_json::json!({ "title": "Still in progress" }),
    );
    assert_eq!(
        mergeability(&edited),
        expect(yes.clone(), "draft", yes.clone())
    );

    // Closed by a deleted head, by a deleted base, and merged.
    open("spike", "main", false);
    open("docs", "release", false);
    git(src, clock, &["push", "-q", &url, ":spike", ":release"]);
    assert_eq!(
        mergeability(&get(4)),
        expect(yes.clone(), "clean", no.clone())
    );
    assert_eq!(
        mergeability(&get(5)),
        expect(no.clone(), "dirty", no.clone())
    );
    face_ok(
        "PUT",
        &format!("{api}/pulls/1/merge"),
        &alice,
        serde_json::json!({}),
    );
    assert_eq!(
        mergeability(&get(1)),
        expect(null.clone(), "unknown", null.clone())
    );

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
