//! The published schema of the log's events is the one their types make, and
//! the events are the ones recorded for this schema version.

use orrery_world::SCHEMA_VERSION;

#[test]
fn the_published_event_schema_is_the_one_the_types_make() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/event.schema.json");
    let published = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        published == orrery_world::events::schema_file(),
        "{path} is not the schema the event types make; write it again with \
         `cargo run -p orrery-world --example event_schema`"
    );
}

#[test]
fn the_events_are_the_ones_recorded_for_this_schema_version() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/event-schemas");
    let ledger = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let version = SCHEMA_VERSION.to_string();
    let recorded = ledger
        .lines()
        .find_map(|line| line.split_once(' ').filter(|(v, _)| *v == version))
        .map(|(_, digest)| digest);
    let Some(recorded) = recorded else {
        panic!(
            "{path} has no line for schema version {version}; record it with \
             `cargo run -p orrery-world --example event_schema`"
        );
    };
    assert!(
        recorded == orrery_world::events::schema_digest(),
        "{path} records other events for schema version {version}. An engine \
         at this version would open a world with these events and fail to read \
         them, so a change to the events needs a migration, with no SQL if no \
         table changes (tests/fixtures/README.md). If this branch recorded the \
         line, delete it and record it again."
    );
}
