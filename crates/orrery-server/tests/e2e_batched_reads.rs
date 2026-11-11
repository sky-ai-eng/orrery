//! End-to-end for the git work behind a read of many pull requests: GraphQL
//! works out every node's mergeability, diff, and head commit date together,
//! and keeps what it worked out. Each answer is the one a read of that pull
//! request alone gives, whether it is worked out or kept, and the one a
//! fresh instance of the same world gives, which has kept nothing.

mod common;

use common::{boot, boot_at, face_ok, git, mint_actor, snapshot};
use orrery_world::WorldSecret;

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    let resp: serde_json::Value = ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "query": query, "variables": variables }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json");
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    resp["data"].clone()
}

const FIELDS: &str = "... on PullRequest {
    id number state mergeable additions deletions changedFiles
    files(first: 100) { pageInfo { hasNextPage endCursor } nodes { path changeType } }
    commits(last: 1) { nodes { commit { oid committedDate } } }
}";

fn nodes_query() -> String {
    format!("query($ids: [ID!]!) {{ nodes(ids: $ids) {{ {FIELDS} }} }}")
}

fn node_query() -> String {
    format!("query($id: ID!) {{ node(id: $id) {{ {FIELDS} }} }}")
}

/// A REST read's answers that git works out: the test merge's three fields,
/// and the files.
fn rest_reads(base: &str, token: &str, repo: &str, number: i64) -> serde_json::Value {
    let api = format!("{base}/api/v3/repos/alice/{repo}/pulls/{number}");
    let pull = face_ok("GET", &api, token, serde_json::json!(null));
    let files = face_ok(
        "GET",
        &format!("{api}/files"),
        token,
        serde_json::json!(null),
    );
    serde_json::json!({
        "mergeable": pull["mergeable"],
        "mergeable_state": pull["mergeable_state"],
        "rebaseable": pull["rebaseable"],
        "files": files,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_pull_requests_reads_as_each_one_alone() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let clock = inst.world.clock().unwrap();
    let mut pulls: Vec<(&str, i64)> = Vec::new();
    let work = tempfile::tempdir().unwrap();

    // Two repositories, so a batch spans both: in each, a clean pull, one
    // that conflicts once main moves, a rename, a closed one whose head
    // branch is gone after main moved, and a merged one.
    for repo in ["webapp", "api"] {
        face_ok(
            "POST",
            &format!("{base}/api/v3/user/repos"),
            &alice,
            serde_json::json!({ "name": repo }),
        );
        let url = format!("{base}/alice/{repo}.git")
            .replace("http://", &format!("http://x-access-token:{alice}@"));
        let api = format!("{base}/api/v3/repos/alice/{repo}");
        let src = work.path().join(repo);
        std::fs::create_dir_all(&src).unwrap();
        let commit = |branch: &str, from: &str, file: &str, text: &str| {
            git(&src, clock, &["checkout", "-q", "-B", branch, from]);
            std::fs::write(src.join(file), text).unwrap();
            git(&src, clock, &["add", "-A"]);
            git(&src, clock, &["commit", "-q", "-m", file]);
        };
        git(&src, clock, &["init", "-q", "-b", "main", "."]);
        let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(src.join("README.md"), "# readme\n").unwrap();
        std::fs::write(src.join("notes.txt"), &long).unwrap();
        git(&src, clock, &["add", "."]);
        git(&src, clock, &["commit", "-q", "-m", "init"]);
        commit("feature", "main", "feature.txt", "feature\n");
        commit("clash", "main", "README.md", "# clash\n");
        git(&src, clock, &["checkout", "-q", "-B", "rename", "main"]);
        git(&src, clock, &["mv", "notes.txt", "moved.txt"]);
        std::fs::write(src.join("moved.txt"), format!("{long}one more\n")).unwrap();
        git(&src, clock, &["commit", "-q", "-a", "-m", "rename"]);
        commit("gone", "main", "gone.txt", "gone\n");
        commit("merged", "main", "merged.txt", "merged\n");
        git(
            &src,
            clock,
            &[
                "push", "-q", &url, "main", "feature", "clash", "rename", "gone", "merged",
            ],
        );
        for head in ["feature", "clash", "rename", "gone", "merged"] {
            let pull = face_ok(
                "POST",
                &format!("{api}/pulls"),
                &alice,
                serde_json::json!({ "title": head, "head": head, "base": "main" }),
            );
            pulls.push((repo, pull["number"].as_i64().unwrap()));
        }
        face_ok(
            "PATCH",
            &format!("{api}/pulls/4"),
            &alice,
            serde_json::json!({ "state": "closed" }),
        );
        face_ok(
            "PUT",
            &format!("{api}/pulls/5/merge"),
            &alice,
            serde_json::json!({}),
        );
        git(&src, clock, &["fetch", "-q", &url, "main"]);
        commit("main", "FETCH_HEAD", "README.md", "# main moved\n");
        git(&src, clock, &["push", "-q", &url, "main", ":gone"]);
    }

    let mut ids: Vec<serde_json::Value> = Vec::new();
    for (repo, number) in &pulls {
        let pull = face_ok(
            "GET",
            &format!("{base}/api/v3/repos/alice/{repo}/pulls/{number}"),
            &alice,
            serde_json::json!(null),
        );
        ids.push(pull["node_id"].clone());
    }
    // A pull listed twice, and an id that names nothing.
    ids.push(ids[0].clone());
    ids.push(serde_json::json!("O_pull_999999"));
    snapshot(&inst.control, "seed");

    let worked_out = gql(
        &base,
        &alice,
        &nodes_query(),
        serde_json::json!({ "ids": ids }),
    );
    let nodes = worked_out["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), ids.len());
    assert_eq!(nodes[0], nodes[ids.len() - 2]);
    assert!(nodes[ids.len() - 1].is_null());
    // The world is shaped as it says: these are answers a batch must get
    // right, not ones every pull shares.
    let mergeable: Vec<&str> = nodes[..pulls.len()]
        .iter()
        .map(|n| n["mergeable"].as_str().unwrap())
        .collect();
    let per_repo = [
        "MERGEABLE",
        "CONFLICTING",
        "MERGEABLE",
        "UNKNOWN",
        "UNKNOWN",
    ];
    assert_eq!(mergeable, [per_repo, per_repo].concat());
    assert_eq!(nodes[2]["files"]["nodes"][0]["changeType"], "RENAMED");

    let kept = gql(
        &base,
        &alice,
        &nodes_query(),
        serde_json::json!({ "ids": ids }),
    );
    assert_eq!(kept, worked_out, "a kept answer is the one worked out");

    let alone = |base: &str, id: &serde_json::Value| {
        gql(base, &alice, &node_query(), serde_json::json!({ "id": id }))["node"].clone()
    };
    for (id, node) in ids.iter().zip(nodes) {
        assert_eq!(&alone(&base, id), node, "{id} read alone");
    }
    let kept_rest: Vec<serde_json::Value> = pulls
        .iter()
        .map(|(repo, number)| rest_reads(&base, &alice, repo, *number))
        .collect();

    // A fork of the same world, served fresh, has kept nothing.
    let dest = tempfile::tempdir().unwrap();
    let forked_root = orrery_snapshot::fork(
        &inst.dir.path().join("snapshots"),
        "seed",
        dest.path(),
        &WorldSecret::LOCAL,
    )
    .unwrap();
    let fork = boot_at(dest, &forked_root).await;
    for (id, node) in ids.iter().zip(nodes) {
        assert_eq!(&alone(&fork.base, id), node, "{id} read alone, fresh");
    }
    for ((repo, number), kept) in pulls.iter().zip(&kept_rest) {
        assert_eq!(
            &rest_reads(&fork.base, &alice, repo, *number),
            kept,
            "{repo}#{number} over REST, fresh"
        );
    }
}
