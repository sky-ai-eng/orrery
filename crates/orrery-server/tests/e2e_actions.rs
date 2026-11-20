//! End-to-end: a workflow run driven through the control plane, as GitHub's
//! orchestrator and runners drive one, and read through the GitHub face.

mod common;

use common::{boot, face, face_answer, face_ok, fault, faults, git, mint_actor};
use serde_json::{Value, json};

struct Seeded {
    inst: common::Instance,
    token: String,
    api: String,
    main_sha: String,
    feature_sha: String,
    _work: tempfile::TempDir,
}

/// alice/web with `main` and a one-commit `feature` branch, and an open
/// pull request for it.
async fn seed() -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let token = mint_actor(&base, "alice", "user");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        json!({ "name": "web" }),
    );
    let clone_url = repo["clone_url"].as_str().unwrap().to_string();
    let authed_url = clone_url.replace("http://", &format!("http://x-access-token:{token}@"));
    let clock = inst.world.clock().unwrap();
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    std::fs::create_dir(&src).unwrap();
    git(&src, clock, &["init", "-b", "main", "."]);
    std::fs::write(src.join("README.md"), "# web\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(&src, clock, &["commit", "-m", "init"]);
    let main_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["checkout", "-b", "feature"]);
    std::fs::write(src.join("feature.txt"), "v1\n").unwrap();
    git(&src, clock, &["add", "."]);
    git(
        &src,
        clock,
        &["commit", "-m", "Add the feature\n\nWith a body."],
    );
    let feature_sha = git(&src, clock, &["rev-parse", "HEAD"]).trim().to_string();
    git(&src, clock, &["push", &authed_url, "main", "feature"]);
    let api = format!("{base}/api/v3/repos/alice/web");
    face_ok(
        "POST",
        &format!("{api}/pulls"),
        &token,
        json!({ "title": "A feature", "head": "feature", "base": "main" }),
    );
    Seeded {
        inst,
        token,
        api,
        main_sha,
        feature_sha,
        _work: work,
    }
}

impl Seeded {
    /// A control-plane call under the repository: its status and its body.
    fn control(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = format!("{}/_orrery/repos/alice/web{path}", self.inst.base);
        let mut resp = match (method, body) {
            ("GET", _) => agent.get(&url).call(),
            (_, Some(body)) => agent.post(&url).send_json(body),
            (_, None) => agent.post(&url).send_empty(),
        }
        .unwrap();
        let status = resp.status().as_u16();
        (status, resp.body_mut().read_json().unwrap())
    }

    /// A control-plane write that answers the run.
    fn runner(&self, path: &str, body: Option<Value>) -> Value {
        let (status, run) = self.control("POST", path, body);
        assert!(status == 200 || status == 201, "{path}: {status} {run}");
        run
    }

    fn get(&self, path: &str) -> Value {
        face_ok(
            "GET",
            &format!("{}/{path}", self.api),
            &self.token,
            Value::Null,
        )
    }

    fn status(&self, path: &str) -> u16 {
        face_answer(
            "GET",
            &format!("{}/{path}", self.api),
            &self.token,
            &[],
            None,
        )
        .0
    }
}

/// The run's job of this name, as a runner route answered the run.
fn job_id(run: &Value, name: &str) -> i64 {
    run["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["name"] == name)
        .unwrap_or_else(|| panic!("no job {name} in {run}"))["id"]
        .as_i64()
        .unwrap()
}

fn ci() -> Value {
    json!({
        "path": ".github/workflows/ci.yml",
        "name": "ci",
        "jobs": [
            { "name": "lint", "steps": [{ "name": "Say hello" }] },
            { "name": "test", "runs_on": ["self-hosted", "linux"], "steps": [
                { "name": "Run tests" },
                { "name": "Never reached on failure" },
                { "name": "Dump logs", "always": true },
            ] },
            { "name": "report", "needs": ["lint", "test"], "always": true },
            { "name": "build", "needs": ["lint", "test"] },
        ],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_driven_by_its_runners_and_read_as_github_serves_it() {
    let s = seed().await;
    let base = &s.inst.base;

    // A workflow is registered with its jobs, and read back as given.
    let (status, workflow) = s.control("POST", "/workflows", Some(ci()));
    assert_eq!(status, 201, "{workflow}");
    let workflow_id = workflow["id"].as_i64().unwrap();
    let (_, read) = s.control("GET", &format!("/workflows/{workflow_id}"), None);
    assert_eq!(read, workflow);
    assert_eq!(
        workflow["jobs"][1]["steps"][2],
        json!({ "name": "Dump logs", "always": true })
    );
    assert_eq!(workflow["jobs"][0]["runs_on"], json!(["ubuntu-latest"]));
    let (status, again) = s.control("POST", "/workflows", Some(ci()));
    assert_eq!(
        (status, faults(&again)),
        (409, vec![fault("ALREADY_EXISTS", Some("path"))])
    );

    let listed = s.get("actions/workflows");
    assert_eq!(listed["total_count"], 1);
    let served = &listed["workflows"][0];
    assert_eq!(served["id"], workflow_id);
    assert_eq!(served["name"], "ci");
    assert_eq!(served["path"], ".github/workflows/ci.yml");
    assert_eq!(served["state"], "active");
    assert_eq!(
        served["html_url"],
        format!("{base}/alice/web/blob/main/.github/workflows/ci.yml").as_str()
    );
    assert_eq!(&s.get(&format!("actions/workflows/{workflow_id}")), served);
    assert_eq!(&s.get("actions/workflows/ci.yml"), served);
    assert_eq!(s.status("actions/workflows/nope.yml"), 404);
    assert_eq!(s.status("actions/workflows/999"), 404);

    // A push run starts with the jobs that need nothing queued, titled by
    // its commit's subject.
    let runs = format!("/workflows/{workflow_id}/runs");
    let start = |event: &str, sha: &str, branch: &str| json!({ "event": event, "head_sha": sha, "head_branch": branch, "actor": "alice" });
    let (status, run) = s.control(
        "POST",
        &runs,
        Some(start("push", &s.feature_sha, "feature")),
    );
    assert_eq!(status, 201, "{run}");
    let run_id = run["id"].as_i64().unwrap();
    assert_eq!(run["title"], "Add the feature");
    assert_eq!(run["status"], "queued");
    let (lint, test) = (job_id(&run, "lint"), job_id(&run, "test"));
    assert_eq!(run["jobs"].as_array().unwrap().len(), 2);

    let served = s.get(&format!("actions/runs/{run_id}"));
    assert_eq!(served["name"], "ci");
    assert_eq!(served["path"], ".github/workflows/ci.yml");
    assert_eq!(served["display_title"], "Add the feature");
    assert_eq!(served["event"], "push");
    assert_eq!(
        (&served["status"], &served["conclusion"]),
        (&json!("queued"), &Value::Null)
    );
    assert_eq!(
        (&served["run_number"], &served["run_attempt"]),
        (&json!(1), &json!(1))
    );
    assert_eq!(served["workflow_id"], workflow_id);
    assert_eq!(served["head_branch"], "feature");
    assert_eq!(served["head_sha"], s.feature_sha.as_str());
    assert_eq!(served["head_commit"]["id"], s.feature_sha.as_str());
    assert_eq!(
        served["head_commit"]["message"],
        "Add the feature\n\nWith a body."
    );
    assert_eq!(served["actor"]["login"], "alice");
    assert_eq!(served["triggering_actor"]["login"], "alice");
    assert_eq!(served["pull_requests"][0]["number"], 1);
    assert_eq!(served["repository"]["full_name"], "alice/web");
    assert_eq!(
        served["html_url"],
        format!("{base}/alice/web/actions/runs/{run_id}").as_str()
    );
    assert_eq!(
        served["jobs_url"],
        format!("{}/actions/runs/{run_id}/jobs", s.api).as_str()
    );
    assert!(served["previous_attempt_url"].is_null());
    let without = s.get(&format!("actions/runs/{run_id}?exclude_pull_requests=true"));
    assert_eq!(without["pull_requests"], json!([]));

    // The run's suite is its own, and GitHub's Actions App's.
    let suite = s.get(&format!("check-suites/{}", served["check_suite_id"]));
    assert_eq!(suite["app"]["slug"], "github-actions");
    assert_eq!(suite["app"]["id"], 15368);
    assert_eq!(suite["head_branch"], "feature");
    assert_eq!(suite["runs_rerequestable"], false);
    assert_eq!(suite["latest_check_runs_count"], 2);

    // A queued job lists no steps, names no runner, and says it started
    // when it was listed.
    let jobs = s.get(&format!("actions/runs/{run_id}/jobs"));
    assert_eq!(jobs["total_count"], 2);
    let queued = &jobs["jobs"][1];
    assert_eq!(
        (&queued["id"], &queued["name"]),
        (&json!(test), &json!("test"))
    );
    assert_eq!(queued["status"], "queued");
    assert_eq!(queued["steps"], json!([]));
    assert_eq!(queued["started_at"], queued["created_at"]);
    assert!(queued["runner_id"].is_null() && queued["runner_name"].is_null());
    assert_eq!(queued["labels"], json!(["self-hosted", "linux"]));
    assert_eq!(
        (&queued["run_id"], &queued["run_attempt"]),
        (&json!(run_id), &json!(1))
    );
    assert_eq!(queued["workflow_name"], "ci");
    assert_eq!(queued["head_branch"], "feature");

    // A runner takes the job: its steps are listed, "Set up job" first and
    // in progress.
    let run = s.runner(&format!("/workflow-jobs/{test}/start"), None);
    assert_eq!(run["status"], "in_progress");
    let job = s.get(&format!("actions/jobs/{test}"));
    assert_eq!(job["status"], "in_progress");
    let steps: Vec<(&str, &str)> = job["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["name"].as_str().unwrap(),
                step["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("Set up job", "in_progress"),
            ("Run tests", "pending"),
            ("Never reached on failure", "pending"),
            ("Dump logs", "pending"),
        ]
    );
    assert_eq!(job["steps"][0]["number"], 1);
    assert!(job["steps"][1]["started_at"].is_null());
    assert_eq!(job["runner_group_name"], "GitHub Actions");
    assert!(
        job["runner_name"]
            .as_str()
            .unwrap()
            .starts_with("GitHub Actions ")
    );

    // The job is a check run of the Actions App, and both link to the job.
    let check = s.get(&format!("check-runs/{test}"));
    let page = format!("{base}/alice/web/actions/runs/{run_id}/job/{test}");
    assert_eq!(check["app"]["slug"], "github-actions");
    assert_eq!(check["html_url"], page.as_str());
    assert_eq!(check["details_url"], page.as_str());
    assert_eq!(job["html_url"], page.as_str());
    assert_eq!(check["check_suite"]["id"], served["check_suite_id"]);

    // The job fails at its second step: the next is skipped, and the one
    // that runs regardless is in progress.
    let step = format!("/workflow-jobs/{test}/complete-step");
    s.runner(&step, Some(json!({ "conclusion": "success" })));
    let run = s.runner(&step, Some(json!({ "conclusion": "failure" })));
    let conclusions: Vec<&Value> = run["jobs"][1]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| &step["conclusion"])
        .collect();
    assert_eq!(
        conclusions,
        [
            &json!("success"),
            &json!("failure"),
            &json!("skipped"),
            &Value::Null
        ]
    );
    assert_eq!(run["jobs"][1]["status"], "in_progress");

    // Its last step finishes it, with "Complete job" listed. Lint has not
    // finished, so nothing that needs both is listed.
    let run = s.runner(&step, Some(json!({ "conclusion": "success" })));
    assert_eq!(run["jobs"][1]["conclusion"], "failure");
    assert_eq!(run["jobs"][1]["steps"][4]["name"], "Complete job");
    assert_eq!(run["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(run["status"], "queued");

    // Lint finishes outright. The job that runs regardless is queued and
    // the other is listed as skipped, with no steps and no runner.
    s.runner(&format!("/workflow-jobs/{lint}/start"), None);
    let run = s.runner(
        &format!("/workflow-jobs/{lint}/complete"),
        Some(json!({ "conclusion": "success" })),
    );
    assert_eq!(run["status"], "queued");
    let report = job_id(&run, "report");
    let jobs = s.get(&format!("actions/runs/{run_id}/jobs"));
    assert_eq!(jobs["total_count"], 4);
    let skipped = &jobs["jobs"][3];
    assert_eq!(
        (&skipped["name"], &skipped["status"]),
        (&json!("build"), &json!("completed"))
    );
    assert_eq!(skipped["conclusion"], "skipped");
    assert_eq!(skipped["steps"], json!([]));
    assert_eq!(skipped["started_at"], skipped["completed_at"]);
    assert!(skipped["runner_id"].is_null());

    // The run ends when its last job does, as a failure.
    s.runner(&format!("/workflow-jobs/{report}/start"), None);
    let run = s.runner(
        &format!("/workflow-jobs/{report}/complete"),
        Some(json!({ "conclusion": "success" })),
    );
    assert_eq!(
        (&run["status"], &run["conclusion"]),
        (&json!("completed"), &json!("failure"))
    );
    let served = s.get(&format!("actions/runs/{run_id}"));
    assert_eq!(
        (&served["status"], &served["conclusion"]),
        (&json!("completed"), &json!("failure"))
    );

    // A job moves only as its state allows, and only a job is one.
    let finished = format!("/workflow-jobs/{report}");
    for (path, body) in [
        (format!("{finished}/start"), None),
        (
            format!("{finished}/complete"),
            Some(json!({ "conclusion": "success" })),
        ),
        (
            format!("{finished}/complete-step"),
            Some(json!({ "conclusion": "success" })),
        ),
    ] {
        let (status, answer) = s.control("POST", &path, body);
        assert_eq!(
            (status, faults(&answer)),
            (400, vec![fault("FAILED_PRECONDITION", None)])
        );
    }
    let (status, _) = s.control("POST", "/workflow-jobs/999/start", None);
    assert_eq!(status, 404);

    // A pull request's run is titled by the pull request. A second run of
    // the workflow is numbered after the first.
    let (status, second) = s.control(
        "POST",
        &runs,
        Some(start("pull_request", &s.feature_sha, "feature")),
    );
    assert_eq!(status, 201, "{second}");
    assert_eq!(
        (&second["title"], &second["run_number"]),
        (&json!("A feature"), &json!(2))
    );

    // The list is newest first, and each filter keeps what matches.
    let count = |query: &str| s.get(&format!("actions/runs{query}"))["total_count"].clone();
    let all = s.get("actions/runs");
    assert_eq!(all["total_count"], 2);
    assert_eq!(all["workflow_runs"][0]["id"], second["id"]);
    assert_eq!(count("?event=push"), 1);
    assert_eq!(count("?status=failure"), 1);
    assert_eq!(count("?status=completed"), 1);
    assert_eq!(count("?status=queued"), 1);
    assert_eq!(count("?status=bogus"), 0);
    assert_eq!(count("?branch=feature"), 2);
    assert_eq!(count("?branch=main"), 0);
    assert_eq!(count("?actor=alice"), 2);
    assert_eq!(count("?actor=nobody"), 0);
    assert_eq!(count(&format!("?head_sha={}", s.feature_sha)), 2);
    assert_eq!(count(&format!("?head_sha={}", &s.feature_sha[..7])), 0);
    assert_eq!(
        count(&format!("?check_suite_id={}", served["check_suite_id"])),
        1
    );
    assert_eq!(count("?unknown=1&per_page=500"), 2);
    assert_eq!(
        s.get("actions/workflows/ci.yml/runs?event=pull_request")["total_count"],
        1
    );

    // A page links to the next at the repository's id, with the query as
    // asked, and that path serves it.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let paged = agent
        .get(format!("{}/actions/runs?per_page=1&branch=feature", s.api))
        .header("authorization", format!("token {}", s.token))
        .call()
        .unwrap();
    let repo_id = face_ok("GET", &s.api, &s.token, Value::Null)["id"]
        .as_i64()
        .unwrap();
    let at = format!("{base}/api/v3/repositories/{repo_id}/actions/runs?per_page=1&branch=feature");
    assert_eq!(
        paged.headers().get("link").unwrap().to_str().unwrap(),
        format!("<{at}&page=2>; rel=\"next\", <{at}&page=2>; rel=\"last\"")
    );
    let next = face_ok("GET", &format!("{at}&page=2"), &s.token, Value::Null);
    assert_eq!(next["workflow_runs"][0]["id"], run_id);

    // What doesn't exist is 404, and a check run that is no job is no job.
    for missing in [
        "actions/runs/999",
        "actions/runs/999/jobs",
        "actions/jobs/999",
    ] {
        assert_eq!(s.status(missing), 404, "{missing}");
    }
    let ci_token = mint_actor(base, "ci", "bot");
    let plain = face_ok(
        "POST",
        &format!("{}/check-runs", s.api),
        &ci_token,
        json!({ "name": "external", "head_sha": s.feature_sha }),
    );
    assert_eq!(s.status(&format!("actions/jobs/{}", plain["id"])), 404);

    // A job's check run is its runner's alone to move: another App's bot is
    // refused as GitHub refuses one, naming the Actions App. And that App's
    // id finds its check runs: the first run's four jobs and the second's
    // two.
    let (status, refused) = face_answer(
        "PATCH",
        &format!("{}/check-runs/{test}", s.api),
        &ci_token,
        &[],
        Some(json!({ "conclusion": "success" })),
    );
    assert_eq!(status, 403);
    assert_eq!(
        refused["message"],
        "Invalid app_id `15368` - check run can only be modified by the GitHub App that created it."
    );
    let of_actions = s.get(&format!(
        "commits/{}/check-runs?app_id=15368&filter=all",
        s.feature_sha
    ));
    assert_eq!(of_actions["total_count"], 6);
    let (status, _) = s.control(
        "POST",
        &format!("/workflow-jobs/{}/start", plain["id"]),
        None,
    );
    assert_eq!(status, 404);
    assert_eq!(
        face(
            "GET",
            &format!("{}/actions/runs/{run_id}/logs", s.api),
            &s.token,
            Value::Null
        ),
        Err(501),
        "logs are not served yet, and say so"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_or_a_run_that_cannot_be_is_refused_by_field() {
    let s = seed().await;
    let refused = |path: &str, body: Value| {
        let (status, answer) = s.control("POST", path, Some(body));
        assert_eq!(status, 400, "{answer}");
        faults(&answer)
    };
    let invalid = |field: &str| fault("INVALID_ARGUMENT", Some(field));

    // A workflow's path, names, and needs.
    let mut bad = ci();
    bad["path"] = json!("ci.yml");
    bad["name"] = json!(" ");
    bad["jobs"][0]["name"] = json!("");
    bad["jobs"][1]["steps"][0]["name"] = json!("");
    assert_eq!(
        refused("/workflows", bad),
        [
            invalid("path"),
            invalid("name"),
            invalid("jobs[0].name"),
            invalid("jobs[1].steps[0].name"),
            invalid("jobs[2].needs[0]"),
            invalid("jobs[3].needs[0]"),
        ]
    );
    let mut cycle = ci();
    cycle["jobs"][0]["needs"] = json!(["report"]);
    assert_eq!(
        refused("/workflows", cycle),
        [
            invalid("jobs[0].needs"),
            invalid("jobs[2].needs"),
            invalid("jobs[3].needs")
        ]
    );
    let mut unknown = ci();
    unknown["jobs"][0]["if"] = json!("always()");
    assert_eq!(refused("/workflows", unknown), [invalid("jobs")]);
    let mut empty = ci();
    empty["jobs"] = json!([]);
    assert_eq!(refused("/workflows", empty), [invalid("jobs")]);
    let (status, _) = s.control("GET", "/workflows/999", None);
    assert_eq!(status, 404);

    // A run's event, commit, pull request, and actor.
    let (_, workflow) = s.control("POST", "/workflows", Some(ci()));
    let runs = format!("/workflows/{}/runs", workflow["id"]);
    let start = |event: &str, sha: &str, actor: &str| json!({ "event": event, "head_sha": sha, "head_branch": "feature", "actor": actor });
    assert_eq!(
        refused(&runs, start("workflow_dispatch", "abc", "alice")),
        [invalid("event"), invalid("head_sha")]
    );
    assert_eq!(
        refused(&runs, start("push", &"1".repeat(40), "alice")),
        [invalid("head_sha")]
    );
    assert_eq!(
        refused(&runs, start("pull_request", &s.main_sha, "alice")),
        [invalid("head_sha")]
    );
    assert_eq!(
        refused(&runs, start("push", &s.feature_sha, "nobody")),
        [invalid("actor")]
    );
    let (status, _) = s.control(
        "POST",
        "/workflows/999/runs",
        Some(start("push", &s.feature_sha, "alice")),
    );
    assert_eq!(status, 404);
    assert_eq!(
        s.get("actions/runs")["total_count"],
        0,
        "nothing refused started a run"
    );

    // A scheduled run is titled by its workflow.
    let (status, run) = s.control("POST", &runs, Some(start("schedule", &s.main_sha, "alice")));
    assert_eq!((status, &run["title"]), (201, &json!("ci")));
}
