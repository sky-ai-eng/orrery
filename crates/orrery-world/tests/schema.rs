//! The published schema of the log's events is the one their types make.

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
