//! End-to-end for the pages that register and install a GitHub App
//! (spec/surface-github.md §2.2): the manifest form and the conversion of
//! its code, the install page and where it sends a person, and the two
//! reads a caller makes with a person's token afterwards.

mod common;

use common::{app_key_from_pem, boot, control_post, mint_actor};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

fn query_value(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
        .map(str::to_string)
}

/// A form post that follows no redirect: the status, the `Location`, and
/// the page.
fn post_form(url: &str, fields: &[(&str, &str)]) -> (u16, Option<String>, String) {
    let mut resp = agent().post(url).send_form(fields.to_vec()).unwrap();
    let status = resp.status().as_u16();
    let to = resp
        .headers()
        .get("location")
        .map(|v| v.to_str().unwrap().to_string());
    (status, to, resp.body_mut().read_to_string().unwrap())
}

fn get(url: &str, authorization: Option<&str>) -> (u16, Option<String>, serde_json::Value) {
    let mut req = agent().get(url);
    if let Some(authorization) = authorization {
        req = req.header("authorization", authorization);
    }
    let mut resp = req.call().unwrap();
    let status = resp.status().as_u16();
    let link = resp
        .headers()
        .get("link")
        .map(|v| v.to_str().unwrap().to_string());
    let text = resp.body_mut().read_to_string().unwrap();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, link, json)
}

fn log_types(base: &str) -> Vec<(String, String, String)> {
    control_post(
        base,
        "/_orrery/log/list",
        serde_json::json!({ "page_size": 200 }),
    )["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["type"].as_str().unwrap().to_string(),
                e["actor"]["login"].as_str().unwrap().to_string(),
                e["door"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn manifest(name: &str) -> String {
    serde_json::json!({
        "name": name,
        "url": "https://helper.test",
        "redirect_url": "http://127.0.0.1:9/manifest/callback",
        "callback_urls": ["https://helper.test/callback", "https://helper.test/other"],
        "public": false,
        "default_permissions": { "contents": "read", "pull_requests": "write" },
        "default_events": ["pull_request", "push"],
        "hook_attributes": { "url": "https://helper.test/hook", "active": false },
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_registers_an_app_whose_credentials_work() {
    let inst = boot().await;
    let base = &inst.base;
    let api = format!("{base}/api/v3");
    mint_actor(base, "alice", "user");
    mint_actor(base, "acme", "org");
    mint_actor(base, "ci", "bot");
    let form = format!("{base}/settings/apps/new?state=xyz");
    let manifest = manifest("Helper App");

    // The caller's page posts the manifest, and the person gets a page to
    // confirm on. Nothing is registered yet.
    let (status, _, page) = post_form(&form, &[("manifest", &manifest)]);
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Create GitHub App"), "{page}");
    assert!(
        page.contains("action=\"/settings/apps/new?state=xyz\""),
        "{page}"
    );
    assert!(page.contains("value=\"Helper App\""), "{page}");
    assert_eq!(get(&format!("{base}/_orrery/apps/helper-app"), None).0, 404);

    // Signing in as no one, or as a bot, shows the page again.
    for login in ["ghost", "ci"] {
        let (status, _, page) = post_form(&form, &[("manifest", &manifest), ("login", login)]);
        assert_eq!(status, 200);
        assert!(page.contains("Incorrect username or password."), "{login}");
    }

    // Confirming registers the App and sends the person back with a code.
    let (status, back, _) = post_form(&form, &[("manifest", &manifest), ("login", "alice")]);
    assert_eq!(status, 302);
    let back = back.unwrap();
    assert!(
        back.starts_with("http://127.0.0.1:9/manifest/callback?code="),
        "{back}"
    );
    assert_eq!(query_value(&back, "state").as_deref(), Some("xyz"));
    let code = query_value(&back, "code").unwrap();
    assert_eq!(code.len(), 40, "{code}");

    // The code is traded once, with no credential, for the App and what it
    // is given.
    let conversions = format!("{api}/app-manifests/{code}/conversions");
    let mut converted = agent().post(&conversions).send_empty().unwrap();
    assert_eq!(converted.status(), 201);
    let app: serde_json::Value = converted.body_mut().read_json().unwrap();
    assert_eq!(app["slug"], "helper-app");
    assert_eq!(app["name"], "Helper App");
    assert_eq!(app["owner"]["login"], "alice");
    assert_eq!(
        app["permissions"],
        serde_json::json!({ "contents": "read", "pull_requests": "write" })
    );
    assert_eq!(app["events"], serde_json::json!(["pull_request", "push"]));
    for key in [
        "id",
        "client_id",
        "node_id",
        "html_url",
        "created_at",
        "updated_at",
    ] {
        assert!(!app[key].is_null(), "{key}");
    }
    let pem = app["pem"].as_str().unwrap();
    assert!(
        pem.starts_with("-----BEGIN RSA PRIVATE KEY-----\n"),
        "{pem}"
    );
    let client_secret = app["client_secret"].as_str().unwrap();
    assert!(client_secret.starts_with("orrfake_secret_"));
    // A webhook that starts inactive has a secret all the same, as on
    // GitHub.
    assert!(
        app["webhook_secret"]
            .as_str()
            .unwrap()
            .starts_with("orrfake_whsec_")
    );
    assert_eq!(
        agent().post(&conversions).send_empty().unwrap().status(),
        404,
        "a code is traded once"
    );

    // The key signs as the App, and the secret is the App's.
    let key = app_key_from_pem(pem);
    let now = inst.world.clock().unwrap();
    let jwt = format!("Bearer {}", key.jwt(app["id"].clone(), now));
    let (status, _, itself) = get(&format!("{api}/app"), Some(&jwt));
    assert_eq!(status, 200, "{itself}");
    assert_eq!(itself["slug"], "helper-app");
    let (status, _, config) = get(&format!("{api}/app/hook/config"), Some(&jwt));
    assert_eq!(status, 200);
    assert_eq!(config["url"], "https://helper.test/hook");
    let mut exchange = agent()
        .post(&format!("{base}/login/oauth/access_token"))
        .header("accept", "application/json")
        .send_form([
            ("client_id", app["client_id"].as_str().unwrap()),
            ("client_secret", client_secret),
            ("code", "nope"),
        ])
        .unwrap();
    let refused: serde_json::Value = exchange.body_mut().read_json().unwrap();
    assert_eq!(refused["error"], "bad_verification_code", "{refused}");

    // The person registered it, through the page, and the conversion made
    // its key.
    let log = log_types(base);
    let from = log.iter().position(|e| e.0 == "app:created").unwrap();
    let wrote: Vec<(&str, &str, &str)> = log[from..]
        .iter()
        .map(|(t, a, d)| (t.as_str(), a.as_str(), d.as_str()))
        .collect();
    assert_eq!(
        wrote,
        [
            ("app:created", "alice", "web"),
            ("actor:created", "alice", "web"),
            ("oauth_app:created", "alice", "web"),
            ("app_manifest:created", "alice", "web"),
            ("app_manifest:converted", "alice", "rest"),
            ("app_key:created", "alice", "rest"),
        ]
    );
    // The control plane reads it as it reads any App, with its one key.
    let (_, _, read) = get(&format!("{base}/_orrery/apps/helper-app"), None);
    assert_eq!(read["keys"].as_array().unwrap().len(), 1);
    assert_eq!(read["webhook_active"], false);
    assert_eq!(
        read["callback_urls"],
        serde_json::json!(["https://helper.test/callback", "https://helper.test/other"])
    );

    // What the page refuses.
    let (status, _, page) = post_form(&form, &[("manifest", &manifest), ("login", "alice")]);
    assert_eq!(status, 422);
    assert!(page.contains("Name is already in use"), "{page}");
    for (body, message) in [
        ("{nope", "Invalid GitHub App manifest"),
        (
            r#"{"name":"A name that is far too long to be an App's","url":"https://x.test"}"#,
            "Name is too long (maximum is 34 characters)",
        ),
        (r#"{"name":"No Home"}"#, "Url can't be blank"),
        (r#"{"url":"https://x.test"}"#, "Name can't be blank"),
        (
            r#"{"name":"Bad Event","url":"https://x.test","default_events":["nope nope"]}"#,
            "events",
        ),
    ] {
        let (status, _, page) = post_form(&form, &[("manifest", body), ("login", "alice")]);
        assert_eq!(status, 422, "{body}: {page}");
        // The page escapes what it shows.
        assert!(
            page.contains(&message.replace('\'', "&#39;")),
            "{body}: {page}"
        );
    }
    assert_eq!(
        agent()
            .post(&format!("{api}/app-manifests/0000/conversions"))
            .send_empty()
            .unwrap()
            .status(),
        404
    );
}

/// A manifest with the fields, permissions, and events a caller that runs
/// as an App commonly sends, and a name that is not already a slug.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_manifest_registers_and_converts() {
    let inst = boot().await;
    let base = &inst.base;
    let api = format!("{base}/api/v3");
    mint_actor(base, "alice", "user");
    mint_actor(base, "acme", "org");
    let manifest = serde_json::json!({
        "name": "Review Helper (Acme Corp)",
        "url": "https://www.helper.test",
        "redirect_url": "http://127.0.0.1:9/api/orgs/7/github/app/register/callback",
        "callback_urls": [
            "http://127.0.0.1:9/api/orgs/7/github/app/register/callback",
            "http://127.0.0.1:9/api/orgs/7/github/connect/callback",
        ],
        "public": false,
        "default_permissions": {
            "emails": "read",
            "issues": "write",
            "pull_requests": "write",
            "contents": "write",
            "metadata": "read",
            "checks": "read",
            "actions": "read",
            "statuses": "read",
            "members": "read",
            "vulnerability_alerts": "read",
        },
        "default_events": [
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "issue_comment",
            "push",
            "check_run",
            "check_suite",
            "dependabot_alert",
        ],
        "hook_attributes": { "url": "https://example.com/unused-webhook", "active": false },
    })
    .to_string();
    // The state a caller signs is an opaque string with characters a URL
    // escapes, and comes back as it was sent.
    let state = "eyJvcmciOiI3In0=.c2lnbmF0dXJl+/==";
    let form = format!(
        "{base}/organizations/acme/settings/apps/new?state={}",
        state
            .replace('+', "%2B")
            .replace('/', "%2F")
            .replace('=', "%3D")
    );
    let (status, back, page) = post_form(&form, &[("manifest", &manifest), ("login", "alice")]);
    assert_eq!(status, 302, "{page}");
    let back = back.unwrap();
    let returned = query_value(&back, "state").unwrap();
    assert_eq!(
        returned
            .replace("%2B", "+")
            .replace("%2F", "/")
            .replace("%3D", "="),
        state
    );
    let code = query_value(&back, "code").unwrap();
    let mut converted = agent()
        .post(&format!("{api}/app-manifests/{code}/conversions"))
        .header("accept", "application/vnd.github+json")
        .send_empty()
        .unwrap();
    assert_eq!(converted.status(), 201);
    let app: serde_json::Value = converted.body_mut().read_json().unwrap();
    assert!(app["id"].is_i64());
    assert_eq!(app["slug"], "review-helper-acme-corp");
    assert_eq!(app["owner"]["login"], "acme");
    for key in ["client_id", "client_secret", "pem", "webhook_secret"] {
        assert!(!app[key].as_str().unwrap().trim().is_empty(), "{key}");
    }
    assert_eq!(app["permissions"]["vulnerability_alerts"], "read");
    assert_eq!(app["events"].as_array().unwrap().len(), 8);
    // The App's bot is found by the name GitHub gives it.
    let jwt = format!(
        "Bearer {}",
        app_key_from_pem(app["pem"].as_str().unwrap())
            .jwt(app["id"].clone(), inst.world.clock().unwrap())
    );
    assert_eq!(get(&format!("{api}/app"), Some(&jwt)).0, 200);
    let alice = mint_actor(base, "reader", "user");
    let (status, _, bot) = get(
        &format!("{api}/users/review-helper-acme-corp%5Bbot%5D"),
        Some(&format!("Bearer {alice}")),
    );
    assert_eq!(status, 200, "{bot}");
    assert_eq!(bot["type"], "Bot");
    // And the install page is at the slug the conversion answered.
    assert_eq!(
        agent()
            .get(&format!(
                "{base}/apps/review-helper-acme-corp/installations/new"
            ))
            .call()
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manifest_registers_an_app_for_an_organization_and_its_code_expires() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    mint_actor(base, "acme", "org");
    // A manifest with no callback URL is given the one it sends the person
    // back to, and one with no webhook has none.
    let manifest = serde_json::json!({
        "name": "Org Helper",
        "url": "https://helper.test",
        "redirect_url": "https://helper.test/done",
        "setup_url": "https://helper.test/setup",
    })
    .to_string();
    let form = format!("{base}/organizations/acme/settings/apps/new");
    let (status, _, page) = post_form(&form, &[("manifest", &manifest)]);
    assert_eq!(status, 200);
    assert!(page.contains("Create GitHub App for acme"), "{page}");
    let (status, back, _) = post_form(&form, &[("manifest", &manifest), ("login", "alice")]);
    assert_eq!(status, 302);
    let back = back.unwrap();
    assert!(query_value(&back, "state").is_none(), "{back}");
    let code = query_value(&back, "code").unwrap();
    let (_, _, read) = get(&format!("{base}/_orrery/apps/org-helper"), None);
    assert_eq!(read["owner"], "acme");
    assert_eq!(
        read["callback_urls"],
        serde_json::json!(["https://helper.test/done"])
    );
    assert_eq!(read["webhook_url"], serde_json::Value::Null);
    assert_eq!(read["setup_url"], "https://helper.test/setup");
    assert_eq!(read["keys"], serde_json::json!([]));

    // A code lasts an hour of the world's clock.
    control_post(
        base,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 3600 }),
    );
    let conversions = format!("{base}/api/v3/app-manifests/{code}/conversions");
    assert_eq!(
        agent().post(&conversions).send_empty().unwrap().status(),
        404
    );
    for path in [
        "/organizations/nobody/settings/apps/new",
        "/organizations/alice/settings/apps/new",
    ] {
        let (status, _, _) = post_form(&format!("{base}{path}"), &[("manifest", &manifest)]);
        assert_eq!(status, 404, "{path}");
    }
}

/// A world with alice, bob, the organization acme and its repository `web`,
/// and three Apps: `helper` with a setup URL, `signin`, which asks for
/// authorization on install, and `plain`, with neither.
async fn installable() -> (common::Instance, String, serde_json::Value) {
    let inst = boot().await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    mint_actor(&base, "bob", "user");
    mint_actor(&base, "acme", "org");
    common::face_ok(
        "POST",
        &format!("{base}/api/v3/orgs/acme/repos"),
        &alice,
        serde_json::json!({ "name": "web", "private": true }),
    );
    let register = |slug: &str, more: serde_json::Value| {
        let mut body = serde_json::json!({
            "slug": slug,
            "name": slug,
            "owner": "acme",
            "callback_urls": [format!("https://{slug}.test/callback")],
        });
        for (key, value) in more.as_object().unwrap() {
            body[key] = value.clone();
        }
        control_post(&base, "/_orrery/apps", body)
    };
    register(
        "helper",
        serde_json::json!({ "setup_url": "https://helper.test/setup" }),
    );
    let signin = register(
        "signin",
        serde_json::json!({ "request_oauth_on_install": true }),
    );
    register("plain", serde_json::json!({}));
    (inst, alice, signin)
}

fn installations(base: &str, slug: &str) -> Vec<serde_json::Value> {
    control_post(
        base,
        &format!("/_orrery/apps/{slug}/installations/list"),
        serde_json::json!({}),
    )["items"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_person_installs_an_app_from_its_page() {
    let (inst, _, _) = installable().await;
    let base = &inst.base;
    let page_url = format!("{base}/apps/helper/installations/new");

    let mut page = agent().get(&page_url).call().unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.body_mut()
            .read_to_string()
            .unwrap()
            .contains("Install helper")
    );
    assert_eq!(
        agent()
            .get(&format!("{base}/apps/nobody/installations/new"))
            .call()
            .unwrap()
            .status(),
        404
    );

    // Signing in as no one shows the page again, and installs nothing.
    let (status, _, again) = post_form(&page_url, &[("login", "ghost"), ("account", "acme")]);
    assert_eq!(status, 200);
    assert!(again.contains("Incorrect username or password."));
    assert!(installations(base, "helper").is_empty());

    // Installing sends the person to the App's setup URL, with the state
    // the install URL carried.
    let (status, to, _) = post_form(
        &format!("{page_url}?state=s1"),
        &[
            ("login", "alice"),
            ("account", "acme"),
            ("repository_selection", "selected"),
            ("repositories", "web"),
            ("setup_action", "install"),
        ],
    );
    assert_eq!(status, 302);
    let to = to.unwrap();
    assert!(
        to.starts_with("https://helper.test/setup?installation_id="),
        "{to}"
    );
    assert_eq!(query_value(&to, "setup_action").as_deref(), Some("install"));
    assert_eq!(query_value(&to, "state").as_deref(), Some("s1"));
    let made = installations(base, "helper");
    assert_eq!(made.len(), 1);
    assert_eq!(made[0]["account"], "acme");
    assert_eq!(made[0]["repository_selection"], "selected");
    assert_eq!(
        query_value(&to, "installation_id").unwrap(),
        made[0]["id"].to_string()
    );
    // The person installed it, through the page and the App.
    let log = log_types(base);
    let created = log.iter().find(|e| e.0 == "installation:created").unwrap();
    assert_eq!((created.1.as_str(), created.2.as_str()), ("alice", "web"));

    // An account that has the App is shown its installation.
    let (status, to, _) = post_form(&page_url, &[("login", "bob"), ("account", "acme")]);
    assert_eq!(status, 302);
    let shown = to.unwrap();
    assert_eq!(
        shown,
        format!(
            "{base}/organizations/acme/settings/installations/{}",
            made[0]["id"]
        )
    );
    assert_eq!(agent().get(&shown).call().unwrap().status(), 200);
    assert_eq!(
        agent()
            .get(&format!("{base}/settings/installations/{}", made[0]["id"]))
            .call()
            .unwrap()
            .status(),
        404,
        "an organization's installation is under the organization"
    );

    // A person who may only ask is sent on with no installation.
    let (status, to, _) = post_form(&page_url, &[("login", "bob"), ("setup_action", "request")]);
    assert_eq!(status, 302);
    assert_eq!(
        to.unwrap(),
        "https://helper.test/setup?setup_action=request"
    );
    assert_eq!(installations(base, "helper").len(), 1);

    // With no setup URL the person lands on the installation, which is on
    // their own account unless they name another.
    let (status, to, _) = post_form(
        &format!("{base}/apps/plain/installations/new"),
        &[("login", "bob")],
    );
    assert_eq!(status, 302);
    let plain = installations(base, "plain");
    assert_eq!(plain[0]["account"], "bob");
    assert_eq!(plain[0]["repository_selection"], "all");
    let own = to.unwrap();
    assert_eq!(
        own,
        format!("{base}/settings/installations/{}", plain[0]["id"])
    );
    assert_eq!(agent().get(&own).call().unwrap().status(), 200);

    // What the page refuses.
    for (fields, message) in [
        (vec![("login", "alice"), ("account", "nobody")], "account"),
        (
            vec![
                ("login", "alice"),
                ("account", "alice"),
                ("repository_selection", "selected"),
                ("repositories", "nope"),
            ],
            "repositories",
        ),
    ] {
        let (status, _, page) = post_form(&format!("{base}/apps/plain/installations/new"), &fields);
        assert_eq!(status, 422, "{page}");
        assert!(page.contains(message), "{page}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_app_that_asks_for_authorization_on_install_gets_a_code() {
    let (inst, alice, signin) = installable().await;
    let base = &inst.base;
    let api = format!("{base}/api/v3");
    let (_, _, alice_user) = get(
        &format!("{api}/users/alice"),
        Some(&format!("token {alice}")),
    );
    let (_, _, acme) = get(&format!("{api}/orgs/acme"), Some(&format!("token {alice}")));

    // The page with its account chosen, as a caller that knows the account
    // links to it.
    let chosen = format!(
        "{base}/apps/signin/installations/new/permissions?target_id={}",
        alice_user["id"]
    );
    let mut page = agent().get(&chosen).call().unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.body_mut()
            .read_to_string()
            .unwrap()
            .contains("On alice")
    );
    assert_eq!(
        agent()
            .get(&format!(
                "{base}/apps/signin/installations/new/permissions?target_id=999999"
            ))
            .call()
            .unwrap()
            .status(),
        404
    );

    // Installing sends the person to the App's callback URL with a code
    // for them, as an authorization does, and the installation.
    let (status, to, _) = post_form(&chosen, &[("login", "alice"), ("account", "bob")]);
    assert_eq!(status, 302);
    let to = to.unwrap();
    assert!(to.starts_with("https://signin.test/callback?code="), "{to}");
    assert_eq!(query_value(&to, "setup_action").as_deref(), Some("install"));
    let on_alice = installations(base, "signin");
    assert_eq!(on_alice[0]["account"], "alice", "the URL chose the account");
    assert_eq!(
        query_value(&to, "installation_id").unwrap(),
        on_alice[0]["id"].to_string()
    );
    let mut exchanged = agent()
        .post(&format!("{base}/login/oauth/access_token"))
        .header("accept", "application/json")
        .send_form([
            ("client_id", signin["client_id"].as_str().unwrap()),
            ("client_secret", signin["client_secret"].as_str().unwrap()),
            ("code", query_value(&to, "code").unwrap().as_str()),
            // A caller may name the URL it was sent to, as it does for a
            // code an authorization gave it.
            ("redirect_uri", "https://signin.test/callback"),
        ])
        .unwrap();
    let token: serde_json::Value = exchanged.body_mut().read_json().unwrap();
    assert!(token["error"].is_null(), "{token}");
    let as_alice = format!("Bearer {}", token["access_token"].as_str().unwrap());
    let (_, _, me) = get(&format!("{api}/user"), Some(&as_alice));
    assert_eq!(me["login"], "alice");

    // The person's token lists the App's installations they reach: their
    // own account's, and, once they are a member, an organization's.
    let list = format!("{api}/user/installations");
    let (status, _, mine) = get(&list, Some(&as_alice));
    assert_eq!(status, 200, "{mine}");
    assert_eq!(mine["total_count"], 1);
    assert_eq!(mine["installations"][0]["id"], on_alice[0]["id"]);
    assert_eq!(mine["installations"][0]["account"]["login"], "alice");
    let on_org = format!(
        "{base}/apps/signin/installations/new/permissions?target_id={}",
        acme["id"]
    );
    let (status, _, _) = post_form(&on_org, &[("login", "bob"), ("setup_action", "install")]);
    assert_eq!(status, 302);
    assert_eq!(get(&list, Some(&as_alice)).2["total_count"], 1);
    let membership = format!("{api}/orgs/acme/memberships/alice");
    assert_eq!(get(&membership, Some(&as_alice)).0, 404);
    common::face_ok(
        "PUT",
        &membership,
        &alice,
        serde_json::json!({ "role": "admin" }),
    );
    let (status, _, read) = get(&membership, Some(&as_alice));
    assert_eq!(status, 200, "{read}");
    assert_eq!(
        (&read["role"], &read["state"]),
        (&"admin".into(), &"active".into())
    );
    assert_eq!(read["organization"]["login"], "acme");
    assert_eq!(read["user"]["login"], "alice");
    for path in [
        "/orgs/acme/memberships/bob",
        "/orgs/nobody/memberships/alice",
        "/orgs/alice/memberships/alice",
    ] {
        assert_eq!(
            get(&format!("{api}{path}"), Some(&as_alice)).0,
            404,
            "{path}"
        );
    }
    let (_, _, both) = get(&list, Some(&as_alice));
    assert_eq!(both["total_count"], 2);
    let accounts: Vec<&str> = both["installations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["account"]["login"].as_str().unwrap())
        .collect();
    assert_eq!(accounts, ["acme", "alice"], "newest first");
    let (_, link, first) = get(&format!("{list}?per_page=1"), Some(&as_alice));
    assert_eq!(first["total_count"], 2);
    assert_eq!(first["installations"].as_array().unwrap().len(), 1);
    assert!(link.unwrap().contains("rel=\"next\""));

    // A token no App was given lists none, in GitHub's words.
    let (status, _, refused) = get(&list, Some(&format!("token {alice}")));
    assert_eq!(status, 403);
    assert_eq!(
        refused["message"],
        "You must authenticate with an access token authorized to a GitHub App in order to list installations"
    );
    assert_eq!(get(&list, None).0, 401);

    // Asking to install still authorizes the App, and installs nothing.
    let (status, to, _) = post_form(
        &format!("{base}/apps/signin/installations/new?state=s9"),
        &[("login", "bob"), ("setup_action", "request")],
    );
    assert_eq!(status, 302);
    let to = to.unwrap();
    assert!(to.starts_with("https://signin.test/callback?code="), "{to}");
    assert_eq!(query_value(&to, "setup_action").as_deref(), Some("request"));
    assert_eq!(query_value(&to, "state").as_deref(), Some("s9"));
    assert!(query_value(&to, "installation_id").is_none(), "{to}");
    assert_eq!(installations(base, "signin").len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_control_plane_sets_where_an_install_sends_a_person() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "acme", "org");
    let register = |slug: &str, more: serde_json::Value| {
        let mut body = serde_json::json!({
            "slug": slug,
            "name": slug,
            "owner": "acme",
            "callback_urls": ["https://app.test/callback"],
        });
        for (key, value) in more.as_object().unwrap() {
            body[key] = value.clone();
        }
        let mut resp = agent()
            .post(&format!("{base}/_orrery/apps"))
            .send_json(body)
            .unwrap();
        let status = resp.status().as_u16();
        let json: serde_json::Value = resp.body_mut().read_json().unwrap();
        (status, json)
    };
    // Neither unless given.
    let (status, plain) = register("plain", serde_json::json!({}));
    assert_eq!(status, 201, "{plain}");
    assert_eq!(plain["setup_url"], serde_json::Value::Null);
    assert_eq!(plain["request_oauth_on_install"], false);

    let (status, set) = register(
        "set",
        serde_json::json!({ "setup_url": "https://app.test/setup" }),
    );
    assert_eq!(status, 201, "{set}");
    assert_eq!(set["setup_url"], "https://app.test/setup");
    let (status, asks) = register(
        "asks",
        serde_json::json!({ "request_oauth_on_install": true }),
    );
    assert_eq!(status, 201, "{asks}");
    assert_eq!(asks["request_oauth_on_install"], true);
    // A read answers both.
    let (_, _, read) = get(&format!("{base}/_orrery/apps/set"), None);
    assert_eq!(read["setup_url"], "https://app.test/setup");
    assert_eq!(read["request_oauth_on_install"], false);

    // An App takes one or the other, as on GitHub, and a setup URL is a
    // URL.
    for more in [
        serde_json::json!({ "setup_url": "https://app.test/setup", "request_oauth_on_install": true }),
        serde_json::json!({ "setup_url": "ftp://app.test/setup" }),
        serde_json::json!({ "setup_url": 3 }),
    ] {
        let (status, refused) = register("bad", more.clone());
        assert_eq!(status, 400, "{more}: {refused}");
        assert_eq!(
            common::faults(&refused),
            [common::fault("INVALID_ARGUMENT", Some("setup_url"))],
            "{more}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manifest_page_refuses_what_would_strand_the_person() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let form = format!("{base}/settings/apps/new");
    let registered = |slug: &str| get(&format!("{base}/_orrery/apps/{slug}"), None).0 == 200;

    // The person is sent to `redirect_url`, so one a redirect cannot carry,
    // or that is no web address, is refused before the App is registered.
    for (name, redirect) in [
        ("Scripted", "javascript:alert(1)"),
        ("Split", "https://x.test/cb\nx"),
        ("Bare", "nope"),
    ] {
        let manifest = serde_json::json!({
            "name": name,
            "url": "https://x.test",
            "redirect_url": redirect,
            "callback_urls": ["https://x.test/callback"],
        })
        .to_string();
        let (status, to, page) = post_form(&form, &[("manifest", &manifest), ("login", "alice")]);
        assert_eq!(status, 422, "{redirect}: {to:?}");
        assert!(
            page.contains("Redirect url must be an http or https URL"),
            "{page}"
        );
        assert!(!registered(&name.to_lowercase()), "{name}");
    }

    // An empty `redirect_url` is none: the App is registered, its callback
    // URL is its home page, and the person is shown a page.
    let nowhere = serde_json::json!({
        "name": "Nowhere", "url": "https://x.test", "redirect_url": "",
    })
    .to_string();
    let (status, to, page) = post_form(&form, &[("manifest", &nowhere), ("login", "alice")]);
    assert_eq!((status, to), (200, None), "{page}");
    assert!(page.contains("Registered."), "{page}");
    let (_, _, read) = get(&format!("{base}/_orrery/apps/nowhere"), None);
    assert_eq!(read["callback_urls"], serde_json::json!(["https://x.test"]));

    // A name the person cleared on the page is blank, whatever the manifest
    // named, and one they changed is the App's.
    let named = serde_json::json!({ "name": "Named", "url": "https://x.test" }).to_string();
    let (status, _, page) = post_form(
        &form,
        &[("manifest", &named), ("name", ""), ("login", "alice")],
    );
    assert_eq!(status, 422);
    assert!(page.contains("Name can&#39;t be blank"), "{page}");
    assert!(!registered("named"));
    let (status, _, _) = post_form(
        &form,
        &[
            ("manifest", &named),
            ("name", "Renamed"),
            ("login", "alice"),
        ],
    );
    assert_eq!(status, 200);
    assert!(registered("renamed") && !registered("named"));

    // An App that asks for authorization on install sends the person to a
    // callback URL with a code, so it names one of its own: the URL the
    // manifest's own code goes back to cannot stand in.
    let asks = serde_json::json!({
        "name": "Asks",
        "url": "https://x.test",
        "redirect_url": "https://x.test/manifest",
        "request_oauth_on_install": true,
    })
    .to_string();
    let (status, _, page) = post_form(&form, &[("manifest", &asks), ("login", "alice")]);
    assert_eq!(status, 422);
    assert!(page.contains("Callback url can&#39;t be blank"), "{page}");
    assert!(!registered("asks"));

    // The control plane holds a setup URL to the same rule.
    let mut refused = agent()
        .post(&format!("{base}/_orrery/apps"))
        .send_json(serde_json::json!({
            "slug": "control",
            "name": "control",
            "owner": "alice",
            "callback_urls": ["https://x.test/callback"],
            "setup_url": "https://x.test/s\u{1}etup",
        }))
        .unwrap();
    assert_eq!(refused.status(), 400);
    let refused: serde_json::Value = refused.body_mut().read_json().unwrap();
    assert_eq!(
        common::faults(&refused),
        [common::fault("INVALID_ARGUMENT", Some("setup_url"))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_install_form_shown_again_keeps_the_repositories_chosen() {
    let (inst, _, _) = installable().await;
    let base = &inst.base;
    let page_url = format!("{base}/apps/plain/installations/new");
    let (status, _, page) = post_form(
        &page_url,
        &[
            ("login", "alice"),
            ("account", "acme"),
            ("repository_selection", "selected"),
            ("repositories", "nope"),
        ],
    );
    assert_eq!(status, 422);
    // Shown again with every repository chosen, a corrected form would
    // install on more than the person asked for.
    assert!(page.contains("value=\"selected\" checked"), "{page}");
    assert!(!page.contains("value=\"all\" checked"), "{page}");
    assert!(page.contains("value=\"nope\""), "{page}");
    let mut fresh = agent().get(&page_url).call().unwrap();
    assert!(
        fresh
            .body_mut()
            .read_to_string()
            .unwrap()
            .contains("value=\"all\" checked")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bots_token_lists_no_user_installations() {
    let inst = boot().await;
    let base = &inst.base;
    let ci = mint_actor(base, "ci", "bot");
    let (status, _, refused) = get(
        &format!("{base}/api/v3/user/installations"),
        Some(&format!("token {ci}")),
    );
    assert_eq!(status, 403);
    assert_eq!(refused["message"], "Resource not accessible by integration");
}

/// A world whose pages ask for a token, with alice, bob, and the App
/// `helper`, which asks for authorization on install: alice's token, bob's,
/// and what registering the App answered.
async fn token_sign_in() -> (common::Instance, String, String, serde_json::Value) {
    let inst = common::boot_with(orrery_server::Options {
        page_sign_in: orrery_server::SignIn::Token,
        ..Default::default()
    })
    .await;
    let base = inst.base.clone();
    let alice = mint_actor(&base, "alice", "user");
    let bob = mint_actor(&base, "bob", "user");
    let helper = control_post(
        &base,
        "/_orrery/apps",
        serde_json::json!({
            "slug": "helper",
            "name": "helper",
            "owner": "alice",
            "callback_urls": ["https://helper.test/callback"],
            "request_oauth_on_install": true,
        }),
    );
    (inst, alice, bob, helper)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_world_can_ask_for_a_token_to_sign_in_on_its_pages() {
    let (inst, alice, bob, helper) = token_sign_in().await;
    let base = &inst.base;
    let manifest = manifest("Helper App");
    let client_id = helper["client_id"].as_str().unwrap();
    let install = format!("{base}/apps/helper/installations/new");
    let register = format!("{base}/settings/apps/new");
    let authorize = format!("{base}/login/oauth/authorize");

    // Each page asks for the token.
    let (_, _, page) = post_form(&register, &[("manifest", &manifest)]);
    assert!(page.contains("name=\"password\""), "{page}");
    for url in [
        install.clone(),
        format!("{authorize}?client_id={client_id}"),
    ] {
        let mut page = agent().get(&url).call().unwrap();
        assert_eq!(page.status(), 200, "{url}");
        let page = page.body_mut().read_to_string().unwrap();
        assert!(page.contains("name=\"password\""), "{url}: {page}");
    }

    // A login alone signs in nobody, nor does it with anyone else's token,
    // and nothing is registered, installed, or authorized.
    let wrong: [Vec<(&str, &str)>; 3] = [
        vec![("login", "alice")],
        vec![("login", "alice"), ("password", &bob)],
        vec![("login", "alice"), ("password", "orrfake_nope")],
    ];
    for sign_in in &wrong {
        let mut fields = sign_in.clone();
        fields.push(("manifest", &manifest));
        let (status, to, page) = post_form(&register, &fields);
        assert_eq!((status, to), (200, None), "{sign_in:?}");
        assert!(page.contains("Incorrect username or password."), "{page}");

        let (status, to, page) = post_form(&install, sign_in);
        assert_eq!((status, to), (200, None), "{sign_in:?}");
        assert!(page.contains("Incorrect username or password."), "{page}");

        let mut fields = sign_in.clone();
        fields.push(("client_id", client_id));
        let (status, to, page) = post_form(&authorize, &fields);
        assert_eq!((status, to), (200, None), "{sign_in:?}");
        assert!(page.contains("Incorrect username or password."), "{page}");
        // The page shown again keeps the login and never the token.
        assert!(!page.contains(&bob), "{page}");
    }
    assert_eq!(get(&format!("{base}/_orrery/apps/helper-app"), None).0, 404);
    assert!(installations(base, "helper").is_empty());

    // With their own token a person signs in on each.
    let own = [("login", "alice"), ("password", alice.as_str())];
    let (status, to, _) = post_form(
        &register,
        &[own[0], own[1], ("manifest", manifest.as_str())],
    );
    assert_eq!(status, 302, "{to:?}");
    assert!(query_value(&to.unwrap(), "code").is_some());
    let (status, to, _) = post_form(&authorize, &[own[0], own[1], ("client_id", client_id)]);
    assert_eq!(status, 302);
    let authorized = to.unwrap();
    assert!(
        authorized.starts_with("https://helper.test/callback?code="),
        "{authorized}"
    );
    let (status, to, _) = post_form(&install, &own);
    assert_eq!(status, 302);
    let installed = to.unwrap();
    assert_eq!(installations(base, "helper").len(), 1);

    // A token an App was given acts as the person through that App, and is
    // not theirs to sign in with: the holder of one App's secret cannot
    // turn it into an authorization of another.
    let mut exchanged = agent()
        .post(&format!("{base}/login/oauth/access_token"))
        .header("accept", "application/json")
        .send_form([
            ("client_id", client_id),
            ("client_secret", helper["client_secret"].as_str().unwrap()),
            ("code", query_value(&installed, "code").unwrap().as_str()),
        ])
        .unwrap();
    let given: serde_json::Value = exchanged.body_mut().read_json().unwrap();
    let given = given["access_token"].as_str().unwrap();
    assert_eq!(
        get(
            &format!("{base}/api/v3/user"),
            Some(&format!("token {given}"))
        )
        .2["login"],
        "alice"
    );
    let (status, to, page) = post_form(
        &authorize,
        &[
            ("login", "alice"),
            ("password", given),
            ("client_id", client_id),
        ],
    );
    assert_eq!((status, to), (200, None));
    assert!(page.contains("Incorrect username or password."), "{page}");
}
