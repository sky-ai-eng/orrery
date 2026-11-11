//! End-to-end: real `git` against a served world, exercising the recipe from
//! spec/surface-git.md — blobless clone, clock-dated commits, push, and the
//! log observation. This is rule 2 (real tools judge realness) as a
//! test: no simulated requests anywhere.

mod common;

use common::{boot, boot_with, control_post, face_answer, face_ok, git, git_output, mint_actor};

#[tokio::test(flavor = "multi_thread")]
async fn author_push_clone_observe() {
    let inst = boot().await;
    let base = &inst.base;

    // Control plane: mint the authoring actor.
    let token = mint_actor(base, "alice", "user");
    assert!(token.starts_with("orrfake_"));

    // Face: create the repo through GitHub's API shape (spec/usage.md — the
    // face's first REST endpoint; no push-to-create).
    let repo: serde_json::Value = ureq::post(format!("{base}/api/v3/user/repos"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "name": "webapp", "private": true }))
        .expect("create repo")
        .body_mut()
        .read_json()
        .expect("repo json");
    assert_eq!(repo["full_name"], "alice/webapp");
    let clone_url = repo["clone_url"].as_str().expect("clone_url");

    // Author a commit with clock-driven dates and push it, authenticated the
    // way a token-holding client does: token as the Basic password.
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{token}@"));
    git(&src, clock, &["push", &authed_url, "main"]);

    // Blobless clone — requires allowFilter on the served bare, and the
    // subsequent checkout exercises the promisor by-OID fetch path.
    let dst = work.path().join("clone");
    git(
        work.path(),
        clock,
        &[
            "clone",
            "--filter=blob:none",
            &authed_url,
            dst.to_str().unwrap(),
        ],
    );
    let readme = std::fs::read_to_string(dst.join("README.md")).unwrap();
    assert_eq!(readme, "# webapp\n");

    // The commit's dates came from the virtual clock, so the SHA is a pure
    // function of content — replay would reproduce it byte-for-byte.
    let sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();

    // Observation: the push landed in the log, attributed to alice through
    // git and clock-stamped, with the commit it brought in, and pushed_at
    // ticked.
    let log = control_post(base, "/_orrery/log/list", serde_json::json!({}));
    let items = log["items"].as_array().unwrap();
    let created = items
        .iter()
        .find(|e| e["type"] == "ref:created")
        .expect("push observed as ref:created");
    assert_eq!(created["actor"]["login"], "alice");
    assert_eq!(created["door"], "git");
    assert_eq!(created["clock_unix"].as_i64().unwrap(), clock);
    assert_eq!(created["subject"]["owner"]["login"], "alice");
    assert_eq!(created["subject"]["repo"]["name"], "webapp");
    assert_eq!(created["subject"]["name"], "refs/heads/main");
    assert_eq!(created["object"]["sha"], sha.as_str());
    assert_eq!(created["commits"], serde_json::json!([sha]));
    assert_eq!(created["commit_count"], 1);
    let stored = inst
        .world
        .repo_by_owner_name("alice", "webapp")
        .unwrap()
        .unwrap();
    assert_eq!(stored.pushed_at_clock_unix, Some(clock));

    // Advance the crank; a second commit lands at the new world time.
    let advanced = control_post(
        base,
        "/_orrery/clock/advance",
        serde_json::json!({
            "seconds": 3600,
        }),
    );
    let clock2 = advanced["clock_unix"].as_i64().unwrap();
    assert_eq!(clock2, clock + 3600);
    std::fs::write(src.join("app.txt"), "v2\n").unwrap();
    git(&src, clock2, &["add", "."]);
    git(&src, clock2, &["commit", "-m", "v2"]);
    git(&src, clock2, &["push", &authed_url, "main"]);
    let sha2 = git(&src, clock2, &["rev-parse", "HEAD"]).trim().to_string();
    let log = control_post(base, "/_orrery/log/list", serde_json::json!({}));
    let last_push = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "ref:updated")
        .unwrap();
    assert_eq!(last_push["clock_unix"].as_i64().unwrap(), clock2);
    assert_eq!(
        last_push["changes"]["sha"],
        serde_json::json!({ "from": sha, "to": sha2 })
    );
    assert_eq!(
        (&last_push["forced"], &last_push["commits"]),
        (&false.into(), &serde_json::json!([sha2]))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tag_moved_onto_a_tree_is_a_forced_update() {
    let inst = boot().await;
    let base = &inst.base;
    let token = mint_actor(base, "alice", "user");
    face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        serde_json::json!({ "name": "webapp" }),
    );
    let url = format!("{base}/alice/webapp.git")
        .replace("http://", &format!("http://x-access-token:{token}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    git(src, clock, &["init", "-b", "main", "."]);
    git(src, clock, &["commit", "--allow-empty", "-m", "init"]);
    git(src, clock, &["push", &url, "main", "HEAD:refs/tags/v1"]);
    let commit = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    let tree = git(src, clock, &["rev-parse", "HEAD^{tree}"])
        .trim()
        .to_string();

    // A tag can hold any object, so git accepts the move, and a commit is
    // never an ancestor of a tree.
    git(
        src,
        clock,
        &["push", "--force", &url, &format!("{tree}:refs/tags/v1")],
    );
    let log = control_post(base, "/_orrery/log/list", serde_json::json!({}));
    let moved = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "ref:updated")
        .expect("the move is recorded");
    assert_eq!(moved["subject"]["name"], "refs/tags/v1");
    assert_eq!(
        moved["changes"]["sha"],
        serde_json::json!({ "from": commit, "to": tree })
    );
    assert_eq!(
        (&moved["forced"], &moved["commits"], &moved["commit_count"]),
        (&true.into(), &serde_json::json!([]), &0.into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_and_not_mocked_contracts() {
    let inst = boot().await;
    let base = &inst.base;
    let token = mint_actor(base, "bob", "user");

    // No credentials → GitHub-shaped 401.
    let resp = ureq::post(format!("{base}/api/v3/user/repos"))
        .send_json(serde_json::json!({ "name": "x" }));
    let mut resp = resp.expect_err("401 surfaces as status error");
    match &mut resp {
        ureq::Error::StatusCode(code) => assert_eq!(*code, 401),
        other => panic!("expected status error, got {other:?}"),
    }

    // Unknown surface → loud 501 NOT-MOCKED, never a plausible 404, naming
    // the path as sent rather than what is left of it under `/api/v3`...
    let (status, body) = face_answer(
        "GET",
        &format!("{base}/api/v3/rate_limit"),
        &token,
        &[],
        None,
    );
    assert_eq!(status, 501, "NOT MOCKED is 501: {body}");
    assert_eq!(
        body["message"], "orrery: GET /api/v3/rate_limit is not mocked",
        "{body}"
    );
    assert_eq!(body["orrery"]["path"], "/api/v3/rate_limit", "{body}");
    assert_eq!(body["orrery"]["method"], "GET", "{body}");

    // ...while a matched route with missing data is a faithful 404.
    let err = ureq::post(format!("{base}/api/v3/orgs/ghost/repos"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "name": "x" }))
        .expect_err("unknown org 404s");
    match err {
        ureq::Error::StatusCode(code) => assert_eq!(code, 404),
        other => panic!("expected status error, got {other:?}"),
    }

    // Control plane is strict: unknown fields are rejected, not ignored.
    let err = ureq::post(format!("{base}/_orrery/clock/advance"))
        .send_json(serde_json::json!({ "seconds": 60, "bogus": true }))
        .expect_err("strict decoding rejects unknown fields");
    match err {
        ureq::Error::StatusCode(code) => assert_eq!(code, 400),
        other => panic!("expected status error, got {other:?}"),
    }
    let _ = inst;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_push_adopts_default_branch() {
    let inst = boot().await;
    let base = &inst.base;
    let token = mint_actor(base, "carol", "user");
    let repo: serde_json::Value = ureq::post(format!("{base}/api/v3/user/repos"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "name": "legacy" }))
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    let clone_url = repo["clone_url"].as_str().unwrap();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{token}@"));

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "trunk", "."]);
    std::fs::write(src.join("f"), "x").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["push", &authed_url, "trunk"]);

    // GitHub's first-push rule: the first branch pushed into an empty repo
    // becomes the default, and a fresh clone checks it out via HEAD.
    let stored = inst
        .world
        .repo_by_owner_name("carol", "legacy")
        .unwrap()
        .unwrap();
    assert_eq!(stored.default_branch, "trunk");
    let dst = work.path().join("clone");
    git(
        work.path(),
        clock,
        &["clone", &authed_url, dst.to_str().unwrap()],
    );
    let head = git(&dst, clock, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert_eq!(head.trim(), "trunk");
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_git_reads_follow_github_rules() {
    let inst = boot().await;
    let base = &inst.base;
    let token = mint_actor(base, "erin", "user");
    for (name, private) in [("open", false), ("secret", true)] {
        ureq::post(format!("{base}/api/v3/user/repos"))
            .header("authorization", &format!("token {token}"))
            .send_json(serde_json::json!({ "name": name, "private": private }))
            .unwrap();
    }
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    for name in ["open", "secret"] {
        let src = work.path().join(format!("src-{name}"));
        std::fs::create_dir(&src).unwrap();
        git(&src, clock, &["init", "-b", "main", "."]);
        std::fs::write(src.join("f"), name).unwrap();
        git(&src, clock, &["add", "."]);
        git(&src, clock, &["commit", "-m", "init"]);
        git(
            &src,
            clock,
            &[
                "push",
                &format!("{base}/erin/{name}.git")
                    .replace("http://", &format!("http://x-access-token:{token}@")),
                "main",
            ],
        );
    }

    // A public repo clones with no credentials at all — GitHub's rule, and
    // what a client that never sends tokens over plain http relies on.
    let dst = work.path().join("anon-clone");
    git(
        work.path(),
        clock,
        &[
            "clone",
            &format!("{base}/erin/open.git"),
            dst.to_str().unwrap(),
        ],
    );
    assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "open");

    // A private repo refuses anonymously (401, credential-shaped), and an
    // anonymous push refuses even on the public repo.
    let refused = ureq::get(format!(
        "{base}/erin/secret.git/info/refs?service=git-upload-pack"
    ))
    .call()
    .expect_err("private repo refuses anonymous fetch");
    match refused {
        ureq::Error::StatusCode(code) => assert_eq!(code, 401),
        other => panic!("expected status error, got {other:?}"),
    }
    let refused = ureq::get(format!(
        "{base}/erin/open.git/info/refs?service=git-receive-pack"
    ))
    .call()
    .expect_err("anonymous push negotiation refuses");
    match refused {
        ureq::Error::StatusCode(code) => assert_eq!(code, 401),
        other => panic!("expected status error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worlds_can_refuse_anonymous_git_reads() {
    let inst = boot_with(orrery_server::Options {
        anonymous_git_reads: false,
        ..Default::default()
    })
    .await;
    let base = &inst.base;
    let token = mint_actor(base, "frank", "user");
    ureq::post(format!("{base}/api/v3/user/repos"))
        .header("authorization", &format!("token {token}"))
        .send_json(serde_json::json!({ "name": "open", "private": false }))
        .unwrap();
    let authed_url = format!("{base}/frank/open.git")
        .replace("http://", &format!("http://x-access-token:{token}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("f"), "open").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["push", &authed_url, "main"]);

    // Even a public repo needs a token: both legs of a fetch get the same
    // 401 as any other unauthenticated face request.
    let anon = work.path().join("anon-clone");
    let out = git_output(
        work.path(),
        clock,
        &[
            "clone",
            &format!("{base}/frank/open.git"),
            anon.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success(), "anonymous clone must fail");
    let refused = ureq::get(format!(
        "{base}/frank/open.git/info/refs?service=git-upload-pack"
    ))
    .call()
    .expect_err("anonymous ref advertisement refuses");
    match refused {
        ureq::Error::StatusCode(code) => assert_eq!(code, 401),
        other => panic!("expected status error, got {other:?}"),
    }
    let refused = ureq::post(format!("{base}/frank/open.git/git-upload-pack"))
        .header("content-type", "application/x-git-upload-pack-request")
        .send("")
        .expect_err("anonymous upload-pack refuses");
    match refused {
        ureq::Error::StatusCode(code) => assert_eq!(code, 401),
        other => panic!("expected status error, got {other:?}"),
    }

    let dst = work.path().join("clone");
    git(
        work.path(),
        clock,
        &["clone", &authed_url, dst.to_str().unwrap()],
    );
    assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "open");
}
