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
