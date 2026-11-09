//! End-to-end: the control plane's lists, which all page by token, and the
//! point reads of one actor or app by its key. The lists of actors and OAuth
//! apps are how a caller handed a world it did not build learns whom to issue
//! credentials for.

mod common;

use common::{boot, boot_at, control_post, fault, faults, mint_actor, restore, snapshot};
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

/// The lists keyed by a name, and the field of an item that holds its key.
const KEYED_LISTS: [(&str, &str); 3] = [
    ("/actors/list", "login"),
    ("/oauth-apps/list", "client_id"),
    ("/snapshots/list", "name"),
];

/// Five items in each keyed list. The logins and snapshot names mix case, so
/// a walk resumes in the order each list compares its keys in.
fn populate(base: &str) {
    for login in ["bob", "Alice", "carol", "Dave", "erin"] {
        mint_actor(base, login, "user");
    }
    for name in ["Sign-in", "Deploys", "Reviews", "Builds", "Docs"] {
        register_app(base, name);
    }
    for name in ["beta", "Alpha", "gamma", "Delta", "epsilon"] {
        control_post(base, "/_orrery/snapshots", json!({ "name": name }));
    }
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
            "total_count": 3,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_keyed_list_walks_by_token_in_key_order() {
    let inst = boot().await;
    let base = &inst.base;
    populate(base);
    for (path, key) in KEYED_LISTS {
        let whole = list(base, path, json!({ "page_size": 500 }));
        assert!(whole.get("next_page_token").is_none(), "{whole}");
        let items = whole["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 5, "{path}");
        assert_eq!(whole["total_count"], 5, "{path}");
        let keys: Vec<&str> = items.iter().map(|i| i[key].as_str().unwrap()).collect();
        let mut sorted = keys.clone();
        if key == "login" {
            sorted.sort_by_key(|k| k.to_ascii_lowercase());
        } else {
            sorted.sort();
        }
        assert_eq!(keys, sorted, "{path}");

        let pages = walk_pages(base, path, json!({ "page_size": 2 }), || {});
        assert_eq!(pages.len(), 3, "{path}");
        for page in &pages {
            assert_eq!(page["total_count"], 5, "{path}: {page}");
        }
        assert_eq!(items_of(&pages), items, "{path}");
    }
}

/// A token holds the key of the last item served, so an actor minted mid-walk
/// shifts nothing: one that sorts before the walk's position is not visited,
/// and one after it is reached in order. Both are capitalized, so a resume
/// that compared logins with case would misplace them.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_is_exact_while_the_actors_grow() {
    let inst = boot().await;
    let base = &inst.base;
    for login in ["bob", "carol", "dave", "erin"] {
        mint_actor(base, login, "user");
    }
    let mut grown = false;
    let pages = walk_pages(base, "/actors/list", json!({ "page_size": 2 }), || {
        if !grown {
            mint_actor(base, "Aaron", "user");
            mint_actor(base, "Zed", "user");
            grown = true;
        }
    });
    let logins: Vec<Value> = items_of(&pages)
        .iter()
        .map(|a| a["login"].clone())
        .collect();
    assert_eq!(logins, ["bob", "carol", "dave", "erin", "Zed"]);
    assert_eq!(pages[0]["total_count"], 4);
    assert_eq!(pages[1]["total_count"], 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_world_lists_nothing() {
    let inst = boot().await;
    for (path, _) in KEYED_LISTS {
        assert_eq!(
            list(&inst.base, path, json!({})),
            json!({ "items": [], "total_count": 0 }),
            "{path}"
        );
    }
    // An empty body is all defaults.
    let mut resp = agent()
        .post(format!("{}/_orrery/actors/list", inst.base))
        .send_empty()
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.body_mut().read_json::<Value>().unwrap(),
        json!({ "items": [], "total_count": 0 })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oauth_apps_list_in_client_id_order_without_secrets() {
    let inst = boot().await;
    let base = &inst.base;
    let mut apps: Vec<(String, String)> = ["Sign-in", "Deploys", "Reviews"]
        .into_iter()
        .map(|name| (register_app(base, name), name.to_string()))
        .collect();
    apps.sort();

    let pages = walk_pages(base, "/oauth-apps/list", json!({ "page_size": 2 }), || {});
    let items = items_of(&pages);
    assert_eq!(items.len(), 3);
    for (item, (client_id, name)) in items.iter().zip(&apps) {
        assert_eq!(
            item,
            &json!({
                "name": name,
                "client_id": client_id,
                "callback_url": "http://auth.test/callback",
            })
        );
    }

    // A page that ends on the last app knows it is the last.
    let exact = list(base, "/oauth-apps/list", json!({ "page_size": 3 }));
    assert_eq!(exact["items"], json!(items));
    assert!(exact.get("next_page_token").is_none(), "{exact}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_size_of_zero_counts_every_keyed_list() {
    let inst = boot().await;
    let base = &inst.base;
    populate(base);
    for (path, _) in KEYED_LISTS {
        assert_eq!(
            list(base, path, json!({ "page_size": 0 })),
            json!({ "items": [], "total_count": 5 }),
            "{path}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_token_answers_only_the_list_it_was_issued_for() {
    let inst = boot().await;
    let base = &inst.base;
    populate(base);
    let token_of = |path: &str| -> Value {
        let token = list(base, path, json!({ "page_size": 1 }))["next_page_token"].clone();
        assert!(token.is_string(), "{path}: {token}");
        token
    };
    let refused = |path: &str, body: Value| -> Vec<(String, Option<String>)> {
        let (status, answer) = post(base, path, body);
        assert_eq!(status, 400, "{path}: {answer}");
        faults(&answer)
    };
    let log_token = token_of("/log/list");

    for (path, key) in KEYED_LISTS {
        let token = token_of(path);
        // The default order and the same order named are one query.
        let plain = list(base, path, json!({ "page_size": 1, "page_token": token }));
        for named in [
            json!({ "page_size": 1, "sort_key": key, "page_token": token }),
            json!({ "page_size": 1, "sort_key": key, "sort_dir": "asc", "page_token": token }),
        ] {
            assert_eq!(list(base, path, named), plain, "{path}");
        }

        let mut elsewhere: Vec<(&str, Value)> = vec![
            (path, json!({ "page_token": log_token })),
            ("/log/list", json!({ "page_token": token })),
        ];
        for (other, _) in KEYED_LISTS {
            if other != path {
                elsewhere.push((other, json!({ "page_token": token })));
            }
        }
        for (at, body) in elsewhere {
            assert_eq!(
                refused(at, body.clone()),
                [fault("INVALID_ARGUMENT", Some("page_token"))],
                "{at}: {body}"
            );
        }
    }
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

    for path in [
        "/actors/list",
        "/oauth-apps/list",
        "/snapshots/list",
        "/log/list",
    ] {
        assert_eq!(
            refused(path, json!({ "page_size": 501 })),
            [fault("OUT_OF_RANGE", Some("page_size"))]
        );
        assert_eq!(
            refused(path, json!({ "page_sise": 10 })),
            [fault("INVALID_ARGUMENT", Some("page_sise"))]
        );
        // An order is a key from the list's own vocabulary and a direction,
        // and a direction alone sets nothing.
        assert_eq!(
            refused(path, json!({ "sort_key": "id" })),
            [fault("INVALID_ARGUMENT", Some("sort_key"))]
        );
        assert_eq!(
            refused(path, json!({ "sort_dir": "desc" })),
            [fault("INVALID_ARGUMENT", Some("sort_dir"))]
        );
        assert_eq!(
            refused(path, json!({ "sort_key": "id", "page_size": 501 })),
            [
                fault("INVALID_ARGUMENT", Some("sort_key")),
                fault("OUT_OF_RANGE", Some("page_size")),
            ]
        );
        // A field that fails to decode hides none of the others.
        assert_eq!(
            refused(path, json!({ "page_size": "10" })),
            [fault("INVALID_ARGUMENT", Some("page_size"))]
        );
        assert_eq!(
            refused(path, json!({ "page_size": -1, "sort_key": "id" })),
            [
                fault("INVALID_ARGUMENT", Some("sort_key")),
                fault("INVALID_ARGUMENT", Some("page_size")),
            ]
        );
        assert_eq!(
            refused(path, json!({ "page_size": 501, "bogus": 1 })),
            [
                fault("OUT_OF_RANGE", Some("page_size")),
                fault("INVALID_ARGUMENT", Some("bogus")),
            ]
        );
    }
    // Only the log reads newest first; the others sort by their key
    // ascending, and say so.
    for (path, key) in KEYED_LISTS {
        assert_eq!(
            refused(path, json!({ "sort_key": key, "sort_dir": "desc" })),
            [fault("INVALID_ARGUMENT", Some("sort_dir"))],
            "{path}"
        );
    }
    // The bounds are inclusive.
    list(base, "/actors/list", json!({ "page_size": 1 }));
    list(base, "/actors/list", json!({ "page_size": 500 }));

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

    // A direction is not case-folded.
    assert_eq!(
        refused(
            "/log/list",
            json!({ "sort_key": "seq", "sort_dir": "DESC" })
        ),
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

/// Follow `next_page_token` on the list at `path` from `body` to the last
/// page, calling `between` after each page but the last, and answer the
/// pages. Every page but the last is full.
fn walk_pages(base: &str, path: &str, mut body: Value, mut between: impl FnMut()) -> Vec<Value> {
    let page_size = body["page_size"].as_u64().unwrap() as usize;
    let mut pages = Vec::new();
    let mut walked = 0;
    loop {
        let page = list(base, path, body.clone());
        let items = page["items"].as_array().unwrap().len();
        walked += items;
        let token = page.get("next_page_token").cloned();
        match token {
            None => assert!(items <= page_size, "{page}"),
            Some(_) => assert_eq!(items, page_size, "{page}"),
        }
        pages.push(page);
        let Some(token) = token else {
            return pages;
        };
        assert!(walked < 1000, "the walk does not end");
        body["page_token"] = token;
        between();
    }
}

/// The items of `pages`, in the order they were served.
fn items_of(pages: &[Value]) -> Vec<Value> {
    pages
        .iter()
        .flat_map(|p| p["items"].as_array().unwrap().iter().cloned())
        .collect()
}

/// The entries a walk of the log serves, as [`walk_pages`] walks it.
fn walk(base: &str, body: Value, between: impl FnMut()) -> Vec<Value> {
    items_of(&walk_pages(base, "/log/list", body, between))
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

/// A world written before the log recorded real times migrates with those
/// entries' times unrecorded: each reads as null, and none is invented.
#[tokio::test(flavor = "multi_thread")]
async fn entries_from_before_real_times_were_recorded_read_null() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("world");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../orrery-world/tests/fixtures/v6.db"
        ),
        root.join("world.db"),
    )
    .unwrap();
    let inst = boot_at(dir, &root).await;

    let log = list(&inst.base, "/log/list", json!({ "page_size": 500 }));
    let items = log["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(log["total_count"], items.len(), "the whole log");
    for item in items {
        assert_eq!(item.get("wall_unix_ms"), Some(&Value::Null), "{item}");
    }
}

/// A position a restore discarded, or one never issued, is refused: the
/// entries numbered above it in this log did not follow it.
#[tokio::test(flavor = "multi_thread")]
async fn after_seq_must_name_an_entry_in_the_log() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let before = snapshot(base, "before");
    mint_actor(base, "bob", "user");
    let after = snapshot(base, "after");
    restore(base, "before");
    mint_actor(base, "carol", "user");
    let newest = whole_log(base).last().unwrap()["seq"].as_i64().unwrap();
    assert!(
        newest > after,
        "the second trial is numbered past the first"
    );

    for seq in [after, newest + 1000] {
        for order in [json!({}), newest_first()] {
            for page_size in [json!(null), json!(0)] {
                let mut body = order.clone();
                body["after_seq"] = json!(seq);
                body["page_size"] = page_size;
                let (status, answer) = post(base, "/log/list", body.clone());
                assert_eq!(status, 404, "{body}: {answer}");
                assert_eq!(
                    faults(&answer),
                    [fault("NOT_FOUND", Some("after_seq"))],
                    "{body}"
                );
                let message = answer["errors"][0]["message"].as_str().unwrap();
                assert!(
                    message.contains(&format!("the newest is {newest}")),
                    "{answer}"
                );
            }
        }
    }

    // A position the restore kept still reads what followed it.
    let followed = list(base, "/log/list", json!({ "after_seq": before }));
    let after_before: Vec<Value> = whole_log(base)
        .into_iter()
        .filter(|e| e["seq"].as_i64() > Some(before))
        .collect();
    assert_eq!(followed["items"], json!(after_before));
    assert_eq!(followed["total_count"], after_before.len());
}

/// A token resumes from the last entry it served. A restore that discarded
/// that entry ends the walk; one that kept it does not.
#[tokio::test(flavor = "multi_thread")]
async fn a_walk_goes_on_across_a_restore_only_while_the_log_holds_its_last_entry() {
    let inst = boot().await;
    let base = &inst.base;
    mint_actor(base, "alice", "user");
    let early = snapshot(base, "early");

    let mut late = 0;
    for (n, mut first) in [
        json!({ "after_seq": early, "page_size": 1 }),
        json!({ "sort_key": "seq", "sort_dir": "desc", "page_size": 1 }),
    ]
    .into_iter()
    .enumerate()
    {
        for _ in 0..3 {
            late += 1;
            mint_actor(base, &format!("late{late}"), "user");
        }
        let page = list(base, "/log/list", first.clone());
        let served = page["items"][0]["seq"].as_i64().unwrap();
        assert!(served > early, "{page}");
        restore(base, "early");

        first["page_token"] = page["next_page_token"].clone();
        let (status, answer) = post(base, "/log/list", first.clone());
        assert_eq!(status, 400, "walk {n}: {answer}");
        assert_eq!(
            faults(&answer),
            [fault("INVALID_ARGUMENT", Some("page_token"))],
            "walk {n}"
        );
        let message = answer["errors"][0]["message"].as_str().unwrap();
        assert!(message.contains("restore"), "{answer}");

        // Started again, the walk reads the restored log, whose newest entry
        // is the snapshot's.
        first.as_object_mut().unwrap().remove("page_token");
        let restarted = list(base, "/log/list", first);
        let log = whole_log(base);
        assert_eq!(log.last().unwrap()["seq"].as_i64(), Some(early));
        let expected = match n {
            0 => json!({ "items": [], "total_count": 0 }),
            _ => json!({
                "items": [log.last().unwrap()],
                "total_count": log.len(),
                "next_page_token": restarted["next_page_token"],
            }),
        };
        assert_eq!(restarted, expected, "walk {n}");
    }

    for login in ["bob", "carol", "dana"] {
        mint_actor(base, login, "user");
    }
    for (n, body) in [
        json!({ "page_size": 2 }),
        json!({ "sort_key": "seq", "sort_dir": "desc", "page_size": 2 }),
    ]
    .into_iter()
    .enumerate()
    {
        // After the first page: a snapshot that holds everything served, a
        // second trial, and a restore that discards only the trial.
        let name = format!("kept{n}");
        let mut restored = false;
        let walked = walk(base, body.clone(), || {
            if !restored {
                restored = true;
                snapshot(base, &name);
                mint_actor(base, &format!("trial{n}"), "user");
                restore(base, &name);
            }
        });
        let mut log = whole_log(base);
        if body.get("sort_dir").is_some() {
            log.reverse();
        }
        assert_eq!(walked, log, "walk {n}");
    }
}

/// A snapshot list opens only the snapshots it may serve: the page, and the
/// one after it that says another page follows. The total and the position
/// need only names. So a snapshot that cannot be opened is still counted, and
/// fails only a read that reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_list_opens_only_the_snapshots_it_may_serve() {
    let inst = boot().await;
    let base = &inst.base;
    for name in ["a", "b", "c", "d"] {
        control_post(base, "/_orrery/snapshots", json!({ "name": name }));
    }
    std::fs::write(
        inst.dir.path().join("snapshots/d/world.db"),
        b"not a database",
    )
    .unwrap();

    assert_eq!(
        list(base, "/snapshots/list", json!({ "page_size": 0 })),
        json!({ "items": [], "total_count": 4 })
    );
    let first = list(base, "/snapshots/list", json!({ "page_size": 1 }));
    assert_eq!(first["items"][0]["name"], "a");
    assert_eq!(first["total_count"], 4);
    let second = list(
        base,
        "/snapshots/list",
        json!({ "page_size": 1, "page_token": first["next_page_token"] }),
    );
    assert_eq!(second["items"][0]["name"], "b");
    // The page of `c` looks past it, to `d`.
    let (status, answer) = post(
        base,
        "/snapshots/list",
        json!({ "page_size": 1, "page_token": second["next_page_token"] }),
    );
    assert_eq!(status, 500, "{answer}");
}
