//! End-to-end authoring flow (M3): open a PR against real pushed branches,
//! review it, comment on it, fabricate CI on it, push more commits and watch
//! the head re-pin, then merge — with real git verifying the merge landed.

mod common;

use common::{boot, control_post, face, face_ok, git, mint_actor};

struct Seeded {
    inst: common::Instance,
    token: String,
    ci_token: String,
    bob_token: String,
    work: tempfile::TempDir,
    src: std::path::PathBuf,
    authed_url: String,
    api: String,
    main_sha: String,
    feature_sha: String,
}

/// Boot, mint alice/bob/ci, create alice/webapp, push main and a `feature`
/// branch with one commit each.
async fn seed() -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let token = mint_actor(&base, "alice", "user");
    let bob_token = mint_actor(&base, "bob", "user");
    let ci_token = mint_actor(&base, "ci", "bot");

    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        serde_json::json!({ "name": "webapp" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap().to_string();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{token}@"));

    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# webapp\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    let main_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "add feature"]);
    let feature_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["push", &authed_url, "main", "feature"]);

    let api = format!("{base}/api/v3/repos/alice/webapp");
    Seeded {
        inst,
        token,
        ci_token,
        bob_token,
        work,
        src,
        authed_url,
        api,
        main_sha,
        feature_sha,
    }
}

/// A face call's status and JSON body, whatever the status.
fn answer(
    method: &str,
    url: &str,
    token: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let auth = format!("token {token}");
    let sent = match method {
        "POST" => agent
            .post(url)
            .header("authorization", &auth)
            .send_json(body),
        "PATCH" => agent
            .patch(url)
            .header("authorization", &auth)
            .send_json(body),
        other => panic!("unsupported method {other}"),
    };
    let mut resp = sent.unwrap_or_else(|e| panic!("{method} {url}: {e:?}"));
    let status = resp.status().as_u16();
    (status, resp.body_mut().read_json().expect("face json"))
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_lifecycle() {
    let s = seed().await;

    // Open the PR; the response pins head to the pushed tip and base to
    // main's tip.
    let pr = face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Add feature", "body": "does the thing",
                            "head": "feature", "base": "main" }),
    );
    assert_eq!(pr["number"], 1);
    assert_eq!(pr["state"], "open");
    assert_eq!(pr["user"]["login"], "alice");
    assert_eq!(pr["head"]["sha"], s.feature_sha.as_str());
    assert_eq!(pr["base"]["sha"], s.main_sha.as_str());
    assert_eq!(pr["head"]["repo"]["full_name"], "alice/webapp");

    // Validation contracts: duplicate, self-identical branches, missing
    // title, unknown branch.
    assert_eq!(
        face(
            "POST",
            &format!("{}/pulls", s.api),
            &s.token,
            serde_json::json!({ "title": "dup", "head": "feature", "base": "main" }),
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &format!("{}/pulls", s.api),
            &s.token,
            serde_json::json!({ "title": "same", "head": "main", "base": "main" }),
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &format!("{}/pulls", s.api),
            &s.token,
            serde_json::json!({ "head": "feature", "base": "main" }),
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &format!("{}/pulls", s.api),
            &s.token,
            serde_json::json!({ "title": "x", "head": "ghost", "base": "main" }),
        ),
        Err(422)
    );

    // Reviews: no self-approval; a missing event is a loud refusal (pending
    // reviews are not mocked); bob's approval pins the reviewed sha.
    let reviews_url = format!("{}/pulls/1/reviews", s.api);
    assert_eq!(
        face(
            "POST",
            &reviews_url,
            &s.token,
            serde_json::json!({ "event": "APPROVE" }),
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &reviews_url,
            &s.bob_token,
            serde_json::json!({ "body": "hm" })
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "POST",
            &reviews_url,
            &s.bob_token,
            serde_json::json!({ "event": "NOD" }),
        ),
        Err(422)
    );
    let review = face_ok(
        "POST",
        &reviews_url,
        &s.bob_token,
        serde_json::json!({ "event": "APPROVE", "body": "lgtm" }),
    );
    assert_eq!(review["state"], "APPROVED");
    assert_eq!(review["user"]["login"], "bob");
    assert_eq!(review["commit_id"], s.feature_sha.as_str());

    // Comments ride the issues route, as on GitHub.
    let comment = face_ok(
        "POST",
        &format!("{}/issues/1/comments", s.api),
        &s.bob_token,
        serde_json::json!({ "body": "ship it" }),
    );
    assert_eq!(comment["body"], "ship it");
    assert_eq!(
        face(
            "POST",
            &format!("{}/issues/1/comments", s.api),
            &s.bob_token,
            serde_json::json!({}),
        ),
        Err(422)
    );

    // Fabricated CI: bots only; conclusion completes the run.
    let checks_url = format!("{}/check-runs", s.api);
    assert_eq!(
        face(
            "POST",
            &checks_url,
            &s.token,
            serde_json::json!({ "name": "ci/test", "head_sha": s.feature_sha }),
        ),
        Err(403),
        "non-bot actors cannot fabricate CI"
    );
    let run = face_ok(
        "POST",
        &checks_url,
        &s.ci_token,
        serde_json::json!({ "name": "ci/test", "head_sha": s.feature_sha }),
    );
    assert_eq!(run["status"], "queued");
    let run_id = run["id"].as_i64().unwrap();
    assert_eq!(
        face(
            "PATCH",
            &format!("{}/{run_id}", checks_url),
            &s.ci_token,
            serde_json::json!({ "status": "queued", "conclusion": "success" }),
        ),
        Err(422),
        "conclusion contradicting an explicit non-completed status"
    );
    let done = face_ok(
        "PATCH",
        &format!("{}/{run_id}", checks_url),
        &s.ci_token,
        serde_json::json!({ "conclusion": "success" }),
    );
    assert_eq!(done["status"], "completed");
    assert_eq!(done["conclusion"], "success");
    assert!(done["completed_at"].is_string());

    // Push another commit to the head branch: the PR re-pins (synchronize).
    let clock = s.inst.world.clock().unwrap();
    std::fs::write(s.src.join("feature.txt"), "v2\n").unwrap();
    git(&s.src, clock, &["add", "."]);
    git(&s.src, clock, &["commit", "-m", "feature v2"]);
    let v2_sha = git(&s.src, clock, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    git(&s.src, clock, &["push", &s.authed_url, "feature"]);
    let repo_row = s
        .inst
        .world
        .repo_by_owner_name("alice", "webapp")
        .unwrap()
        .unwrap();
    let pull_row = s
        .inst
        .world
        .pull_by_number(repo_row.id, 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        pull_row.head_sha, v2_sha,
        "push re-pinned the open PR's head"
    );

    // Merge: a stale expected sha is a 409; squash is a loud refusal; the
    // real merge moves main and closes the PR as merged.
    let merge_url = format!("{}/pulls/1/merge", s.api);
    assert_eq!(
        face(
            "PUT",
            &merge_url,
            &s.token,
            serde_json::json!({ "sha": s.feature_sha }),
        ),
        Err(409),
        "expected-sha guard catches the synchronize"
    );
    assert_eq!(
        face(
            "PUT",
            &merge_url,
            &s.token,
            serde_json::json!({ "merge_method": "squash" }),
        ),
        Err(422),
        "unbuilt merge methods refuse loudly"
    );
    let merged = face_ok("PUT", &merge_url, &s.token, serde_json::json!({}));
    assert_eq!(merged["merged"], true);
    let merge_sha = merged["sha"].as_str().unwrap().to_string();

    let pull_row = s
        .inst
        .world
        .pull_by_number(repo_row.id, 1)
        .unwrap()
        .unwrap();
    assert!(pull_row.merged);
    assert_eq!(
        pull_row.merge_commit_sha.as_deref(),
        Some(merge_sha.as_str())
    );
    assert_eq!(pull_row.merged_by_login.as_deref(), Some("alice"));

    // Real git is the judge: a fresh clone of main contains the feature work
    // and HEAD is the merge commit.
    let dst = s.work.path().join("merged-clone");
    git(
        s.work.path(),
        clock,
        &["clone", &s.authed_url, dst.to_str().unwrap()],
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("feature.txt")).unwrap(),
        "v2\n"
    );
    assert_eq!(
        git(&dst, clock, &["rev-parse", "HEAD"]).trim(),
        merge_sha.as_str()
    );

    // Terminal states: no second merge, no reopen of a merged PR.
    assert_eq!(
        face("PUT", &merge_url, &s.token, serde_json::json!({})),
        Err(405)
    );
    assert_eq!(
        face(
            "PATCH",
            &format!("{}/pulls/1", s.api),
            &s.token,
            serde_json::json!({ "state": "open" }),
        ),
        Err(422)
    );

    // The log tells the whole story in order.
    let log = control_post(&s.inst.base, "/_orrery/log/list", serde_json::json!({}));
    let actions: Vec<&str> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    for expected in [
        "pull.open",
        "review.submit",
        "comment.create",
        "check_run.create",
        "check_run.update",
        "pull.merge",
    ] {
        assert!(actions.contains(&expected), "log is missing {expected}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn patch_edits_close_and_reopen() {
    let s = seed().await;
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "WIP", "body": "draft text",
                            "head": "feature", "base": "main" }),
    );
    let url = format!("{}/pulls/1", s.api);

    // Field-by-field PATCH: title set, body cleared with an explicit null.
    let patched = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "title": "Add feature", "body": null }),
    );
    assert_eq!(patched["title"], "Add feature");
    assert_eq!(patched["body"], serde_json::Value::Null);

    // A blank title is refused with the create's own 422 and changes
    // nothing; a null title keeps the one stored.
    let (status, create_refusal) = answer(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": " ", "head": "feature", "base": "main" }),
    );
    assert_eq!(status, 422);
    assert_eq!(create_refusal["errors"][0]["code"], "missing_field");
    assert_eq!(create_refusal["errors"][0]["field"], "title");
    for blank in ["", "   "] {
        let (status, edit_refusal) = answer(
            "PATCH",
            &url,
            &s.token,
            serde_json::json!({ "title": blank, "state": "closed" }),
        );
        assert_eq!(status, 422, "title {blank:?}");
        assert_eq!(edit_refusal, create_refusal, "title {blank:?}");
    }
    let kept = face_ok("GET", &url, &s.token, serde_json::json!(null));
    assert_eq!(kept["title"], "Add feature");
    assert_eq!(kept["state"], "open");
    let kept = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "title": null }),
    );
    assert_eq!(kept["title"], "Add feature");
    let trimmed = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "title": "  Add feature  " }),
    );
    assert_eq!(trimmed["title"], "Add feature");

    let closed = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "state": "closed" }),
    );
    assert_eq!(closed["state"], "closed");
    assert!(closed["closed_at"].is_string());
    let reopened = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "state": "open" }),
    );
    assert_eq!(reopened["state"], "open");
    assert_eq!(reopened["closed_at"], serde_json::Value::Null);

    // Bad values are named rejections, and unknown pulls are faithful 404s.
    assert_eq!(
        face(
            "PATCH",
            &url,
            &s.token,
            serde_json::json!({ "state": "parked" })
        ),
        Err(422)
    );
    assert_eq!(
        face(
            "PATCH",
            &format!("{}/pulls/99", s.api),
            &s.token,
            serde_json::json!({ "title": "x" }),
        ),
        Err(404)
    );

    // A draft PR refuses to merge while a work in progress.
    let clock = s.inst.world.clock().unwrap();
    git(&s.src, clock, &["checkout", "-b", "feature2"]);
    std::fs::write(s.src.join("two.txt"), "2\n").unwrap();
    git(&s.src, clock, &["add", "."]);
    git(&s.src, clock, &["commit", "-m", "two"]);
    git(&s.src, clock, &["push", &s.authed_url, "feature2"]);
    let draft = face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Two", "head": "feature2", "base": "main", "draft": true }),
    );
    assert_eq!(draft["draft"], true);
    assert_eq!(
        face(
            "PUT",
            &format!("{}/pulls/{}/merge", s.api, draft["number"]),
            &s.token,
            serde_json::json!({}),
        ),
        Err(405)
    );
}
