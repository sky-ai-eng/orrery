//! Writes `spec/event.schema.json`, the JSON Schema of the log's events, from
//! the types that define them, and records the events of this schema version
//! in `tests/fixtures/event-schemas`. `tests/schema.rs` fails until both are
//! the ones this writes.
//!
//! ```text
//! cargo run -p orrery-world --example event_schema
//! ```

use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use orrery_world::SCHEMA_VERSION;
use orrery_world::events::{schema_digest, schema_file};

fn main() -> std::io::Result<ExitCode> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ledger_path = crate_dir.join("tests/fixtures/event-schemas");
    let ledger = std::fs::read_to_string(&ledger_path)?;
    let version = SCHEMA_VERSION.to_string();
    let digest = schema_digest();
    match ledger
        .lines()
        .find_map(|line| line.split_once(' ').filter(|(v, _)| *v == version))
    {
        Some((_, recorded)) if recorded == digest => {}
        // A world at this version may hold the events it records.
        Some(_) => {
            eprintln!(
                "schema version {version} records other events. Add a migration \
                 first, with no SQL if no table changes (tests/fixtures/README.md)"
            );
            return Ok(ExitCode::FAILURE);
        }
        None => {
            let mut ledger = std::fs::OpenOptions::new()
                .append(true)
                .open(&ledger_path)?;
            writeln!(ledger, "{version} {digest}")?;
            println!(
                "recorded schema version {version} in {}",
                ledger_path.display()
            );
        }
    }
    let dest = crate_dir.join("../../spec/event.schema.json");
    std::fs::write(&dest, schema_file())?;
    println!("wrote {}", dest.display());
    Ok(ExitCode::SUCCESS)
}
