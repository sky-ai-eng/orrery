//! End-to-end: a served world's clock in its two modes, through the control
//! plane and the face (spec/usage.md, "Time"). The host's time is set
//! through the world, so no test waits for time to pass.

mod common;

use common::{boot, control_post, face_ok, fault, faults, mint_actor, restore, snapshot};
use orrery_world::DEFAULT_T0_UNIX;
use serde_json::{Value, json};

const T0: i64 = DEFAULT_T0_UNIX;

/// A control-plane call answered with its status and body, whatever the
/// status.
fn call(base: &str, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{base}/_orrery{path}");
    let mut resp = match (method, body) {
        ("GET", None) => agent.get(&url).call(),
        ("PATCH", Some(body)) => agent.patch(&url).send_json(body),
        ("POST", Some(body)) => agent.post(&url).send_json(body),
        (method, _) => panic!("unsupported call {method}"),
    }
    .unwrap();
    (resp.status().as_u16(), resp.body_mut().read_json().unwrap())
}

fn clock(base: &str) -> Value {
    let (status, answer) = call(base, "GET", "/clock", None);
    assert_eq!(status, 200, "{answer}");
    answer
}

fn set_mode(base: &str, mode: &str) -> (u16, Value) {
    call(base, "PATCH", "/clock", Some(json!({ "mode": mode })))
}

fn log(base: &str) -> Vec<Value> {
    control_post(base, "/_orrery/log/list", json!({ "page_size": 500 }))["items"]
        .as_array()
        .unwrap()
        .clone()
}

/// The `type` of each event after the first `n`.
fn types_after(base: &str, n: usize) -> Vec<String> {
    log(base)[n..]
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

fn rfc3339(unix: i64) -> String {
    jiff::Timestamp::from_second(unix).unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_served_world_follows_the_hosts_time_once_it_is_real() {
    let inst = boot().await;
    let base = &inst.base;
    assert_eq!(
        clock(base),
        json!({ "clock_unix": T0, "rfc3339": rfc3339(T0), "mode": "manual" })
    );

    inst.world.set_host_time(T0 + 3600);
    let n = log(base).len();
    let (status, answer) = set_mode(base, "real");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer,
        json!({ "clock_unix": T0 + 3600, "rfc3339": rfc3339(T0 + 3600), "mode": "real" })
    );
    assert_eq!(
        types_after(base, n),
        ["world:clock_advanced", "world:clock_mode_set"]
    );

    // A read answers the present and writes nothing.
    inst.world.set_host_time(T0 + 7200);
    let n = log(base).len();
    assert_eq!(clock(base)["clock_unix"], T0 + 7200);
    let (_, health) = call(base, "GET", "/healthz", None);
    assert_eq!(health["clock_unix"], T0 + 7200);
    assert_eq!(log(base).len(), n);

    // A write through the face stamps the host's time, after the advance
    // that records it.
    let token = mint_actor(base, "alice", "user");
    inst.world.set_host_time(T0 + 7300);
    let n = log(base).len();
    let repo = face_ok(
        "POST",
        &format!("{base}/api/v3/user/repos"),
        &token,
        json!({ "name": "webapp" }),
    );
    assert_eq!(repo["created_at"], rfc3339(T0 + 7300));
    let added = &log(base)[n..];
    assert_eq!(added.len(), 2);
    assert_eq!(added[0]["type"], "world:clock_advanced");
    assert_eq!(added[0]["door"], "control");
    assert_eq!(added[0]["actor"]["login"], "orrery");
    assert_eq!(
        added[0]["changes"],
        json!({ "clock_unix": { "from": T0 + 7200, "to": T0 + 7300 } })
    );
    assert_eq!(added[1]["type"], "repo:created");
    assert_eq!(added[1]["door"], "rest");
    assert_eq!(added[1]["clock_unix"], T0 + 7300);

    // Advancing it by hand is refused, and changes nothing.
    let n = log(base).len();
    let (status, answer) = call(
        base,
        "POST",
        "/clock/advance",
        Some(json!({ "seconds": 60 })),
    );
    assert_eq!(status, 409, "{answer}");
    assert_eq!(faults(&answer), [fault("CLOCK_IS_REAL", None)]);
    assert_eq!(log(base).len(), n);

    // Made manual, it stops at the present.
    inst.world.set_host_time(T0 + 8000);
    let (status, answer) = set_mode(base, "manual");
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["clock_unix"], T0 + 8000);
    assert_eq!(answer["mode"], "manual");
    assert_eq!(
        types_after(base, n),
        ["world:clock_advanced", "world:clock_mode_set"]
    );
    inst.world.set_host_time(T0 + 9000);
    assert_eq!(clock(base)["clock_unix"], T0 + 8000);
    let (status, answer) = call(
        base,
        "POST",
        "/clock/advance",
        Some(json!({ "seconds": 60 })),
    );
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["clock_unix"], T0 + 8060);
    assert_eq!(answer["mode"], "manual");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mode_change_is_refused_as_its_body_or_the_clock_requires() {
    let inst = boot().await;
    let base = &inst.base;
    let before = log(base);

    for (body, expected) in [
        (
            json!({ "mode": null }),
            [fault("INVALID_ARGUMENT", Some("mode"))],
        ),
        (
            json!({ "mode": "Real" }),
            [fault("INVALID_ARGUMENT", Some("mode"))],
        ),
        (
            json!({ "mode": "running" }),
            [fault("INVALID_ARGUMENT", Some("mode"))],
        ),
        (
            json!({ "clock_unix": T0 }),
            [fault("INVALID_ARGUMENT", Some("clock_unix"))],
        ),
    ] {
        let (status, answer) = call(base, "PATCH", "/clock", Some(body.clone()));
        assert_eq!(status, 400, "{body}: {answer}");
        assert_eq!(faults(&answer), expected, "{body}");
    }
    // Every fault is named, the null among them.
    let (status, answer) = call(
        base,
        "PATCH",
        "/clock",
        Some(json!({ "mode": null, "bogus": 1 })),
    );
    assert_eq!(status, 400, "{answer}");
    assert_eq!(
        faults(&answer),
        [
            fault("INVALID_ARGUMENT", Some("mode")),
            fault("INVALID_ARGUMENT", Some("bogus")),
        ]
    );

    // A clock ahead of the host's time never goes back.
    inst.world.set_host_time(T0 - 1);
    let (status, answer) = set_mode(base, "real");
    assert_eq!(status, 409, "{answer}");
    assert_eq!(faults(&answer), [fault("CLOCK_AHEAD", None)]);

    // An empty change keeps the mode.
    let (status, answer) = call(base, "PATCH", "/clock", Some(json!({})));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["mode"], "manual");

    assert_eq!(log(base), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_snapshot_of_a_real_world_holds_the_time_it_was_taken() {
    let inst = boot().await;
    let base = &inst.base;
    inst.world.set_host_time(T0 + 100);
    set_mode(base, "real");

    inst.world.set_host_time(T0 + 200);
    let n = log(base).len();
    let last_seq = snapshot(base, "noon");
    assert_eq!(types_after(base, n), ["world:clock_advanced"]);
    assert_eq!(last_seq, log(base).last().unwrap()["seq"]);
    let (_, listed) = call(base, "POST", "/snapshots/list", Some(json!({})));
    assert_eq!(listed["items"][0]["clock_unix"], T0 + 200);

    inst.world.set_host_time(T0 + 500);
    mint_actor(base, "alice", "user");
    restore(base, "noon");

    // The restored world holds the snapshot's clock, and answers the present.
    assert_eq!(
        clock(base),
        json!({ "clock_unix": T0 + 500, "rfc3339": rfc3339(T0 + 500), "mode": "real" })
    );
    let n = log(base).len();
    mint_actor(base, "bob", "user");
    let added = &log(base)[n..];
    assert_eq!(added[0]["type"], "world:clock_advanced");
    assert_eq!(
        added[0]["changes"],
        json!({ "clock_unix": { "from": T0 + 200, "to": T0 + 500 } })
    );
    assert_eq!(added[1]["type"], "actor:created");
}
