//! End-to-end for GitHub's OAuth web flow (spec/surface-github.md §2): the
//! authorize page, the code exchange in both response encodings, the token's
//! use on the identity reads, and the refusals each step makes.

mod common;

use common::{boot, control_post};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

fn location(resp: &ureq::http::Response<ureq::Body>) -> String {
    resp.headers()
        .get("location")
        .expect("a Location header")
        .to_str()
        .unwrap()
        .to_string()
}

fn query_value(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_web_flow() {
    let inst = boot().await;
    let base = inst.base.clone();
    let http = agent();

    control_post(
        &base,
        "/_orrery/actors",
        serde_json::json!({ "login": "alice", "kind": "user" }),
    );
    control_post(
        &base,
        "/_orrery/actors",
        serde_json::json!({ "login": "ci", "kind": "bot" }),
    );
    let mut set = http
        .put(format!("{base}/_orrery/actors/alice/emails"))
        .send_json(serde_json::json!({ "emails": [
            { "email": "alice@corp.test", "primary": true, "verified": true },
            { "email": "alice@home.test", "primary": false, "verified": false },
        ]}))
        .unwrap();
    assert_eq!(set.status(), 200);
    let set: serde_json::Value = set.body_mut().read_json().unwrap();
    assert_eq!(set["emails"][0]["email"], "alice@corp.test");

    let app = control_post(
        &base,
        "/_orrery/oauth-apps",
        serde_json::json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
    );
    let client_id = app["client_id"].as_str().unwrap().to_string();
    let client_secret = app["client_secret"].as_str().unwrap().to_string();
    let authorize = format!("{base}/login/oauth/authorize");

    // The page names the app and carries the request through its form.
    let mut page = http
        .get(format!(
            "{authorize}?client_id={client_id}&scope=user%3Aemail&state=s1"
        ))
        .call()
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.body_mut().read_to_string().unwrap();
    assert!(html.contains("Authorize Sign-in"), "{html}");
    assert!(html.contains(r#"name="state" value="s1""#), "{html}");
    assert!(
        html.contains(r#"name="redirect_uri" value="http://auth.test/callback""#),
        "{html}"
    );

    // Unknown client: GitHub's 404 page. A redirect off the callback is sent
    // back to the callback as an error.
    let unknown = http
        .get(format!("{authorize}?client_id=nope"))
        .call()
        .unwrap();
    assert_eq!(unknown.status(), 404);
    let off = http
        .get(format!(
            "{authorize}?client_id={client_id}&redirect_uri=http%3A%2F%2Fevil.test%2Fcallback&state=s2"
        ))
        .call()
        .unwrap();
    assert_eq!(off.status(), 302);
    let back = location(&off);
    assert!(back.starts_with("http://auth.test/callback?"), "{back}");
    assert_eq!(
        query_value(&back, "error").as_deref(),
        Some("redirect_uri_mismatch")
    );
    assert_eq!(query_value(&back, "state").as_deref(), Some("s2"));

    // Signing in as no one, or as a bot, re-renders the page.
    for login in ["ghost", "ci"] {
        let mut refused = http
            .post(&authorize)
            .send_form([
                ("client_id", client_id.as_str()),
                ("login", login),
                ("state", "s1"),
            ])
            .unwrap();
        assert_eq!(refused.status(), 200);
        assert!(
            refused
                .body_mut()
                .read_to_string()
                .unwrap()
                .contains("Incorrect username or password."),
            "{login}"
        );
    }

    // A sub-path of the callback is allowed and the code comes back to it.
    let granted = http
        .post(&authorize)
        .send_form([
            ("client_id", client_id.as_str()),
            ("redirect_uri", "http://auth.test/callback/deeper"),
            ("scope", "user:email"),
            ("state", "s1"),
            ("login", "alice"),
        ])
        .unwrap();
    assert_eq!(granted.status(), 302);
    let back = location(&granted);
    assert!(
        back.starts_with("http://auth.test/callback/deeper?code="),
        "{back}"
    );
    assert_eq!(query_value(&back, "state").as_deref(), Some("s1"));
    let code = query_value(&back, "code").unwrap();

    // The exchange: JSON when asked for, client credentials as HTTP Basic.
    let token_url = format!("{base}/login/oauth/access_token");
    let basic = base64_basic(&client_id, &client_secret);
    let mut exchanged = http
        .post(&token_url)
        .header("accept", "application/json")
        .header("authorization", &basic)
        .send_form([
            ("code", code.as_str()),
            ("redirect_uri", "http://auth.test/callback/deeper"),
        ])
        .unwrap();
    assert_eq!(exchanged.status(), 200);
    let exchanged: serde_json::Value = exchanged.body_mut().read_json().unwrap();
    assert_eq!(exchanged["token_type"], "bearer");
    assert_eq!(exchanged["scope"], "user:email");
    let token = exchanged["access_token"].as_str().unwrap().to_string();

    // The token reads the actor and the addresses the control plane set.
    let mut user = http
        .get(format!("{base}/api/v3/user"))
        .header("authorization", &format!("token {token}"))
        .call()
        .unwrap();
    assert_eq!(user.status(), 200);
    let user: serde_json::Value = user.body_mut().read_json().unwrap();
    assert_eq!(user["login"], "alice");
    let mut emails = http
        .get(format!("{base}/api/v3/user/emails"))
        .header("authorization", &format!("Bearer {token}"))
        .call()
        .unwrap();
    let emails: serde_json::Value = emails.body_mut().read_json().unwrap();
    assert_eq!(
        emails,
        serde_json::json!([
            { "email": "alice@corp.test", "primary": true, "verified": true, "visibility": "private" },
            { "email": "alice@home.test", "primary": false, "verified": false, "visibility": null },
        ])
    );

    // A spent code is refused, form-encoded by default, still with a 200.
    let mut again = http
        .post(&token_url)
        .send_form([
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
        ])
        .unwrap();
    assert_eq!(again.status(), 200);
    let body = again.body_mut().read_to_string().unwrap();
    assert!(body.starts_with("error=bad_verification_code&"), "{body}");

    let mut wrong = http
        .post(&token_url)
        .header("accept", "application/json")
        .send_json(serde_json::json!({
            "client_id": client_id, "client_secret": "nope", "code": "x",
        }))
        .unwrap();
    let wrong: serde_json::Value = wrong.body_mut().read_json().unwrap();
    assert_eq!(wrong["error"], "incorrect_client_credentials");

    // XML is refused by name, as a field like any other.
    let mut xml = http
        .post(&token_url)
        .header("accept", "application/xml")
        .send_form([
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", "x"),
        ])
        .unwrap();
    assert_eq!(xml.status(), 422);
    let xml: serde_json::Value = xml.body_mut().read_json().unwrap();
    assert_eq!(xml["orrery"]["not_mocked"], true, "{xml}");
    assert_eq!(xml["orrery"]["field"], "Accept", "{xml}");
    assert_eq!(xml["errors"][0]["field"], "Accept", "{xml}");
    assert_eq!(xml["errors"][0]["resource"], "OauthAccess", "{xml}");

    // The control plane validates strictly and names the field.
    let bad = http
        .put(format!("{base}/_orrery/actors/alice/emails"))
        .send_json(serde_json::json!({ "emails": [
            { "email": "a@corp.test", "primary": false, "verified": true },
        ]}))
        .unwrap();
    assert_eq!(bad.status(), 400);
    let unknown_field = http
        .post(format!("{base}/_orrery/oauth-apps"))
        .send_json(serde_json::json!({ "name": "x", "callback_url": "http://a.test/", "extra": 1 }))
        .unwrap();
    assert_eq!(unknown_field.status(), 400);
    let missing_actor = http
        .put(format!("{base}/_orrery/actors/ghost/emails"))
        .send_json(serde_json::json!({ "emails": [
            { "email": "g@corp.test", "primary": true, "verified": true },
        ]}))
        .unwrap();
    assert_eq!(missing_actor.status(), 404);
}

fn base64_basic(id: &str, secret: &str) -> String {
    use base64::Engine as _;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"))
    )
}
