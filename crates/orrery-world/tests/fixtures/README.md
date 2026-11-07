# World database fixtures

`v<version>.db` is a small world database written by the binary at that schema version. A world database outlives the binary that wrote it, so every version a real world can be at needs one here. `crates/orrery-snapshot/tests/migrations.rs` migrates each fixture to the current version, checks that every table still has its rows, and diffs it.

There is one fixture for the baseline (`BASELINE_VERSION`), plus one per migration for the version it migrates from. The test fails if one is missing or stray.

## Building one

```bash
cargo run -p orrery-world --example fixture
```

This writes `v<SCHEMA_VERSION>.db` for the checked-out code. The world it builds is in `crates/orrery-world/examples/fixture.rs`. It has rows in every table, so a table added to the schema gets rows there in the same change.

## When

- **Adding a migration:** if the current version has no fixture yet, build it first, then add the migration. Once the migration is in, the code at the old version is gone, and so is the only thing that can write a faithful fixture for it.
- **Never change the baseline.** World databases outlive the binary that wrote them, so every schema change is a new forward migration with a fixture, and the baseline never changes. `BASELINE_VERSION` stays where it is, and `v<BASELINE_VERSION>.db` stays the oldest fixture.
- **Never rebuild a committed fixture.** It records what a world at that version really looked like. If you rebuild it with today's builder, it stops being that record. The example refuses to overwrite one.
