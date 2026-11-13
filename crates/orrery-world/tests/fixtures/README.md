# World database fixtures

`v<version>.db` is a small world database written by the binary at that schema version. A world database outlives the binary that wrote it, so every version a real world can be at needs one here. `crates/orrery-snapshot/tests/migrations.rs` migrates each fixture to the current version, checks that every table still has its rows, and diffs it.

There is one fixture for the baseline (`BASELINE_VERSION`), plus one per migration for the version it migrates from. The test fails if one is missing or stray.

## Building one

```bash
cargo run -p orrery-world --example fixture
```

This writes `v<SCHEMA_VERSION>.db` for the checked-out code. The world it builds is in `crates/orrery-world/examples/fixture.rs`. It has rows in every table, so a table added to the schema gets rows there in the same change.

## When

- **Adding a migration:** if the current version has no fixture yet, build it first, then add the migration. Once the migration is in, the code at the old version is gone, and so is the only thing that can write a faithful fixture for it. Then record the new version's events (below).
- **Never change the baseline.** World databases outlive the binary that wrote them, so every schema change is a new forward migration with a fixture, and the baseline never changes. `BASELINE_VERSION` stays where it is, and `v<BASELINE_VERSION>.db` stays the oldest fixture.
- **Never rebuild a committed fixture.** It records what a world at that version really looked like. If you rebuild it with today's builder, it stops being that record. The example refuses to overwrite one.

## The events of each version

`event-schemas` holds a line for each schema version from 14: the version, then a digest of the shape of the event bodies a world at that version stores (`schema_digest` in `src/events.rs`). A doc comment or the order of a declaration doesn't change it. The schema version covers the events a world logs as well as its tables, so a change to the events (a type, a field, or a value a field can hold) is a migration too. `tests/schema.rs` fails until the current version's line matches the events.

Events only grow (`spec/events.md`, rule 7). A migration lets an older binary refuse a world whose events it doesn't know, but a type removed or a field renamed leaves the newer binary unable to read the events a world already logged. So `crates/orrery-snapshot/tests/migrations.rs` reads the whole log of each fixture from version 10, the first whose log holds events.

To change the events:

1. Build the current version's fixture, if it has none.
2. Add a migration. When no table changes, it is a comment naming the change to the events, and nothing else.
3. Change the events, then record the new version and write `spec/event.schema.json`:

   ```bash
   cargo run -p orrery-world --example event_schema
   ```

The example refuses to change a version's line once it is there, since a world at that version may hold the events it records. No world is at a version your own branch added yet, so to change the events again before the branch merges, delete the line it recorded and run the example again.

An upgrade of schemars can change how the same types render, and with them the digest. When that upgrade's diff of `spec/event.schema.json` changes no event, write the new digest over the current version's line in the same change.
