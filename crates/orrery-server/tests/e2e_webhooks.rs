//! End-to-end for an App's webhook (spec/surface-github.md §1.12): a real
//! receiver gets a signed delivery for each event, the App reads its
//! deliveries and asks for one again, and suspending and removing an
//! installation do what GitHub does.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
    AppKey, app_key, boot, boot_with, control_post, face_ok, fault, faults, git, mint_actor,
};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// A call with `authorization` as its `Authorization` header: the status,
/// the `Link` header, and the JSON body.
fn call(
    method: &str,
    url: &str,
    authorization: &str,
    body: Option<serde_json::Value>,
) -> (u16, Option<String>, serde_json::Value) {
    let http = agent();
    let mut resp = match (method, body) {
        ("GET", None) => http.get(url).header("authorization", authorization).call(),
        ("POST", None) => http
            .post(url)
            .header("authorization", authorization)
            .send_empty(),
        ("POST", Some(body)) => http
            .post(url)
            .header("authorization", authorization)
            .send_json(body),
        ("PUT", None) => http
            .put(url)
            .header("authorization", authorization)
            .send_empty(),
        ("PATCH", Some(body)) => http
            .patch(url)
            .header("authorization", authorization)
            .send_json(body),
        ("DELETE", None) => http
            .delete(url)
            .header("authorization", authorization)
            .call(),
        (other, _) => panic!("unsupported call {other}"),
    }
    .unwrap_or_else(|e| panic!("{method} {url} transport error: {e:?}"));
    let link = resp
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string());
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().expect("a body");
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, link, json)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// One request a receiver got: its headers, lower-cased, and its body.
#[derive(Clone, Debug)]
struct Received {
    headers: Vec<(String, String)>,
    body: String,
}

impl Received {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} header in {:?}", self.headers))
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("a JSON payload")
    }

    /// GitHub's event name and the payload's action.
    fn kind(&self) -> (String, Option<String>) {
        (
            self.header("x-github-event").to_string(),
            self.json()["action"].as_str().map(str::to_string),
        )
    }
}

/// A webhook receiver on a local port, which answers every POST with
/// `status` and keeps what it got.
struct Receiver {
    url: String,
    got: Arc<Mutex<Vec<Received>>>,
    /// The status it answers with, which a test may change.
    status: Arc<std::sync::atomic::AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Receiver {
    async fn start(status: u16) -> Receiver {
        let got: Arc<Mutex<Vec<Received>>> = Arc::default();
        let kept = got.clone();
        let status = Arc::new(std::sync::atomic::AtomicU16::new(status));
        let answered = status.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
                let (kept, answered) = (kept.clone(), answered.clone());
                async move {
                    kept.lock().unwrap().push(Received {
                        headers: headers
                            .iter()
                            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
                            .collect(),
                        body,
                    });
                    let status = answered.load(std::sync::atomic::Ordering::SeqCst);
                    axum::http::StatusCode::from_u16(status).unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Receiver {
            url,
            got,
            status,
            task,
        }
    }

    /// What the receiver has got, once it has got `n` requests.
    async fn wait(&self, n: usize) -> Vec<Received> {
        for _ in 0..200 {
            let got = self.got.lock().unwrap().clone();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let got = self.got.lock().unwrap().clone();
        panic!(
            "the receiver got {} of {n} deliveries: {:?}",
            got.len(),
            got.iter().map(Received::kind).collect::<Vec<_>>()
        );
    }
}

/// A world that sends, with an organization, a user, a repository, and an
/// App whose webhook points at a receiver, with a key.
struct Setup {
    inst: common::Instance,
    base: String,
    api: String,
    alice: String,
    key: AppKey,
    app_id: i64,
    secret: String,
    receiver: Receiver,
}

fn sending() -> orrery_server::Options {
    orrery_server::Options {
        outbound: Some(
            orrery_outbound::Client::new(orrery_outbound::Egress::Direct, None).unwrap(),
        ),
        ..Default::default()
    }
}

async fn setup_with(options: orrery_server::Options, answer: u16) -> Setup {
    let inst = boot_with(options).await;
    let base = inst.base.clone();
    let api = format!("{base}/api/v3");
    let alice = mint_actor(&base, "alice", "user");
    mint_actor(&base, "acme", "org");
    face_ok(
        "POST",
        &format!("{api}/orgs/acme/repos"),
        &alice,
        serde_json::json!({ "name": "web", "private": true }),
    );
    let receiver = Receiver::start(answer).await;
    let app = control_post(
        &base,
        "/_orrery/apps",
        serde_json::json!({
            "slug": "helper",
            "name": "Helper",
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
            "permissions": { "checks": "write", "contents": "write", "pull_requests": "write" },
            "events": ["push", "pull_request", "pull_request_review", "issue_comment", "check_run"],
            "webhook_url": receiver.url,
            "webhook_active": true,
        }),
    );
    let key = app_key();
    control_post(
        &base,
        "/_orrery/apps/helper/keys",
        serde_json::json!({ "public_key": key.public_pem }),
    );
    Setup {
        app_id: app["id"].as_i64().unwrap(),
        secret: app["webhook_secret"].as_str().unwrap().to_string(),
        inst,
        base,
        api,
        alice,
        key,
        receiver,
    }
}

impl Setup {
    fn jwt(&self) -> String {
        let now = self.inst.world.clock().unwrap();
        bearer(&self.key.jwt(serde_json::json!(self.app_id), now))
    }

    fn install(&self) -> i64 {
        control_post(
            &self.base,
            "/_orrery/apps/helper/installations",
            serde_json::json!({ "account": "acme" }),
        )["id"]
            .as_i64()
            .unwrap()
    }

    fn token(&self, installation: i64) -> String {
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.api
        );
        let (status, _, issued) = call("POST", &url, &self.jwt(), None);
        assert_eq!(status, 201, "{issued}");
        issued["token"].as_str().unwrap().to_string()
    }

    /// Push a commit on a new branch `head`, and `main` first when the
    /// repository has none.
    fn push(&self, head: &str) -> String {
        let clock = self.inst.world.clock().unwrap();
        let work = tempfile::tempdir().unwrap();
        let src = work.path();
        let remote = format!("{}/acme/web.git", self.base)
            .replace("http://", &format!("http://x-access-token:{}@", self.alice));
        git(src, clock, &["init", "-q", "-b", "main", "."]);
        std::fs::write(src.join("README.md"), "# readme\n").unwrap();
        git(src, clock, &["add", "."]);
        git(src, clock, &["commit", "-q", "-m", "init"]);
        git(src, clock, &["push", "-q", &remote, "main"]);
        git(src, clock, &["checkout", "-q", "-b", head]);
        std::fs::write(src.join("change.txt"), "a change\n").unwrap();
        git(src, clock, &["add", "."]);
        git(src, clock, &["commit", "-q", "-m", "a change"]);
        git(src, clock, &["push", "-q", &remote, head]);
        git(src, clock, &["rev-parse", "HEAD"]).trim().to_string()
    }
}

fn signed(received: &Received, secret: &str) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let signature = received
        .header("x-hub-signature-256")
        .strip_prefix("sha256=")
        .expect("a sha256 signature");
    let tag: Vec<u8> = (0..signature.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&signature[i..i + 2], 16).unwrap())
        .collect();
    ring::hmac::verify(&key, received.body.as_bytes(), &tag).is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receiver_gets_a_signed_delivery_for_each_event() {
    let s = setup_with(sending(), 204).await;
    let api = &s.api;
    let installation = s.install();
    let token = s.token(installation);
    let head_sha = s.push("feature");
    let pull = face_ok(
        "POST",
        &format!("{api}/repos/acme/web/pulls"),
        &s.alice,
        serde_json::json!({ "title": "A change", "head": "feature", "base": "main" }),
    );
    let number = pull["number"].as_i64().unwrap();
    face_ok(
        "PATCH",
        &format!("{api}/repos/acme/web/pulls/{number}"),
        &s.alice,
        serde_json::json!({ "title": "A change, retitled" }),
    );
    face_ok(
        "POST",
        &format!("{api}/repos/acme/web/issues/{number}/comments"),
        &s.alice,
        serde_json::json!({ "body": "a comment" }),
    );
    face_ok(
        "POST",
        &format!("{api}/repos/acme/web/pulls/{number}/reviews"),
        &s.alice,
        serde_json::json!({ "event": "COMMENT", "body": "a review" }),
    );
    let run = face_ok(
        "POST",
        &format!("{api}/repos/acme/web/check-runs"),
        &token,
        serde_json::json!({ "name": "build", "head_sha": head_sha, "status": "in_progress" }),
    );
    face_ok(
        "PATCH",
        &format!("{api}/repos/acme/web/check-runs/{}", run["id"]),
        &token,
        serde_json::json!({ "status": "completed", "conclusion": "success" }),
    );
    face_ok(
        "PATCH",
        &format!("{api}/repos/acme/web/pulls/{number}"),
        &s.alice,
        serde_json::json!({ "state": "closed" }),
    );

    let expected: Vec<(&str, Option<&str>)> = vec![
        ("installation", Some("created")),
        ("push", None),
        ("push", None),
        ("pull_request", Some("opened")),
        ("pull_request", Some("edited")),
        ("issue_comment", Some("created")),
        ("pull_request_review", Some("submitted")),
        ("check_run", Some("created")),
        ("check_run", Some("completed")),
        ("pull_request", Some("closed")),
    ];
    let got = s.receiver.wait(expected.len()).await;
    let kinds: Vec<(String, Option<String>)> = got.iter().map(Received::kind).collect();
    let kinds: Vec<(&str, Option<&str>)> = kinds
        .iter()
        .map(|(event, action)| (event.as_str(), action.as_deref()))
        .collect();
    // One at a time, in the order the events were logged.
    assert_eq!(kinds, expected);

    let repo_id = face_ok(
        "GET",
        &format!("{api}/repos/acme/web"),
        &s.alice,
        serde_json::json!(null),
    )["id"]
        .clone();
    let mut guids = std::collections::BTreeSet::new();
    for received in &got {
        // Every delivery is signed under the hook's secret, and carries
        // GitHub's headers.
        assert!(signed(received, &s.secret), "{:?}", received.kind());
        assert!(received.header("x-hub-signature").starts_with("sha1="));
        assert_eq!(received.header("content-type"), "application/json");
        assert!(
            received
                .header("user-agent")
                .starts_with("GitHub-Hookshot/")
        );
        assert_eq!(
            received.header("x-github-hook-installation-target-id"),
            s.app_id.to_string()
        );
        assert_eq!(
            received.header("x-github-hook-installation-target-type"),
            "integration"
        );
        let guid = received.header("x-github-delivery");
        assert!(guids.insert(guid.to_string()));
        // A UUID, with a random UUID's version.
        assert_eq!((guid.len(), &guid[14..15]), (36, "4"), "{guid}");
        let payload = received.json();
        assert_eq!(payload["installation"]["id"], installation);
        if received.header("x-github-event") != "installation" {
            assert_eq!(
                payload["repository"]["id"],
                repo_id,
                "{:?}",
                received.kind()
            );
            assert!(payload["sender"]["login"].is_string());
        }
    }
    let payload = |n: usize| got[n].json();

    // installation: the object `GET /app/installations/{id}` answers.
    let created = payload(0);
    assert_eq!(created["installation"]["account"]["login"], "acme");
    assert_eq!(created["installation"]["repository_selection"], "all");
    assert!(created["installation"]["created_at"].is_string());
    assert_eq!(created["repositories"][0]["full_name"], "acme/web");

    // push: the branch, and the commit it added.
    let pushed = payload(2);
    assert_eq!(pushed["ref"], "refs/heads/feature");
    assert_eq!(pushed["after"], head_sha.as_str());
    assert_eq!(pushed["created"], true);
    assert_eq!(pushed["head_commit"]["id"], head_sha.as_str());
    assert_eq!(pushed["head_commit"]["message"], "a change");
    assert_eq!(
        pushed["head_commit"]["added"],
        serde_json::json!(["change.txt"])
    );
    assert_eq!(pushed["pusher"]["name"], "alice");
    assert!(pushed["pusher"]["email"].as_str().unwrap().contains('@'));
    assert_eq!(pushed["commits"][0]["distinct"], true);
    assert_eq!(pushed["head_commit"]["distinct"], true);
    // The commit was written under an address alice holds.
    assert_eq!(pushed["head_commit"]["author"]["username"], "alice");
    assert_eq!(pushed["base_ref"], serde_json::Value::Null);
    assert!(pushed["repository"]["created_at"].is_i64());

    // pull_request: the pull request as a read answers it.
    let opened = payload(3);
    assert_eq!(opened["number"], number);
    assert_eq!(opened["pull_request"]["number"], number);
    assert_eq!(opened["pull_request"]["title"], "A change");
    assert_eq!(opened["pull_request"]["mergeable"], serde_json::Value::Null);
    assert_eq!(opened["pull_request"]["mergeable_state"], "unknown");
    let edited = payload(4);
    assert_eq!(
        edited["changes"],
        serde_json::json!({ "title": { "from": "A change" } })
    );
    assert_eq!(edited["pull_request"]["title"], "A change, retitled");
    assert_eq!(payload(9)["pull_request"]["state"], "closed");

    // issue_comment: the pull request as an issue.
    let commented = payload(5);
    assert_eq!(commented["issue"]["number"], number);
    assert!(commented["issue"]["pull_request"]["url"].is_string());
    assert_eq!(commented["comment"]["body"], "a comment");

    // pull_request_review: a webhook spells the state in lower case.
    let reviewed = payload(6);
    assert_eq!(reviewed["review"]["state"], "commented");
    assert_eq!(reviewed["pull_request"]["number"], number);
    assert!(reviewed["pull_request"].get("merged").is_none());
    assert!(reviewed["review"]["_links"]["html"]["href"].is_string());
    assert_eq!(commented["issue"]["reactions"]["total_count"], 0);
    assert_eq!(commented["comment"]["reactions"]["total_count"], 0);

    // check_run: its commit, and the pull requests that commit heads.
    for n in [7, 8] {
        let check = payload(n);
        assert_eq!(check["check_run"]["head_sha"], head_sha.as_str());
        let of = &check["check_run"]["pull_requests"][0];
        assert_eq!(of["number"], number);
        assert_eq!(of["base"]["repo"]["id"], repo_id);
        assert_eq!(check["sender"]["login"], "helper[bot]");
    }
    assert_eq!(payload(8)["check_run"]["conclusion"], "success");

    // Every attempt's result is in the log, with what the receiver answered.
    let log = control_post(
        &s.base,
        "/_orrery/log/list",
        serde_json::json!({ "page_size": 500 }),
    );
    let attempted: Vec<&serde_json::Value> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "delivery:attempted")
        .collect();
    assert_eq!(attempted.len(), expected.len());
    for event in attempted {
        assert_eq!(event["door"], "engine");
        assert_eq!(event["changes"]["state"]["to"], "answered");
        assert_eq!(event["changes"]["status_code"]["to"], 204);
    }
    assert!(
        !log.to_string().contains(&s.secret),
        "no event carries the secret"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_reads_its_deliveries_and_asks_for_one_again() {
    let s = setup_with(sending(), 500).await;
    let api = &s.api;
    let config = format!("{api}/app/hook/config");
    let (status, _, body) = call("GET", &config, &s.jwt(), None);
    assert_eq!(status, 200);
    assert_eq!(
        body,
        serde_json::json!({
            "content_type": "json",
            "insecure_ssl": "0",
            "secret": "********",
            "url": s.receiver.url,
        })
    );

    let installation = s.install();
    s.push("feature");
    s.receiver.wait(3).await;
    let list = format!("{api}/app/hook/deliveries");
    // The result is recorded after the receiver answers.
    let mut all = serde_json::Value::Null;
    for _ in 0..200 {
        all = call("GET", &list, &s.jwt(), None).2;
        if all.as_array().unwrap().len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let all = all.as_array().unwrap().clone();
    // Newest first.
    let events: Vec<&str> = all.iter().map(|d| d["event"].as_str().unwrap()).collect();
    assert_eq!(events, ["push", "push", "installation"]);
    let push = &all[0];
    assert_eq!(push["status"], "500 ");
    assert_eq!(push["status_code"], 500);
    assert_eq!(push["redelivery"], false);
    assert_eq!(push["installation_id"], installation);
    assert!(push["repository_id"].is_i64());
    assert!(push["delivered_at"].is_string());
    assert!(push["duration"].is_number());
    assert_eq!(push["action"], serde_json::Value::Null);
    assert_eq!(push["throttled_at"], serde_json::Value::Null);
    // GitHub lists an installation event's delivery with no installation.
    assert_eq!(all[2]["action"], "created");
    assert_eq!(all[2]["installation_id"], serde_json::Value::Null);

    // A page names the next one's first delivery as its cursor.
    let (status, link, page) = call("GET", &format!("{list}?per_page=2"), &s.jwt(), None);
    assert_eq!(status, 200);
    assert_eq!(page.as_array().unwrap().len(), 2);
    let next = format!(
        "<{api}/app/hook/deliveries?per_page=2&cursor=v1_{}>; rel=\"next\"",
        all[2]["id"]
    );
    assert_eq!(link.as_deref(), Some(next.as_str()));
    let cursor = format!("{list}?per_page=2&cursor=v1_{}", all[2]["id"]);
    let (_, link, page) = call("GET", &cursor, &s.jwt(), None);
    assert_eq!(page, serde_json::json!([all[2]]));
    // A page reached by a cursor names the page before it, by its own
    // first delivery and how far to go back, and that cursor reads it.
    let prev = format!(
        "<{api}/app/hook/deliveries?per_page=2&cursor=v1_p{}l2>; rel=\"prev\"",
        all[2]["id"]
    );
    assert_eq!(link.as_deref(), Some(prev.as_str()));
    let back = format!("{list}?per_page=2&cursor=v1_p{}l2", all[2]["id"]);
    let (_, link, page) = call("GET", &back, &s.jwt(), None);
    assert_eq!(page, serde_json::json!([all[0], all[1]]));
    assert_eq!(
        link.as_deref(),
        Some(
            format!(
                "{next}, <{api}/app/hook/deliveries?per_page=2&cursor=v1_p{}l2>; rel=\"prev\"",
                all[0]["id"]
            )
            .as_str()
        )
    );
    // Before the newest delivery there is nothing.
    let none = format!("{list}?per_page=2&cursor=v1_p{}l2", all[0]["id"]);
    let (status, link, page) = call("GET", &none, &s.jwt(), None);
    assert_eq!((status, page, link), (200, serde_json::json!([]), None));

    // `status` keeps the failures or the successes, and the next link
    // repeats it.
    let failures = format!("{list}?per_page=1&status=failure");
    let (_, link, page) = call("GET", &failures, &s.jwt(), None);
    assert_eq!(page, serde_json::json!([all[0]]));
    assert!(
        link.unwrap()
            .contains("per_page=1&status=failure&cursor=v1_")
    );
    let (_, _, page) = call("GET", &format!("{list}?status=success"), &s.jwt(), None);
    assert_eq!(page, serde_json::json!([]));

    // What GitHub refuses.
    for (query, message) in [
        (
            "status=bogus",
            "Invalid request.\n\nbogus is not a member of [\"failure\", \"success\"].",
        ),
        (
            "page=2",
            "Invalid request.\n\n\"page\" is not a permitted key.",
        ),
        ("cursor=garbage", "Error parsing cursor parameter"),
        ("cursor=v1_px", "Error parsing cursor parameter"),
        (
            "per_page=abc",
            "Invalid request.\n\nabc does not match /^\\d+$/.",
        ),
        (
            "per_page=-1",
            "Invalid request.\n\n-1 does not match /^\\d+$/.",
        ),
    ] {
        let (status, _, body) = call("GET", &format!("{list}?{query}"), &s.jwt(), None);
        assert_eq!(
            (status, &body["message"]),
            (422, &message.into()),
            "{query}"
        );
    }
    assert_eq!(call("GET", &list, &bearer(&s.alice), None).0, 401);

    // Asking for a delivery again sends it as a new one with the same GUID.
    let again = format!("{list}/{}/attempts", all[2]["id"]);
    let (status, _, body) = call("POST", &again, &s.jwt(), None);
    assert_eq!((status, body), (202, serde_json::json!({})));
    let got = s.receiver.wait(4).await;
    assert_eq!(
        got[3].kind(),
        ("installation".to_string(), Some("created".to_string()))
    );
    assert_eq!(
        got[3].header("x-github-delivery"),
        all[2]["guid"].as_str().unwrap()
    );
    assert_eq!(got[3].body, got[0].body);
    let mut newest = serde_json::Value::Null;
    for _ in 0..200 {
        newest = call("GET", &format!("{list}?per_page=1"), &s.jwt(), None).2[0].clone();
        if newest["redelivery"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(newest["redelivery"], true);
    assert_eq!(newest["guid"], all[2]["guid"]);
    assert_ne!(newest["id"], all[2]["id"]);

    // What the list says of each answer, as GitHub's does: a status below
    // 400 is a success, and 500 and 502 are listed as themselves.
    let mut expected = Vec::new();
    for (answer, status) in [
        (204, "OK"),
        (302, "Invalid HTTP Response: 302"),
        (404, "Invalid HTTP Response: 404"),
        (502, "502 "),
        (503, "Invalid HTTP Response: 503"),
    ] {
        s.receiver
            .status
            .store(answer, std::sync::atomic::Ordering::SeqCst);
        let sent = s.receiver.got.lock().unwrap().len();
        assert_eq!(call("POST", &again, &s.jwt(), None).0, 202);
        s.receiver.wait(sent + 1).await;
        expected.insert(0, (i64::from(answer), status));
        for _ in 0..200 {
            let newest = call("GET", &format!("{list}?per_page=1"), &s.jwt(), None).2;
            if newest[0]["status_code"] == answer {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let listed = |query: &str| -> Vec<(i64, String)> {
        let (_, _, page) = call("GET", &format!("{list}?{query}"), &s.jwt(), None);
        page.as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["status_code"].as_i64().unwrap(),
                    d["status"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let expected: Vec<(i64, String)> = expected
        .into_iter()
        .map(|(code, status)| (code, status.to_string()))
        .collect();
    assert_eq!(listed("per_page=5"), expected);
    let codes = |query: &str| -> Vec<i64> { listed(query).into_iter().map(|(c, _)| c).collect() };
    assert_eq!(codes("status=success"), [302, 204]);
    assert_eq!(codes("status=failure&per_page=4"), [503, 502, 404, 500]);

    let (status, _, body) = call("POST", &format!("{list}/999/attempts"), &s.jwt(), None);
    assert_eq!((status, &body["message"]), (404, &"Not Found".into()));
    let (status, _, body) = call("POST", &format!("{list}/abc/attempts"), &s.jwt(), None);
    assert_eq!(
        (status, &body["message"]),
        (422, &"The delivery_id parameter must be an integer.".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_suspended_installation_issues_and_uses_no_token() {
    let s = setup_with(sending(), 204).await;
    let api = &s.api;
    let installation = s.install();
    let token = s.token(installation);
    let repo = format!("{api}/repos/acme/web");
    assert_eq!(call("GET", &repo, &bearer(&token), None).0, 200);

    let suspended = format!("{api}/app/installations/{installation}/suspended");
    let one = format!("{api}/app/installations/{installation}");
    // Suspending answers 204, and again while suspended.
    for _ in 0..2 {
        assert_eq!(call("PUT", &suspended, &s.jwt(), None).0, 204);
    }
    let (_, _, read) = call("GET", &one, &s.jwt(), None);
    assert!(read["suspended_at"].as_str().unwrap().ends_with('Z'));
    assert!(!read["suspended_at"].as_str().unwrap().contains('.'));
    assert_eq!(read["suspended_by"]["login"], "helper[bot]");
    assert_eq!(read["suspended_by"]["type"], "Bot");
    let (_, _, listed) = call("GET", &format!("{api}/app/installations"), &s.jwt(), None);
    assert_eq!(listed[0]["id"], installation);

    let tokens = format!("{one}/access_tokens");
    let (status, _, body) = call("POST", &tokens, &s.jwt(), None);
    assert_eq!(
        (status, &body["message"]),
        (403, &"This installation has been suspended".into())
    );
    let (status, _, body) = call("GET", &repo, &bearer(&token), None);
    assert_eq!(
        (status, &body["message"]),
        (
            403,
            &"This GitHub App installation is currently suspended.".into()
        )
    );
    // Git is told the token is no good.
    let mut over_git = agent()
        .get(&format!(
            "{}/acme/web.git/info/refs?service=git-upload-pack",
            s.base
        ))
        .header("authorization", bearer(&token))
        .call()
        .unwrap();
    assert_eq!(over_git.status(), 401);
    assert_eq!(
        over_git.body_mut().read_to_string().unwrap(),
        "Invalid username or token. Password authentication is not supported for Git operations."
    );
    // A suspended installation is sent nothing about its repositories.
    s.push("feature");

    // Lifting it answers 204, and again, and the earlier token works.
    for _ in 0..2 {
        assert_eq!(call("DELETE", &suspended, &s.jwt(), None).0, 204);
    }
    let (_, _, read) = call("GET", &one, &s.jwt(), None);
    assert_eq!(read["suspended_at"], serde_json::Value::Null);
    assert_eq!(read["suspended_by"], serde_json::Value::Null);
    assert_eq!(call("GET", &repo, &bearer(&token), None).0, 200);

    // Removing it answers 204, then 404, and its token is unknown.
    assert_eq!(call("DELETE", &one, &s.jwt(), None).0, 204);
    assert_eq!(call("DELETE", &one, &s.jwt(), None).0, 404);
    assert_eq!(call("GET", &one, &s.jwt(), None).0, 404);
    assert_eq!(call("POST", &tokens, &s.jwt(), None).0, 404);
    let (status, _, body) = call("GET", &repo, &bearer(&token), None);
    assert_eq!((status, &body["message"]), (401, &"Bad credentials".into()));
    // The App is installed on the account again, as a new installation.
    let again = s.install();
    assert!(again > installation);

    let got = s.receiver.wait(5).await;
    let kinds: Vec<(String, Option<String>)> = got.iter().map(Received::kind).collect();
    let actions: Vec<&str> = kinds.iter().map(|(_, a)| a.as_deref().unwrap()).collect();
    assert_eq!(
        actions,
        ["created", "suspend", "unsuspend", "deleted", "created"]
    );
    assert!(kinds.iter().all(|(event, _)| event == "installation"));
    let suspend = got[1].json();
    assert_eq!(suspend["sender"]["login"], "helper[bot]");
    assert_eq!(
        suspend["installation"]["suspended_by"]["login"],
        "helper[bot]"
    );
    assert!(suspend["installation"]["suspended_at"].is_string());
    assert_eq!(
        got[2].json()["installation"]["suspended_at"],
        serde_json::Value::Null
    );
    let deleted = got[3].json();
    assert_eq!(deleted["installation"]["id"], installation);
    assert_eq!(deleted["repositories"][0]["name"], "web");
    // Who asked for an installation is in its `created` alone.
    assert!(deleted.get("requester").is_none());
    let created = got[0].json();
    assert_eq!(created["requester"], serde_json::Value::Null);
    assert_eq!(created["sender"]["login"], "acme");
    assert_eq!(deleted["sender"]["login"], "helper[bot]");
    assert_eq!(got[4].json()["installation"]["id"], again);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_world_that_does_not_send_makes_its_deliveries_and_keeps_them() {
    // The default options send nothing, as a replay needs.
    let s = setup_with(orrery_server::Options::default(), 204).await;
    s.install();
    s.push("feature");
    let log = control_post(
        &s.base,
        "/_orrery/log/list",
        serde_json::json!({ "page_size": 500 }),
    );
    let of = |kind: &str| {
        log["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == kind)
            .count()
    };
    assert_eq!(of("delivery:created"), 3);
    assert_eq!(of("delivery:attempted"), 0);
    // A delivery is made in the write directly after the one it is for, by
    // the same actor through the same door.
    let items = log["items"].as_array().unwrap();
    let made = items
        .iter()
        .position(|e| e["type"] == "delivery:created" && e["object"]["event"] == "installation")
        .unwrap();
    assert_eq!(items[made - 1]["type"], "installation:created");
    assert_eq!(items[made]["door"], items[made - 1]["door"]);
    assert_eq!(items[made]["actor"], items[made - 1]["actor"]);
    assert_eq!(items[made]["clock_unix"], items[made - 1]["clock_unix"]);
    assert_eq!(items[made]["object"]["state"], "pending");
    assert_eq!(items[made]["object"]["url"], s.receiver.url);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(s.receiver.got.lock().unwrap().is_empty());
    // A delivery that was never attempted is in no list.
    let list = format!("{}/app/hook/deliveries", s.api);
    assert_eq!(call("GET", &list, &s.jwt(), None).2, serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_plane_sets_an_apps_webhook() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "acme", "org");
    let register = |body: serde_json::Value| {
        let mut resp = agent()
            .post(&format!("{base}/_orrery/apps"))
            .send_json(body)
            .unwrap();
        let status = resp.status().as_u16();
        let json: serde_json::Value = resp.body_mut().read_json().unwrap();
        (status, json)
    };
    let new = |slug: &str, more: serde_json::Value| {
        let mut body = serde_json::json!({
            "slug": slug,
            "name": slug,
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
        });
        for (key, value) in more.as_object().unwrap() {
            body[key] = value.clone();
        }
        body
    };

    // No webhook unless it is given one, and a secret either way.
    let (status, plain) = register(new("plain", serde_json::json!({})));
    assert_eq!(status, 201, "{plain}");
    assert_eq!(plain["webhook_url"], serde_json::Value::Null);
    assert_eq!(plain["webhook_active"], false);
    let secret = plain["webhook_secret"].as_str().unwrap();
    assert!(secret.starts_with("orrfake_whsec_"), "{secret}");

    for (more, field) in [
        (
            serde_json::json!({ "webhook_url": "ftp://x.test/hook" }),
            "webhook_url",
        ),
        (
            serde_json::json!({ "webhook_url": "https://" }),
            "webhook_url",
        ),
        (
            serde_json::json!({ "webhook_active": true }),
            "webhook_active",
        ),
    ] {
        let (status, refused) = register(new("bad", more));
        assert_eq!(status, 400, "{refused}");
        assert_eq!(faults(&refused), [fault("INVALID_ARGUMENT", Some(field))]);
    }

    // A read answers the webhook and never its secret.
    let one = format!("{base}/_orrery/apps/plain");
    let (_, _, read) = call("GET", &one, "", None);
    assert_eq!(read["webhook_url"], serde_json::Value::Null);
    assert!(!read.to_string().contains("whsec"));

    // PATCH changes the field it names and keeps the other.
    let patch = |body: serde_json::Value| call("PATCH", &one, "", Some(body));
    let (status, _, on) = patch(serde_json::json!({ "webhook_active": true }));
    assert_eq!(status, 400, "{on}");
    assert_eq!(
        faults(&on),
        [fault("INVALID_ARGUMENT", Some("webhook_active"))]
    );
    let (status, _, set) = patch(serde_json::json!({ "webhook_url": "https://x.test/hook" }));
    assert_eq!(status, 200, "{set}");
    assert_eq!(set["webhook_url"], "https://x.test/hook");
    assert_eq!(set["webhook_active"], false);
    let (_, _, on) = patch(serde_json::json!({ "webhook_active": true }));
    assert_eq!(on["webhook_active"], true);
    assert_eq!(on["webhook_url"], "https://x.test/hook");
    let (status, _, cleared) = patch(serde_json::json!({ "webhook_url": null }));
    assert_eq!(status, 400, "{cleared}");
    let (_, _, off) = patch(serde_json::json!({ "webhook_active": false, "webhook_url": null }));
    assert_eq!(off["webhook_url"], serde_json::Value::Null);
    for (body, field) in [
        (
            serde_json::json!({ "webhook_active": null }),
            "webhook_active",
        ),
        (serde_json::json!({ "webhook_url": 3 }), "webhook_url"),
        (serde_json::json!({ "name": "x" }), "name"),
    ] {
        let (status, _, refused) = patch(body);
        assert_eq!(status, 400, "{refused}");
        assert_eq!(faults(&refused)[0].1.as_deref(), Some(field));
    }
    assert_eq!(
        call(
            "PATCH",
            &format!("{base}/_orrery/apps/nobody"),
            "",
            Some(serde_json::json!({}))
        )
        .0,
        404
    );

    // A secret issued replaces the one before, and is a new one.
    let issue = format!("{one}/webhook-secrets");
    let (status, _, issued) = call("POST", &issue, "", None);
    assert_eq!(status, 201, "{issued}");
    assert_eq!(issued["slug"], "plain");
    assert_ne!(issued["webhook_secret"], secret);
    let (status, _, refused) = call("POST", &issue, "", Some(serde_json::json!({ "x": 1 })));
    assert_eq!(status, 400, "{refused}");
    assert_eq!(
        call(
            "POST",
            &format!("{base}/_orrery/apps/nobody/webhook-secrets"),
            "",
            None
        )
        .0,
        404
    );
}
