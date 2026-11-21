//! End-to-end: a check run's body, held to what github.com answered.
//!
//! `fixtures/check-runs-github.json` holds requests that were sent to
//! github.com and what it answered to each: every fault of a body's shape,
//! what it keeps of each field, and what an update makes of a check run in
//! each state. The same requests are sent to a world, which must answer
//! the same.

mod common;

use common::{boot, face_ok, git, mint_actor};
use serde_json::{Value, json};

/// A request to the face with a body of these bytes: its status and its
/// own body.
fn send(method: &str, url: &str, token: &str, body: &[u8]) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let request = match method {
        "POST" => agent.post(url),
        _ => agent.patch(url),
    };
    let mut answer = request
        .header("authorization", format!("token {token}"))
        .header("content-type", "application/json")
        .send(body)
        .expect("the face answers");
    let status = answer.status().as_u16();
    (
        status,
        answer.body_mut().read_to_string().unwrap_or_default(),
    )
}

/// A fixture's text with `{rep:x:3}` written out as `xxx`.
fn repeated(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("{rep:") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 5..];
        let end = after.find('}').expect("a repeat ends");
        let (letter, count) = after[..end].rsplit_once(':').expect("a repeat has a count");
        out.push_str(&letter.repeat(count.parse().expect("a repeat's count")));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_check_run_body_is_answered_as_github_answered_it() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bot = mint_actor(&base, "ci", "bot");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        json!({ "name": "web" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{alice}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    git(src, clock, &["init", "-b", "main", "."]);
    for (file, message) in [("README.md", "init"), ("more.txt", "Add more")] {
        std::fs::write(src.join(file), "text\n").unwrap();
        git(src, clock, &["add", "."]);
        git(src, clock, &["commit", "-m", message]);
    }
    let sha = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    let parent = git(src, clock, &["rev-parse", "HEAD~1"]).trim().to_string();
    git(src, clock, &["push", &authed_url, "main"]);
    let runs = format!("{base}/api/v3/repos/alice/web/check-runs");

    // The time of a write is the world's clock, which nothing here moves.
    let (status, first) = send(
        "POST",
        &runs,
        &bot,
        json!({ "name": "first", "head_sha": sha })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(status, 201, "{first}");
    let now = serde_json::from_str::<Value>(&first).unwrap()["started_at"]
        .as_str()
        .unwrap()
        .to_string();
    let filled = |text: &str| {
        repeated(text)
            .replace("{SHA20}", &sha[..20].to_uppercase())
            .replace("{SHA}", &sha.to_uppercase())
            .replace("{sha39}", &sha[..39])
            .replace("{sha7}", &sha[..7])
            .replace("{sha4}", &sha[..4])
            .replace("{sha3}", &sha[..3])
            .replace("{sha}", &sha)
            .replace("{parent}", &parent)
            .replace("{now}", &now)
            .replace("{app_url}", &format!("{base}/apps/ci"))
    };
    // Every string of a fixture's value, filled in.
    fn fill(value: &Value, filled: &dyn Fn(&str) -> String) -> Value {
        match value {
            Value::String(text) => Value::String(filled(text)),
            Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, filled)).collect()),
            Value::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(key, v)| (key.clone(), fill(v, filled)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/check-runs-github.json")).unwrap();
    let mut differences = Vec::new();
    let mut steps = 0;
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let mut run_id = None;
        for step in scenario["steps"].as_array().unwrap() {
            steps += 1;
            let label = format!(
                "{} {} ({})",
                step["method"].as_str().unwrap(),
                step["label"].as_str().unwrap(),
                scenario["from"].as_str().unwrap()
            );
            let method = step["method"].as_str().unwrap();
            let url = match (method, run_id) {
                ("POST", _) => runs.clone(),
                (_, Some(id)) => format!("{runs}/{id}"),
                (_, None) => {
                    differences.push(format!("{label}: no check run was made to update"));
                    continue;
                }
            };
            let body = filled(step["body"].as_str().unwrap());
            let (status, answer) = send(method, &url, &bot, body.as_bytes());
            let answered: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
            if method == "POST" {
                run_id = answered["id"].as_i64();
            }
            let wanted = step["status"].as_u64().unwrap() as u16;
            if status != wanted {
                differences.push(format!("{label}: {status}, not {wanted}: {answer:.300}"));
                continue;
            }
            if step.get("empty").is_some() && !answer.is_empty() {
                differences.push(format!(
                    "{label}: a body, where there was none: {answer:.300}"
                ));
            }
            if let Some(message) = step.get("message") {
                let message = fill(message, &filled);
                if answered["message"] != message {
                    differences.push(format!(
                        "{label}: message\n     got {}\n  wanted {message}",
                        answered["message"]
                    ));
                }
                let errors = step.get("errors").map(|errors| fill(errors, &filled));
                if answered.get("errors") != errors.as_ref() {
                    differences.push(format!(
                        "{label}: errors\n     got {:?}\n  wanted {errors:?}",
                        answered.get("errors")
                    ));
                }
            }
            if let Some(run) = step.get("run") {
                for (key, value) in fill(run, &filled).as_object().unwrap() {
                    let got = match key.as_str() {
                        "output" => json!({
                            "title": answered["output"]["title"],
                            "summary": answered["output"]["summary"],
                            "text": answered["output"]["text"],
                        }),
                        _ => answered[key].clone(),
                    };
                    if got != *value {
                        differences.push(format!(
                            "{label}: {key}\n     got {got:.300}\n  wanted {value:.300}"
                        ));
                    }
                }
            }
        }
    }
    assert!(steps > 400, "the fixture holds the requests: {steps}");
    assert!(
        differences.is_empty(),
        "{} of {steps} answers differ from GitHub's:\n{}",
        differences.len(),
        differences.join("\n")
    );
}

/// GitHub's parser reads a body on every route, so a body it takes is one
/// every route takes. Each as github.com answered it on 2026-11-21.
#[tokio::test(flavor = "multi_thread")]
async fn a_body_is_read_as_github_reads_one_on_every_route() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        json!({ "name": "web" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{alice}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    git(src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "text\n").unwrap();
    git(src, clock, &["add", "."]);
    git(src, clock, &["commit", "-m", "init"]);
    let sha = git(src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(src, clock, &["push", &authed_url, "main"]);

    let statuses = format!("{base}/api/v3/repos/alice/web/statuses/{sha}");
    let status = |body: &str| {
        let (status, answer) = send("POST", &statuses, &alice, body.as_bytes());
        (
            status,
            serde_json::from_str::<Value>(&answer).unwrap_or(Value::Null),
        )
    };
    // A body that stops part way is what was open when it stopped.
    let (code, made) = status(r#"{"state": "success", "context": "cut-body""#);
    assert_eq!(
        (code, &made["context"]),
        (201, &json!("cut-body")),
        "{made}"
    );
    // A comment is passed over.
    let (code, made) = status(r#"{"state": "success", /* c */ "context": "comment-body"}"#);
    assert_eq!(
        (code, &made["context"]),
        (201, &json!("comment-body")),
        "{made}"
    );
    // Text that is no value is refused.
    let (code, refused) = status("nonsense");
    assert_eq!(
        (code, &refused["message"]),
        (400, &json!("Problems parsing JSON"))
    );

    let graphql = format!("{base}/api/graphql");
    for body in [
        r#"{"query": "{ __typename }""#,
        r#"{"query": "{ __typename }", /* c */ "variables": {}}"#,
    ] {
        let (code, answer) = send("POST", &graphql, &alice, body.as_bytes());
        let answer: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(
            (code, &answer["data"]["__typename"]),
            (200, &json!("QueryRoot")),
            "{body}: {answer}"
        );
    }
}

/// A revision can read a branch's history: `main@{1}` is what `main` held
/// before its last move, and `main@{yesterday}` what it held then. Each
/// answer here is what github.com gave on 2026-11-21 for a branch moved the
/// same way.
#[tokio::test(flavor = "multi_thread")]
async fn a_revision_reads_a_branchs_history_as_github_reads_it() {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bot = mint_actor(&base, "ci", "bot");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &alice,
        json!({ "name": "web" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{alice}@"));
    // git reads `hist@{+1}` and `hist@{ 1}` as the first of the month. The
    // world starts on a first, so every commit would fall after that day;
    // GitHub was seen on the 21st, where the first is before them all.
    common::control_post(
        &base,
        "/_orrery/clock/advance",
        json!({ "seconds": 10 * 86_400 }),
    );
    let started = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path();
    let later = |seconds: i64| {
        common::control_post(
            &base,
            "/_orrery/clock/advance",
            json!({ "seconds": seconds }),
        );
    };
    git(src, started, &["init", "-b", "main", "."]);
    // Three commits, each pushed to `main` on its own, ten seconds apart.
    let mut commits = Vec::new();
    for (at, file) in ["README.md", "one.txt", "two.txt"].into_iter().enumerate() {
        if at > 0 {
            later(10);
        }
        std::fs::write(src.join(file), "text\n").unwrap();
        git(src, started, &["add", "."]);
        git(src, started, &["commit", "-m", file]);
        commits.push(git(src, started, &["rev-parse", "HEAD"]).trim().to_string());
        git(src, started, &["push", &authed_url, "main"]);
    }
    let (oldest, middle, newest) = (&commits[0], &commits[1], &commits[2]);
    let push = |spec: &str| {
        git(src, started, &["push", "--force", &authed_url, spec]);
    };

    let runs = format!("{base}/api/v3/repos/alice/web/check-runs");
    // What a new check run on `rev` answers: the commit it is on, or none
    // for a `422`.
    let named = |rev: &str| -> Option<String> {
        let body = json!({ "name": "history", "head_sha": rev }).to_string();
        let (status, answer) = send("POST", &runs, &bot, body.as_bytes());
        let answer: Value = serde_json::from_str(&answer).unwrap_or(Value::Null);
        match status {
            201 => Some(answer["head_sha"].as_str().unwrap().to_string()),
            _ => {
                assert_eq!(
                    (status, &answer["message"]),
                    (422, &json!(format!("No commit found for SHA: {rev}"))),
                    "{rev}"
                );
                None
            }
        }
    };
    let holds = |cases: &[(&str, Option<&String>)]| {
        for (rev, wanted) in cases {
            assert_eq!(named(rev).as_ref(), *wanted, "{rev}");
        }
    };
    // A time as text, the way it was sent to GitHub.
    let time = |unix: i64| {
        jiff::Timestamp::from_second(unix)
            .unwrap()
            .strftime("%Y-%m-%d %H:%M:%S +0000")
            .to_string()
    };

    holds(&[
        ("main@{0}", Some(newest)),
        ("main@{1}", Some(middle)),
        ("main@{2}", Some(oldest)),
        ("main@{500}", None),
        ("@{0}", Some(newest)),
        ("@{1}", Some(middle)),
        ("HEAD@{0}", Some(newest)),
        ("HEAD@{1}", Some(middle)),
        // A time before the branch was made reads as what it first held.
        ("main@{yesterday}", Some(oldest)),
        ("main@{1.year.ago}", Some(oldest)),
        ("main@{now}", Some(newest)),
        ("main@{upstream}", None),
        ("main@{push}", None),
        ("@{-1}", None),
        ("refs/heads/main@{0}", Some(newest)),
        ("refs/heads/main@{1}", Some(middle)),
    ]);

    // A second branch, made at the oldest commit and moved twice.
    later(10);
    let made = inst.world.clock().unwrap();
    push(&format!("{oldest}:refs/heads/hist"));
    later(10);
    push(&format!("{middle}:refs/heads/hist"));
    later(10);
    push(&format!("{newest}:refs/heads/hist"));
    holds(&[
        ("hist@{0}", Some(newest)),
        ("hist@{1}", Some(middle)),
        ("hist@{2}", Some(oldest)),
        ("hist@{3}", None),
        ("hist@{4}", None),
        (&format!("hist@{{{}}}", time(made + 5)), Some(oldest)),
        (&format!("hist@{{{}}}", time(made + 15)), Some(middle)),
        ("hist@{1.hour.ago}", Some(oldest)),
        ("hist@{1}~1", Some(oldest)),
        ("hist@{01}", Some(middle)),
        // git reads these two as times, not as counts.
        ("hist@{+1}", Some(oldest)),
        ("hist@{ 1}", Some(oldest)),
        ("hist@{}", None),
        ("hist@{garbage}", None),
        ("hist@{never}", None),
        ("hist@{1}@{0}", None),
        ("refs/heads/hist@{2}", Some(oldest)),
        ("heads/hist@{2}", Some(oldest)),
        ("HIST@{1}", None),
    ]);

    // Forced back, the branch holds a fourth entry, and the one past its
    // first is still nothing.
    later(10);
    push(&format!("{oldest}:refs/heads/hist"));
    holds(&[
        ("hist@{0}", Some(oldest)),
        ("hist@{1}", Some(newest)),
        ("hist@{3}", Some(oldest)),
        ("hist@{4}", None),
    ]);

    // A branch that is deleted loses its history, and one made again under
    // its name starts a new one.
    later(10);
    push(":refs/heads/hist");
    holds(&[("hist@{0}", None), ("hist@{1}", None)]);
    later(10);
    push(&format!("{middle}:refs/heads/hist"));
    holds(&[
        ("hist@{0}", Some(middle)),
        ("hist@{1}", None),
        ("hist@{2}", None),
        ("hist@{1.hour.ago}", Some(middle)),
    ]);

    // A tag has no history, even of what it holds now.
    push(&format!("{middle}:refs/tags/marked"));
    push(&format!("{newest}:refs/tags/marked"));
    holds(&[
        ("marked@{0}", None),
        ("marked@{1}", None),
        ("refs/tags/marked@{1}", None),
    ]);

    // Every read that takes a revision reads a history the same way.
    let api = format!("{base}/api/v3/repos/alice/web");
    let get = |path: &str| common::face_answer("GET", &format!("{api}{path}"), &alice, &[], None);
    let before = "main%40%7B1%7D";
    let (status, commit) = get(&format!("/commits/{before}"));
    assert_eq!((status, &commit["sha"]), (200, &json!(middle)), "{commit}");
    let (status, file) = get(&format!("/contents/README.md?ref={before}"));
    assert_eq!(
        (status, &file["path"]),
        (200, &json!("README.md")),
        "{file}"
    );
    let (status, absent) = get(&format!("/contents/two.txt?ref={before}"));
    assert_eq!((status, &absent["message"]), (404, &json!("Not Found")));
    let (status, combined) = get(&format!("/commits/{before}/status"));
    assert_eq!(
        (status, &combined["sha"]),
        (200, &json!(middle)),
        "{combined}"
    );
    let (status, listed) = get(&format!("/commits/{before}/check-runs"));
    // The check runs above share one name, and the list keeps its newest.
    assert_eq!(
        (status, &listed["total_count"]),
        (200, &json!(1)),
        "{listed}"
    );
    assert_eq!(listed["check_runs"][0]["head_sha"], json!(middle));
    let (status, compared) = get(&format!("/compare/{before}...main"));
    assert_eq!(
        (status, &compared["ahead_by"]),
        (200, &json!(1)),
        "{compared}"
    );

    // A history that names nothing is answered as any revision that names
    // nothing is, but for the contents of a file, where GitHub fails on a
    // history git itself gives up on.
    for (rev, contents) in [
        ("nosuch", (404, "No commit found for the ref nosuch")),
        ("main%40%7B500%7D", (500, "Internal Server Error")),
        ("main%40%7Bnever%7D", (500, "Internal Server Error")),
        ("main%40%7Bupstream%7D", (500, "Internal Server Error")),
        (
            "main%40%7Bgarbage%7D",
            (404, "No commit found for the ref main@{garbage}"),
        ),
        (
            "main%40%7B-1%7D",
            (404, "No commit found for the ref main@{-1}"),
        ),
        (
            "nosuch%40%7B0%7D",
            (404, "No commit found for the ref nosuch@{0}"),
        ),
        (
            "marked%40%7B0%7D",
            (404, "No commit found for the ref marked@{0}"),
        ),
    ] {
        let (status, answer) = get(&format!("/contents/README.md?ref={rev}"));
        assert_eq!(
            (status, answer["message"].as_str().unwrap()),
            contents,
            "{rev}"
        );
        let (status, _) = get(&format!("/commits/{rev}/status"));
        assert_eq!(status, 404, "{rev}");
        let (status, _) = get(&format!("/compare/{rev}...main"));
        assert_eq!(status, 404, "{rev}");
    }
}
