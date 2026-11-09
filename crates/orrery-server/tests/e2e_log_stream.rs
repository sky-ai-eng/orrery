//! The log's events as server-sent events (`GET /_orrery/log/stream`), read
//! over HTTP from a served world: the catch-up and what follows it, resuming
//! from `Last-Event-ID`, the refusals, and how a restore ends a stream.

mod common;

use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::time::Duration;

use common::{boot, control_post};

/// How long a test waits for the next event before it fails.
const WAIT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct SseEvent {
    id: Option<String>,
    event: String,
    data: serde_json::Value,
}

/// Open a stream and read it on a thread of its own, so the test can write
/// to the world while it reads. The receiver disconnects when the server
/// ends the response.
fn open(control: &str, query: &str, last_event_id: Option<&str>) -> mpsc::Receiver<SseEvent> {
    let mut request = ureq::get(format!("{control}/_orrery/log/stream?{query}"));
    if let Some(id) = last_event_id {
        request = request.header("last-event-id", id);
    }
    let resp = request.call().expect("open the stream");
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let reader = BufReader::new(resp.into_body().into_reader());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut id, mut event, mut data) = (None, String::new(), String::new());
        for line in reader.lines() {
            let Ok(line) = line else { return };
            if line.is_empty() {
                if data.is_empty() {
                    continue;
                }
                let sent = SseEvent {
                    id: id.take(),
                    event: std::mem::take(&mut event),
                    data: serde_json::from_str(&std::mem::take(&mut data)).expect("event data"),
                };
                if tx.send(sent).is_err() {
                    return;
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((&line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value).to_string();
            match field {
                "id" => id = Some(value),
                "event" => event = value,
                "data" => data.push_str(&value),
                other => panic!("unexpected field {other:?}"),
            }
        }
    });
    rx
}

fn next(events: &mpsc::Receiver<SseEvent>) -> SseEvent {
    events.recv_timeout(WAIT).expect("the next event")
}

/// One more entry in the log.
fn append(control: &str) {
    control_post(
        control,
        "/_orrery/clock/advance",
        serde_json::json!({ "seconds": 1 }),
    );
}

/// The whole log, as `POST /_orrery/log/list` answers it.
fn whole_log(control: &str) -> Vec<serde_json::Value> {
    let mut log = Vec::new();
    let mut body = serde_json::json!({ "page_size": 500 });
    loop {
        let page = control_post(control, "/_orrery/log/list", body.clone());
        log.extend(page["items"].as_array().unwrap().iter().cloned());
        match page.get("next_page_token") {
            Some(token) => body["page_token"] = token.clone(),
            None => return log,
        }
    }
}

/// Read entries until the one with `last`, checking that each event's id is
/// its entry's `seq`.
fn entries_through(events: &mpsc::Receiver<SseEvent>, last: i64) -> Vec<serde_json::Value> {
    let mut got = Vec::new();
    while got
        .last()
        .and_then(|e: &serde_json::Value| e["seq"].as_i64())
        != Some(last)
    {
        let event = next(events);
        assert_eq!(event.event, "entry", "{event:?}");
        assert_eq!(event.id, Some(event.data["seq"].to_string()), "{event:?}");
        got.push(event.data);
    }
    got
}

fn refused(control: &str, query: &str, last_event_id: Option<&str>) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent.get(format!("{control}/_orrery/log/stream?{query}"));
    if let Some(id) = last_event_id {
        request = request.header("last-event-id", id);
    }
    let mut resp = request.call().expect("call the stream route");
    let status = resp.status().as_u16();
    (status, resp.body_mut().read_json().expect("error json"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_sends_the_log_then_each_entry_as_it_is_appended() {
    let inst = boot().await;
    let control = inst.control.clone();
    // More than one page, so the catch-up takes several reads.
    for _ in 0..150 {
        append(&control);
    }

    let events = open(&control, "after_seq=0", None);
    let writer = {
        let control = control.clone();
        std::thread::spawn(move || {
            for _ in 0..150 {
                append(&control);
            }
        })
    };
    writer.join().unwrap();

    let log = whole_log(&control);
    let last = log.last().unwrap()["seq"].as_i64().unwrap();
    assert_eq!(entries_through(&events, last), log);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_resumes_after_its_last_event_id() {
    let inst = boot().await;
    for _ in 0..5 {
        append(&inst.control);
    }
    let log = whole_log(&inst.control);
    let resumed_after = log[2]["seq"].to_string();

    // An `EventSource` reconnects to the URL it opened, whose position is
    // stale by then, and sends the last id it saw.
    let events = open(&inst.control, "after_seq=0", Some(&resumed_after));
    let last = log.last().unwrap()["seq"].as_i64().unwrap();
    assert_eq!(entries_through(&events, last), log[3..]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_position_the_log_does_not_hold_is_refused() {
    let inst = boot().await;
    append(&inst.control);
    let newest = whole_log(&inst.control).last().unwrap()["seq"]
        .as_i64()
        .unwrap();
    let unissued = (newest + 1000).to_string();

    let (status, body) = refused(&inst.control, &format!("after_seq={unissued}"), None);
    assert_eq!(status, 404, "{body}");
    let error = &body["errors"][0];
    assert_eq!(error["reason"], "NOT_FOUND");
    assert_eq!(error["field"], "after_seq");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains(&format!("the newest is {newest}")),
        "{body}"
    );

    let (status, body) = refused(&inst.control, "", Some(&unissued));
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["errors"][0]["reason"], "NOT_FOUND");
    assert!(body["errors"][0].get("field").is_none(), "{body}");

    let (status, body) = refused(&inst.control, "", None);
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        body["errors"],
        serde_json::json!([{
            "reason": "INVALID_ARGUMENT",
            "message": "after_seq is required unless the Last-Event-ID header names the position",
            "field": "after_seq",
        }])
    );

    // Malformed inputs are refused with every fault named, never read as
    // some other position.
    for (query, header, faults) in [
        (
            "after_seq=1.5",
            None,
            vec![("INVALID_ARGUMENT", Some("after_seq"))],
        ),
        (
            "after_seq=-1",
            None,
            vec![("OUT_OF_RANGE", Some("after_seq"))],
        ),
        (
            "after_seq=1&after_seq=2",
            None,
            vec![("INVALID_ARGUMENT", Some("after_seq"))],
        ),
        (
            "after_seq=x&page_size=5",
            None,
            vec![
                ("INVALID_ARGUMENT", Some("after_seq")),
                ("INVALID_ARGUMENT", Some("page_size")),
            ],
        ),
        ("", Some("seven"), vec![("INVALID_ARGUMENT", None)]),
        ("", Some("-7"), vec![("OUT_OF_RANGE", None)]),
    ] {
        let (status, body) = refused(&inst.control, query, header);
        assert_eq!(status, 400, "{query:?} {header:?}: {body}");
        let found: Vec<_> = body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["reason"].as_str().unwrap(), e["field"].as_str()))
            .collect();
        assert_eq!(found, faults, "{query:?} {header:?}: {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_ends_open_streams_and_no_seq_is_issued_twice() {
    let inst = boot().await;
    let control = inst.control.clone();
    append(&control);
    let snapshot = control_post(
        &control,
        "/_orrery/snapshots",
        serde_json::json!({ "name": "before" }),
    );
    let snapshot_seq = snapshot["last_seq"].as_i64().unwrap();
    for _ in 0..3 {
        append(&control);
    }
    let discarded = whole_log(&control).last().unwrap()["seq"].as_i64().unwrap();

    let events = open(&control, &format!("after_seq={discarded}"), None);
    let restored = control_post(
        &control,
        "/_orrery/snapshots/before/restore",
        serde_json::json!({}),
    );
    assert_eq!(restored["last_seq"].as_i64(), Some(snapshot_seq));

    let event = next(&events);
    assert_eq!(event.event, "restored", "{event:?}");
    assert_eq!(event.id, None);
    assert_eq!(
        event.data,
        serde_json::json!({ "newest_seq": snapshot_seq })
    );
    assert!(
        matches!(
            events.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ),
        "the stream did not end"
    );

    // The next entry is numbered past every one the world issued, the
    // discarded ones included.
    append(&control);
    let log = whole_log(&control);
    let appended = log.last().unwrap()["seq"].as_i64().unwrap();
    assert_eq!(log[log.len() - 2]["seq"].as_i64(), Some(snapshot_seq));
    assert!(appended > discarded, "{appended} reuses a discarded seq");

    let (status, body) = refused(&control, &format!("after_seq={discarded}"), None);
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["errors"][0]["reason"], "NOT_FOUND");
}

/// `POST /_orrery/log/list` with `after_seq` set to `seq`: `(status, json)`.
fn list_after(control: &str, seq: i64) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(format!("{control}/_orrery/log/list"))
        .send_json(serde_json::json!({ "after_seq": seq }))
        .expect("call the list route");
    let status = resp.status().as_u16();
    (status, resp.body_mut().read_json().expect("list json"))
}

/// The stream and the list read a position by one rule, so each refuses
/// what the other refuses, with the same answer, and takes what it takes.
#[tokio::test(flavor = "multi_thread")]
async fn the_stream_and_the_list_check_a_position_alike() {
    let inst = boot().await;
    let control = inst.control.clone();
    append(&control);
    let kept = control_post(
        &control,
        "/_orrery/snapshots",
        serde_json::json!({ "name": "kept" }),
    )["last_seq"]
        .as_i64()
        .unwrap();
    append(&control);
    let discarded = whole_log(&control).last().unwrap()["seq"].as_i64().unwrap();
    control_post(
        &control,
        "/_orrery/snapshots/kept/restore",
        serde_json::json!({}),
    );
    append(&control);
    let newest = whole_log(&control).last().unwrap()["seq"].as_i64().unwrap();

    for seq in [discarded, newest + 1000] {
        let streamed = refused(&control, &format!("after_seq={seq}"), None);
        assert_eq!(streamed.0, 404, "{seq}: {}", streamed.1);
        assert_eq!(list_after(&control, seq), streamed, "{seq}");
    }
    for seq in [0, kept, newest] {
        let (status, body) = list_after(&control, seq);
        assert_eq!(status, 200, "{seq}: {body}");
        let events = open(&control, &format!("after_seq={seq}"), None);
        if seq < newest {
            assert_eq!(
                entries_through(&events, newest),
                body["items"].as_array().unwrap()[..]
            );
        }
    }
}
