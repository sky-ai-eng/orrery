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
    seed_as(false).await
}

/// The same, in a repository that is private when `private` is set.
async fn seed_as(private: bool) -> Seeded {
    let inst = boot().await;
    let base = inst.base.clone();
    let token = mint_actor(&base, "alice", "user");
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        json!({ "name": "web", "private": private }),
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

    /// The run as the control plane's routes answer it, read without
    /// moving it: its jobs of the latest attempt, from the face.
    fn runner_state(&self, run_id: i64) -> Value {
        let run = self.get(&format!("actions/runs/{run_id}"));
        let jobs = self.get(&format!("actions/runs/{run_id}/jobs?per_page=100"));
        json!({ "id": run_id, "attempt": run["run_attempt"], "jobs": jobs["jobs"] })
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
    // A filter GitHub has and a world does not is refused by name.
    let (status, unserved) = face_answer(
        "GET",
        &format!("{}/actions/runs?created=%3E2026-01-01", s.api),
        &s.token,
        &[],
        None,
    );
    assert_eq!(
        (status, &unserved["orrery"]["field"]),
        (422, &json!("created"))
    );
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
            &format!("{}/actions/runs/{run_id}/artifacts", s.api),
            &s.token,
            Value::Null
        ),
        Err(501),
        "artifacts are not served, and say so"
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
    // Every name that names nothing is reported, not the first.
    let mut lost = start("push", &"1".repeat(40), "nobody");
    lost["head_branch"] = json!("mian");
    assert_eq!(
        refused(&runs, lost),
        [
            invalid("actor"),
            invalid("head_sha"),
            invalid("head_branch")
        ]
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

#[tokio::test(flavor = "multi_thread")]
async fn graphql_reads_a_jobs_steps_and_the_run_its_suite_is() {
    let s = seed().await;
    let (_, workflow) = s.control("POST", "/workflows", Some(ci()));
    let run = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "pull_request", "head_sha": s.feature_sha, "head_branch": "feature",
            "actor": "alice",
        })),
    );
    let test = job_id(&run, "test");
    s.runner(&format!("/workflow-jobs/{test}/start"), None);
    s.runner(
        &format!("/workflow-jobs/{test}/complete-step"),
        Some(json!({ "conclusion": "success" })),
    );
    s.inst
        .world
        .advance_clock(
            7,
            orrery_world::Attribution::new(
                orrery_world::SYSTEM_ACTOR_ID,
                orrery_world::Door::Control,
            ),
        )
        .unwrap();
    s.runner(
        &format!("/workflow-jobs/{test}/complete-step"),
        Some(json!({ "conclusion": "failure" })),
    );
    // A check run that is no job, on the same commit.
    let ci_token = mint_actor(&s.inst.base, "ci", "bot");
    face_ok(
        "POST",
        &format!("{}/check-runs", s.api),
        &ci_token,
        json!({ "name": "external", "head_sha": s.feature_sha }),
    );

    let query = r#"
        query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest {
            commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 50) { nodes {
                __typename
                ... on CheckRun {
                    databaseId name status detailsUrl
                    checkSuite {
                        databaseId status
                        app { slug databaseId }
                        workflowRun { databaseId runNumber event url workflow { name } }
                    }
                    steps(first: 50) {
                        totalCount
                        nodes { number name status conclusion startedAt completedAt secondsToCompletion }
                    }
                }
            } } } } } }
        } } }"#;
    let answer: Value = ureq::post(format!("{}/api/graphql", s.inst.base))
        .header("authorization", &format!("token {}", s.token))
        .send_json(json!({ "query": query, "variables": { "ids": ["O_pull_1"] } }))
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert!(answer.get("errors").is_none(), "{answer}");
    let contexts = answer["data"]["nodes"][0]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
        ["contexts"]["nodes"]
        .as_array()
        .unwrap()
        .clone();
    let named = |name: &str| {
        contexts
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("no {name} in {contexts:?}"))
            .clone()
    };

    // The job names its run through its suite, and its page.
    let job = named("test");
    assert_eq!(job["status"], "IN_PROGRESS");
    assert_eq!(
        job["detailsUrl"],
        format!(
            "{}/alice/web/actions/runs/{}/job/{test}",
            s.inst.base, run["id"]
        )
        .as_str()
    );
    let suite = &job["checkSuite"];
    assert_eq!(suite["databaseId"], run["check_suite_id"]);
    assert_eq!(
        suite["app"],
        json!({ "slug": "github-actions", "databaseId": 15368 })
    );
    assert_eq!(
        suite["workflowRun"],
        json!({
            "databaseId": run["id"],
            "runNumber": 1,
            "event": "pull_request",
            "url": format!("{}/alice/web/actions/runs/{}", s.inst.base, run["id"]),
            "workflow": { "name": "ci" },
        })
    );

    // Its steps are as the runner has brought them.
    let steps = &job["steps"];
    assert_eq!(steps["totalCount"], 4);
    let brief: Vec<(i64, &str, &str, &Value)> = steps["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| {
            (
                step["number"].as_i64().unwrap(),
                step["name"].as_str().unwrap(),
                step["status"].as_str().unwrap(),
                &step["conclusion"],
            )
        })
        .collect();
    assert_eq!(
        brief,
        [
            (1, "Set up job", "COMPLETED", &json!("SUCCESS")),
            (2, "Run tests", "COMPLETED", &json!("FAILURE")),
            (
                3,
                "Never reached on failure",
                "COMPLETED",
                &json!("SKIPPED")
            ),
            (4, "Dump logs", "IN_PROGRESS", &Value::Null),
        ]
    );
    assert_eq!(steps["nodes"][1]["secondsToCompletion"], 7);
    assert!(steps["nodes"][3]["completedAt"].is_null());
    assert!(steps["nodes"][3]["secondsToCompletion"].is_null());

    // A queued job has listed no steps, and a check run that is no job has
    // none and no run.
    assert_eq!(named("lint")["steps"]["totalCount"], 0);
    let plain = named("external");
    assert_eq!(plain["steps"]["nodes"], json!([]));
    assert!(plain["checkSuite"]["workflowRun"].is_null());
    assert!(plain["checkSuite"]["app"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_rerun_cancelled_and_dispatched_through_githubs_api() {
    let s = seed().await;
    let bob = mint_actor(&s.inst.base, "bob", "user");
    let post = |token: &str, path: &str, body: Option<Value>| {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let request = agent
            .post(format!("{}/{path}", s.api))
            .header("authorization", format!("token {token}"));
        let mut resp = match body {
            Some(body) => request.send_json(body),
            None => request.send_empty(),
        }
        .unwrap();
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    };
    let message =
        |answer: (u16, Value)| (answer.0, answer.1["message"].as_str().unwrap().to_string());
    let finish = |run: &Value, name: &str, conclusion: &str| {
        let id = job_id(run, name);
        s.runner(&format!("/workflow-jobs/{id}/start"), None);
        s.runner(
            &format!("/workflow-jobs/{id}/complete"),
            Some(json!({ "conclusion": conclusion })),
        )
    };

    // A workflow that can be dispatched, with one input.
    let mut dispatched = ci();
    dispatched["dispatch_inputs"] = json!(["target"]);
    let (status, workflow) = s.control("POST", "/workflows", Some(dispatched));
    assert_eq!(status, 201, "{workflow}");
    assert_eq!(workflow["dispatch_inputs"], json!(["target"]));
    let mut other = ci();
    other["path"] = json!(".github/workflows/other.yml");
    let (_, undispatched) = s.control("POST", "/workflows", Some(other));
    assert!(undispatched["dispatch_inputs"].is_null());

    // A dispatch starts a run on the branch's tip, started by the caller
    // and titled by the workflow.
    let dispatch = "actions/workflows/ci.yml/dispatches";
    let (status, body) = post(
        &bob,
        dispatch,
        Some(json!({ "ref": "feature", "inputs": { "target": "prod" } })),
    );
    assert_eq!((status, body), (204, Value::String(String::new())));
    let run = s.get("actions/runs")["workflow_runs"][0].clone();
    let run_id = run["id"].as_i64().unwrap();
    assert_eq!(run["event"], "workflow_dispatch");
    assert_eq!(run["display_title"], "ci");
    assert_eq!(run["head_branch"], "feature");
    assert_eq!(run["head_sha"], s.feature_sha.as_str());
    assert_eq!(run["actor"]["login"], "bob");
    assert_eq!(
        s.get(&format!("actions/runs/{run_id}/jobs"))["total_count"],
        2
    );
    // A full ref names the branch too, and a caller that asks is told
    // which run it started.
    let (status, started) = post(
        &bob,
        dispatch,
        Some(json!({ "ref": "refs/heads/main", "return_run_details": true })),
    );
    let newest = s.get("actions/runs")["workflow_runs"][0].clone();
    assert_eq!(newest["head_branch"], "main");
    assert_eq!(
        (status, started),
        (
            200,
            json!({
                "workflow_run_id": newest["id"],
                "run_url": newest["url"],
                "html_url": newest["html_url"],
            })
        )
    );
    // What GitHub refuses, in its words. A ref is a branch's name, not an
    // expression git would resolve.
    for missing in ["nope", "main~1", "main^{tree}"] {
        assert_eq!(
            message(post(&bob, dispatch, Some(json!({ "ref": missing })))),
            (422, format!("No ref found for: {missing}"))
        );
    }
    for (body, said) in [
        (
            json!({ "ref": 5 }),
            "For 'properties/ref', 5 is not a string.",
        ),
        (
            json!({ "ref": "nope", "inputs": null }),
            "For 'properties/inputs', nil is not an object.",
        ),
        (
            json!({ "ref": "main", "inputs": ["x"] }),
            "For 'properties/inputs', [\"x\"] is not an object.",
        ),
        (
            json!({ "ref": "main", "return_run_details": "yes" }),
            "For 'properties/return_run_details', \"yes\" is not a boolean.",
        ),
    ] {
        assert_eq!(
            message(post(&bob, dispatch, Some(body))),
            (422, format!("Invalid request.\n\n{said}"))
        );
    }
    assert_eq!(
        message(post(
            &bob,
            dispatch,
            Some(json!({ "ref": "main", "inputs": { "zzz": "1" } }))
        )),
        (422, "Unexpected inputs provided: [\"zzz\"]".to_string())
    );
    assert_eq!(
        message(post(
            &bob,
            "actions/workflows/other.yml/dispatches",
            Some(json!({ "ref": "main" }))
        )),
        (
            422,
            "Workflow does not have 'workflow_dispatch' trigger".to_string()
        )
    );
    assert_eq!(
        message(post(&bob, dispatch, Some(json!({})))),
        (
            422,
            "Invalid request.\n\n\"ref\" wasn't supplied.".to_string()
        )
    );
    assert_eq!(
        post(
            &bob,
            "actions/workflows/nope.yml/dispatches",
            Some(json!({ "ref": "main" }))
        )
        .0,
        404
    );
    assert_eq!(
        s.get("actions/runs")["total_count"],
        2,
        "nothing refused started a run"
    );

    // A run that has not finished cannot be run again.
    let run_path = format!("actions/runs/{run_id}");
    let state = || s.runner_state(run_id);
    for (path, said) in [
        (
            format!("{run_path}/rerun"),
            "This workflow is already running",
        ),
        (
            format!("{run_path}/rerun-failed-jobs"),
            "This workflow is already running",
        ),
        (
            format!("actions/jobs/{}/rerun", job_id(&state(), "lint")),
            "The workflow run containing this job is already running",
        ),
    ] {
        assert_eq!(
            message(post(&bob, &path, None)),
            (403, said.to_string()),
            "{path}"
        );
    }

    // Attempt 1: lint passes, the tests fail, the report runs, the build
    // is skipped.
    finish(&state(), "lint", "success");
    finish(&state(), "test", "failure");
    finish(&state(), "report", "success");
    let first = s.get(&run_path);
    assert_eq!(
        (&first["status"], &first["conclusion"]),
        (&json!("completed"), &json!("failure"))
    );
    assert_eq!(
        message(post(&bob, &format!("{run_path}/cancel"), None)),
        (
            409,
            "Cannot cancel a workflow run that is completed.".to_string()
        )
    );
    assert_eq!(post(&bob, &format!("{run_path}/force-cancel"), None).0, 409);

    // Re-running what failed begins attempt 2, with lint copied under a
    // new id and the tests queued.
    s.inst
        .world
        .advance_clock(
            30,
            orrery_world::Attribution::new(
                orrery_world::SYSTEM_ACTOR_ID,
                orrery_world::Door::Control,
            ),
        )
        .unwrap();
    assert_eq!(
        post(&s.token, &format!("{run_path}/rerun-failed-jobs"), None),
        (201, json!({}))
    );
    let second = s.get(&run_path);
    assert_eq!(second["run_attempt"], 2);
    assert_eq!(second["status"], "queued");
    assert_eq!(second["actor"]["login"], "bob");
    assert_eq!(second["triggering_actor"]["login"], "alice");
    assert_eq!(second["created_at"], first["created_at"]);
    assert_ne!(second["run_started_at"], first["run_started_at"]);
    assert_eq!(second["check_suite_id"], first["check_suite_id"]);
    assert_eq!(
        second["previous_attempt_url"],
        format!("{}/{run_path}/attempts/1", s.api).as_str()
    );
    let latest = s.get(&format!("{run_path}/jobs"));
    let brief = |jobs: &Value| -> Vec<(String, i64, String)> {
        jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| {
                (
                    job["name"].as_str().unwrap().to_string(),
                    job["run_attempt"].as_i64().unwrap(),
                    job["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    // The job that runs is listed before the one carried over.
    assert_eq!(
        brief(&latest),
        [
            ("test".to_string(), 2, "queued".to_string()),
            ("lint".to_string(), 2, "completed".to_string()),
        ]
    );
    assert_eq!(
        s.get(&format!("{run_path}/jobs?filter=all"))["total_count"],
        6
    );

    // The first attempt is read as it ended.
    let kept = s.get(&format!("{run_path}/attempts/1"));
    assert_eq!(kept["run_attempt"], 1);
    assert_eq!(
        (&kept["status"], &kept["conclusion"]),
        (&json!("completed"), &json!("failure"))
    );
    assert_eq!(kept["run_started_at"], first["run_started_at"]);
    assert_eq!(kept["triggering_actor"]["login"], "bob");
    assert!(kept["previous_attempt_url"].is_null());
    let kept_jobs = s.get(&format!("{run_path}/attempts/1/jobs"));
    assert_eq!(kept_jobs["total_count"], 4);
    assert!(
        brief(&kept_jobs)
            .iter()
            .all(|(_, attempt, _)| *attempt == 1)
    );
    assert_eq!(s.get(&format!("{run_path}/attempts/2"))["run_attempt"], 2);
    assert_eq!(s.status(&format!("{run_path}/attempts/3")), 404);
    assert_eq!(s.status(&format!("{run_path}/attempts/3/jobs")), 404);

    // Asked to cancel while the tests run: their step is cancelled, the
    // step that runs regardless goes on, and asking again is taken again.
    let test = job_id(&state(), "test");
    s.runner(&format!("/workflow-jobs/{test}/start"), None);
    s.runner(
        &format!("/workflow-jobs/{test}/complete-step"),
        Some(json!({ "conclusion": "success" })),
    );
    assert_eq!(
        post(&bob, &format!("{run_path}/cancel"), None),
        (202, json!({}))
    );
    assert_eq!(
        post(&bob, &format!("{run_path}/cancel"), None),
        (202, json!({}))
    );
    let job = s.get(&format!("actions/jobs/{test}"));
    let concluded: Vec<&Value> = job["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| &step["conclusion"])
        .collect();
    assert_eq!(
        concluded,
        [
            &json!("success"),
            &json!("cancelled"),
            &json!("skipped"),
            &Value::Null
        ]
    );
    assert_eq!(job["status"], "in_progress");
    // The job ends as cancelled, the report still runs, and the build is
    // listed as cancelled.
    s.runner(
        &format!("/workflow-jobs/{test}/complete-step"),
        Some(json!({ "conclusion": "success" })),
    );
    let after = state();
    let concluded = |name: &str| {
        let jobs = after["jobs"].as_array().unwrap();
        jobs.iter().find(|job| job["name"] == name).unwrap()["conclusion"].clone()
    };
    assert_eq!(concluded("test"), "cancelled");
    assert_eq!(concluded("build"), "cancelled");
    assert_eq!(concluded("report"), Value::Null);
    finish(&after, "report", "success");
    let ended = s.get(&run_path);
    assert_eq!(
        (&ended["status"], &ended["conclusion"]),
        (&json!("completed"), &json!("cancelled"))
    );

    // Re-running one job runs it alone, and only a job of the current
    // attempt can be. Cancelled by force, it ends where it is.
    let old_report = s.get(&format!("{run_path}/attempts/1/jobs"))["jobs"][2]["id"].clone();
    assert_eq!(
        message(post(
            &bob,
            &format!("actions/jobs/{old_report}/rerun"),
            None
        )),
        (
            403,
            "Only jobs from the current attempt can be re-run".to_string()
        )
    );
    let rerun_report = format!("actions/jobs/{}/rerun", job_id(&state(), "report"));
    assert_eq!(
        message(post(
            &bob,
            &rerun_report,
            Some(json!({ "enable_debug_logging": "yes" }))
        )),
        (
            422,
            "Invalid request.\n\nInvalid property /enable_debug_logging: `\"yes\"` is not of \
             type `boolean`."
                .to_string()
        )
    );
    assert_eq!(
        post(
            &bob,
            &rerun_report,
            Some(json!({ "enable_debug_logging": true }))
        ),
        (201, json!({}))
    );
    let third = state();
    assert_eq!(third["attempt"], 3);
    let report = job_id(&third, "report");
    s.runner(&format!("/workflow-jobs/{report}/start"), None);
    assert_eq!(
        post(&bob, &format!("{run_path}/force-cancel"), None),
        (202, json!({}))
    );
    let forced = s.get(&format!("actions/jobs/{report}"));
    assert_eq!(
        (&forced["status"], &forced["conclusion"]),
        (&json!("completed"), &json!("cancelled"))
    );
    assert_eq!(
        forced["steps"][0]["status"], "in_progress",
        "left as it was"
    );
    assert_eq!(s.get(&run_path)["conclusion"], "cancelled");

    // A whole re-run copies nothing, and a run with nothing failed has
    // nothing to retry.
    assert_eq!(
        post(&bob, &format!("{run_path}/rerun"), None),
        (201, json!({}))
    );
    assert_eq!(brief(&s.get(&format!("{run_path}/jobs"))).len(), 2);
    for name in ["lint", "test", "report", "build"] {
        finish(&state(), name, "success");
    }
    assert_eq!(s.get(&run_path)["conclusion"], "success");
    assert_eq!(
        message(post(&bob, &format!("{run_path}/rerun-failed-jobs"), None)),
        (403, "This workflow run cannot be retried".to_string())
    );
    for missing in [
        "actions/runs/999/rerun",
        "actions/runs/999/cancel",
        "actions/jobs/999/rerun",
    ] {
        assert_eq!(post(&bob, missing, None).0, 404, "{missing}");
    }
}

/// One answer, with no redirect followed: its status, the headers a log
/// route's answer is told by, and its body.
struct Answer {
    status: u16,
    location: Option<String>,
    content_type: Option<String>,
    disposition: Option<String>,
    error_code: Option<String>,
    /// What GitHub says the route asks of an installation token.
    accepted: Option<String>,
    link: Option<String>,
    body: Vec<u8>,
}

impl Answer {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn message(&self) -> (u16, String) {
        let body = self.json();
        (self.status, body["message"].as_str().unwrap().to_string())
    }
}

/// `method` on `url`, with `token` when one is given.
fn fetch(method: &str, url: &str, token: Option<&str>) -> Answer {
    send(method, url, token, None)
}

/// `method` on `url` with `body` as JSON when one is given, and no body
/// otherwise.
fn send(method: &str, url: &str, token: Option<&str>, body: Option<Value>) -> Answer {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into();
    let authorized = |request: ureq::RequestBuilder<ureq::typestate::WithoutBody>| match token {
        Some(token) => request.header("authorization", format!("token {token}")),
        None => request,
    };
    let mut resp = match method {
        "GET" => authorized(agent.get(url)).call(),
        "HEAD" => authorized(agent.head(url)).call(),
        "DELETE" => authorized(agent.delete(url)).call(),
        "POST" | "PATCH" => {
            let request = match method {
                "POST" => agent.post(url),
                _ => agent.patch(url),
            };
            let request = match token {
                Some(token) => request.header("authorization", format!("token {token}")),
                None => request,
            };
            match body {
                Some(body) => request.send_json(body),
                None => request.send_empty(),
            }
        }
        other => panic!("no {other} here"),
    }
    .unwrap();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
    };
    Answer {
        status: resp.status().as_u16(),
        location: header("location"),
        content_type: header("content-type"),
        link: header("link"),
        disposition: header("content-disposition"),
        error_code: header("x-ms-error-code"),
        accepted: header("x-accepted-github-permissions"),
        body: resp.body_mut().read_to_vec().unwrap(),
    }
}

/// The files of a zip whose entries are stored: each name with its body.
fn unzip(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    let u16_at = |at: usize| u16::from_le_bytes([zip[at], zip[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(zip[at..at + 4].try_into().unwrap()) as usize;
    let mut files = Vec::new();
    let mut at = 0;
    while zip[at..at + 4] == [0x50, 0x4b, 0x03, 0x04] {
        let (size, name_len, extra) = (u32_at(at + 18), u16_at(at + 26), u16_at(at + 28));
        let name = at + 30;
        let body = name + name_len + extra;
        files.push((
            String::from_utf8(zip[name..name + name_len].to_vec()).unwrap(),
            zip[body..body + size].to_vec(),
        ));
        at = body + size;
    }
    files
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runs_logs_are_read_through_links_and_deleted_with_the_run() {
    let s = seed().await;
    let base = s.inst.base.clone();
    let system =
        orrery_world::Attribution::new(orrery_world::SYSTEM_ACTOR_ID, orrery_world::Door::Control);
    let api = |path: &str| fetch("GET", &format!("{}/{path}", s.api), Some(&s.token));
    // A link is followed with no token: it carries its own authority.
    let follow = |answer: &Answer| fetch("GET", answer.location.as_deref().unwrap(), None);
    let write = |job: i64, lines: &[&str]| {
        s.runner(
            &format!("/workflow-jobs/{job}/log"),
            Some(json!({ "lines": lines })),
        )
    };
    let step = |job: i64, conclusion: &str| {
        s.runner(
            &format!("/workflow-jobs/{job}/complete-step"),
            Some(json!({ "conclusion": conclusion })),
        )
    };
    let finish = |run: &Value, name: &str, conclusion: &str| {
        let id = job_id(run, name);
        s.runner(&format!("/workflow-jobs/{id}/start"), None);
        write(id, &[&format!("{name} ran")]);
        s.runner(
            &format!("/workflow-jobs/{id}/complete"),
            Some(json!({ "conclusion": conclusion })),
        );
        id
    };

    let (_, workflow) = s.control("POST", "/workflows", Some(ci()));
    let run = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "push", "head_sha": s.feature_sha, "head_branch": "feature", "actor": "alice",
        })),
    );
    let run_id = run["id"].as_i64().unwrap();
    let run_path = format!("actions/runs/{run_id}");
    let state = || s.runner_state(run_id);
    let lint = job_id(&run, "lint");

    // A job's log route redirects for any job, to a link on the web host.
    // The link answers nothing until the job finishes.
    let queued = api(&format!("actions/jobs/{lint}/logs"));
    assert_eq!(queued.status, 302);
    assert_eq!(
        queued.content_type.as_deref(),
        Some("text/html;charset=utf-8")
    );
    assert!(queued.body.is_empty());
    let link = queued.location.clone().unwrap();
    assert!(
        link.starts_with(&format!(
            "{base}/_actions-results/jobs/{lint}/job-logs.txt?se="
        )),
        "{link}"
    );
    let nothing = follow(&queued);
    assert_eq!(
        (nothing.status, nothing.error_code.as_deref()),
        (404, Some("BlobNotFound"))
    );
    assert_eq!(nothing.content_type.as_deref(), Some("application/xml"));
    assert!(nothing.text().starts_with(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>BlobNotFound</Code>"
    ));
    assert_eq!(
        fetch(
            "HEAD",
            &format!("{}/actions/jobs/{lint}/logs", s.api),
            Some(&s.token)
        )
        .status,
        303
    );
    // A first attempt has no archive until it finishes.
    assert_eq!(api(&format!("{run_path}/logs")).status, 404);
    assert_eq!(api(&format!("{run_path}/attempts/1/logs")).status, 404);

    // A runner writes to the step it is on. A step's log is there once the
    // step finishes, while the job still runs.
    s.runner(&format!("/workflow-jobs/{lint}/start"), None);
    write(lint, &["Runner ready"]);
    step(lint, "success");
    write(lint, &["lint ok", "no warnings"]);
    let first = follow(&api(&format!("actions/jobs/{lint}/steps/1/logs")));
    assert_eq!(
        (first.status, first.content_type.as_deref()),
        (200, Some("text/plain"))
    );
    // The clock has not moved since the run began, so every stamp is its
    // second.
    let day = s.get(&run_path)["created_at"]
        .as_str()
        .unwrap()
        .trim_end_matches('Z')
        .to_string();
    assert_eq!(
        first.text(),
        format!("\u{feff}{day}.0000001Z Runner ready\n")
    );
    for missing in ["2", "3", "0", "9", "x"] {
        assert_eq!(
            api(&format!("actions/jobs/{lint}/steps/{missing}/logs")).status,
            404,
            "step {missing}"
        );
    }
    assert_eq!(
        follow(&api(&format!("actions/jobs/{lint}/logs"))).status,
        404
    );
    step(lint, "success");

    // The job's log is every line under a byte-order mark, each after a
    // stamp of its own, and a step's log is that step's lines.
    let whole = follow(&api(&format!("actions/jobs/{lint}/logs")));
    assert_eq!(
        whole.text(),
        format!(
            "\u{feff}{day}.0000001Z Runner ready\n{day}.0000002Z lint ok\n\
             {day}.0000003Z no warnings\n"
        )
    );
    assert_eq!(
        follow(&api(&format!("actions/jobs/{lint}/steps/2/logs"))).text(),
        format!("\u{feff}{day}.0000002Z lint ok\n{day}.0000003Z no warnings\n")
    );
    assert_eq!(
        follow(&api(&format!("actions/jobs/{lint}/steps/3/logs"))).text(),
        "\u{feff}"
    );

    // The tests fail, the report runs, and the build is skipped. A job
    // that never ran has a link that answers nothing, and no step logs.
    let test = finish(&state(), "test", "failure");
    finish(&state(), "report", "success");
    let build = job_id(&state(), "build");
    let skipped = api(&format!("actions/jobs/{build}/logs"));
    assert_eq!((skipped.status, follow(&skipped).status), (302, 404));
    assert_eq!(
        api(&format!("actions/jobs/{build}/steps/1/logs")).status,
        404
    );
    // A skipped step has none either: the tests' third step.
    assert_eq!(
        api(&format!("actions/jobs/{test}/steps/3/logs")).status,
        404
    );
    for missing in ["actions/jobs/999/logs", "actions/jobs/999/steps/1/logs"] {
        assert_eq!(api(missing).message(), (404, "Not Found".to_string()));
    }

    // The archive of the attempt: each job that ran, newest first and
    // numbered so, with what the system says of it and a file per step.
    let redirect = api(&format!("{run_path}/logs"));
    assert_eq!(redirect.status, 302);
    let archive = follow(&redirect);
    let suite = s.get(&run_path)["check_suite_id"].as_i64().unwrap();
    assert_eq!(
        (
            archive.status,
            archive.content_type.as_deref(),
            archive.disposition.as_deref()
        ),
        (
            200,
            Some("application/zip"),
            Some(format!("attachment; filename=logs_{suite}.zip").as_str())
        )
    );
    let files = unzip(&archive.body);
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "1_report.txt",
            "report/system.txt",
            "report/1_Set up job.txt",
            "report/2_Complete job.txt",
            "2_test.txt",
            "test/system.txt",
            "test/1_Set up job.txt",
            "test/4_Dump logs.txt",
            "test/5_Complete job.txt",
            "3_lint.txt",
            "lint/system.txt",
            "lint/1_Set up job.txt",
            "lint/2_Say hello.txt",
            "lint/3_Complete job.txt",
        ]
    );
    let file = |name: &str| {
        let (_, body) = files.iter().find(|(n, _)| n == name).unwrap();
        String::from_utf8(body.clone()).unwrap()
    };
    assert_eq!(file("3_lint.txt"), whole.text());
    assert_eq!(
        file("lint/system.txt"),
        format!(
            "{day}.0000000Z Requested labels: ubuntu-latest\n\
             {day}.0000000Z Job defined at: alice/web/.github/workflows/ci.yml@refs/heads/feature\n\
             {day}.0000000Z Waiting for a runner to pick up this job...\n\
             {day}.0000000Z Job is about to start running on the hosted runner: \
             GitHub Actions {}",
            1_000_000_000 + lint
        )
    );
    assert_eq!(
        follow(&api(&format!("{run_path}/attempts/1/logs"))).body,
        archive.body
    );
    assert_eq!(api(&format!("{run_path}/attempts/2/logs")).status, 404);
    assert_eq!(api("actions/runs/999/logs").status, 404);

    // A link answers only as it was signed, and only for a while: ten
    // minutes for text, and a minute for an archive.
    let job_link = api(&format!("actions/jobs/{lint}/logs")).location.unwrap();
    let forged = fetch("GET", &format!("{job_link}0"), None);
    assert_eq!(
        (forged.status, forged.error_code.as_deref()),
        (403, Some("AuthenticationFailed"))
    );
    let other = job_link.replace(&format!("jobs/{lint}/"), &format!("jobs/{test}/"));
    assert_eq!(fetch("GET", &other, None).status, 403);
    let archive_link = redirect.location.clone().unwrap();
    s.inst.world.advance_clock(61, system).unwrap();
    let late = fetch("GET", &archive_link, None);
    assert_eq!((late.status, late.body.len()), (403, 0));
    assert_eq!(fetch("GET", &job_link, None).status, 200);
    s.inst.world.advance_clock(540, system).unwrap();
    assert_eq!(fetch("GET", &job_link, None).status, 403);

    // A re-run carries lint over with its log. The new attempt has an
    // archive from its start, empty until a job of its own finishes, and
    // neither the logs nor the run can be deleted while it runs.
    let rerun = fetch(
        "POST",
        &format!("{}/{run_path}/rerun-failed-jobs", s.api),
        Some(&s.token),
    );
    assert_eq!(rerun.status, 201);
    let carried = job_id(&state(), "lint");
    assert_ne!(carried, lint);
    assert_eq!(
        follow(&api(&format!("actions/jobs/{carried}/logs"))).text(),
        whole.text()
    );
    assert_eq!(
        follow(&api(&format!("actions/jobs/{carried}/steps/2/logs"))).text(),
        file("lint/2_Say hello.txt")
    );
    let early = follow(&api(&format!("{run_path}/logs")));
    assert_eq!((early.status, early.body.len()), (200, 22));
    let delete = |path: &str| fetch("DELETE", &format!("{}/{path}", s.api), Some(&s.token));
    assert_eq!(
        delete(&format!("{run_path}/logs")).message(),
        (
            403,
            "Unable to delete logs while the workflow is running".to_string()
        )
    );
    assert_eq!(
        delete(&run_path).message(),
        (403, "Could not delete the workflow run".to_string())
    );
    finish(&state(), "test", "success");
    finish(&state(), "report", "success");
    finish(&state(), "build", "success");
    // The archive of the second attempt holds the jobs that ran in it, and
    // not the one it carried over.
    let second = unzip(&follow(&api(&format!("{run_path}/logs"))).body);
    let top: Vec<&str> = second
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| !name.contains('/'))
        .collect();
    assert_eq!(top, ["0_build.txt", "1_report.txt", "2_test.txt"]);

    // Deleting the logs: the job's log and the archives are gone from the
    // API, a step's link answers nothing, and the run's time moves.
    let before = s.get(&run_path);
    s.inst.world.advance_clock(30, system).unwrap();
    for _ in 0..2 {
        let deleted = delete(&format!("{run_path}/logs"));
        assert_eq!((deleted.status, deleted.body.len()), (204, 0));
    }
    assert_eq!(api(&format!("actions/jobs/{lint}/logs")).status, 404);
    assert_eq!(api(&format!("actions/jobs/{carried}/logs")).status, 404);
    let gone = api(&format!("actions/jobs/{lint}/steps/1/logs"));
    assert_eq!((gone.status, follow(&gone).status), (302, 404));
    assert_eq!(api(&format!("{run_path}/logs")).status, 404);
    assert_eq!(api(&format!("{run_path}/attempts/1/logs")).status, 404);
    let after = s.get(&run_path);
    assert_ne!(after["updated_at"], before["updated_at"]);
    assert_eq!(after["conclusion"], before["conclusion"]);
    assert_eq!(
        s.get(&format!("actions/jobs/{lint}"))["steps"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // A re-run after the logs were deleted has logs again, in what it runs.
    // The jobs it carries over have the logs of the jobs that ran, which
    // are gone: the job's route is `404`, and a step's link answers
    // nothing. The attempts before it still have no archive.
    let rerun = fetch(
        "POST",
        &format!("{}/actions/jobs/{}/rerun", s.api, job_id(&state(), "build")),
        Some(&s.token),
    );
    assert_eq!(rerun.status, 201);
    let twice = job_id(&state(), "lint");
    assert_ne!(twice, carried);
    assert_eq!(api(&format!("actions/jobs/{twice}/logs")).status, 404);
    let gone = api(&format!("actions/jobs/{twice}/steps/1/logs"));
    let nothing = follow(&gone);
    assert_eq!(
        (gone.status, nothing.status, nothing.error_code.as_deref()),
        (302, 404, Some("BlobNotFound"))
    );
    finish(&state(), "build", "success");
    let build = job_id(&state(), "build");
    assert_eq!(
        follow(&api(&format!("actions/jobs/{build}/logs"))).status,
        200
    );
    let third = unzip(&follow(&api(&format!("{run_path}/logs"))).body);
    assert_eq!(
        third
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| !name.contains('/'))
            .collect::<Vec<_>>(),
        ["0_build.txt"]
    );
    for attempt in [1, 2] {
        assert_eq!(
            api(&format!("{run_path}/attempts/{attempt}/logs")).status,
            404
        );
    }

    // Deleting the run: it, its jobs, their check runs, and its suite are
    // gone, and the commit lists no check run of its.
    let deleted = delete(&run_path);
    assert_eq!((deleted.status, deleted.body.len()), (204, 0));
    for missing in [
        run_path.clone(),
        format!("{run_path}/jobs"),
        format!("{run_path}/attempts/1"),
        format!("{run_path}/logs"),
        format!("actions/jobs/{lint}"),
        format!("actions/jobs/{lint}/logs"),
        format!("check-runs/{lint}"),
        format!("check-suites/{suite}"),
        format!("check-suites/{suite}/check-runs"),
    ] {
        assert_eq!(api(&missing).status, 404, "{missing}");
    }
    assert_eq!(
        s.get(&format!("commits/{}/check-runs?filter=all", s.feature_sha))["total_count"],
        0
    );
    assert_eq!(s.get("actions/runs")["total_count"], 0);
    assert_eq!(delete(&run_path).status, 404);
    assert_eq!(delete(&format!("{run_path}/logs")).status, 404);

    // The workflow's next run does not take the deleted run's number, and
    // its jobs do not take the deleted jobs' ids.
    assert_eq!(run["run_number"], 1);
    let next = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "push", "head_sha": s.feature_sha, "head_branch": "feature", "actor": "alice",
        })),
    );
    assert_eq!(next["run_number"], 2);
    assert!(job_id(&next, "lint") > build);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_runner_writes_only_whole_lines_to_a_job_in_progress() {
    let s = seed().await;
    let (_, workflow) = s.control("POST", "/workflows", Some(ci()));
    let run = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "push", "head_sha": s.main_sha, "head_branch": "main", "actor": "alice",
        })),
    );
    let lint = job_id(&run, "lint");
    let log = format!("/workflow-jobs/{lint}/log");
    // A queued job has no step to write in.
    let (status, body) = s.control("POST", &log, Some(json!({ "lines": ["early"] })));
    assert_eq!(
        (status, faults(&body)),
        (400, vec![fault("FAILED_PRECONDITION", None)])
    );
    s.runner(&format!("/workflow-jobs/{lint}/start"), None);
    let (status, body) = s.control("POST", &log, Some(json!({ "lines": [] })));
    assert_eq!(
        (status, faults(&body)),
        (400, vec![fault("INVALID_ARGUMENT", Some("lines"))])
    );
    let (status, body) = s.control(
        "POST",
        &log,
        Some(json!({ "lines": ["fine", "two\nlines", "a\rreturn"] })),
    );
    assert_eq!(
        (status, faults(&body)),
        (
            400,
            vec![
                fault("INVALID_ARGUMENT", Some("lines[1]")),
                fault("INVALID_ARGUMENT", Some("lines[2]"))
            ]
        )
    );
    let (status, _) = s.control("POST", &log, Some(json!({ "lines": ["fine"] })));
    assert_eq!(status, 200);
    let (status, _) = s.control(
        "POST",
        "/workflow-jobs/999/log",
        Some(json!({ "lines": ["x"] })),
    );
    assert_eq!(status, 404);
}

/// What GitHub was observed to allow an installation token on each route,
/// by the permissions the token holds.
#[tokio::test(flavor = "multi_thread")]
async fn an_installation_token_does_what_its_permissions_allow() {
    // The permissions are asked of a private repository: anyone reads a
    // public one.
    let s = seed_as(true).await;
    let base = s.inst.base.clone();
    // A finished run with one job, to read and to try to write.
    let (_, workflow) = s.control(
        "POST",
        "/workflows",
        Some(json!({
            "path": ".github/workflows/one.yml", "name": "one", "dispatch_inputs": [],
            "jobs": [{ "name": "only" }],
        })),
    );
    let run = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "push", "head_sha": s.main_sha, "head_branch": "main", "actor": "alice",
        })),
    );
    let (run_id, job) = (run["id"].as_i64().unwrap(), job_id(&run, "only"));
    s.runner(&format!("/workflow-jobs/{job}/start"), None);
    s.runner(
        &format!("/workflow-jobs/{job}/complete"),
        Some(json!({ "conclusion": "success" })),
    );
    let suite = s.get(&format!("actions/runs/{run_id}"))["check_suite_id"]
        .as_i64()
        .unwrap();

    // An App that may write both, and tokens of its installation narrowed
    // to less.
    let app = common::control_post(
        &base,
        "/_orrery/apps",
        json!({
            "slug": "ci-app", "name": "CI App", "owner": "alice",
            "callback_urls": ["https://app.test/callback"],
            "permissions": { "actions": "write", "checks": "write", "metadata": "read" },
        }),
    );
    let key = common::app_key();
    common::control_post(
        &base,
        "/_orrery/apps/ci-app/keys",
        json!({ "public_key": key.public_pem }),
    );
    let installation = common::control_post(
        &base,
        "/_orrery/apps/ci-app/installations",
        json!({ "account": "alice" }),
    );
    let issued = |body: Value| {
        let jwt = key.jwt(app["id"].clone(), s.inst.world.clock().unwrap());
        let mut resp = ureq::post(format!(
            "{base}/api/v3/app/installations/{}/access_tokens",
            installation["id"]
        ))
        .header("authorization", format!("Bearer {jwt}"))
        .send_json(body)
        .unwrap();
        let issued: Value = resp.body_mut().read_json().unwrap();
        issued["token"].as_str().unwrap().to_string()
    };
    let issue = |permissions: Option<Value>| match permissions {
        Some(permissions) => issued(json!({ "permissions": permissions })),
        None => issued(json!({})),
    };
    // A token of every permission, narrowed to the repositories named.
    let issue_for = |repositories: Option<Value>| issued(json!({ "repositories": repositories }));
    let all = issue(None);
    let read = issue(Some(json!({ "actions": "read", "checks": "read" })));
    let none = issue(Some(json!({ "metadata": "read" })));
    let ask = |token: &str, method: &str, path: &str, body: Option<Value>| {
        let answer = send(method, &format!("{}/{path}", s.api), Some(token), body);
        (answer.status, answer.accepted.unwrap_or_default())
    };
    let refused = |token: &str, method: &str, path: &str, body: Option<Value>| {
        let answer = send(method, &format!("{}/{path}", s.api), Some(token), body);
        assert_eq!(
            answer.message(),
            (403, "Resource not accessible by integration".to_string()),
            "{method} {path}"
        );
        answer.accepted.unwrap_or_default()
    };

    // A read needs the permission to read, and is refused before the route
    // looks for what its path names.
    let run_path = format!("actions/runs/{run_id}");
    for path in [
        "actions/workflows".to_string(),
        "actions/workflows/one.yml".to_string(),
        "actions/workflows/nope.yml".to_string(),
        "actions/runs".to_string(),
        run_path.clone(),
        "actions/runs/999".to_string(),
        format!("{run_path}/jobs"),
        format!("{run_path}/attempts/1"),
        format!("{run_path}/logs"),
        format!("actions/jobs/{job}"),
        "actions/jobs/999".to_string(),
        format!("actions/jobs/{job}/logs"),
        format!("actions/jobs/{job}/steps/1/logs"),
    ] {
        assert_eq!(refused(&none, "GET", &path, None), "actions=read", "{path}");
    }
    for path in [
        format!("commits/{}/check-runs", s.main_sha),
        format!("commits/{}/check-suites", s.main_sha),
        format!("check-runs/{job}"),
        format!("check-suites/{suite}"),
        "check-suites/999".to_string(),
        format!("check-suites/{suite}/check-runs"),
        format!("check-runs/{job}/annotations"),
    ] {
        assert_eq!(refused(&none, "GET", &path, None), "checks=read", "{path}");
    }
    // Statuses ask for a permission of their own, which this App has not.
    for path in [
        format!("commits/{}/status", s.main_sha),
        format!("commits/{}/statuses", s.main_sha),
        format!("statuses/{}", s.main_sha),
    ] {
        assert_eq!(refused(&all, "GET", &path, None), "statuses=read", "{path}");
    }
    assert_eq!(
        refused(
            &all,
            "POST",
            &format!("statuses/{}", s.main_sha),
            Some(json!({ "state": "success" }))
        ),
        "statuses=write"
    );
    // The one route that looks first: a check run that does not exist, or
    // that is another repository's.
    face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &s.token,
        json!({ "name": "open" }),
    );
    let open = format!("{base}/api/v3/repos/alice/open");
    let elsewhere = format!("{open}/check-runs/{job}");
    for token in [&none, &read] {
        assert_eq!(
            ask(token, "GET", "check-runs/999", None),
            (404, "checks=read".to_string())
        );
        let answer = send("GET", &elsewhere, Some(token), None);
        assert_eq!(
            (answer.status, answer.accepted.as_deref()),
            (404, Some("checks=read"))
        );
    }
    // `HEAD` is a read, refused as one.
    assert_eq!(
        ask(&none, "HEAD", "actions/runs", None),
        (403, "actions=read".to_string())
    );
    assert_eq!(
        ask(&read, "HEAD", "actions/runs", None),
        (200, "actions=read".to_string())
    );
    // The routes that name the repository by its id ask the same.
    let repo_id = send("GET", &s.api, Some(&s.token), None).json()["id"]
        .as_i64()
        .unwrap();
    let by_id = format!("{base}/api/v3/repositories/{repo_id}/actions/runs");
    for (token, status) in [(&none, 403), (&read, 200)] {
        let answer = send("GET", &by_id, Some(token), None);
        assert_eq!(
            (answer.status, answer.accepted.as_deref()),
            (status, Some("actions=read"))
        );
    }

    // A repository is looked for before the permission: one that does not
    // exist is `404` whatever the token holds, by name and by id.
    for url in [
        format!("{base}/api/v3/repos/alice/nope/actions/runs"),
        format!("{base}/api/v3/repositories/999999/actions/runs"),
    ] {
        for token in [&none, &read] {
            let answer = send("GET", &url, Some(token), None);
            assert_eq!(
                (answer.message(), answer.accepted.as_deref()),
                ((404, "Not Found".to_string()), Some("actions=read")),
                "{url}"
            );
        }
    }
    // So is a private repository the token does not reach, which a token
    // narrowed to another repository shows.
    let elsewhere_only = issue_for(Some(json!(["open"])));
    let unreached = send(
        "GET",
        &format!("{}/actions/runs", s.api),
        Some(&elsewhere_only),
        None,
    );
    assert_eq!(
        (unreached.message(), unreached.accepted.as_deref()),
        ((404, "Not Found".to_string()), Some("actions=read"))
    );

    // Anyone reads a public repository, so a token without the permission
    // does, and is still told what the route asks. A write there needs the
    // permission.
    for (path, accepted) in [
        ("actions/runs", "actions=read"),
        ("actions/workflows", "actions=read"),
    ] {
        let answer = send("GET", &format!("{open}/{path}"), Some(&none), None);
        assert_eq!(
            (answer.status, answer.accepted.as_deref()),
            (200, Some(accepted)),
            "{path}"
        );
    }
    let write = send(
        "POST",
        &format!("{open}/actions/runs/999/cancel"),
        Some(&none),
        None,
    );
    assert_eq!(
        (write.message(), write.accepted.as_deref()),
        (
            (403, "Resource not accessible by integration".to_string()),
            Some("actions=write")
        )
    );

    // With the permission to read, each read answers as it does anyone.
    for (path, status, accepted) in [
        ("actions/runs".to_string(), 200, "actions=read"),
        (run_path.clone(), 200, "actions=read"),
        ("actions/runs/999".to_string(), 404, "actions=read"),
        // A redirect to a log names no permission.
        (format!("actions/jobs/{job}/logs"), 302, ""),
        (format!("{run_path}/logs"), 302, ""),
        (format!("{run_path}/attempts/1/logs"), 302, ""),
        (format!("{run_path}/attempts/1/jobs"), 200, "actions=read"),
        (format!("check-runs/{job}"), 200, "checks=read"),
        ("check-suites/999".to_string(), 404, "checks=read"),
        (
            format!("commits/{}/check-runs", s.main_sha),
            200,
            "checks=read",
        ),
    ] {
        assert_eq!(
            ask(&read, "GET", &path, None),
            (status, accepted.to_string()),
            "{path}"
        );
    }

    // A write needs the permission to write, and is refused before the run's
    // state is looked at: a finished run's cancel is not the `409`.
    for (method, path, body) in [
        ("POST", format!("{run_path}/cancel"), None),
        ("POST", format!("{run_path}/force-cancel"), None),
        ("POST", format!("{run_path}/rerun"), None),
        ("POST", format!("{run_path}/rerun-failed-jobs"), None),
        ("POST", format!("actions/jobs/{job}/rerun"), None),
        ("POST", "actions/runs/999/cancel".to_string(), None),
        (
            "POST",
            "actions/workflows/one.yml/dispatches".to_string(),
            Some(json!({ "ref": "main" })),
        ),
        ("DELETE", format!("{run_path}/logs"), None),
        ("DELETE", run_path.clone(), None),
    ] {
        for token in [&read, &none] {
            assert_eq!(
                refused(token, method, &path, body.clone()),
                "actions=write",
                "{method} {path}"
            );
        }
    }
    for (method, path, body) in [
        ("POST", "check-runs".to_string(), json!({})),
        (
            "PATCH",
            "check-runs/999".to_string(),
            json!({ "status": "completed" }),
        ),
        ("PATCH", format!("check-runs/{job}"), json!({ "name": "x" })),
    ] {
        for token in [&read, &none] {
            assert_eq!(
                refused(token, method, &path, Some(body.clone())),
                "checks=write",
                "{method} {path}"
            );
        }
    }
    assert_eq!(
        s.get("actions/runs")["total_count"],
        1,
        "nothing refused ran"
    );

    // With the permission to write, each write answers as it does anyone.
    for (method, path, body, status, accepted) in [
        (
            "POST",
            format!("{run_path}/cancel"),
            None,
            409,
            "actions=write",
        ),
        (
            "POST",
            "actions/runs/999/cancel".to_string(),
            None,
            404,
            "actions=write",
        ),
        (
            "DELETE",
            "actions/runs/999".to_string(),
            None,
            404,
            "actions=write",
        ),
        (
            "POST",
            "actions/workflows/one.yml/dispatches".to_string(),
            Some(json!({ "ref": "nope" })),
            422,
            "actions=write",
        ),
        (
            "PATCH",
            "check-runs/999".to_string(),
            Some(json!({ "status": "completed" })),
            404,
            "checks=write",
        ),
        ("DELETE", run_path.clone(), None, 204, "actions=write"),
    ] {
        assert_eq!(
            ask(&all, method, &path, body),
            (status, accepted.to_string()),
            "{method} {path}"
        );
    }

    // A person's token is told nothing of permissions, and a route the
    // world does not serve says so to any token.
    let person = send(
        "GET",
        &format!("{}/actions/runs", s.api),
        Some(&s.token),
        None,
    );
    assert_eq!((person.status, person.accepted), (200, None));
    assert_eq!(
        ask(&none, "GET", "actions/artifacts", None),
        (501, String::new())
    );
}

/// Commit statuses as GitHub was observed to serve them: each write, the
/// two reads, and the state GraphQL makes of them with the check runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_commit_is_given_statuses_and_comes_to_what_they_say() {
    let s = seed().await;
    let base = s.inst.base.clone();
    let sha = s.feature_sha.clone();
    let post = |to: &str, body: Value| {
        send(
            "POST",
            &format!("{}/statuses/{to}", s.api),
            Some(&s.token),
            Some(body),
        )
    };
    let give = |context: &str, state: &str| {
        let answer = post(&sha, json!({ "state": state, "context": context }));
        assert_eq!(answer.status, 201, "{}", answer.text());
        serde_json::from_slice::<Value>(&answer.body).unwrap()
    };
    let combined = || s.get(&format!("commits/{sha}/status"));
    let shown = |combined: &Value| -> (String, Vec<(String, String)>) {
        let statuses = combined["statuses"].as_array().unwrap();
        (
            combined["state"].as_str().unwrap().to_string(),
            statuses
                .iter()
                .map(|status| {
                    (
                        status["context"].as_str().unwrap().to_string(),
                        status["state"].as_str().unwrap().to_string(),
                    )
                })
                .collect(),
        )
    };
    let of = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(context, state)| (context.to_string(), state.to_string()))
            .collect()
    };

    // A commit with no statuses is pending, with none.
    let none = combined();
    assert_eq!(shown(&none), ("pending".to_string(), Vec::new()));
    assert_eq!(none["total_count"], 0);
    assert_eq!(none["sha"], sha.as_str());
    assert_eq!(none["repository"]["full_name"], "alice/web");
    assert_eq!(
        none["commit_url"],
        format!("{}/commits/{sha}", s.api).as_str()
    );
    assert_eq!(
        none["url"],
        format!("{}/commits/{sha}/status", s.api).as_str()
    );
    assert_eq!(s.get(&format!("commits/{sha}/statuses")), json!([]));

    // A status, with what GitHub answers for it.
    let answer = post(
        &sha,
        json!({
            "state": "pending", "context": "ext/deploy", "description": "Deploying",
            "target_url": "https://deploy.example/1", "bogus": 1,
        }),
    );
    assert_eq!(answer.status, 201, "{}", answer.text());
    let made: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(made["url"], format!("{}/statuses/{sha}", s.api).as_str());
    assert_eq!(
        (
            &made["state"],
            &made["context"],
            &made["description"],
            &made["target_url"]
        ),
        (
            &json!("pending"),
            &json!("ext/deploy"),
            &json!("Deploying"),
            &json!("https://deploy.example/1")
        )
    );
    assert_eq!(made["creator"]["login"], "alice");
    assert_eq!(made["created_at"], made["updated_at"]);
    assert!(made["id"].is_i64() && made["node_id"].is_string());
    // No context is `default`, and so is a null one. An empty description
    // is kept as it is.
    let unnamed = post(
        &sha,
        json!({ "state": "success", "context": null, "description": "" }),
    );
    let unnamed: Value = serde_json::from_slice(&unnamed.body).unwrap();
    assert_eq!(
        (
            &unnamed["context"],
            &unnamed["description"],
            &unnamed["target_url"]
        ),
        (&json!("default"), &json!(""), &Value::Null)
    );

    // What GitHub refuses, in its words: a field of the wrong type by name,
    // then a commit it cannot find, then every other fault in one string.
    // The types are checked in the order context, description, target_url,
    // state, and before the commit is looked for.
    for (field, body) in [
        (
            "context",
            json!({ "state": 5, "context": 7, "description": 6 }),
        ),
        (
            "description",
            json!({ "state": 5, "description": 5, "target_url": 6 }),
        ),
        ("target_url", json!({ "state": 5, "target_url": 5 })),
        ("state", json!({ "state": 5 })),
    ] {
        assert_eq!(
            post(&sha, body).message(),
            (
                422,
                format!("Invalid value for '{field}'. Must be a string.")
            )
        );
    }
    assert_eq!(
        post(&"0".repeat(40), json!({ "state": 5 })).message(),
        (
            422,
            "Invalid value for 'state'. Must be a string.".to_string()
        )
    );
    for missing in ["0".repeat(40), "abc".to_string(), "nope".to_string()] {
        assert_eq!(
            post(&missing, json!({ "state": "bogus" })).message(),
            (422, format!("No commit found for SHA: {missing}"))
        );
    }
    let failed = |answer: Answer| {
        let body: Value = serde_json::from_slice(&answer.body).unwrap();
        (
            answer.status,
            body["message"].as_str().unwrap().to_string(),
            body["errors"].as_str().unwrap().to_string(),
        )
    };
    let validation = |faults: &str| {
        (
            422,
            "Validation Failed".to_string(),
            format!("Validation failed: {faults}"),
        )
    };
    assert_eq!(
        failed(post(&sha, json!({}))),
        validation("State is not included in the list")
    );
    assert_eq!(
        failed(post(&sha, json!({ "state": "SUCCESS" }))),
        validation("State is not included in the list")
    );
    assert_eq!(
        failed(post(
            &sha,
            json!({
                "state": "bogus", "context": "", "description": "x".repeat(141),
                "target_url": "ftp://x",
            })
        )),
        validation(
            "Context can't be blank, State is not included in the list, Description is too \
             long (maximum is 140 characters), Target url must use http(s) scheme"
        )
    );
    assert_eq!(
        post(
            &sha,
            json!({ "state": "success", "context": "long", "description": "x".repeat(140) })
        )
        .status,
        201
    );
    // A commit named by a branch, in upper case, by the start of its sha,
    // or by a revision is found, and is not a valid sha. That fault comes
    // first among several.
    for named in [
        "feature".to_string(),
        sha.to_uppercase(),
        sha[..7].to_string(),
        "feature~0".to_string(),
    ] {
        assert_eq!(
            failed(post(&named, json!({ "state": "success" }))),
            validation("Sha must be a valid hex object ID"),
            "{named}"
        );
    }
    assert_eq!(
        failed(post(
            &sha[..7],
            json!({ "state": "nope", "context": "   " })
        )),
        validation(
            "Sha must be a valid hex object ID, Context can't be blank, State is not included \
             in the list"
        )
    );
    // The scheme of a URL is read in lower case alone.
    assert_eq!(
        failed(post(
            &sha,
            json!({ "state": "success", "target_url": "HTTPS://deploy.example/1" })
        )),
        validation("Target url must use http(s) scheme")
    );

    // The combined status keeps the newest of each context, in the order of
    // those newest, and comes to a failure when any failed or errored, to
    // pending when any is, and otherwise to a success.
    let other = s.main_sha.clone();
    let on_main = |context: &str, state: &str| {
        let answer = send(
            "POST",
            &format!("{}/statuses/{other}", s.api),
            Some(&s.token),
            Some(json!({ "state": state, "context": context })),
        );
        assert_eq!(answer.status, 201);
    };
    let main_combined = || shown(&s.get(&format!("commits/{other}/status")));
    on_main("one", "success");
    assert_eq!(
        main_combined(),
        ("success".to_string(), of(&[("one", "success")]))
    );
    on_main("two", "pending");
    assert_eq!(main_combined().0, "pending");
    on_main("three", "failure");
    on_main("four", "error");
    assert_eq!(
        main_combined(),
        (
            "failure".to_string(),
            of(&[
                ("one", "success"),
                ("two", "pending"),
                ("three", "failure"),
                ("four", "error")
            ])
        )
    );
    on_main("three", "success");
    assert_eq!(
        main_combined(),
        (
            "failure".to_string(),
            of(&[
                ("one", "success"),
                ("two", "pending"),
                ("four", "error"),
                ("three", "success")
            ])
        )
    );
    on_main("four", "success");
    assert_eq!(main_combined().0, "pending");
    on_main("two", "success");
    assert_eq!(
        main_combined(),
        (
            "success".to_string(),
            of(&[
                ("one", "success"),
                ("three", "success"),
                ("four", "success"),
                ("two", "success")
            ])
        )
    );

    // The list has every status, newest first, with who gave each. Both
    // reads page, with links at the repository's id, and take a branch.
    let all = s.get(&format!("commits/{other}/statuses"));
    let listed: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|status| status["context"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed,
        ["two", "four", "three", "four", "three", "two", "one"]
    );
    assert_eq!(all[0]["creator"]["login"], "alice");
    assert!(s.get(&format!("commits/{other}/status"))["statuses"][0]["creator"].is_null());
    assert_eq!(s.get(&format!("statuses/{other}")), all);
    assert_eq!(s.get("commits/main/statuses"), all);
    assert_eq!(s.get("commits/main/status")["total_count"], 4);
    let paged = fetch(
        "GET",
        &format!("{}/commits/{other}/status?per_page=3", s.api),
        Some(&s.token),
    );
    let page: Value = serde_json::from_slice(&paged.body).unwrap();
    assert_eq!(
        (
            page["total_count"].as_i64(),
            page["statuses"].as_array().unwrap().len()
        ),
        (Some(4), 3)
    );
    assert_eq!(page["state"], "success");
    // A ref that names nothing is not found, each read in its own words,
    // and a sha no commit has is a commit with no statuses.
    let unknown = |path: &str| fetch("GET", &format!("{}/{path}", s.api), Some(&s.token)).message();
    assert_eq!(
        unknown("commits/nope/status"),
        (404, "Ref not found".to_string())
    );
    assert_eq!(
        unknown("commits/nope/statuses"),
        (404, "Not Found".to_string())
    );
    let nowhere = "0".repeat(40);
    assert_eq!(
        shown(&s.get(&format!("commits/{nowhere}/status"))),
        ("pending".to_string(), Vec::new())
    );
    assert_eq!(s.get(&format!("commits/{nowhere}/statuses")), json!([]));

    // A read's `{ref}` is whatever git resolves: the start of a sha, a
    // revision, or a branch. The check run lists take the same.
    for named in [&other[..7], "feature~1", "main"] {
        assert_eq!(
            s.get(&format!("commits/{named}/status"))["sha"],
            other.as_str(),
            "{named}"
        );
        assert_eq!(s.get(&format!("commits/{named}/statuses")), all, "{named}");
        assert!(s.get(&format!("commits/{named}/check-runs"))["total_count"].is_i64());
    }

    // A context is kept without the spaces around it, and an empty URL as
    // it is. A context is the same one in either case, spelled as its
    // newest status spells it.
    let padded = send(
        "POST",
        &format!("{}/statuses/{other}", s.api),
        Some(&s.token),
        Some(json!({ "state": "success", "context": "  padded  ", "target_url": "" })),
    );
    assert_eq!(padded.status, 201, "{}", padded.text());
    assert_eq!(
        (&padded.json()["context"], &padded.json()["target_url"]),
        (&json!("padded"), &json!(""))
    );
    on_main("Case/X", "pending");
    assert_eq!(main_combined().0, "pending");
    on_main("case/x", "success");
    assert_eq!(
        main_combined(),
        (
            "success".to_string(),
            of(&[
                ("one", "success"),
                ("three", "success"),
                ("four", "success"),
                ("two", "success"),
                ("padded", "success"),
                ("case/x", "success")
            ])
        )
    );

    // A page holds 30 unless `per_page` says otherwise, 100 at most, and
    // a `per_page` that is no positive number is 30. A page past the last
    // is empty, and names the last.
    let listed = |query: &str| {
        let answer = fetch(
            "GET",
            &format!("{}/commits/{other}/statuses?{query}", s.api),
            Some(&s.token),
        );
        (answer.json().as_array().unwrap().len(), answer.link)
    };
    for all_of_them in ["per_page=0", "per_page=-1", "per_page=abc", "per_page=200"] {
        assert_eq!(listed(all_of_them), (10, None), "{all_of_them}");
    }
    let repo_id = send("GET", &s.api, Some(&s.token), None).json()["id"]
        .as_i64()
        .unwrap();
    let at = |page: usize, rel: &str| {
        format!(
            "<{base}/api/v3/repositories/{repo_id}/statuses/{other}?per_page=4&page={page}>; \
             rel=\"{rel}\""
        )
    };
    assert_eq!(
        listed("per_page=4&page=99"),
        (
            0,
            Some([at(3, "prev"), at(3, "last"), at(1, "first")].join(", "))
        )
    );

    // GraphQL: a commit's statuses, and the rollup's state over them and
    // the check runs. The head of the pull request has three contexts.
    give("ext/deploy", "error");
    let query = r#"
        query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest {
            commits(last: 1) { nodes { commit {
                status { id state contexts { context state description targetUrl creator { login } } }
                statusCheckRollup { state contexts(first: 50) {
                    totalCount checkRunCount statusContextCount
                    nodes { __typename ... on StatusContext { context state } ... on CheckRun { name } }
                } }
            } } }
        } } }"#;
    let ask = || -> Value {
        let answer: Value = ureq::post(format!("{base}/api/graphql"))
            .header("authorization", &format!("token {}", s.token))
            .send_json(json!({ "query": query, "variables": { "ids": ["O_pull_1"] } }))
            .unwrap()
            .body_mut()
            .read_json()
            .unwrap();
        assert!(answer.get("errors").is_none(), "{answer}");
        answer["data"]["nodes"][0]["commits"]["nodes"][0]["commit"].clone()
    };
    let commit = ask();
    assert_eq!(commit["status"]["state"], "FAILURE");
    // What a commit's statuses come to is one thing in each repository.
    assert_eq!(
        commit["status"]["id"],
        format!("O_commit_status_{repo_id}_{sha}").as_str()
    );
    // The contexts that errored come first, and the rest by name.
    assert_eq!(
        commit["status"]["contexts"],
        json!([
            { "context": "ext/deploy", "state": "ERROR", "description": null, "targetUrl": null,
              "creator": { "login": "alice" } },
            { "context": "default", "state": "SUCCESS", "description": "", "targetUrl": null,
              "creator": { "login": "alice" } },
            { "context": "long", "state": "SUCCESS", "description": "x".repeat(140),
              "targetUrl": null, "creator": { "login": "alice" } },
        ])
    );
    let rollup = &commit["statusCheckRollup"];
    assert_eq!(rollup["state"], "FAILURE");
    assert_eq!(
        (
            &rollup["contexts"]["totalCount"],
            &rollup["contexts"]["checkRunCount"],
            &rollup["contexts"]["statusContextCount"]
        ),
        (&json!(3), &json!(0), &json!(3))
    );
    assert_eq!(
        rollup["contexts"]["nodes"][0]["__typename"],
        "StatusContext"
    );
    // With every status a success, a check run decides: queued is pending,
    // and a failure is a failure.
    give("ext/deploy", "success");
    assert_eq!(ask()["statusCheckRollup"]["state"], "SUCCESS");
    let ci = mint_actor(&base, "ci", "bot");
    let run = face_ok(
        "POST",
        &format!("{}/check-runs", s.api),
        &ci,
        json!({ "name": "build", "head_sha": sha }),
    );
    let commit = ask();
    assert_eq!(commit["statusCheckRollup"]["state"], "PENDING");
    assert_eq!(commit["status"]["state"], "SUCCESS");
    assert_eq!(
        commit["statusCheckRollup"]["contexts"]["nodes"][0]["name"],
        "build"
    );
    assert_eq!(commit["statusCheckRollup"]["contexts"]["totalCount"], 4);
    send(
        "PATCH",
        &format!("{}/check-runs/{}", s.api, run["id"]),
        Some(&ci),
        Some(json!({ "status": "completed", "conclusion": "failure" })),
    );
    assert_eq!(ask()["statusCheckRollup"]["state"], "FAILURE");
}

/// What GitHub was observed to do with a check run's output and its
/// annotations.
#[tokio::test(flavor = "multi_thread")]
async fn a_check_run_keeps_the_annotations_github_keeps_and_lists_them() {
    let s = seed().await;
    let base = s.inst.base.clone();
    let bot = mint_actor(&base, "ci", "bot");
    let sha = s.feature_sha.clone();
    let runs = format!("{}/check-runs", s.api);
    let post = |body: Value| send("POST", &runs, Some(&bot), Some(body));
    let patch =
        |id: i64, body: Value| send("PATCH", &format!("{runs}/{id}"), Some(&bot), Some(body));
    let ann = |line: i64| {
        json!({
            "path": "README.md", "start_line": line, "end_line": line,
            "annotation_level": "warning", "message": format!("message {line}"),
        })
    };
    let with = |line: i64, more: Value| {
        let mut annotation = ann(line);
        for (key, value) in more.as_object().unwrap() {
            annotation[key] = value.clone();
        }
        annotation
    };
    let without = |line: i64, key: &str| {
        let mut annotation = ann(line);
        annotation.as_object_mut().unwrap().remove(key);
        annotation
    };
    let output =
        |annotations: Value| json!({ "title": "T", "summary": "S", "annotations": annotations });
    let list = |id: i64, query: &str| {
        fetch(
            "GET",
            &format!("{runs}/{id}/annotations{query}"),
            Some(&s.token),
        )
    };
    let listed =
        |id: i64| -> Vec<Value> { list(id, "?per_page=100").json().as_array().unwrap().clone() };
    let annotations_url = |id: i64| format!("{runs}/{id}/annotations");
    let blob = |path: &str| format!("{base}/alice/web/blob/{sha}/{path}");

    // A check run sent no output answers one with nothing in it, and has no
    // list of annotations.
    let plain = post(json!({ "name": "plain", "head_sha": sha }));
    assert_eq!(plain.status, 201, "{}", plain.text());
    let plain = plain.json();
    let plain_id = plain["id"].as_i64().unwrap();
    assert_eq!(
        plain["output"],
        json!({
            "title": null, "summary": null, "text": null, "annotations_count": 0,
            "annotations_url": annotations_url(plain_id),
        })
    );
    assert_eq!(list(plain_id, "").message(), (404, "Not Found".to_string()));
    assert_eq!(
        fetch("GET", &format!("{runs}/999/annotations"), Some(&s.token)).status,
        404
    );

    // A new check run with three. One sent no title is titled by its path
    // and its lines, and an empty title and empty details stay empty.
    let made = post(json!({
        "name": "annotated", "head_sha": sha, "status": "in_progress",
        "output": { "title": "T", "summary": "S", "text": "X", "annotations": [
            { "path": "README.md", "start_line": 1, "end_line": 1, "start_column": 2,
              "end_column": 5, "annotation_level": "notice", "message": "a notice",
              "title": "Notice title", "raw_details": "raw one" },
            { "path": "src/main.rs", "start_line": 2, "end_line": 4,
              "annotation_level": "warning", "message": "a warning" },
            { "path": "no/such/file.txt", "start_line": 7, "end_line": 7,
              "annotation_level": "failure", "message": "a failure", "title": "",
              "raw_details": "" },
        ] },
    }));
    assert_eq!(made.status, 201, "{}", made.text());
    let made = made.json();
    let id = made["id"].as_i64().unwrap();
    let out = |title: &str, summary: &str, text: Value, count: usize| {
        json!({
            "title": title, "summary": summary, "text": text, "annotations_count": count,
            "annotations_url": annotations_url(id),
        })
    };
    assert_eq!(made["output"], out("T", "S", json!("X"), 3));
    assert_eq!(
        Value::Array(listed(id)),
        json!([
            { "path": "README.md", "blob_href": blob("README.md"), "start_line": 1,
              "start_column": 2, "end_line": 1, "end_column": 5, "annotation_level": "notice",
              "title": "Notice title", "message": "a notice", "raw_details": "raw one" },
            { "path": "src/main.rs", "blob_href": blob("src/main.rs"), "start_line": 2,
              "start_column": null, "end_line": 4, "end_column": null,
              "annotation_level": "warning", "title": "src/main.rs#L2-L4",
              "message": "a warning", "raw_details": null },
            { "path": "no/such/file.txt", "blob_href": blob("no/such/file.txt"),
              "start_line": 7, "start_column": null, "end_line": 7, "end_column": null,
              "annotation_level": "failure", "title": "", "message": "a failure",
              "raw_details": "" },
        ])
    );

    // An update's output takes the place of the one before, and its
    // annotations come after those the run has. An update with no output,
    // or a null one, leaves both as they are.
    let updated = patch(
        id,
        json!({ "output": { "title": "T2", "summary": "S2", "annotations": [ann(10), ann(11)] } }),
    );
    assert_eq!(updated.status, 200, "{}", updated.text());
    assert_eq!(updated.json()["output"], out("T2", "S2", Value::Null, 5));
    for body in [
        json!({ "status": "in_progress" }),
        json!({ "output": null }),
    ] {
        assert_eq!(
            patch(id, body).json()["output"],
            out("T2", "S2", Value::Null, 5)
        );
    }
    assert_eq!(listed(id)[3]["title"], "README.md#L10");
    assert_eq!(
        patch(
            id,
            json!({ "output": { "title": "", "summary": "", "text": "" } })
        )
        .json()["output"],
        out("", "", json!(""), 5)
    );

    // What GitHub refuses of an output's shape, in its words.
    let refusals = [
        (
            json!("x"),
            "For 'properties/output', \"x\" is not an object or null.".to_string(),
        ),
        (
            json!({ "annotations": [ann(1)] }),
            "\"summary\", \"title\" weren't supplied.".to_string(),
        ),
        (
            json!({ "summary": "S" }),
            "\"title\" wasn't supplied.".to_string(),
        ),
        (
            json!({ "title": "T", "summary": "S", "text": null }),
            "For 'properties/text', nil is not a string.".to_string(),
        ),
        (
            json!({ "title": "T", "summary": "s".repeat(65_536) }),
            "Only 65535 characters are allowed; 65536 were supplied.".to_string(),
        ),
        (
            output(Value::Null),
            "For 'properties/annotations', nil is not an array.".to_string(),
        ),
        (
            output(json!({ "path": "x" })),
            "For 'properties/annotations', {\"path\" => \"x\"} is not an array.".to_string(),
        ),
        (
            output(Value::Array((1..=51).map(ann).collect())),
            "No more than 50 items are allowed; 51 were supplied.".to_string(),
        ),
        (
            output(json!(["x"])),
            "For 'items', \"x\" is not an object.".to_string(),
        ),
        (
            output(json!([without(1, "path")])),
            "\"path\" wasn't supplied.".to_string(),
        ),
        (
            output(json!([without(1, "annotation_level"), { "path": "x" }])),
            "\"annotation_level\", \"end_line\", \"message\", \"start_line\" weren't supplied."
                .to_string(),
        ),
    ];
    for (given, detail) in refusals {
        let expected = (422, format!("Invalid request.\n\n{detail}"));
        assert_eq!(
            patch(id, json!({ "output": given.clone() })).message(),
            expected,
            "{given}"
        );
        assert_eq!(
            post(json!({ "name": "refused", "head_sha": sha, "output": given.clone() })).message(),
            expected,
            "{given}"
        );
    }
    assert_eq!(listed(id).len(), 5, "nothing refused was kept");

    // What GitHub keeps though its description says otherwise: a level it
    // does not define, a line below 1, a number where a string belongs,
    // and text cut to what it holds.
    let kept = patch(
        id,
        json!({ "output": output(json!([
            with(20, json!({ "annotation_level": "error" })),
            with(21, json!({ "annotation_level": 5 })),
            with(0, json!({})),
            with(22, json!({ "start_line": "3" })),
            with(23, json!({ "start_line": "abc" })),
            with(24, json!({ "start_line": 24.9, "end_line": 24.9 })),
            with(25, json!({ "start_column": "2", "end_column": "4" })),
            with(26, json!({ "start_column": 3 })),
            with(27, json!({ "title": "t".repeat(256) })),
            with(28, json!({ "message": "m".repeat(65_537), "raw_details": "r".repeat(65_537) })),
            with(29, json!({ "message": "  padded  ", "raw_details": 5, "title": null })),
        ])) }),
    );
    assert_eq!(kept.status, 200, "{}", kept.text());
    assert_eq!(kept.json()["output"]["annotations_count"], 16);
    let all = listed(id);
    let shown: Vec<Value> = all[5..]
        .iter()
        .map(|a| {
            json!([
                a["start_line"],
                a["start_column"],
                a["end_line"],
                a["end_column"],
                a["annotation_level"]
            ])
        })
        .collect();
    assert_eq!(
        Value::Array(shown),
        json!([
            [20, null, 20, null, "error"],
            [21, null, 21, null, "5"],
            [0, null, 0, null, "warning"],
            [3, null, 22, null, "warning"],
            [0, null, 23, null, "warning"],
            [24, null, 24, null, "warning"],
            [25, 2, 25, 4, "warning"],
            [26, 3, 26, null, "warning"],
            [27, null, 27, null, "warning"],
            [28, null, 28, null, "warning"],
            [29, null, 29, null, "warning"],
        ])
    );
    assert_eq!(all[8]["title"], "README.md#L3-L22");
    assert_eq!(all[13]["title"].as_str().unwrap().len(), 256);
    assert_eq!(all[14]["message"].as_str().unwrap().len(), 65_535);
    assert_eq!(all[14]["raw_details"].as_str().unwrap().len(), 65_535);
    assert_eq!(
        (
            &all[15]["message"],
            &all[15]["raw_details"],
            &all[15]["title"]
        ),
        (&json!("  padded  "), &json!("5"), &json!("README.md#L29"))
    );

    // What GitHub leaves out of an update, keeping the rest, and still
    // answers 200.
    let left_out = json!([
        ann(40),
        with(41, json!({ "start_line": 9, "end_line": 4 })),
        with(42, json!({ "message": "" })),
        with(43, json!({ "message": "   " })),
        with(44, json!({ "message": null })),
        with(45, json!({ "path": "" })),
        with(46, json!({ "path": null })),
        with(47, json!({ "annotation_level": "" })),
        with(48, json!({ "annotation_level": null })),
        with(49, json!({ "start_line": null })),
        with(
            50,
            json!({ "end_line": 51, "start_column": 1, "end_column": 2 })
        ),
        with(52, json!({ "start_column": 5, "end_column": 2 })),
        ann(53),
    ]);
    let partly = patch(id, json!({ "output": output(left_out.clone()) }));
    assert_eq!(partly.status, 200, "{}", partly.text());
    assert_eq!(partly.json()["output"]["annotations_count"], 18);
    let lines: Vec<i64> = listed(id)[16..]
        .iter()
        .map(|a| a["start_line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, [40, 53]);

    // A new check run is refused for one of those, and is not made.
    let before = s.get(&format!("commits/{sha}/check-runs?filter=all"))["total_count"].clone();
    let refused = post(json!({ "name": "refused", "head_sha": sha, "output": output(left_out) }));
    assert_eq!(refused.status, 422);
    assert_eq!(
        refused.json(),
        json!({
            "message": "Validation Failed",
            "errors": [{ "resource": "CheckRun", "code": "invalid", "field": "annotations" }],
        })
    );
    assert_eq!(
        s.get(&format!("commits/{sha}/check-runs?filter=all"))["total_count"],
        before
    );

    // A number where GitHub fails outright: a `500` with no body, and
    // nothing kept.
    for field in ["path", "message", "title"] {
        let mut body = serde_json::Map::new();
        body.insert(field.to_string(), json!(5));
        let crashed = patch(
            id,
            json!({ "output": output(json!([ann(60), with(61, Value::Object(body))])) }),
        );
        assert_eq!((crashed.status, crashed.body.len()), (500, 0), "{field}");
    }
    assert_eq!(listed(id).len(), 18);

    // The list is paged as the status lists are: 30 to a page, 100 at
    // most, and a page past the last names the last.
    let fifty = patch(
        id,
        json!({ "output": output(Value::Array((100..150).map(ann).collect())) }),
    );
    assert_eq!(fifty.json()["output"]["annotations_count"], 68);
    let repo_id = send("GET", &s.api, Some(&s.token), None).json()["id"]
        .as_i64()
        .unwrap();
    let at = |query: &str, page: usize, rel: &str| {
        format!(
            "<{base}/api/v3/repositories/{repo_id}/check-runs/{id}/annotations?{query}page={page}>; \
             rel=\"{rel}\""
        )
    };
    let first = list(id, "");
    assert_eq!(first.json().as_array().unwrap().len(), 30);
    assert_eq!(
        first.link,
        Some([at("", 2, "next"), at("", 3, "last")].join(", "))
    );
    for (query, held) in [
        ("?per_page=200", 68),
        ("?per_page=0", 30),
        ("?per_page=abc", 30),
    ] {
        assert_eq!(
            list(id, query).json().as_array().unwrap().len(),
            held,
            "{query}"
        );
    }
    let past = list(id, "?per_page=50&page=9");
    assert_eq!(past.json(), json!([]));
    assert_eq!(
        past.link,
        Some(
            [
                at("per_page=50&", 2, "prev"),
                at("per_page=50&", 2, "last"),
                at("per_page=50&", 1, "first")
            ]
            .join(", ")
        )
    );
    let by_id = fetch(
        "GET",
        &format!("{base}/api/v3/repositories/{repo_id}/check-runs/{id}/annotations?page=3"),
        Some(&s.token),
    );
    assert_eq!(by_id.json().as_array().unwrap().len(), 8);

    // GraphQL lists the same annotations, with the output's three texts.
    let query = r#"
        query($ids: [ID!]!) { nodes(ids: $ids) { ... on PullRequest {
            commits(last: 1) { nodes { commit { statusCheckRollup { contexts(first: 50) { nodes {
                __typename
                ... on CheckRun { name title summary text annotations(first: 2) {
                    totalCount pageInfo { hasNextPage }
                    nodes { path annotationLevel message title rawDetails blobUrl
                            location { start { line column } end { line column } } }
                } }
            } } } } } }
        } } }"#;
    let answer: Value = ureq::post(format!("{base}/api/graphql"))
        .header("authorization", &format!("token {}", s.token))
        .send_json(json!({ "query": query, "variables": { "ids": ["O_pull_1"] } }))
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert!(answer.get("errors").is_none(), "{answer}");
    let contexts = answer["data"]["nodes"][0]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
        ["contexts"]["nodes"]
        .as_array()
        .unwrap()
        .clone();
    let annotated = contexts
        .iter()
        .find(|context| context["name"] == "annotated")
        .expect("the check run is in the rollup");
    assert_eq!(
        annotated,
        &json!({
            "__typename": "CheckRun", "name": "annotated", "title": "T", "summary": "S",
            "text": null,
            "annotations": {
                "totalCount": 68,
                "pageInfo": { "hasNextPage": true },
                "nodes": [
                    { "path": "README.md", "annotationLevel": "NOTICE", "message": "a notice",
                      "title": "Notice title", "rawDetails": "raw one",
                      "blobUrl": blob("README.md"),
                      "location": { "start": { "line": 1, "column": 2 },
                                    "end": { "line": 1, "column": 5 } } },
                    { "path": "src/main.rs", "annotationLevel": "WARNING",
                      "message": "a warning", "title": "src/main.rs#L2-L4", "rawDetails": null,
                      "blobUrl": blob("src/main.rs"),
                      "location": { "start": { "line": 2, "column": null },
                                    "end": { "line": 4, "column": null } } },
                ],
            },
        })
    );

    // A runner annotates the job it is on, through the control plane: the
    // job's check run lists them, with the empty title and details
    // GitHub's runner gives one.
    let (_, workflow) = s.control("POST", "/workflows", Some(ci()));
    let run = s.runner(
        &format!("/workflows/{}/runs", workflow["id"]),
        Some(json!({
            "event": "push", "head_sha": sha, "head_branch": "feature", "actor": "alice",
        })),
    );
    let lint = job_id(&run, "lint");
    let annotate = format!("/workflow-jobs/{lint}/annotations");
    let finding = json!({
        "path": ".github", "start_line": 5, "end_line": 5, "level": "failure",
        "message": "Process completed with exit code 1.",
    });
    // A queued job has no runner to annotate it.
    let (status, body) = s.control(
        "POST",
        &annotate,
        Some(json!({ "annotations": [finding.clone()] })),
    );
    assert_eq!(
        (status, faults(&body)),
        (400, vec![fault("FAILED_PRECONDITION", None)])
    );
    s.runner(&format!("/workflow-jobs/{lint}/start"), None);
    let (status, body) = s.control(
        "POST",
        &annotate,
        Some(json!({ "annotations": [
            { "path": "", "start_line": 0, "end_line": 0, "level": "error", "message": " " },
            { "path": "a", "start_line": 3, "end_line": 2, "level": "notice", "message": "m",
              "start_column": 1 },
        ] })),
    );
    assert_eq!(
        (status, faults(&body)),
        (
            400,
            vec![
                fault("INVALID_ARGUMENT", Some("annotations[0].path")),
                fault("INVALID_ARGUMENT", Some("annotations[0].message")),
                fault("INVALID_ARGUMENT", Some("annotations[0].level")),
                fault("INVALID_ARGUMENT", Some("annotations[0].start_line")),
                fault("INVALID_ARGUMENT", Some("annotations[1].end_line")),
                fault("INVALID_ARGUMENT", Some("annotations[1].start_column")),
            ]
        )
    );
    let (status, body) = s.control("POST", &annotate, Some(json!({ "annotations": [] })));
    assert_eq!(
        (status, faults(&body)),
        (400, vec![fault("INVALID_ARGUMENT", Some("annotations"))])
    );
    let (status, _) = s.control(
        "POST",
        &annotate,
        Some(json!({ "annotations": [
            finding.clone(),
            { "path": "README.md", "start_line": 3, "end_line": 4, "level": "notice",
              "message": "from a command", "title": "A notice" },
        ] })),
    );
    assert_eq!(status, 200);
    let job = s.get(&format!("check-runs/{lint}"));
    assert_eq!(job["output"]["annotations_count"], 2);
    assert_eq!(
        Value::Array(listed(lint)),
        json!([
            { "path": ".github", "blob_href": blob(".github"), "start_line": 5,
              "start_column": null, "end_line": 5, "end_column": null,
              "annotation_level": "failure", "title": "",
              "message": "Process completed with exit code 1.", "raw_details": "" },
            { "path": "README.md", "blob_href": blob("README.md"), "start_line": 3,
              "start_column": null, "end_line": 4, "end_column": null,
              "annotation_level": "notice", "title": "A notice", "message": "from a command",
              "raw_details": "" },
        ])
    );
    // No App gives a job annotations: its check run is its runner's.
    assert_eq!(
        patch(lint, json!({ "output": output(json!([ann(1)])) })).status,
        403
    );
    let (status, _) = s.control(
        "POST",
        "/workflow-jobs/999/annotations",
        Some(json!({ "annotations": [finding] })),
    );
    assert_eq!(status, 404);
}
