//! End-to-end for the reads a consumer watching base branches makes
//! (spec/surface-github.md §1.4, §1.8 and §3): a branch's tip with its
//! ETag, the compare between two tips, and a pull request's changed files over
//! REST and GraphQL.

mod common;

use common::{boot, face, face_answer, face_ok, git, mint_actor};

fn gql(base: &str, token: &str, query: &str, variables: serde_json::Value) -> serde_json::Value {
    ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "query": query, "variables": variables }))
        .expect("graphql call")
        .body_mut()
        .read_json()
        .expect("graphql json")
}

/// A consumer's `PRChangedPaths` query, verbatim.
const PR_CHANGED_PATHS: &str = r#"
	query PRChangedPaths($ids: [ID!]!) {
		nodes(ids: $ids) {
			... on PullRequest {
				id headRefOid changedFiles
				files(first: 100) { pageInfo { hasNextPage endCursor } nodes { path changeType } }
			}
		}
		rateLimit { cost }
	}
"#;

/// A consumer's `PRChangedPathsPage` query, verbatim.
const PR_CHANGED_PATHS_PAGE: &str = r#"
	query PRChangedPathsPage($id: ID!, $cursor: String!) {
		node(id: $id) {
			... on PullRequest {
				headRefOid
				files(first: 100, after: $cursor) { pageInfo { hasNextPage endCursor } nodes { path changeType } }
			}
		}
		rateLimit { cost }
	}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn branch_tip_compare_and_changed_files() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        serde_json::json!({ "name": "webapp" }),
    );
    let authed_url = repo["clone_url"]
        .as_str()
        .unwrap()
        .replace("http://", &format!("http://x-access-token:{alice}@"));

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    std::fs::write(
        src.join("old.txt"),
        "a long enough body to be recognized as the same file\n",
    )
    .unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    let first = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "add feature"]);
    git(&src, clock, &["push", &authed_url, "main", "feature"]);

    let api = format!("{base}/api/v3/repos/alice/webapp");
    let pull = face_ok(
        "POST",
        &format!("{api}/pulls"),
        &alice,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "main" }),
    );
    let node_id = pull["node_id"].as_str().unwrap().to_string();

    // The tip, its tag, and the 304 a replayed tag earns.
    let url = format!("{api}/git/ref/heads/main");
    let tip = face_ok("GET", &url, &alice, serde_json::json!(null));
    assert_eq!(tip["ref"], "refs/heads/main");
    assert_eq!(tip["object"]["sha"], first.as_str());
    assert_eq!(tip["object"]["type"], "commit");
    let mut read = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .call()
        .unwrap();
    let etag = read.headers()["etag"].to_str().unwrap().to_string();
    let _ = read.body_mut().read_to_string().unwrap();
    let cached = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .header("if-none-match", &etag)
        .call()
        .unwrap();
    assert_eq!(cached.status().as_u16(), 304);
    assert_eq!(
        face(
            "GET",
            &format!("{api}/git/ref/heads/ghost"),
            &alice,
            serde_json::json!(null)
        ),
        Err(404)
    );

    // Move main: a rename and a modification.
    git(&src, clock, &["checkout", "main"]);
    git(&src, clock, &["mv", "old.txt", "new.txt"]);
    std::fs::write(src.join("README.md"), "# webapp\nmore\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "move main"]);
    let second = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["push", &authed_url, "main"]);

    let moved = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .header("if-none-match", &etag)
        .call()
        .unwrap();
    assert_eq!(
        moved.status().as_u16(),
        200,
        "a moved tip invalidates the tag"
    );
    assert_ne!(moved.headers()["etag"].to_str().unwrap(), etag);

    // Compare old...tip.
    let cmp = face_ok(
        "GET",
        &format!("{api}/compare/{first}...{second}"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(cmp["status"], "ahead");
    assert_eq!(cmp["ahead_by"], 1);
    assert_eq!(cmp["total_commits"], 1);
    assert_eq!(cmp["commits"][0]["sha"], second.as_str());
    assert_eq!(cmp["commits"][0]["parents"][0]["sha"], first.as_str());
    let files = cmp["files"].as_array().unwrap();
    let by_name = |name: &str| files.iter().find(|f| f["filename"] == name).unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(by_name("new.txt")["status"], "renamed");
    assert_eq!(by_name("new.txt")["previous_filename"], "old.txt");
    assert_eq!(by_name("README.md")["status"], "modified");

    let same = face_ok(
        "GET",
        &format!("{api}/compare/{second}...{second}"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(same["status"], "identical");
    let behind = face_ok(
        "GET",
        &format!("{api}/compare/{second}...{first}"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(behind["status"], "behind");
    let diverged = face_ok(
        "GET",
        &format!("{api}/compare/main...feature"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(diverged["status"], "diverged");
    assert_eq!(
        face(
            "GET",
            &format!("{api}/compare/{}...{second}", "0".repeat(40)),
            &alice,
            serde_json::json!(null)
        ),
        Err(404),
        "an old tip that no longer exists is a 404"
    );
    // The diff and patch media are refused by name, never answered with JSON.
    for accept in [
        "application/vnd.github.v3.diff",
        "application/vnd.github.PATCH",
    ] {
        let (status, refused) = face_answer(
            "GET",
            &format!("{api}/compare/{first}...{second}"),
            &alice,
            &[("accept", accept)],
            None,
        );
        assert_eq!(status, 422, "{accept}: {refused}");
        assert_eq!(refused["orrery"]["not_mocked"], true, "{refused}");
        assert_eq!(refused["errors"][0]["field"], "Accept", "{refused}");
        assert_eq!(refused["errors"][0]["resource"], "Comparison", "{refused}");
    }

    // The pull's files over REST: only what the pull changes, against the
    // moved base.
    let pull_files = face_ok(
        "GET",
        &format!("{api}/pulls/1/files?per_page=100&page=1"),
        &alice,
        serde_json::json!(null),
    );
    let names: Vec<&str> = pull_files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["filename"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["feature.txt"]);
    assert_eq!(
        face(
            "GET",
            &format!("{api}/pulls/9/files"),
            &alice,
            serde_json::json!(null)
        ),
        Err(404)
    );

    // ...and over GraphQL, with the consumer's documents verbatim.
    let resp = gql(
        &base,
        &alice,
        PR_CHANGED_PATHS,
        serde_json::json!({ "ids": [node_id, "O_pull_999"] }),
    );
    assert!(resp.get("errors").is_none(), "unexpected errors: {resp}");
    let nodes = resp["data"]["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["id"], node_id.as_str());
    assert_eq!(nodes[0]["changedFiles"], 1);
    assert_eq!(nodes[0]["files"]["pageInfo"]["hasNextPage"], false);
    assert_eq!(nodes[0]["files"]["nodes"][0]["path"], "feature.txt");
    assert_eq!(nodes[0]["files"]["nodes"][0]["changeType"], "ADDED");
    assert!(nodes[1].is_null());

    // The page query: a cursor past the last file reads an empty page at the
    // same head.
    let cursor = nodes[0]["files"]["pageInfo"]["endCursor"].as_str().unwrap();
    let page = gql(
        &base,
        &alice,
        PR_CHANGED_PATHS_PAGE,
        serde_json::json!({ "id": node_id, "cursor": cursor }),
    );
    assert!(page.get("errors").is_none(), "unexpected errors: {page}");
    assert_eq!(page["data"]["node"]["headRefOid"], nodes[0]["headRefOid"]);
    assert_eq!(
        page["data"]["node"]["files"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
