//! End-to-end: no control-plane read returns the world secret or a stored
//! credential. A credential leaves
//! the world only in the response that creates or issues it, and issuing is
//! how anyone gets one for an actor or app that already exists.

mod common;

use common::{boot_at, boot_keyed, control_post, mint_actor};
use orrery_world::WorldSecret;

const SECRET: WorldSecret = WorldSecret::from_bytes([0x6b; 32]);

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// Authorize the app as `login` and return the code from the redirect.
fn authorize(http: &ureq::Agent, base: &str, client_id: &str, login: &str) -> String {
    let granted = http
        .post(format!("{base}/login/oauth/authorize"))
        .send_form([("client_id", client_id), ("login", login)])
        .unwrap();
    assert_eq!(granted.status(), 302);
    let back = granted.headers()["location"].to_str().unwrap();
    back.split_once("code=")
        .expect("a code in the redirect")
        .1
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

/// Trade `code` for a token: `Ok(token)`, or `Err(error)` with GitHub's
/// error code.
fn exchange(
    http: &ureq::Agent,
    base: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<String, String> {
    let mut resp = http
        .post(format!("{base}/login/oauth/access_token"))
        .header("accept", "application/json")
        .send_form([
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
        ])
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.body_mut().read_json().unwrap();
    match body["access_token"].as_str() {
        Some(token) => Ok(token.to_string()),
        None => Err(body["error"].as_str().unwrap().to_string()),
    }
}

/// A control-plane POST with no body: `(status, json)`.
fn control_issue(http: &ureq::Agent, base: &str, path: &str) -> (u16, serde_json::Value) {
    let mut resp = http
        .post(format!("{base}/_orrery{path}"))
        .send_empty()
        .unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
}

/// The login a face token acts as, or the status that refused it.
fn login_of(http: &ureq::Agent, base: &str, token: &str) -> Result<String, u16> {
    let mut resp = http
        .get(format!("{base}/api/v3/user"))
        .header("authorization", &format!("token {token}"))
        .call()
        .unwrap();
    match resp.status().as_u16() {
        200 => {
            let user: serde_json::Value = resp.body_mut().read_json().unwrap();
            Ok(user["login"].as_str().unwrap().to_string())
        }
        status => Err(status),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_control_plane_read_returns_a_credential() {
    let inst = boot_keyed(&SECRET).await;
    let base = inst.base.clone();
    let http = agent();
    let control = |method: &str, path: &str, body: Option<serde_json::Value>| -> String {
        let url = format!("{base}/_orrery{path}");
        let mut resp = match (method, body) {
            ("GET", None) => http.get(&url).call(),
            ("POST", None) => http.post(&url).send_empty(),
            ("POST", Some(b)) => http.post(&url).send_json(b),
            other => panic!("unsupported control call {other:?}"),
        }
        .unwrap();
        assert!(resp.status().is_success(), "{method} {path}");
        resp.body_mut().read_to_string().unwrap()
    };

    let mut reads = Vec::new();
    reads.push(control(
        "POST",
        "/snapshots",
        Some(serde_json::json!({ "name": "empty" })),
    ));

    let alice = mint_actor(&base, "alice", "user");
    let bob = mint_actor(&base, "bob", "user");
    let app = control_post(
        &base,
        "/_orrery/oauth-apps",
        serde_json::json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
    );
    let client_id = app["client_id"].as_str().unwrap().to_string();
    let client_secret = app["client_secret"].as_str().unwrap().to_string();
    let issued_token = control_issue(&http, &base, "/actors/alice/tokens").1["token"]
        .as_str()
        .unwrap()
        .to_string();
    let issued_secret = control_issue(&http, &base, &format!("/oauth-apps/{client_id}/secrets")).1
        ["client_secret"]
        .as_str()
        .unwrap()
        .to_string();
    let spent_code = authorize(&http, &base, &client_id, "alice");
    let oauth_token = exchange(&http, &base, &client_id, &issued_secret, &spent_code).unwrap();
    let pending_code = authorize(&http, &base, &client_id, "bob");
    reads.push(control(
        "POST",
        "/clock/advance",
        Some(serde_json::json!({ "seconds": 60 })),
    ));
    reads.push(control(
        "POST",
        "/snapshots",
        Some(serde_json::json!({ "name": "seeded" })),
    ));
    let carol = mint_actor(&base, "carol", "user");

    reads.push(control("GET", "/healthz", None));
    reads.push(control("GET", "/clock", None));
    reads.push(control(
        "POST",
        "/log/list",
        Some(serde_json::json!({ "page_size": 500 })),
    ));
    reads.push(control("POST", "/snapshots/list", None));
    reads.push(control("GET", "/snapshots/empty", None));
    reads.push(control("GET", "/snapshots/seeded", None));
    reads.push(control(
        "POST",
        "/diff",
        Some(serde_json::json!({ "from": "empty" })),
    ));
    reads.push(control(
        "POST",
        "/diff",
        Some(serde_json::json!({ "from": "empty", "to": "seeded" })),
    ));
    reads.push(control("POST", "/snapshots/seeded/restore", None));
    reads.push(control(
        "POST",
        "/log/list",
        Some(serde_json::json!({ "page_size": 500 })),
    ));

    // The walk saw the history it was meant to: every action, and the
    // credentials' objects in the diff.
    let all = reads.join("\n");
    for expected in ["oauth.authorize", "oauth.token", "authorizations_exchanged"] {
        assert!(all.contains(expected), "no {expected} in {all}");
    }
    // Issuing logs which credential it was, never its value.
    let log: serde_json::Value = serde_json::from_str(reads.last().unwrap()).unwrap();
    let issues: Vec<_> = log["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"].as_str().unwrap().ends_with(".issue"))
        .map(|e| (e["action"].as_str().unwrap(), &e["details"]))
        .collect();
    assert_eq!(
        issues,
        [
            (
                "actor.token.issue",
                &serde_json::json!({ "login": "alice", "n": 1 })
            ),
            (
                "oauth_app.secret.issue",
                &serde_json::json!({ "client_id": client_id, "n": 1 })
            ),
        ]
    );
    for credential in [
        "6b".repeat(32),
        alice,
        bob,
        carol,
        issued_token,
        client_secret,
        issued_secret,
        spent_code,
        pending_code,
        oauth_token,
    ] {
        for read in &reads {
            assert!(!read.contains(&credential), "{credential} leaks in {read}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn issued_credentials_work_beside_the_first() {
    let inst = boot_keyed(&SECRET).await;
    let base = inst.base.clone();
    let http = agent();
    let created = mint_actor(&base, "alice", "user");

    let (status, first) = control_issue(&http, &base, "/actors/alice/tokens");
    assert_eq!(status, 201);
    let alice = inst.world.actor_by_login("alice").unwrap().unwrap();
    assert_eq!(
        first["actor"],
        serde_json::json!({ "id": alice.id, "login": "alice", "kind": "user" })
    );
    let first = first["token"].as_str().unwrap().to_string();
    let second = control_issue(&http, &base, "/actors/alice/tokens").1["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);
    for token in [&created, &first, &second] {
        assert_eq!(login_of(&http, &base, token).as_deref(), Ok("alice"));
    }

    // Refusals, in the control plane's error shape.
    let (status, body) = control_issue(&http, &base, "/actors/ghost/tokens");
    assert_eq!(status, 404);
    assert_eq!(body["errors"][0]["reason"], "NOT_FOUND");
    let (status, body) = control_issue(&http, &base, "/actors/orrery/tokens");
    assert_eq!(status, 400);
    assert_eq!(body["errors"][0]["reason"], "INVALID_ARGUMENT");
    assert_eq!(body["errors"][0]["field"], "login");

    let app = control_post(
        &base,
        "/_orrery/oauth-apps",
        serde_json::json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
    );
    let client_id = app["client_id"].as_str().unwrap();
    let (status, issued) = control_issue(&http, &base, &format!("/oauth-apps/{client_id}/secrets"));
    assert_eq!(status, 201);
    let issued_secret = issued["client_secret"].as_str().unwrap();
    assert_eq!(
        issued,
        serde_json::json!({ "client_id": client_id, "client_secret": issued_secret })
    );
    assert_ne!(issued_secret, app["client_secret"]);
    for secret in [issued_secret, app["client_secret"].as_str().unwrap()] {
        let code = authorize(&http, &base, client_id, "alice");
        let token = exchange(&http, &base, client_id, secret, &code).unwrap();
        assert_eq!(login_of(&http, &base, &token).as_deref(), Ok("alice"));
    }
    let (status, body) = control_issue(&http, &base, "/oauth-apps/orrfake_client_nope/secrets");
    assert_eq!(status, 404);
    assert_eq!(body["errors"][0]["reason"], "NOT_FOUND");
}

/// A world made from another's snapshot is re-keyed, so
/// nobody knows its credentials until the control plane issues new ones.
#[tokio::test(flavor = "multi_thread")]
async fn a_rekeyed_world_is_entered_through_issued_credentials() {
    let source = boot_keyed(&SECRET).await;
    let http = agent();
    let source_token = mint_actor(&source.base, "alice", "user");
    let app = control_post(
        &source.base,
        "/_orrery/oauth-apps",
        serde_json::json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
    );
    let client_id = app["client_id"].as_str().unwrap();
    let source_secret = app["client_secret"].as_str().unwrap();
    let source_issued = control_issue(
        &http,
        &source.base,
        &format!("/oauth-apps/{client_id}/secrets"),
    )
    .1["client_secret"]
        .as_str()
        .unwrap()
        .to_string();
    control_post(
        &source.base,
        "/_orrery/snapshots",
        serde_json::json!({ "name": "seed" }),
    );

    let dest = tempfile::tempdir().unwrap();
    let forked_root = orrery_snapshot::fork(
        &source.dir.path().join("snapshots"),
        "seed",
        dest.path(),
        &WorldSecret::from_bytes([0x2e; 32]),
    )
    .unwrap();
    let fork = boot_at(dest, &forked_root).await;

    assert_eq!(login_of(&http, &fork.base, &source_token), Err(401));
    let token = control_issue(&http, &fork.base, "/actors/alice/tokens").1["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(login_of(&http, &fork.base, &token).as_deref(), Ok("alice"));
    assert_eq!(login_of(&http, &source.base, &token), Err(401));

    for secret in [source_secret, &source_issued] {
        let code = authorize(&http, &fork.base, client_id, "alice");
        assert_eq!(
            exchange(&http, &fork.base, client_id, secret, &code).unwrap_err(),
            "incorrect_client_credentials"
        );
    }
    let issued = control_issue(
        &http,
        &fork.base,
        &format!("/oauth-apps/{client_id}/secrets"),
    )
    .1["client_secret"]
        .as_str()
        .unwrap()
        .to_string();
    let code = authorize(&http, &fork.base, client_id, "alice");
    let oauth_token = exchange(&http, &fork.base, client_id, &issued, &code).unwrap();
    assert_eq!(
        login_of(&http, &fork.base, &oauth_token).as_deref(),
        Ok("alice")
    );
}
