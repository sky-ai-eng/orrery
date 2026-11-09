//! End-to-end authoring flow (M3): open a PR against real pushed branches,
//! review it, comment on it, fabricate CI on it, push more commits and watch
//! the head re-pin, then merge — with real git verifying the merge landed.

mod common;

use common::{boot, control_post, face, face_answer, face_ok, git, mint_actor};
use orrery_face_git::store;

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
    let absent = answer(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "head": "feature", "base": "main" }),
    );
    assert_eq!(absent, (422, create_refusal.clone()));
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

    // A base is refused with GitHub's message for what is wrong with it,
    // and the title beside it still lands. The pull's own head, or a branch
    // that already holds its commits, has no new commits to propose.
    let base_refused = |message: &str| {
        (
            422,
            serde_json::json!({
                "message": "Validation Failed",
                "errors": [{
                    "resource": "PullRequest",
                    "code": "invalid",
                    "field": "base",
                    "message": message,
                }],
            }),
        )
    };
    let clock = s.inst.world.clock().unwrap();
    git(
        &s.src,
        clock,
        &["push", &s.authed_url, "feature:refs/heads/holds-feature"],
    );
    for (base, message) in [
        (
            "feature",
            "There are no new commits between base branch 'feature' and head branch 'feature'",
        ),
        (
            "holds-feature",
            "There are no new commits between base branch 'holds-feature' and head branch 'feature'",
        ),
        ("ghost", "Proposed base branch 'ghost' was not found"),
    ] {
        assert_eq!(
            answer(
                "PATCH",
                &url,
                &s.token,
                serde_json::json!({ "base": base, "title": base })
            ),
            base_refused(message)
        );
    }
    let kept = face_ok("GET", &url, &s.token, serde_json::json!(null));
    assert_eq!(kept["base"]["ref"], "main");
    assert_eq!(kept["title"], "ghost");

    // An unknown state is ignored, as GitHub ignores it, and the rest of the
    // request lands. Unknown pulls are faithful 404s.
    let ignored = face_ok(
        "PATCH",
        &url,
        &s.token,
        serde_json::json!({ "state": "parked", "title": "Add feature" }),
    );
    assert_eq!(ignored["state"], "open");
    assert_eq!(ignored["title"], "Add feature");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_pull_refuses_the_edits_github_refuses() {
    let s = seed().await;
    let clock = s.inst.world.clock().unwrap();
    git(
        &s.src,
        clock,
        &["push", &s.authed_url, "main:dev", "main:release"],
    );
    let open = |base: &str| {
        face_ok(
            "POST",
            &format!("{}/pulls", s.api),
            &s.token,
            serde_json::json!({ "title": "Add feature", "head": "feature", "base": base }),
        )
    };
    let patch = |n: i64, body: serde_json::Value| {
        answer("PATCH", &format!("{}/pulls/{n}", s.api), &s.token, body)
    };
    let get = |n: i64| {
        face_ok(
            "GET",
            &format!("{}/pulls/{n}", s.api),
            &s.token,
            serde_json::json!(null),
        )
    };
    let bare = store::bare_dir(&s.inst.world.repos_dir(), "alice", "webapp");
    let pull_ref = |n: i64| store::rev_parse(&bare, &store::pull_head_ref(n)).unwrap();
    // The pull entries in the log, which the pushes between the requests do
    // not add to.
    let entries = || {
        let log = s.inst.world.log_after(0, 10_000).unwrap();
        log.iter().filter(|e| e.action.starts_with("pull.")).count()
    };
    let refused = |field: &str, code: &str, message: &str| {
        (
            422,
            serde_json::json!({
                "message": "Validation Failed",
                "errors": [{
                    "resource": "PullRequest",
                    "code": code,
                    "field": field,
                    "message": message,
                }],
            }),
        )
    };
    let base_refused = refused(
        "base",
        "invalid",
        "Cannot change the base branch of a closed pull request.",
    );
    let state_refused = |why: &str| {
        refused(
            "state",
            "custom",
            &format!("state cannot be changed. {why}"),
        )
    };
    open("dev");

    // GitHub judges a base after the request's other writes land, a close
    // among them, so a base beside a close is refused on a pull that is
    // closed by then, and the close, title, and body stay written.
    let before = entries();
    assert_eq!(
        patch(
            1,
            serde_json::json!({ "state": "closed", "base": "release", "title": "T1", "body": "B1" })
        ),
        base_refused
    );
    let kept = get(1);
    assert_eq!(
        (
            &kept["state"],
            &kept["base"]["ref"],
            &kept["title"],
            &kept["body"]
        ),
        (&"closed".into(), &"dev".into(), &"T1".into(), &"B1".into())
    );
    assert_eq!(entries(), before + 2, "the edit and the close");

    // On a closed pull, any base is refused, its own included.
    let before = entries();
    assert_eq!(
        patch(
            1,
            serde_json::json!({ "base": "dev", "title": "T2", "body": "B2" })
        ),
        base_refused
    );
    assert_eq!(
        patch(1, serde_json::json!({ "base": "release" })),
        base_refused
    );
    let kept = get(1);
    assert_eq!(
        (&kept["base"]["ref"], &kept["title"], &kept["body"]),
        (&"dev".into(), &"T2".into(), &"B2".into())
    );
    assert_eq!(entries(), before + 1);

    // A refused reopen keeps only the body: GitHub writes it first and drops
    // the title. A base beside it is judged against the pull it left closed,
    // and that refusal is the one answered. A reopen is refused while its
    // base branch is gone, naming both branches, base first, when its head
    // branch is gone too.
    git(&s.src, clock, &["push", &s.authed_url, ":dev"]);
    let before = entries();
    assert_eq!(
        patch(
            1,
            serde_json::json!({ "state": "open", "base": "release", "title": "T3", "body": "B3" })
        ),
        base_refused
    );
    assert_eq!(
        patch(
            1,
            serde_json::json!({ "state": "open", "title": "T4", "body": "B4" })
        ),
        state_refused("The dev branch has been deleted.")
    );
    git(&s.src, clock, &["push", &s.authed_url, ":feature"]);
    let reopen_and_retitle = serde_json::json!({ "state": "open", "title": "T5" });
    assert_eq!(
        patch(1, reopen_and_retitle.clone()),
        state_refused("The dev and feature branches have been deleted.")
    );
    let feature_back = format!("{}:refs/heads/feature", s.feature_sha);
    git(&s.src, clock, &["push", &s.authed_url, &feature_back]);
    assert_eq!(
        patch(1, reopen_and_retitle),
        state_refused("The dev branch has been deleted.")
    );
    let kept = get(1);
    assert_eq!(
        (&kept["state"], &kept["title"], &kept["body"]),
        (&"closed".into(), &"T2".into(), &"B4".into())
    );
    assert_eq!(pull_ref(1).as_deref(), Some(s.feature_sha.as_str()));
    assert_eq!(entries(), before + 2, "the two body edits");

    // Restoring the base branch lets it reopen.
    let dev_back = format!("{}:refs/heads/dev", s.main_sha);
    git(&s.src, clock, &["push", &s.authed_url, &dev_back]);
    let (status, reopened) = patch(
        1,
        serde_json::json!({ "state": "open", "title": "Retitled" }),
    );
    assert_eq!(status, 200, "{reopened}");
    assert_eq!(reopened["state"], "open");
    assert_eq!(reopened["title"], "Retitled");

    // Closed again, with a second pull opened on the same head and base, it
    // is refused as the create of a third would be, and only the second stays
    // open.
    let (status, _) = patch(1, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    assert_eq!(open("dev")["number"], 2);
    assert_eq!(
        patch(
            1,
            serde_json::json!({ "state": "open", "title": "T6", "body": "B6" })
        ),
        state_refused("There is already an open pull request from feature to dev.")
    );
    let kept = get(1);
    assert_eq!(
        (&kept["state"], &kept["title"], &kept["body"]),
        (&"closed".into(), &"Retitled".into(), &"B6".into())
    );
    assert_eq!(get(2)["state"], "open");

    // Retargeting an open pull onto a pair another open pull holds is
    // refused, and the title and body beside it land. A pull's own base is
    // not another pull's.
    assert_eq!(open("release")["number"], 3);
    let before = entries();
    assert_eq!(
        patch(
            3,
            serde_json::json!({ "base": "dev", "title": "Moved", "body": "Moved too" })
        ),
        refused(
            "base",
            "invalid",
            "A pull request already exists for base branch 'dev' and head branch 'feature'"
        )
    );
    let kept = get(3);
    assert_eq!(
        (&kept["base"]["ref"], &kept["title"], &kept["body"]),
        (&"release".into(), &"Moved".into(), &"Moved too".into())
    );
    assert_eq!(entries(), before + 1, "the title and body edit");
    let (status, _) = patch(3, serde_json::json!({ "base": "release" }));
    assert_eq!(status, 200);

    // Force-pushed as well as held, the first pull is refused for the
    // duplicate; with the duplicate closed, for the force-push. The pull open
    // from the same head into release blocks neither.
    git(&s.src, clock, &["commit", "--amend", "-m", "rewritten"]);
    git(
        &s.src,
        clock,
        &["push", "--force", &s.authed_url, "feature"],
    );
    let reopen = serde_json::json!({ "state": "open", "body": "B7" });
    assert_eq!(
        patch(1, reopen.clone()),
        state_refused("There is already an open pull request from feature to dev.")
    );
    let (status, _) = patch(2, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    assert_eq!(
        patch(1, reopen),
        state_refused("The feature branch was force-pushed or recreated.")
    );
    let kept = get(1);
    assert_eq!(
        (&kept["state"], &kept["body"]),
        (&"closed".into(), &"B7".into())
    );

    // A base beside a reopen that lands is judged against the reopened pull:
    // it moves the pull after the reopen, or is refused with the reopen kept.
    assert_eq!(open("dev")["number"], 4);
    let (status, _) = patch(4, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    let before = entries();
    let (status, moved) = patch(
        4,
        serde_json::json!({ "state": "open", "base": "main", "title": "TA", "body": "BA" }),
    );
    assert_eq!(status, 200, "{moved}");
    assert_eq!(
        (
            &moved["state"],
            &moved["base"]["ref"],
            &moved["title"],
            &moved["body"]
        ),
        (&"open".into(), &"main".into(), &"TA".into(), &"BA".into())
    );
    let log = s.inst.world.log_after(0, 10_000).unwrap();
    let actions: Vec<&str> = log
        .iter()
        .filter(|e| e.action.starts_with("pull."))
        .map(|e| e.action.as_str())
        .skip(before)
        .collect();
    assert_eq!(actions, ["pull.edit", "pull.reopen", "pull.edit"]);
    let (status, _) = patch(4, serde_json::json!({ "state": "closed" }));
    assert_eq!(status, 200);
    assert_eq!(
        patch(
            4,
            serde_json::json!({ "state": "open", "base": "release", "title": "TA2", "body": "BA2" })
        ),
        refused(
            "base",
            "invalid",
            "A pull request already exists for base branch 'release' and head branch 'feature'"
        )
    );
    let kept = get(4);
    assert_eq!(
        (
            &kept["state"],
            &kept["base"]["ref"],
            &kept["title"],
            &kept["body"]
        ),
        (&"open".into(), &"main".into(), &"TA2".into(), &"BA2".into())
    );
}

/// The fields of a not-mocked refusal: its status, the `field` it names, and
/// the `resource` its `errors[]` entry is about.
fn not_mocked_field(answer: &(u16, serde_json::Value)) -> (u16, &str, &str) {
    let (status, body) = answer;
    assert_eq!(body["orrery"]["not_mocked"], true, "{body}");
    let error = &body["errors"][0];
    assert_eq!(error["field"], body["orrery"]["field"], "{body}");
    (
        *status,
        error["field"].as_str().unwrap(),
        error["resource"].as_str().unwrap(),
    )
}

/// A GitHub media type asking for a rendering the face doesn't produce is
/// refused by name, on reads and writes alike, rather than answered with JSON
/// that lacks what it asked for, once the route has found what its path
/// names. The JSON types, and the previews gh sends, get the answer they
/// always did.
#[tokio::test(flavor = "multi_thread")]
async fn unrendered_media_types_are_not_mocked() {
    let s = seed().await;
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Feature", "head": "feature", "base": "main" }),
    );
    let url = format!("{}/pulls/1", s.api);
    for accept in [
        "application/vnd.github.full+json",
        "application/vnd.github.v3.raw+json",
        "application/vnd.github.text+json",
        "application/vnd.github.html+json",
        "application/vnd.github.v3.diff",
        "application/vnd.github.v3.patch",
        "Application/VND.GitHub.DIFF",
        // One unrendered type among JSON ones is enough.
        "application/json, application/vnd.github.v3.full+json;q=0.9",
    ] {
        let answer = face_answer("GET", &url, &s.token, &[("accept", accept)], None);
        assert_eq!(
            not_mocked_field(&answer),
            (422, "Accept", "PullRequest"),
            "{accept}"
        );
    }

    let plain = face_ok("GET", &url, &s.token, serde_json::json!(null));
    assert_eq!(plain["head"]["sha"], s.feature_sha.as_str());
    for accept in [
        "application/vnd.github+json",
        "application/vnd.github.v3+json",
        "application/json",
        "*/*",
        "application/vnd.github.merge-info-preview+json, application/vnd.github.nebula-preview",
    ] {
        let (status, pull) = face_answer("GET", &url, &s.token, &[("accept", accept)], None);
        assert_eq!((status, &pull), (200, &plain), "{accept}");
    }

    // Something that doesn't exist is GitHub's 404, whatever the request
    // accepts.
    let full = [("accept", "application/vnd.github.full+json")];
    for missing in ["2", "abc"] {
        let missing = format!("{}/pulls/{missing}", s.api);
        let (status, body) = face_answer("GET", &missing, &s.token, &full, None);
        assert_eq!((status, &body["message"]), (404, &"Not Found".into()));
    }

    // A write is refused before it lands.
    let comments = format!("{}/issues/1/comments", s.api);
    let answer = face_answer(
        "POST",
        &comments,
        &s.bob_token,
        &full,
        Some(serde_json::json!({ "body": "ship it" })),
    );
    assert_eq!(not_mocked_field(&answer), (422, "Accept", "IssueComment"));
    let html = [("accept", "application/vnd.github.html+json")];
    let answer = face_answer("GET", &comments, &s.token, &html, None);
    assert_eq!(not_mocked_field(&answer), (422, "Accept", "IssueComment"));
    let listed = face_ok("GET", &comments, &s.token, serde_json::json!(null));
    assert_eq!(listed, serde_json::json!([]));

    // An organization write finds what its path names before it is refused,
    // and is refused before it lands.
    mint_actor(&s.inst.base, "acme", "org");
    let base = &s.inst.base;
    for missing in [
        "/orgs/nope/memberships/bob",
        "/orgs/acme/memberships/nobody",
    ] {
        let (status, body) = face_answer(
            "PUT",
            &format!("{base}/api/v3{missing}"),
            &s.token,
            &full,
            Some(serde_json::json!({})),
        );
        assert_eq!(
            (status, &body["message"]),
            (404, &"Not Found".into()),
            "{missing}"
        );
    }
    let answer = face_answer(
        "PUT",
        &format!("{base}/api/v3/orgs/acme/memberships/bob"),
        &s.token,
        &full,
        Some(serde_json::json!({})),
    );
    assert_eq!(not_mocked_field(&answer), (422, "Accept", "Membership"));
    assert_eq!(s.inst.world.org_membership("acme", "bob").unwrap(), None);

    // A file's raw bytes and its rendering are refused; the object media type
    // answers the object the JSON one does.
    let readme = format!("{}/contents/README.md", s.api);
    for accept in [
        "application/vnd.github.raw+json",
        "application/vnd.github.v3.raw",
        "application/vnd.github.html+json",
    ] {
        let answer = face_answer("GET", &readme, &s.token, &[("accept", accept)], None);
        assert_eq!(
            not_mocked_field(&answer),
            (422, "Accept", "Content"),
            "{accept}"
        );
    }
    let file = face_ok("GET", &readme, &s.token, serde_json::json!(null));
    let object = [("accept", "application/vnd.github.object+json")];
    assert_eq!(
        face_answer("GET", &readme, &s.token, &object, None),
        (200, file)
    );
    // The object media type is a rendering of contents alone.
    let answer = face_answer("GET", &url, &s.token, &object, None);
    assert_eq!(not_mocked_field(&answer), (422, "Accept", "PullRequest"));

    // A method the path doesn't serve is still the not-mocked 501.
    let (status, body) = face_answer("PUT", &url, &s.token, &full, Some(serde_json::json!({})));
    assert_eq!((status, &body["orrery"]["method"]), (501, &"PUT".into()));
}

/// Each not-mocked field refusal names the resource its route is about.
#[tokio::test(flavor = "multi_thread")]
async fn not_mocked_fields_name_their_resource() {
    let s = seed().await;
    face_ok(
        "POST",
        &format!("{}/pulls", s.api),
        &s.token,
        serde_json::json!({ "title": "Feature", "head": "feature", "base": "main" }),
    );
    let cases = [
        (
            "PUT",
            format!("{}/pulls/1/merge", s.api),
            &s.token,
            serde_json::json!({ "merge_method": "squash" }),
            ("merge_method=squash", "PullRequest"),
        ),
        (
            "POST",
            format!("{}/pulls/1/reviews", s.api),
            &s.bob_token,
            serde_json::json!({
                "event": "COMMENT",
                "comments": [{ "path": "feature.txt", "line": 1, "body": "nit" }],
            }),
            ("comments", "PullRequestReview"),
        ),
        (
            "POST",
            format!("{}/issues/1/labels", s.api),
            &s.token,
            serde_json::json!({ "labels": [{ "name": "bug" }] }),
            ("labels[] as objects", "Label"),
        ),
        (
            "POST",
            format!("{}/check-runs", s.api),
            &s.ci_token,
            serde_json::json!({
                "name": "build",
                "head_sha": s.feature_sha,
                "actions": [{ "label": "Fix", "description": "fix it", "identifier": "fix" }],
            }),
            ("actions", "CheckRun"),
        ),
    ];
    for (method, url, token, body, (field, resource)) in cases {
        let answer = face_answer(method, &url, token, &[], Some(body));
        assert_eq!(
            not_mocked_field(&answer),
            (422, field, resource),
            "{method} {url}"
        );
    }
}
