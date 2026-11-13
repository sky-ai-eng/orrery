//! Writes `spec/event.schema.json`, the JSON Schema of the log's events, from
//! the types that define them, and records the events of this schema version
//! in `tests/fixtures/event-schemas`. `tests/schema.rs` fails until both are
//! the ones this writes.
//!
//! ```text
//! cargo run -p orrery-world --example event_schema
//! ```

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
                 first (tests/fixtures/README.md), or, if this branch recorded \
                 the line, delete it and run this again."
            );
            return Ok(ExitCode::FAILURE);
        }
        None => {
            let mut ledger = ledger;
            if !ledger.is_empty() && !ledger.ends_with('\n') {
                ledger.push('\n');
            }
            ledger.push_str(&format!("{version} {digest}\n"));
            std::fs::write(&ledger_path, ledger)?;
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
