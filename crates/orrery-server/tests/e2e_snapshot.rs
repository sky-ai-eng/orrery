//! End-to-end M2: snapshot a served world, keep living in it, snapshot
//! again, diff the two moments over HTTP, restore the earlier one (real git
//! confirms the refs rewound), and fork a snapshot into a second, fully
//! independent served instance.

mod common;

use common::{boot, boot_at, control_post, face, face_ok, git, mint_actor};

fn control(
    method: &str,
    base: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(u16, serde_json::Value), u16> {
    let url = format!("{base}{path}");
    let result = match (method, body) {
        ("POST", Some(b)) => ureq::post(&url).send_json(b),
        ("GET", None) => ureq::get(&url).call(),
        ("DELETE", None) => ureq::delete(&url).call(),
        other => panic!("unsupported control call {other:?}"),
    };
    match result {
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            let value = if status == 204 {
                serde_json::Value::Null
            } else {
                resp.body_mut().read_json().expect("control json")
            };
            Ok((status, value))
        }
        Err(ureq::Error::StatusCode(code)) => Err(code),
        Err(other) => panic!("control transport error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_diff_restore_over_http() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bob = mint_actor(&base, "bob", "user");

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
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    let main_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["push", &authed_url, "main"]);

    // The "before" moment.
    let (status, before) = control(
        "POST",
        &base,
        "/_orrery/snapshots",
        Some(serde_json::json!({ "name": "before" })),
    )
    .unwrap();
    assert_eq!(status, 201);
    assert_eq!(before["clock_unix"].as_i64().unwrap(), clock);

    // Life goes on: an hour passes, a feature lands as a merged PR, and a
    // new actor arrives.
    control_post(
        &base,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 3600 }),
    );
    let clock2 = inst.world.clock().unwrap();
    git(&src, clock2, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock2, &["add", "."]);
    git(&src, clock2, &["commit", "-m", "add feature"]);
    git(&src, clock2, &["push", &authed_url, "feature"]);
    let api = format!("{base}/api/v3/repos/alice/webapp");
    face_ok(
        "POST",
        &format!("{api}/pulls"),
        &alice,
        serde_json::json!({ "title": "Add feature", "head": "feature", "base": "main" }),
    );
    face_ok(
        "POST",
        &format!("{api}/pulls/1/reviews"),
        &bob,
        serde_json::json!({ "event": "APPROVE" }),
    );
    face_ok(
        "PUT",
        &format!("{api}/pulls/1/merge"),
        &alice,
        serde_json::json!({}),
    );
    let dave = mint_actor(&base, "dave", "user");

    let (status, after) = control(
        "POST",
        &base,
        "/_orrery/snapshots",
        Some(serde_json::json!({ "name": "after" })),
    )
    .unwrap();
    assert_eq!(status, 201);
    assert_eq!(after["clock_unix"].as_i64().unwrap(), clock2);

    // Snapshot read contracts: point read, keyset list, conflict, bad name.
    let (_, got) = control("GET", &base, "/_orrery/snapshots/before", None).unwrap();
    assert_eq!(got, before);
    assert_eq!(
        control("GET", &base, "/_orrery/snapshots/ghost", None),
        Err(404)
    );
    assert_eq!(
        control(
            "POST",
            &base,
            "/_orrery/snapshots",
            Some(serde_json::json!({ "name": "before" })),
        ),
        Err(409)
    );
    assert_eq!(
        control(
            "POST",
            &base,
            "/_orrery/snapshots",
            Some(serde_json::json!({ "name": "no/slashes" })),
        ),
        Err(400)
    );
    let (_, page) = control(
        "POST",
        &base,
        "/_orrery/snapshots/list",
        Some(serde_json::json!({ "page_size": 1 })),
    )
    .unwrap();
    assert_eq!(page["items"][0]["name"], "after");
    assert_eq!(page["next_after_name"], "after");
    let (_, page2) = control(
        "POST",
        &base,
        "/_orrery/snapshots/list",
        Some(serde_json::json!({ "page_size": 1, "after_name": "after" })),
    )
    .unwrap();
    assert_eq!(page2["items"][0]["name"], "before");

    // The diff between the two moments is the engine's verdict on the hour.
    let (_, d) = control(
        "POST",
        &base,
        "/_orrery/diff",
        Some(serde_json::json!({ "from": "before", "to": "after" })),
    )
    .unwrap();
    assert_eq!(d["from"]["clock_unix"].as_i64().unwrap(), clock);
    assert_eq!(d["to"]["clock_unix"].as_i64().unwrap(), clock2);
    assert_eq!(d["actors_added"][0]["login"], "dave");
    assert_eq!(d["pulls_opened"][0]["title"], "Add feature");
    assert_eq!(d["pulls_merged"][0]["number"], 1);
    assert_eq!(d["reviews_added"][0]["reviewer"], "bob");
    let refs_touched: Vec<&str> = d["ref_changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["ref"].as_str())
        .collect();
    assert!(refs_touched.contains(&"refs/heads/feature"));
    assert!(
        refs_touched.contains(&"refs/heads/main"),
        "merge moved main"
    );
    assert!(d["log"]["appended"].as_i64().unwrap() > 0);

    // Omitting `to` diffs against the live world, which currently equals
    // "after".
    let (_, live) = control(
        "POST",
        &base,
        "/_orrery/diff",
        Some(serde_json::json!({ "from": "after" })),
    )
    .unwrap();
    assert_eq!(live["to"]["name"], "live");
    assert_eq!(live["log"]["appended"].as_i64().unwrap(), 0);
    assert!(live["ref_changes"].as_array().unwrap().is_empty());
    assert_eq!(
        control(
            "POST",
            &base,
            "/_orrery/diff",
            Some(serde_json::json!({ "from": "ghost" })),
        ),
        Err(404)
    );

    // Restore "before": the clock rewinds, the PR never happened, dave's
    // token stops resolving, and — real git as judge — main is back on the
    // first commit.
    let (status, restored) = control(
        "POST",
        &base,
        "/_orrery/snapshots/before/restore",
        Some(serde_json::json!({})),
    )
    .unwrap();
    assert_eq!(status, 200);
    assert_eq!(restored["clock_unix"].as_i64().unwrap(), clock);
    assert_eq!(inst.world.clock().unwrap(), clock);
    let repo_row = inst
        .world
        .repo_by_owner_name("alice", "webapp")
        .unwrap()
        .unwrap();
    assert!(inst.world.pull_by_number(repo_row.id, 1).unwrap().is_none());
    assert_eq!(
        face(
            "POST",
            &format!("{base}/api/v3/user/repos"),
            &dave,
            serde_json::json!({ "name": "x" }),
        ),
        Err(401),
        "a restored world never minted dave's token"
    );
    let dst = work.path().join("restored-clone");
    git(
        work.path(),
        clock,
        &["clone", &authed_url, dst.to_str().unwrap()],
    );
    assert_eq!(git(&dst, clock, &["rev-parse", "HEAD"]).trim(), main_sha);
    assert!(!dst.join("feature.txt").exists());

    // The restored world is live: pushes and further snapshots work.
    git(&src, clock, &["push", &authed_url, "feature"]);
    let (status, _) = control(
        "POST",
        &base,
        "/_orrery/snapshots",
        Some(serde_json::json!({ "name": "again" })),
    )
    .unwrap();
    assert_eq!(status, 201);

    // Delete is rm -rf with a receipt.
    let (status, _) = control("DELETE", &base, "/_orrery/snapshots/after", None).unwrap();
    assert_eq!(status, 204);
    assert_eq!(
        control("GET", &base, "/_orrery/snapshots/after", None),
        Err(404)
    );
    assert_eq!(
        control(
            "POST",
            &base,
            "/_orrery/snapshots/after/restore",
            Some(serde_json::json!({}))
        ),
        Err(404)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fork_serves_an_independent_world() {
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
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    git(&src, clock, &["push", &authed_url, "main"]);
    control(
        "POST",
        &base,
        "/_orrery/snapshots",
        Some(serde_json::json!({ "name": "seed" })),
    )
    .unwrap();

    // Fork the snapshot into a second instance and serve it.
    let dest = tempfile::tempdir().unwrap();
    let forked_root =
        orrery_snapshot::fork(&inst.dir.path().join("snapshots"), "seed", dest.path()).unwrap();
    let fork = boot_at(dest, &forked_root).await;

    // Alice's derived token works in the fork (same world lineage), and the
    // repo clones from the fork's own URL.
    let fork_url = format!(
        "{}/alice/webapp.git",
        fork.base
            .replace("http://", &format!("http://x-access-token:{alice}@"))
    );
    let dst = work.path().join("fork-clone");
    git(
        work.path(),
        clock,
        &["clone", &fork_url, dst.to_str().unwrap()],
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("README.md")).unwrap(),
        "# webapp\n"
    );

    // Divergence is local: a push to the fork moves the fork's main only.
    std::fs::write(dst.join("fork.txt"), "fork\n").unwrap();
    git(&dst, clock, &["add", "."]);
    git(&dst, clock, &["commit", "-m", "fork-only"]);
    git(&dst, clock, &["push", &fork_url, "main"]);
    let fork_row = fork
        .world
        .repo_by_owner_name("alice", "webapp")
        .unwrap()
        .unwrap();
    assert_eq!(fork_row.pushed_at_clock_unix, Some(clock));
    let origin_clone = work.path().join("origin-clone");
    git(
        work.path(),
        clock,
        &["clone", &authed_url, origin_clone.to_str().unwrap()],
    );
    assert!(
        !origin_clone.join("fork.txt").exists(),
        "the origin never saw the fork's push"
    );
}
