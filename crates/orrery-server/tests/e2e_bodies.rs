//! End-to-end: how every control-plane route that takes a body refuses a bad
//! one. Each fault is named by its field, and none hides another, so a
//! caller fixes a body in one round trip.

mod common;

use common::{boot, fault, faults};
use serde_json::{Value, json};

struct Route {
    method: &'static str,
    path: &'static str,
    /// A body the route accepts.
    accepted: Value,
    /// Every field the route requires, in the order it reads them.
    required: &'static [&'static str],
    /// A field the route takes, and a value of the wrong type for it.
    mistyped: (&'static str, Value),
}

fn routes() -> Vec<Route> {
    vec![
        Route {
            method: "POST",
            path: "/clock/advance",
            accepted: json!({ "seconds": 60 }),
            required: &["seconds"],
            mistyped: ("seconds", json!("60")),
        },
        Route {
            method: "POST",
            path: "/actors",
            accepted: json!({ "login": "alice", "kind": "user" }),
            required: &["login", "kind"],
            mistyped: ("kind", json!(5)),
        },
        Route {
            method: "PUT",
            path: "/actors/alice/emails",
            accepted: json!({ "emails": [
                { "email": "alice@home.test", "primary": true, "verified": true },
            ] }),
            required: &["emails"],
            mistyped: ("emails", json!("alice@home.test")),
        },
        Route {
            method: "POST",
            path: "/oauth-apps",
            accepted: json!({ "name": "Sign-in", "callback_url": "http://auth.test/callback" }),
            required: &["name", "callback_url"],
            mistyped: ("callback_url", json!(true)),
        },
        Route {
            method: "POST",
            path: "/actors/list",
            accepted: json!({ "page_size": 10 }),
            required: &[],
            mistyped: ("after_login", json!(5)),
        },
        Route {
            method: "POST",
            path: "/oauth-apps/list",
            accepted: json!({ "page_size": 10 }),
            required: &[],
            mistyped: ("after_client_id", json!(5)),
        },
        Route {
            method: "POST",
            path: "/log/list",
            accepted: json!({ "page_size": 10 }),
            required: &[],
            mistyped: ("after_seq", json!("1")),
        },
        Route {
            method: "POST",
            path: "/snapshots",
            accepted: json!({ "name": "before" }),
            required: &["name"],
            mistyped: ("name", json!(5)),
        },
        Route {
            method: "POST",
            path: "/snapshots/list",
            accepted: json!({ "page_size": 10 }),
            required: &[],
            mistyped: ("after_name", json!(["before"])),
        },
        Route {
            method: "POST",
            path: "/diff",
            accepted: json!({ "from": "before" }),
            required: &["from"],
            mistyped: ("to", json!(5)),
        },
    ]
}

/// A control-plane call with a raw body: `(status, json)`, whatever the
/// status.
fn call(base: &str, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{base}/_orrery{path}");
    let request = match method {
        "POST" => agent.post(&url),
        "PUT" => agent.put(&url),
        "DELETE" => agent.delete(&url).force_send_body(),
        other => panic!("unsupported method {other}"),
    };
    let mut resp = request
        .header("content-type", "application/json")
        .send(body)
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap();
    let answer = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    };
    (status, answer)
}

/// The faults a refused body answers, asserting it was refused.
fn refused(base: &str, route: &Route, body: &[u8]) -> Vec<(String, Option<String>)> {
    let (status, answer) = call(base, route.method, route.path, body);
    assert_eq!(
        status,
        400,
        "{} {} {}: {answer}",
        route.method,
        route.path,
        String::from_utf8_lossy(body)
    );
    assert_no_field_positions(&answer);
    faults(&answer)
}

/// A field is decoded from its own text, so a line and column in its
/// message would point into that value, not into the body the caller sent.
fn assert_no_field_positions(answer: &Value) {
    for error in answer["errors"].as_array().unwrap() {
        if error.get("field").is_some() {
            let message = error["message"].as_str().unwrap();
            assert!(!message.contains(" at line "), "{message}");
        }
    }
}

fn with(body: &Value, field: &str, value: Value) -> Vec<u8> {
    let mut body = body.clone();
    body[field] = value;
    serde_json::to_vec(&body).unwrap()
}

fn log(base: &str) -> Value {
    call(base, "POST", "/log/list", br#"{"page_size": 500}"#).1
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_mistyped_fields_are_named_on_every_route() {
    let inst = boot().await;
    let base = &inst.base;
    let before = log(base);

    for route in routes() {
        let (name, wrong) = &route.mistyped;
        assert_eq!(
            refused(base, &route, &with(&route.accepted, "bogus", json!(1))),
            [fault("INVALID_ARGUMENT", Some("bogus"))]
        );
        assert_eq!(
            refused(base, &route, &with(&route.accepted, name, wrong.clone())),
            [fault("INVALID_ARGUMENT", Some(name))]
        );
        // Unknown fields come after the fields the route reads, each named.
        let mut both = route.accepted.clone();
        both[name] = wrong.clone();
        both["zzz"] = json!(null);
        both["bogus"] = json!(1);
        assert_eq!(
            refused(base, &route, &serde_json::to_vec(&both).unwrap()),
            [
                fault("INVALID_ARGUMENT", Some(name)),
                fault("INVALID_ARGUMENT", Some("bogus")),
                fault("INVALID_ARGUMENT", Some("zzz")),
            ]
        );
    }

    // Nothing a refused body asked for happened.
    assert_eq!(log(base), before);
    let (_, snapshots) = call(base, "POST", "/snapshots/list", b"{}");
    assert_eq!(snapshots["items"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_missing_field_is_named_and_an_empty_body_is_all_defaults() {
    let inst = boot().await;
    let base = &inst.base;
    for route in routes() {
        for body in [&b""[..], b"{}"] {
            if route.required.is_empty() {
                let (status, answer) = call(base, route.method, route.path, body);
                assert_eq!(status, 200, "{}: {answer}", route.path);
            } else {
                let expected: Vec<_> = route
                    .required
                    .iter()
                    .map(|name| fault("INVALID_ARGUMENT", Some(name)))
                    .collect();
                assert_eq!(refused(base, &route, body), expected);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_an_object_is_one_fault_with_no_field() {
    let inst = boot().await;
    let base = &inst.base;
    for route in routes() {
        for body in [
            &b"[]"[..],
            br#""x""#,
            b"1",
            b"null",
            b"{",
            br#"{"seconds": 60} {}"#,
        ] {
            assert_eq!(
                refused(base, &route, body),
                [fault("INVALID_ARGUMENT", None)]
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_field_is_refused_by_name() {
    let inst = boot().await;
    let base = &inst.base;
    let routes = routes();
    let list = routes.iter().find(|r| r.path == "/actors/list").unwrap();
    // A JSON map would keep the second value; the body is refused instead.
    assert_eq!(
        refused(base, list, br#"{"page_size": 10, "page_size": 600}"#),
        [fault("INVALID_ARGUMENT", Some("page_size"))]
    );
    let advance = routes.iter().find(|r| r.path == "/clock/advance").unwrap();
    assert_eq!(
        refused(base, advance, br#"{"seconds": 60, "seconds": 60}"#),
        [fault("INVALID_ARGUMENT", Some("seconds"))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_value_inside_a_field_is_held_to_its_own_rules() {
    let inst = boot().await;
    let base = &inst.base;
    common::mint_actor(base, "alice", "user");
    let routes = routes();
    let emails = routes
        .iter()
        .find(|r| r.path == "/actors/alice/emails")
        .unwrap();
    // A repeated or unknown key inside an address is a fault in `emails`.
    for entry in [
        r#"{"email": "nope", "email": "alice@home.test", "primary": true, "verified": true}"#,
        r#"{"email": "alice@home.test", "primary": true, "verified": true, "primay": true}"#,
    ] {
        let body = format!(r#"{{"emails": [{entry}], "bogus": 1}}"#);
        assert_eq!(
            refused(base, emails, body.as_bytes()),
            [
                fault("INVALID_ARGUMENT", Some("emails")),
                fault("INVALID_ARGUMENT", Some("bogus")),
            ]
        );
    }

    // A number too large for any type is a fault in its field, not in the
    // body.
    let advance = routes.iter().find(|r| r.path == "/clock/advance").unwrap();
    assert_eq!(
        refused(base, advance, br#"{"seconds": 1e400, "bogus": 1}"#),
        [
            fault("INVALID_ARGUMENT", Some("seconds")),
            fault("INVALID_ARGUMENT", Some("bogus")),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_kind_is_named_as_kind() {
    let inst = boot().await;
    let (status, answer) = call(
        &inst.base,
        "POST",
        "/actors",
        br#"{"login": "alice", "kind": "system"}"#,
    );
    assert_eq!(status, 400, "{answer}");
    assert_eq!(faults(&answer), [fault("INVALID_ARGUMENT", Some("kind"))]);
}

/// An OAuth app's name is measured in characters, so a name in a script of
/// three bytes a character fits as many characters as an ASCII one.
#[tokio::test(flavor = "multi_thread")]
async fn an_oauth_app_name_is_measured_in_characters() {
    let inst = boot().await;
    let base = &inst.base;
    let body = |name: String| {
        json!({ "name": name, "callback_url": "http://auth.test/callback" }).to_string()
    };

    let longest = "名".repeat(100);
    let (status, app) = call(
        base,
        "POST",
        "/oauth-apps",
        body(longest.clone()).as_bytes(),
    );
    assert_eq!(status, 201, "{app}");
    assert_eq!(app["name"], longest.as_str());

    let (status, answer) = call(
        base,
        "POST",
        "/oauth-apps",
        body("名".repeat(101)).as_bytes(),
    );
    assert_eq!(status, 400, "{answer}");
    assert_eq!(faults(&answer), [fault("INVALID_ARGUMENT", Some("name"))]);
    assert_eq!(
        answer["errors"][0]["message"],
        "must be 1-100 characters and not blank"
    );
}

/// A create route checks its values while it reads the body, so a bad value
/// is named beside every other fault, and one bad value hides no other.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_route_names_every_bad_value_with_the_rest_of_the_body() {
    let inst = boot().await;
    let base = &inst.base;
    let before = log(base);

    for (path, body, expected) in [
        (
            "/actors",
            r#"{"login": "-x", "kind": "system"}"#,
            &["login", "kind"][..],
        ),
        (
            "/actors",
            r#"{"login": "-x", "kind": "user", "bogus": 1}"#,
            &["login", "bogus"],
        ),
        (
            "/actors",
            r#"{"login": "-x", "kind": 5}"#,
            &["login", "kind"],
        ),
        (
            "/oauth-apps",
            r#"{"name": "", "callback_url": "nope"}"#,
            &["name", "callback_url"],
        ),
        (
            "/oauth-apps",
            r#"{"callback_url": "nope"}"#,
            &["name", "callback_url"],
        ),
        (
            "/oauth-apps",
            r#"{"name": "Sign-in", "callback_url": "nope", "bogus": 1}"#,
            &["callback_url", "bogus"],
        ),
        (
            "/snapshots",
            r#"{"name": "../x", "bogus": 1}"#,
            &["name", "bogus"],
        ),
    ] {
        let (status, answer) = call(base, "POST", path, body.as_bytes());
        assert_eq!(status, 400, "{path} {body}: {answer}");
        let expected: Vec<_> = expected
            .iter()
            .map(|field| fault("INVALID_ARGUMENT", Some(field)))
            .collect();
        assert_eq!(faults(&answer), expected, "{path} {body}");
    }

    // Nothing a refused body asked for happened.
    assert_eq!(log(base), before);
    let (_, snapshots) = call(base, "POST", "/snapshots/list", b"{}");
    assert_eq!(snapshots["items"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diff_names_the_snapshot_it_could_not_open() {
    let inst = boot().await;
    let base = &inst.base;
    let (status, _) = call(base, "POST", "/snapshots", br#"{"name": "before"}"#);
    assert_eq!(status, 201);

    for (body, status, reason, field) in [
        (r#"{"from": "../x"}"#, 400, "INVALID_ARGUMENT", "from"),
        (r#"{"from": "ghost"}"#, 404, "NOT_FOUND", "from"),
        (
            r#"{"from": "before", "to": "../x"}"#,
            400,
            "INVALID_ARGUMENT",
            "to",
        ),
        (
            r#"{"from": "before", "to": "ghost"}"#,
            404,
            "NOT_FOUND",
            "to",
        ),
    ] {
        let (got, answer) = call(base, "POST", "/diff", body.as_bytes());
        assert_eq!(got, status, "{body}: {answer}");
        assert_eq!(faults(&answer), [fault(reason, Some(field))], "{body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diff_checks_both_snapshot_names_with_the_rest_of_the_body() {
    let inst = boot().await;
    let base = &inst.base;
    for (body, expected) in [
        (
            r#"{"from": "../x", "to": "../y"}"#,
            [
                fault("INVALID_ARGUMENT", Some("from")),
                fault("INVALID_ARGUMENT", Some("to")),
            ],
        ),
        (
            r#"{"from": "../x", "bogus": 1}"#,
            [
                fault("INVALID_ARGUMENT", Some("from")),
                fault("INVALID_ARGUMENT", Some("bogus")),
            ],
        ),
    ] {
        let (status, answer) = call(base, "POST", "/diff", body.as_bytes());
        assert_eq!(status, 400, "{body}: {answer}");
        assert_eq!(faults(&answer), expected, "{body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_fields_are_named_in_the_order_first_repeated() {
    let inst = boot().await;
    let body = br#"{"page_size": 1, "after_seq": 0, "page_size": 2, "after_seq": 1, "page_size": 3, "bogus": 1}"#;
    let (status, answer) = call(&inst.base, "POST", "/log/list", body);
    assert_eq!(status, 400, "{answer}");
    assert_eq!(
        faults(&answer),
        [
            fault("INVALID_ARGUMENT", Some("page_size")),
            fault("INVALID_ARGUMENT", Some("after_seq")),
            fault("INVALID_ARGUMENT", Some("bogus")),
        ]
    );
}

/// Routes that act on the thing their path names take no body fields, and
/// refuse one rather than ignore it.
#[tokio::test(flavor = "multi_thread")]
async fn a_route_that_takes_no_fields_refuses_one() {
    let inst = boot().await;
    let base = &inst.base;
    common::mint_actor(base, "alice", "user");
    let (status, app) = call(
        base,
        "POST",
        "/oauth-apps",
        br#"{"name": "Sign-in", "callback_url": "http://auth.test/callback"}"#,
    );
    assert_eq!(status, 201, "{app}");
    let client_id = app["client_id"].as_str().unwrap();
    let (status, _) = call(base, "POST", "/snapshots", br#"{"name": "before"}"#);
    assert_eq!(status, 201);

    let secrets = format!("/oauth-apps/{client_id}/secrets");
    let routes = [
        ("POST", "/actors/alice/tokens", 201),
        ("POST", secrets.as_str(), 201),
        ("POST", "/snapshots/before/restore", 200),
        ("DELETE", "/snapshots/before", 204),
    ];
    let before = log(base);
    for (method, path, _) in routes {
        let (status, answer) = call(base, method, path, br#"{"bogus": 1}"#);
        assert_eq!(status, 400, "{method} {path}: {answer}");
        assert_eq!(faults(&answer), [fault("INVALID_ARGUMENT", Some("bogus"))]);
    }
    // Nothing the refused calls asked for happened: no credential was
    // issued, no restore ran, and the snapshot is still there.
    assert_eq!(log(base), before);
    let (_, snapshots) = call(base, "POST", "/snapshots/list", b"{}");
    assert_eq!(snapshots["items"][0]["name"], "before");

    for body in [&b""[..], b"{}"] {
        for (method, path, ok) in routes {
            let (status, answer) = call(base, method, path, body);
            assert_eq!(status, ok, "{method} {path}: {answer}");
        }
        let (status, _) = call(base, "POST", "/snapshots", br#"{"name": "before"}"#);
        assert_eq!(status, 201);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clock_advance_past_the_latest_time_is_out_of_range() {
    let inst = boot().await;
    let base = &inst.base;
    let latest = jiff::Timestamp::MAX.as_second();
    let clock = inst.world.clock().unwrap();
    let advance = |seconds: i64| {
        let body = serde_json::to_vec(&json!({ "seconds": seconds })).unwrap();
        call(base, "POST", "/clock/advance", &body)
    };

    let before = log(base);
    for seconds in [latest - clock + 1, i64::MAX] {
        let (status, answer) = advance(seconds);
        assert_eq!(status, 400, "{seconds}: {answer}");
        assert_eq!(
            faults(&answer),
            [fault("OUT_OF_RANGE", Some("seconds"))],
            "{seconds}"
        );
    }
    assert_eq!(inst.world.clock().unwrap(), clock);
    assert_eq!(log(base), before);

    let (status, answer) = advance(latest - clock);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["clock_unix"], latest);
    assert_eq!(answer["rfc3339"], "9999-12-30T22:00:00Z");
    let (status, answer) = advance(1);
    assert_eq!(status, 400, "{answer}");
    assert_eq!(faults(&answer), [fault("OUT_OF_RANGE", Some("seconds"))]);
    assert_eq!(inst.world.clock().unwrap(), latest);
}
