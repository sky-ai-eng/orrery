//! End-to-end for the poller read surface (spec/surface-github.md §1): the
//! listing order and ETag/304 fast path TF's discovery depends on, the
//! detail reads, contents, branches, and the identity endpoints.

mod common;

use common::{boot, face, face_ok, git, mint_actor};

#[tokio::test(flavor = "multi_thread")]
async fn poll_path_reads() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bob = mint_actor(&base, "bob", "user");
    let ci = mint_actor(&base, "ci", "bot");

    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        serde_json::json!({ "name": "webapp", "description": "the app" }),
    );
    let authed_url = repo["clone_url"]
        .as_str()
        .unwrap()
        .replace("http://", &format!("http://x-access-token:{alice}@"));

    // Seed: main + two feature branches, one PR each; close the second.
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    for branch in ["one", "two"] {
        git(&src, clock, &["checkout", "-b", branch, "main"]);
        std::fs::write(src.join(format!("{branch}.txt")), branch).unwrap();
        git(&src, clock, &["add", "."]);
        git(&src, clock, &["commit", "-m", branch]);
    }
    git(&src, clock, &["push", &authed_url, "main", "one", "two"]);
    let api = format!("{base}/api/v3/repos/alice/webapp");
    let pr1 = face_ok(
        "POST",
        &format!("{api}/pulls"),
        &alice,
        serde_json::json!({ "title": "One", "head": "one", "base": "main" }),
    );
    face_ok(
        "POST",
        &format!("{api}/pulls"),
        &alice,
        serde_json::json!({ "title": "Two", "head": "two", "base": "main" }),
    );
    face_ok(
        "PATCH",
        &format!("{api}/pulls/2"),
        &alice,
        serde_json::json!({ "state": "closed" }),
    );

    // Repo object: the fields TF's meta/rename path reads.
    let read = face_ok("GET", &api, &alice, serde_json::json!(null));
    assert_eq!(read["full_name"], "alice/webapp");
    assert_eq!(read["default_branch"], "main");
    assert!(read["clone_url"].as_str().unwrap().ends_with("webapp.git"));
    assert_eq!(
        face(
            "GET",
            &format!("{base}/api/v3/repos/alice/ghost"),
            &alice,
            serde_json::json!(null)
        ),
        Err(404),
        "reachability probe contract: missing repo is a conclusive 404"
    );

    // Pulls list: state=open by default (the closed PR is invisible),
    // state filters work, invalid state refuses.
    let open = face_ok(
        "GET",
        &format!("{api}/pulls?state=open&per_page=100&page=1"),
        &alice,
        serde_json::json!(null),
    );
    let open = open.as_array().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["number"], 1);
    assert_eq!(open[0]["user"]["login"], "alice");
    assert_eq!(open[0]["head"]["repo"]["full_name"], "alice/webapp");
    assert!(open[0]["requested_teams"].as_array().unwrap().is_empty());
    let all = face_ok(
        "GET",
        &format!("{api}/pulls?state=all"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(all.as_array().unwrap().len(), 2);
    // Created-descending: the newer PR (#2) leads.
    assert_eq!(all[0]["number"], 2);
    assert_eq!(
        face(
            "GET",
            &format!("{api}/pulls?state=weird"),
            &alice,
            serde_json::json!(null)
        ),
        Err(422)
    );

    // The ETag fast path: 200 carries a tag; replaying it answers 304; new
    // activity invalidates it.
    let url = format!("{api}/pulls?state=open&per_page=100&page=1");
    let mut first = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .call()
        .unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.starts_with("W/\""));
    let _ = first.body_mut().read_to_string().unwrap();
    let cached = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .header("if-none-match", &etag)
        .call()
        .unwrap();
    assert_eq!(cached.status().as_u16(), 304);
    assert_eq!(cached.headers()["etag"].to_str().unwrap(), etag);

    face_ok(
        "PATCH",
        &format!("{api}/pulls/1"),
        &alice,
        serde_json::json!({ "title": "One, retitled" }),
    );
    let after_edit = ureq::get(&url)
        .header("authorization", &format!("token {alice}"))
        .header("if-none-match", &etag)
        .call()
        .unwrap();
    assert_eq!(
        after_edit.status().as_u16(),
        200,
        "activity invalidates the tag"
    );
    assert_ne!(after_edit.headers()["etag"].to_str().unwrap(), etag);

    // Detail reads: single PR (stub-enrichment fields), reviews, comments.
    let one = face_ok(
        "GET",
        &format!("{api}/pulls/1"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(one["node_id"], pr1["node_id"]);
    assert_eq!(one["merged"], false);
    assert_eq!(one["state"], "open");
    face_ok(
        "POST",
        &format!("{api}/pulls/1/reviews"),
        &bob,
        serde_json::json!({ "event": "REQUEST_CHANGES", "body": "needs tests" }),
    );
    let reviews = face_ok(
        "GET",
        &format!("{api}/pulls/1/reviews?per_page=100"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(reviews[0]["state"], "CHANGES_REQUESTED");
    assert_eq!(reviews[0]["user"]["login"], "bob");
    assert!(reviews[0]["submitted_at"].is_string());
    face_ok(
        "POST",
        &format!("{api}/issues/1/comments"),
        &bob,
        serde_json::json!({ "body": "see review" }),
    );
    let comments = face_ok(
        "GET",
        &format!("{api}/issues/1/comments?per_page=100"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(comments[0]["body"], "see review");
    let review_comments = face_ok(
        "GET",
        &format!("{api}/pulls/1/comments?per_page=100"),
        &alice,
        serde_json::json!(null),
    );
    assert!(review_comments.as_array().unwrap().is_empty());

    // Check runs by sha, and by branch name (GitHub accepts either).
    let head_sha = one["head"]["sha"].as_str().unwrap();
    face_ok(
        "POST",
        &format!("{api}/check-runs"),
        &ci,
        serde_json::json!({ "name": "ci/test", "head_sha": head_sha, "conclusion": "failure" }),
    );
    let runs = face_ok(
        "GET",
        &format!("{api}/commits/{head_sha}/check-runs?per_page=100"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(runs["total_count"], 1);
    assert_eq!(runs["check_runs"][0]["name"], "ci/test");
    assert_eq!(runs["check_runs"][0]["conclusion"], "failure");
    let by_branch = face_ok(
        "GET",
        &format!("{api}/commits/one/check-runs"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(by_branch["total_count"], 1);
    assert_eq!(
        face(
            "GET",
            &format!("{api}/commits/ghost/check-runs"),
            &alice,
            serde_json::json!(null)
        ),
        Err(422)
    );

    // Contents: base64 file at HEAD, ?ref= support, only-404-means-absent.
    let readme = face_ok(
        "GET",
        &format!("{api}/contents/README.md"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(readme["encoding"], "base64");
    let decoded = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(readme["content"].as_str().unwrap())
            .unwrap()
    };
    assert_eq!(decoded, b"# webapp\n");
    let on_branch = face_ok(
        "GET",
        &format!("{api}/contents/one.txt?ref=one"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(on_branch["path"], "one.txt");
    assert_eq!(
        face(
            "GET",
            &format!("{api}/contents/CLAUDE.md"),
            &alice,
            serde_json::json!(null)
        ),
        Err(404)
    );

    // Branches, identity, and the owned-repo mirror walk.
    let branches = face_ok(
        "GET",
        &format!("{api}/branches?per_page=50&page=1"),
        &alice,
        serde_json::json!(null),
    );
    let names: Vec<&str> = branches
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["main", "one", "two"]);

    let me = face_ok(
        "GET",
        &format!("{base}/api/v3/user"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(me["login"], "alice");
    let emails = face_ok(
        "GET",
        &format!("{base}/api/v3/user/emails"),
        &alice,
        serde_json::json!(null),
    );
    assert_eq!(emails[0]["primary"], true);
    assert_eq!(emails[0]["verified"], true);
    assert!(
        emails[0]["email"]
            .as_str()
            .unwrap()
            .ends_with("@orrery.invalid")
    );

    face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        serde_json::json!({ "name": "quiet" }),
    );
    let mine = face_ok(
        "GET",
        &format!("{base}/api/v3/user/repos?sort=pushed&direction=desc&per_page=100&page=1"),
        &alice,
        serde_json::json!(null),
    );
    let full_names: Vec<&str> = mine
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["full_name"].as_str().unwrap())
        .collect();
    // Pushed-desc: the pushed repo leads, the never-pushed one trails; and
    // bob owns nothing.
    assert_eq!(full_names, vec!["alice/webapp", "alice/quiet"]);
    let bobs = face_ok(
        "GET",
        &format!("{base}/api/v3/user/repos?sort=pushed&direction=desc"),
        &bob,
        serde_json::json!(null),
    );
    assert!(bobs.as_array().unwrap().is_empty());
}

/// The poller's per-cycle team reconcile reads an org's teams, and the local
/// PAT path reads the user's own. Teams are not modeled, so an org has none and
/// a user is in none; a user login is not an org.
#[tokio::test(flavor = "multi_thread")]
async fn org_teams() {
    let inst = boot().await;
    let base = inst.base.clone();
    mint_actor(&base, "acme", "org");
    let alice = mint_actor(&base, "alice", "user");
    assert_eq!(
        face_ok(
            "GET",
            &format!("{base}/api/v3/orgs/acme/teams?per_page=100&page=1"),
            &alice,
            serde_json::json!(null)
        ),
        serde_json::json!([])
    );
    assert_eq!(
        face_ok(
            "GET",
            &format!("{base}/api/v3/user/teams?per_page=100&page=1"),
            &alice,
            serde_json::json!(null)
        ),
        serde_json::json!([])
    );
    for login in ["alice", "ghost"] {
        assert_eq!(
            face(
                "GET",
                &format!("{base}/api/v3/orgs/{login}/teams"),
                &alice,
                serde_json::json!(null)
            ),
            Err(404),
            "{login}"
        );
    }
}
