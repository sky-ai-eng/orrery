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
        other => panic!("unsupported method {other}"),
    };
    let mut resp = request
        .header("content-type", "application/json")
        .send(body)
        .unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
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
    faults(&answer)
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
