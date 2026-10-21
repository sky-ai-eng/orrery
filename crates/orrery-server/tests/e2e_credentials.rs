//! End-to-end: no control-plane read returns the world secret or a stored
//! credential. A credential leaves
//! the world only in the response that creates it.

mod common;

use common::{boot_keyed, control_post, mint_actor};
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
    let spent_code = authorize(&http, &base, &client_id, "alice");
    let mut exchanged = http
        .post(format!("{base}/login/oauth/access_token"))
        .header("accept", "application/json")
        .send_form([
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", spent_code.as_str()),
        ])
        .unwrap();
    let exchanged: serde_json::Value = exchanged.body_mut().read_json().unwrap();
    let oauth_token = exchanged["access_token"].as_str().unwrap().to_string();
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
    for credential in [
        "6b".repeat(32),
        alice,
        bob,
        carol,
        client_secret,
        spent_code,
        pending_code,
        oauth_token,
    ] {
        for read in &reads {
            assert!(!read.contains(&credential), "{credential} leaks in {read}");
        }
    }
}
