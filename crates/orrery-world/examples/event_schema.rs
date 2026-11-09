//! Writes `spec/event.schema.json`, the JSON Schema of the log's events, from
//! the types that define them. `tests/schema.rs` fails until the file is the
//! one this writes.
//!
//! ```text
//! cargo run -p orrery-world --example event_schema
//! ```

use std::path::Path;

fn main() -> std::io::Result<()> {
    let dest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/event.schema.json");
    std::fs::write(&dest, orrery_world::events::schema_file())?;
    println!("wrote {}", dest.display());
    Ok(())
}
