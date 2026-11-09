//! End-to-end: the control plane's lists of actors and OAuth apps, which are
//! how a caller handed a world it did not build learns whom to issue
//! credentials for, and the point reads of one actor or app by its key.

mod common;

use common::{boot, control_post, fault, faults, mint_actor};
use orrery_world::{OAUTH_CLIENT_ID_PREFIX, SYSTEM_ACTOR_LOGIN};
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

/// A control-plane GET: `(status, json)`, whatever the status.
fn get(base: &str, path: &str) -> (u16, Value) {
    let mut resp = agent().get(format!("{base}/_orrery{path}")).call().unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
}

fn read(base: &str, path: &str) -> Value {
    let (status, answer) = get(base, path);
    assert_eq!(status, 200, "{path}: {answer}");
    answer
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

    for path in ["/actors/list", "/oauth-apps/list", "/snapshots/list"] {
        for size in [0, 501] {
            assert_eq!(
                refused(path, json!({ "page_size": size })),
                [fault("OUT_OF_RANGE", Some("page_size"))]
            );
        }
        assert_eq!(
            refused(path, json!({ "page_sise": 10 })),
            [fault("INVALID_ARGUMENT", Some("page_sise"))]
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
        refused("/snapshots/list", json!({ "after_name": "no/slashes" })),
        [fault("INVALID_ARGUMENT", Some("after_name"))]
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
    assert_eq!(
        refused(
            "/snapshots/list",
            json!({ "page_size": 0, "after_name": ".hidden" })
        ),
        [
            fault("OUT_OF_RANGE", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("after_name")),
        ]
    );

    // A field that fails to decode hides none of the others.
    assert_eq!(
        refused("/actors/list", json!({ "page_size": "10" })),
        [fault("INVALID_ARGUMENT", Some("page_size"))]
    );
    assert_eq!(
        refused(
            "/actors/list",
            json!({ "page_size": -1, "after_login": "-x" })
        ),
        [
            fault("INVALID_ARGUMENT", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("after_login")),
        ]
    );
    assert_eq!(
        refused("/actors/list", json!({ "page_size": 0, "bogus": 1 })),
        [
            fault("OUT_OF_RANGE", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("bogus")),
        ]
    );

    // A log position is never negative, so a negative cursor is refused
    // rather than read as the start of the log.
    assert_eq!(
        refused("/log/list", json!({ "after_seq": -1 })),
        [fault("OUT_OF_RANGE", Some("after_seq"))]
    );
    assert_eq!(
        refused("/log/list", json!({ "after_seq": -1, "page_size": 501 })),
        [
            fault("OUT_OF_RANGE", Some("after_seq")),
            fault("OUT_OF_RANGE", Some("page_size")),
        ]
    );

    // An order is a key from the list's own vocabulary and a direction, and
    // a direction alone sets nothing.
    assert_eq!(
        refused("/log/list", json!({ "sort_key": "clock_unix" })),
        [fault("INVALID_ARGUMENT", Some("sort_key"))]
    );
    assert_eq!(
        refused(
            "/log/list",
            json!({ "sort_key": "seq", "sort_dir": "DESC" })
        ),
        [fault("INVALID_ARGUMENT", Some("sort_dir"))]
    );
    assert_eq!(
        refused("/log/list", json!({ "sort_dir": "desc" })),
        [fault("INVALID_ARGUMENT", Some("sort_dir"))]
    );
    assert_eq!(
        refused(
            "/log/list",
            json!({ "sort_key": "time", "sort_dir": "down", "page_size": -1 })
        ),
        [
            fault("INVALID_ARGUMENT", Some("sort_key")),
            fault("INVALID_ARGUMENT", Some("sort_dir")),
            fault("INVALID_ARGUMENT", Some("page_size")),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_point_read_answers_as_the_list_does() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "bob", "user");
    mint_actor(base, "Alice", "user");
    mint_actor(base, "carol", "bot");
    // `list` is a login like any other, though the list's path ends in it.
    mint_actor(base, "list", "user");
    let set = agent()
        .put(format!("{base}/_orrery/actors/bob/emails"))
        .send_json(json!({ "emails": [
            { "email": "bob@home.test", "primary": false, "verified": true },
            { "email": "bob@work.test", "primary": true, "verified": false },
        ] }))
        .unwrap();
    assert_eq!(set.status(), 200);
    register_app(base, "Sign-in");
    register_app(base, "Deploys");

    let actors = list(base, "/actors/list", json!({}));
    let actors = actors["items"].as_array().unwrap();
    assert_eq!(actors.len(), 4);
    for item in actors {
        let login = item["login"].as_str().unwrap();
        assert_eq!(&read(base, &format!("/actors/{login}")), item);
        // The login compares without case, and the answer carries the
        // stored one.
        for spelling in [login.to_ascii_uppercase(), login.to_ascii_lowercase()] {
            assert_eq!(&read(base, &format!("/actors/{spelling}")), item);
        }
    }

    let apps = list(base, "/oauth-apps/list", json!({}));
    let apps = apps["items"].as_array().unwrap();
    assert_eq!(apps.len(), 2);
    for item in apps {
        let client_id = item["client_id"].as_str().unwrap();
        let answer = read(base, &format!("/oauth-apps/{client_id}"));
        assert_eq!(&answer, item);
        assert!(answer.get("client_secret").is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_point_read_of_a_key_nothing_holds_is_not_found() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let client_id = register_app(base, "Sign-in");

    let missing_app = format!("{OAUTH_CLIENT_ID_PREFIX}00000000000000000000");
    assert_ne!(missing_app, client_id);
    let system_upper = SYSTEM_ACTOR_LOGIN.to_ascii_uppercase();
    for path in [
        "/actors/ghost".to_string(),
        "/actors/-not-a-login".to_string(),
        // The system actor is in no list, so it is not found by key either.
        format!("/actors/{SYSTEM_ACTOR_LOGIN}"),
        format!("/actors/{system_upper}"),
        format!("/oauth-apps/{missing_app}"),
        "/oauth-apps/abc".to_string(),
        // No client id is `list`, so this is not found like any other.
        "/oauth-apps/list".to_string(),
    ] {
        let (status, answer) = get(base, &path);
        assert_eq!(status, 404, "{path}: {answer}");
        assert_eq!(faults(&answer), [fault("NOT_FOUND", None)], "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_writes_nothing() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let client_id = register_app(base, "Sign-in");

    let log = || list(base, "/log/list", json!({ "page_size": 500 }));
    let before = log();
    list(base, "/actors/list", json!({}));
    list(base, "/oauth-apps/list", json!({}));
    list(base, "/log/list", newest_first());
    read(base, "/actors/alice");
    read(base, &format!("/oauth-apps/{client_id}"));
    get(base, "/actors/ghost");
    get(base, "/oauth-apps/abc");
    assert_eq!(log(), before);
}

fn newest_first() -> Value {
    json!({ "sort_key": "seq", "sort_dir": "desc" })
}

/// The whole log, oldest first, in one read.
fn whole_log(base: &str) -> Vec<Value> {
    let all = list(base, "/log/list", json!({ "page_size": 500 }));
    assert!(all.get("next_page_token").is_none(), "{all}");
    all["items"].as_array().unwrap().clone()
}

/// Follow `next_page_token` from `body` to the last page, calling `between`
/// after each page but the last. Every page but the last is full.
fn walk(base: &str, mut body: Value, mut between: impl FnMut()) -> Vec<Value> {
    let page_size = body["page_size"].as_u64().unwrap() as usize;
    let mut walked = Vec::new();
    loop {
        let page = list(base, "/log/list", body.clone());
        let items = page["items"].as_array().unwrap();
        walked.extend(items.iter().cloned());
        let Some(token) = page.get("next_page_token") else {
            assert!(items.len() <= page_size, "{page}");
            return walked;
        };
        assert_eq!(items.len(), page_size, "{page}");
        assert!(walked.len() < 1000, "the walk does not end");
        body["page_token"] = token.clone();
        between();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_pages_by_token_in_either_order() {
    let inst = boot().await;
    let base = &inst.base;
    for login in ["alice", "bob", "carol", "dana", "erin"] {
        mint_actor(base, login, "user");
    }
    let log = whole_log(base);
    let n = log.len();
    // A page size that leaves the last page short.
    let page_size = (2..n).find(|k| !n.is_multiple_of(*k)).unwrap();

    let first = list(base, "/log/list", json!({ "page_size": 1 }));
    assert_eq!(first["items"], json!([log[0]]));
    assert_eq!(first["total_count"], n);
    let mut body = newest_first();
    body["page_size"] = json!(1);
    let newest = list(base, "/log/list", body);
    assert_eq!(newest["items"], json!([log[n - 1]]));
    assert_eq!(newest["total_count"], n);

    let mut body = newest_first();
    body["page_size"] = json!(page_size);
    let mut reversed = log.clone();
    reversed.reverse();
    assert_eq!(walk(base, body, || {}), reversed);
    assert_eq!(walk(base, json!({ "page_size": page_size }), || {}), log);
}

/// A token holds the last entry served, so entries appended mid-walk shift
/// nothing: newest first they land above the walk and are not visited, and
/// oldest first they are reached at its end.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_is_exact_while_the_log_grows() {
    let inst = boot().await;
    let base = &inst.base;
    for login in ["alice", "bob", "carol", "dana"] {
        mint_actor(base, login, "user");
    }
    let log = whole_log(base);
    let mut reversed = log.clone();
    reversed.reverse();
    // Appends after the first two pages only, so an oldest-first walk ends.
    let mut minted = 0;
    let mut grow = |pages: &mut usize| {
        *pages += 1;
        if *pages <= 2 {
            minted += 1;
            mint_actor(base, &format!("late{minted}"), "user");
        }
    };

    let mut body = newest_first();
    body["page_size"] = json!(2);
    let mut pages = 0;
    assert_eq!(walk(base, body, || grow(&mut pages)), reversed);

    let grown = whole_log(base).len();
    assert!(grown > log.len());
    let mut pages = 0;
    let walked = walk(base, json!({ "page_size": 2 }), || grow(&mut pages));
    assert!(walked.len() > grown, "the walk reached the late entries");
    assert_eq!(walked, whole_log(base));
}

/// A position another call reported, such as a snapshot's `last_seq`, starts
/// a read of what followed it, in either order.
#[tokio::test(flavor = "multi_thread")]
async fn after_seq_reads_what_followed_a_position() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let seed = control_post(base, "/_orrery/snapshots", json!({ "name": "seed" }));
    let last_seq = seed["last_seq"].clone();
    mint_actor(base, "bob", "user");
    mint_actor(base, "carol", "user");

    let after: Vec<Value> = whole_log(base)
        .into_iter()
        .filter(|e| e["seq"].as_i64() > last_seq.as_i64())
        .collect();
    assert!(!after.is_empty());

    let oldest = list(base, "/log/list", json!({ "after_seq": last_seq }));
    assert_eq!(oldest["items"], json!(after));
    assert_eq!(oldest["total_count"], after.len());

    let mut body = newest_first();
    body["after_seq"] = last_seq;
    let newest = list(base, "/log/list", body);
    let mut reversed = after.clone();
    reversed.reverse();
    assert_eq!(newest["items"], json!(reversed));
    assert_eq!(newest["total_count"], after.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_size_of_zero_counts_the_log() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    mint_actor(base, "bob", "user");
    let n = whole_log(base).len();
    assert_eq!(
        list(base, "/log/list", json!({ "page_size": 0 })),
        json!({ "items": [], "total_count": n })
    );
    let first_seq = whole_log(base)[0]["seq"].clone();
    assert_eq!(
        list(
            base,
            "/log/list",
            json!({ "page_size": 0, "after_seq": first_seq })
        ),
        json!({ "items": [], "total_count": n - 1 })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_token_answers_only_the_query_it_was_issued_for() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    mint_actor(base, "bob", "user");
    let token = list(base, "/log/list", json!({ "page_size": 1 }))["next_page_token"].clone();
    assert!(token.is_string());

    // The default order and the same order named are one query.
    let named = list(
        base,
        "/log/list",
        json!({ "page_size": 1, "sort_key": "seq", "page_token": token }),
    );
    let plain = list(
        base,
        "/log/list",
        json!({ "page_size": 1, "page_token": token }),
    );
    assert_eq!(named, plain);
    // So are an absent `after_seq` and one that narrows nothing.
    let zero = list(
        base,
        "/log/list",
        json!({ "page_size": 1, "after_seq": 0, "page_token": token }),
    );
    assert_eq!(zero, plain);

    let refused = |body: Value| -> Vec<(String, Option<String>)> {
        let (status, answer) = post(base, "/log/list", body);
        assert_eq!(status, 400, "{answer}");
        faults(&answer)
    };
    let mut other_order = newest_first();
    other_order["page_token"] = token.clone();
    let other_filter = json!({ "after_seq": 1, "page_token": token });
    for body in [
        other_order,
        other_filter,
        json!({ "page_token": "not-a-token" }),
        json!({ "page_token": "" }),
    ] {
        assert_eq!(
            refused(body.clone()),
            [fault("INVALID_ARGUMENT", Some("page_token"))],
            "{body}"
        );
    }
    // A token is checked against a query only once the query itself reads.
    assert_eq!(
        refused(json!({ "sort_dir": "up", "page_token": token })),
        [fault("INVALID_ARGUMENT", Some("sort_dir"))]
    );
}
