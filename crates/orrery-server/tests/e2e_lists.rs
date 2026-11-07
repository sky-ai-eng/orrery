//! End-to-end: the control plane's lists of actors and OAuth apps, which are
//! how a caller handed a world it did not build learns whom to issue
//! credentials for.

mod common;

use common::{boot, control_post, mint_actor};
use serde_json::{Value, json};

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

/// A control-plane POST: `(status, json)`, whatever the status.
fn post(base: &str, path: &str, body: Value) -> (u16, Value) {
    let mut resp = agent()
        .post(format!("{base}/_orrery{path}"))
        .send_json(body)
        .unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
}

fn list(base: &str, path: &str, body: Value) -> Value {
    let (status, answer) = post(base, path, body);
    assert_eq!(status, 200, "{path}: {answer}");
    answer
}

/// Each error's `(reason, field)`, in the order the answer lists them.
fn faults(answer: &Value) -> Vec<(String, Option<String>)> {
    answer["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["reason"].as_str().unwrap().to_string(),
                e["field"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn fault(reason: &str, field: Option<&str>) -> (String, Option<String>) {
    (reason.to_string(), field.map(str::to_string))
}

fn register_app(base: &str, name: &str) -> String {
    control_post(
        base,
        "/_orrery/oauth-apps",
        json!({ "name": name, "callback_url": "http://auth.test/callback" }),
    )["client_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn actors_list_in_login_order_with_their_addresses() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "bob", "user");
    mint_actor(base, "Alice", "user");
    mint_actor(base, "carol", "bot");
    // The primary sorts after the other by address, so the order below is
    // primary first, not alphabetical.
    let set = agent()
        .put(format!("{base}/_orrery/actors/bob/emails"))
        .send_json(json!({ "emails": [
            { "email": "bob@home.test", "primary": false, "verified": true },
            { "email": "bob@work.test", "primary": true, "verified": false },
        ] }))
        .unwrap();
    assert_eq!(set.status(), 200);

    let answer = list(base, "/actors/list", json!({}));
    assert_eq!(
        answer,
        json!({
            "items": [
                { "login": "Alice", "kind": "user", "emails": [
                    { "email": "Alice@orrery.invalid", "primary": true, "verified": true },
                ] },
                { "login": "bob", "kind": "user", "emails": [
                    { "email": "bob@work.test", "primary": true, "verified": false },
                    { "email": "bob@home.test", "primary": false, "verified": true },
                ] },
                { "login": "carol", "kind": "bot", "emails": [
                    { "email": "carol@orrery.invalid", "primary": true, "verified": true },
                ] },
            ],
            "next_after_login": null,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn actors_page_by_login() {
    let inst = boot().await;
    let base = &inst.base;
    for login in ["bob", "Alice", "carol"] {
        mint_actor(base, login, "user");
    }
    let logins = |answer: &Value| -> Vec<String> {
        answer["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["login"].as_str().unwrap().to_string())
            .collect()
    };

    let first = list(base, "/actors/list", json!({ "page_size": 2 }));
    assert_eq!(logins(&first), ["Alice", "bob"]);
    assert_eq!(first["next_after_login"], "bob");

    let second = list(base, "/actors/list", json!({ "after_login": "bob" }));
    assert_eq!(logins(&second), ["carol"]);
    assert_eq!(second["next_after_login"], Value::Null);
    // The cursor compares as logins do, without case.
    assert_eq!(
        list(base, "/actors/list", json!({ "after_login": "BOB" })),
        second
    );

    // A page that ends on the last actor knows it is the last.
    let exact = list(base, "/actors/list", json!({ "page_size": 3 }));
    assert_eq!(logins(&exact), ["Alice", "bob", "carol"]);
    assert_eq!(exact["next_after_login"], Value::Null);

    // The cursor is a position, not a reference: it need not name an actor.
    let between = list(base, "/actors/list", json!({ "after_login": "b" }));
    assert_eq!(logins(&between), ["bob", "carol"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_world_lists_nothing() {
    let inst = boot().await;
    assert_eq!(
        list(&inst.base, "/actors/list", json!({})),
        json!({ "items": [], "next_after_login": null })
    );
    assert_eq!(
        list(&inst.base, "/oauth-apps/list", json!({})),
        json!({ "items": [], "next_after_client_id": null })
    );
    // An empty body is all defaults.
    let mut resp = agent()
        .post(format!("{}/_orrery/actors/list", inst.base))
        .send_empty()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.body_mut().read_json::<Value>().unwrap(),
        json!({ "items": [], "next_after_login": null })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_apps_page_by_client_id_without_secrets() {
    let inst = boot().await;
    let base = &inst.base;
    let mut apps: Vec<(String, String)> = ["Sign-in", "Deploys", "Reviews"]
        .into_iter()
        .map(|name| (register_app(base, name), name.to_string()))
        .collect();
    apps.sort();

    let first = list(base, "/oauth-apps/list", json!({ "page_size": 2 }));
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for (item, (client_id, name)) in items.iter().zip(&apps) {
        assert_eq!(
            item,
            &json!({
                "name": name,
                "client_id": client_id,
                "callback_url": "http://auth.test/callback",
            })
        );
        assert!(item.get("client_secret").is_none());
    }
    assert_eq!(first["next_after_client_id"], apps[1].0);

    let second = list(
        base,
        "/oauth-apps/list",
        json!({ "after_client_id": apps[1].0 }),
    );
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["items"][0]["client_id"], apps[2].0);
    assert!(second["items"][0].get("client_secret").is_none());
    assert_eq!(second["next_after_client_id"], Value::Null);

    let exact = list(base, "/oauth-apps/list", json!({ "page_size": 3 }));
    assert_eq!(exact["items"].as_array().unwrap().len(), 3);
    assert_eq!(exact["next_after_client_id"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_list_bodies_name_every_fault() {
    let inst = boot().await;
    let base = &inst.base;
    let refused = |path: &str, body: Value| -> Vec<(String, Option<String>)> {
        let (status, answer) = post(base, path, body);
        assert_eq!(status, 400, "{path}: {answer}");
        faults(&answer)
    };

    for path in ["/actors/list", "/oauth-apps/list"] {
        for size in [0, 501] {
            assert_eq!(
                refused(path, json!({ "page_size": size })),
                [fault("OUT_OF_RANGE", Some("page_size"))]
            );
        }
        assert_eq!(
            refused(path, json!({ "page_sise": 10 })),
            [fault("INVALID_ARGUMENT", None)]
        );
    }
    // The bounds are inclusive.
    list(base, "/actors/list", json!({ "page_size": 1 }));
    list(base, "/actors/list", json!({ "page_size": 500 }));

    assert_eq!(
        refused("/actors/list", json!({ "after_login": "-x" })),
        [fault("INVALID_ARGUMENT", Some("after_login"))]
    );
    assert_eq!(
        refused("/oauth-apps/list", json!({ "after_client_id": "abc" })),
        [fault("INVALID_ARGUMENT", Some("after_client_id"))]
    );
    assert_eq!(
        refused(
            "/actors/list",
            json!({ "page_size": 0, "after_login": "-x" })
        ),
        [
            fault("OUT_OF_RANGE", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("after_login")),
        ]
    );
    assert_eq!(
        refused(
            "/oauth-apps/list",
            json!({ "page_size": 501, "after_client_id": "abc" })
        ),
        [
            fault("OUT_OF_RANGE", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("after_client_id")),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_writes_nothing() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    register_app(base, "Sign-in");

    let log = || list(base, "/log/list", json!({ "page_size": 500 }));
    let before = log();
    list(base, "/actors/list", json!({}));
    list(base, "/oauth-apps/list", json!({}));
    assert_eq!(log(), before);
}

/// The log pages like the lists above: a page that ends on the last entry
/// has a null cursor.
#[tokio::test(flavor = "multi_thread")]
async fn the_logs_last_page_has_a_null_cursor() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    mint_actor(base, "bob", "user");
    let all = list(base, "/log/list", json!({ "page_size": 500 }));
    let entries = all["items"].as_array().unwrap().len();
    assert!(entries >= 2);
    assert_eq!(all["next_after_seq"], Value::Null);

    let exact = list(base, "/log/list", json!({ "page_size": entries }));
    assert_eq!(exact["items"].as_array().unwrap().len(), entries);
    assert_eq!(exact["next_after_seq"], Value::Null);

    let short = list(base, "/log/list", json!({ "page_size": entries - 1 }));
    let last = short["items"][entries - 2]["seq"].clone();
    assert_eq!(short["next_after_seq"], last);
    let rest = list(base, "/log/list", json!({ "after_seq": last }));
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["next_after_seq"], Value::Null);
}
