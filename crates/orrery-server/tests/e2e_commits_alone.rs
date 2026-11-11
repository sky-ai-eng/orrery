//! End-to-end for what decides a pull request's answers that git works out:
//! its commits alone. A replace ref pushed for the head, and a
//! `.gitattributes` pushed to the default branch marking a changed file
//! `-diff`, change none of them, whether the answers were kept from before
//! the pushes or are worked out fresh in a fork.

mod common;

use common::{boot, boot_at, face_ok, git, mint_actor, snapshot};
use orrery_world::WorldSecret;

const QUERY: &str = "query($id: ID!) { node(id: $id) { ... on PullRequest {
    mergeable additions deletions changedFiles
    files(first: 100) { nodes { path changeType } }
    commits(last: 1) { nodes { commit { oid committedDate } } }
} } }";

/// The pull request's answers over GraphQL and REST.
fn answers(base: &str, token: &str) -> serde_json::Value {
    let api = format!("{base}/api/v3/repos/alice/webapp/pulls/1");
    let pull = face_ok("GET", &api, token, serde_json::json!(null));
    let graphql: serde_json::Value = ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({
            "query": QUERY,
            "variables": { "id": pull["node_id"] },
        }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json");
    assert!(
        graphql.get("errors").is_none(),
        "unexpected errors: {graphql}"
    );
    let files = face_ok(
        "GET",
        &format!("{api}/files"),
        token,
        serde_json::json!(null),
    );
    serde_json::json!({
        "graphql": graphql["data"]["node"],
        "rest": {
            "mergeable": pull["mergeable"],
            "mergeable_state": pull["mergeable_state"],
            "rebaseable": pull["rebaseable"],
            "additions": pull["additions"],
            "deletions": pull["deletions"],
            "changed_files": pull["changed_files"],
            "files": files,
        },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replace_ref_and_in_repo_attributes_change_no_answer() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let clock = inst.world.clock().unwrap();
    face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        serde_json::json!({ "name": "webapp" }),
    );
    let url = format!("{base}/alice/webapp.git")
        .replace("http://", &format!("http://x-access-token:{alice}@"));
    let work = tempfile::tempdir().unwrap();
    let src = work.path();

    // The head adds lines to a text file. Main then changes the README, and
    // so does the replacement, a commit dated a day later that is pushed only
    // as the replace ref: read in the head's place, it changes the diff and
    // the date, and conflicts with main.
    git(src, clock, &["init", "-q", "-b", "main", "."]);
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    std::fs::write(src.join("README.md"), "# readme\n").unwrap();
    std::fs::write(src.join("notes.txt"), &long).unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-q", "-m", "init"]);
    git(src, clock, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(src.join("notes.txt"), format!("{long}two\nmore\n")).unwrap();
    git(src, clock, &["commit", "-q", "-a", "-m", "feature"]);
    let head = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(src, clock, &["checkout", "-q", "-b", "replacement", "main"]);
    std::fs::write(src.join("README.md"), "# replaced\n").unwrap();
    git(
        src,
        clock + 86_400,
        &["commit", "-q", "-a", "-m", "replacement"],
    );
    let replacement = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(src, clock, &["checkout", "-q", "main"]);
    std::fs::write(src.join("README.md"), "# main moved\n").unwrap();
    git(src, clock, &["commit", "-q", "-a", "-m", "main moved"]);
    git(src, clock, &["push", "-q", &url, "main", "feature"]);
    face_ok(
        "POST",
        &format!("{base}/api/v3/repos/alice/webapp/pulls"),
        &alice,
        serde_json::json!({ "title": "feature", "head": "feature", "base": "main" }),
    );

    // A git from 2.43 on reads the attributes of the tree this names, unless
    // the engine's own setting overrides it.
    let bare = inst.world.repos_dir().join("alice").join("webapp.git");
    git(&bare, clock, &["config", "attr.tree", "HEAD"]);

    let before = answers(&base, &alice);
    let pr = &before["graphql"];
    assert_eq!(pr["mergeable"], "MERGEABLE");
    assert_eq!((&pr["additions"], &pr["deletions"]), (&2.into(), &0.into()));
    assert_eq!(pr["commits"]["nodes"][0]["commit"]["oid"], head.as_str());

    git(
        src,
        clock,
        &[
            "push",
            "-q",
            &url,
            &format!("{replacement}:refs/replace/{head}"),
        ],
    );
    std::fs::write(src.join(".gitattributes"), "notes.txt -diff\n").unwrap();
    git(src, clock, &["add", ".gitattributes"]);
    git(src, clock, &["commit", "-q", "-m", "attributes"]);
    git(src, clock, &["push", "-q", &url, "main"]);
    // A git that follows replace refs, as this one does, reads the
    // replacement in the head's place.
    let date = git(&bare, clock, &["show", "-s", "--format=%ct", &head]);
    assert_eq!(date.trim(), (clock + 86_400).to_string());

    assert_eq!(answers(&base, &alice), before, "after the pushes");

    snapshot(&inst.control, "pushed");
    let dest = tempfile::tempdir().unwrap();
    let forked_root = orrery_snapshot::fork(
        &inst.dir.path().join("snapshots"),
        "pushed",
        dest.path(),
        &WorldSecret::LOCAL,
    )
    .unwrap();
    let fork = boot_at(dest, &forked_root).await;
    assert_eq!(answers(&fork.base, &alice), before, "in a fresh fork");
}
