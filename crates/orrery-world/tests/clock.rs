//! A world's clock in its two modes (spec/usage.md, "Time"). The host's time
//! is set through the world, so no test waits for time to pass.

use orrery_world::{
    ActorKind, Attribution, ClockMode, ClockReading, DEFAULT_T0_UNIX, Door, Event, NewClock,
    SYSTEM_ACTOR_ID, World, WorldError, WorldSecret,
};

const T0: i64 = DEFAULT_T0_UNIX;
const CONTROL: Attribution = Attribution::new(SYSTEM_ACTOR_ID, Door::Control);

fn manual() -> (tempfile::TempDir, World) {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create(&dir.path().join("world"), T0, &WorldSecret::LOCAL).unwrap();
    (dir, w)
}

/// A world made `real` with the host's time at `host`.
fn real_at(host: i64) -> (tempfile::TempDir, World) {
    let (dir, w) = manual();
    w.set_host_time(host);
    w.set_clock_mode(ClockMode::Real, CONTROL).unwrap();
    (dir, w)
}

fn log(w: &World) -> Vec<Event> {
    w.log_after(0, 1000).unwrap()
}

/// The events after `seq`.
fn since(w: &World, seq: i64) -> Vec<Event> {
    w.log_after(seq, 1000).unwrap()
}

fn last_seq(w: &World) -> i64 {
    log(w).last().map_or(0, |e| e.seq)
}

fn body(e: &Event) -> serde_json::Value {
    serde_json::to_value(&e.body).unwrap()
}

/// Assert `e` is an advance from `from` to `to`, made as a write of its own
/// by the system actor through `door`.
fn assert_advance(e: &Event, from: i64, to: i64, door: Door) {
    assert_eq!(e.body.type_name(), "world:clock_advanced");
    assert_eq!(
        body(e)["changes"],
        serde_json::json!({ "clock_unix": { "from": from, "to": to } })
    );
    assert_eq!((e.write.seq, e.write.events), (e.seq, 1));
    assert_eq!(e.actor.id, SYSTEM_ACTOR_ID);
    assert_eq!(e.door, door);
    assert_eq!(e.clock_unix, to);
}

#[test]
fn a_manual_world_ignores_the_hosts_time() {
    let (_dir, w) = manual();
    w.set_host_time(T0 + 1000);
    assert_eq!(
        w.read_clock().unwrap(),
        ClockReading {
            clock_unix: T0,
            mode: ClockMode::Manual
        }
    );
    let before = last_seq(&w);
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    assert_eq!(alice.created_clock_unix, T0);
    let added = since(&w, before);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].body.type_name(), "actor:created");
}

#[test]
fn making_a_world_real_advances_it_to_the_hosts_time_then_sets_the_mode() {
    let (_dir, w) = manual();
    let before = last_seq(&w);
    w.set_host_time(T0 + 100);
    let reading = w.set_clock_mode(ClockMode::Real, CONTROL).unwrap();
    assert_eq!(
        reading,
        ClockReading {
            clock_unix: T0 + 100,
            mode: ClockMode::Real
        }
    );
    let added = since(&w, before);
    assert_eq!(added.len(), 2);
    assert_advance(&added[0], T0, T0 + 100, Door::Control);
    assert_eq!(added[1].body.type_name(), "world:clock_mode_set");
    assert_eq!(
        body(&added[1])["changes"],
        serde_json::json!({ "clock_mode": { "from": "manual", "to": "real" } })
    );
    assert_eq!(
        (added[1].write.seq, added[1].write.events),
        (added[1].seq, 1)
    );
}

#[test]
fn a_read_answers_the_present_and_writes_nothing() {
    let (_dir, w) = real_at(T0 + 100);
    let before = log(&w);
    w.set_host_time(T0 + 500);
    assert_eq!(w.clock().unwrap(), T0 + 500);
    assert_eq!(w.read_clock().unwrap().mode, ClockMode::Real);
    assert_eq!(log(&w), before, "an idle world appends nothing");
}

#[test]
fn a_write_first_records_an_advance_to_the_hosts_time() {
    let (_dir, w) = real_at(T0 + 100);
    let before = last_seq(&w);
    w.set_host_time(T0 + 500);
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    assert_eq!(alice.created_clock_unix, T0 + 500);
    let added = since(&w, before);
    assert_eq!(added.len(), 2);
    assert_advance(&added[0], T0 + 100, T0 + 500, Door::Control);
    assert_eq!(added[1].body.type_name(), "actor:created");
    assert_eq!(added[1].clock_unix, T0 + 500);
    assert_eq!(
        (added[1].write.seq, added[1].write.events),
        (added[1].seq, 1)
    );

    // At the same host time there is nothing to record.
    let before = last_seq(&w);
    w.create_actor("bob", ActorKind::User).unwrap();
    let added = since(&w, before);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].body.type_name(), "actor:created");
}

#[test]
fn the_clock_holds_while_the_host_is_behind_it() {
    let (_dir, w) = real_at(T0 + 500);
    w.set_host_time(T0 + 400);
    assert_eq!(w.clock().unwrap(), T0 + 500);
    let before = last_seq(&w);
    let (alice, _) = w.create_actor("alice", ActorKind::User).unwrap();
    assert_eq!(alice.created_clock_unix, T0 + 500);
    assert_eq!(since(&w, before).len(), 1);

    w.set_host_time(T0 + 501);
    assert_eq!(w.clock().unwrap(), T0 + 501);
}

#[test]
fn a_write_that_fails_or_appends_nothing_keeps_no_advance() {
    let (_dir, w) = real_at(T0 + 100);
    w.create_actor("alice", ActorKind::User).unwrap();
    let before = log(&w);
    w.set_host_time(T0 + 200);
    assert!(matches!(
        w.create_actor("alice", ActorKind::User),
        Err(WorldError::ActorExists(_))
    ));
    // Setting the mode it has changes nothing.
    assert_eq!(
        w.set_clock_mode(ClockMode::Real, CONTROL).unwrap(),
        ClockReading {
            clock_unix: T0 + 200,
            mode: ClockMode::Real
        }
    );
    assert_eq!(log(&w), before);
    // The stored clock stayed where it was: with the host behind it, a read
    // answers it.
    w.set_host_time(T0);
    assert_eq!(w.clock().unwrap(), T0 + 100);
}

#[test]
fn a_real_world_is_never_advanced_by_hand() {
    let (_dir, w) = real_at(T0 + 100);
    let before = log(&w);
    w.set_host_time(T0 + 200);
    assert!(matches!(
        w.advance_clock(60, CONTROL),
        Err(WorldError::ClockIsReal)
    ));
    assert_eq!(log(&w), before);
    w.set_host_time(T0);
    assert_eq!(w.clock().unwrap(), T0 + 100);
}

#[test]
fn making_a_world_manual_records_an_advance_to_the_present_and_stops_there() {
    let (_dir, w) = real_at(T0 + 100);
    let before = last_seq(&w);
    w.set_host_time(T0 + 700);
    assert_eq!(
        w.set_clock_mode(ClockMode::Manual, CONTROL).unwrap(),
        ClockReading {
            clock_unix: T0 + 700,
            mode: ClockMode::Manual
        }
    );
    let added = since(&w, before);
    assert_eq!(added.len(), 2);
    assert_advance(&added[0], T0 + 100, T0 + 700, Door::Control);
    assert_eq!(
        body(&added[1])["changes"],
        serde_json::json!({ "clock_mode": { "from": "real", "to": "manual" } })
    );

    w.set_host_time(T0 + 900);
    assert_eq!(w.clock().unwrap(), T0 + 700);
    assert_eq!(w.advance_clock(60, CONTROL).unwrap(), T0 + 760);
}

#[test]
fn a_world_whose_clock_is_ahead_of_the_host_cannot_become_real() {
    let (_dir, w) = manual();
    w.advance_clock(1000, CONTROL).unwrap();
    let before = log(&w);
    w.set_host_time(T0 + 999);
    assert!(matches!(
        w.set_clock_mode(ClockMode::Real, CONTROL),
        Err(WorldError::ClockAhead { clock, host }) if clock == T0 + 1000 && host == T0 + 999
    ));
    assert_eq!(log(&w), before);
    assert_eq!(w.read_clock().unwrap().mode, ClockMode::Manual);

    // With the host at the clock, nothing moves but the mode.
    w.set_host_time(T0 + 1000);
    w.set_clock_mode(ClockMode::Real, CONTROL).unwrap();
    let added = since(&w, before.last().unwrap().seq);
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].body.type_name(), "world:clock_mode_set");
}

#[test]
fn quiescing_a_real_world_brings_its_clock_to_the_present() {
    let (_dir, w) = real_at(T0 + 100);
    let before = last_seq(&w);
    w.set_host_time(T0 + 300);
    w.quiesce(|| ()).unwrap();
    let added = since(&w, before);
    assert_eq!(added.len(), 1);
    assert_advance(&added[0], T0 + 100, T0 + 300, Door::Control);

    // A manual world's is left where it is.
    let (_dir, m) = manual();
    let before = log(&m);
    m.set_host_time(T0 + 300);
    m.quiesce(|| ()).unwrap();
    assert_eq!(log(&m), before);
}

#[test]
fn the_mode_survives_a_reopen() {
    let (dir, w) = real_at(T0 + 100);
    drop(w);
    let w = World::open(&dir.path().join("world")).unwrap();
    assert_eq!(w.read_clock().unwrap().mode, ClockMode::Real);
}

#[test]
fn a_world_created_real_starts_at_the_hosts_time() {
    let dir = tempfile::tempdir().unwrap();
    let w = World::create_with_clock(
        &dir.path().join("world"),
        NewClock::Real,
        &WorldSecret::LOCAL,
    )
    .unwrap();
    let created = &log(&w)[0];
    assert_eq!(created.body.type_name(), "world:created");
    assert_eq!(body(created)["object"]["clock_mode"], "real");
    // The clock was read from the host just before the event recorded the
    // host's time it was appended at.
    let clock = body(created)["object"]["clock_unix"].as_i64().unwrap();
    assert_eq!(created.clock_unix, clock);
    let lag = created.wall_unix_ms - clock * 1000;
    assert!(
        (0..60_000).contains(&lag),
        "{clock} vs {}",
        created.wall_unix_ms
    );
}

#[test]
fn the_advance_before_a_cli_write_is_the_clis() {
    let (dir, w) = real_at(T0 + 100);
    drop(w);
    // Forking re-keys the copy, a write through the CLI. The host's clock is
    // years past the world's.
    let root = dir.path().join("world");
    let before = {
        let w = World::open(&root).unwrap();
        last_seq(&w)
    };
    let w = World::open_rekeyed(&root, &WorldSecret::from_bytes([9; 32])).unwrap();
    let added = since(&w, before);
    assert_eq!(added.len(), 2);
    assert_eq!(added[0].body.type_name(), "world:clock_advanced");
    assert_eq!(added[0].door, Door::Cli);
    assert_eq!(added[0].actor.id, SYSTEM_ACTOR_ID);
    assert_eq!(added[1].body.type_name(), "world:rekeyed");
    assert!(added[1].clock_unix > T0 + 100);
}
