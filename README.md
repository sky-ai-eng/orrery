# Orrery

*(ORR-uh-ree — a clockwork model of a world: turn the crank and everything advances deterministically, in lockstep, while observers watch.)*

A deterministic mock of the software-development world. Orrery serves a GitHub-faithful API backed by **real git**, all from one snapshotable directory, advanced by a virtual clock. It exists so that the tools that watch and act on that world — pollers, CI reactors, coding agents — can be tested and benchmarked against controlled, replayable conditions instead of live SaaS state.

## Why it's built this way

- **Real tools are the judge of realness.** Clone, fetch, and push are served by the actual `git http-backend` over bare repositories, not a reimplementation of the pack protocol. If unmodified `git` (and eventually `gh`) works against Orrery, the mock is faithful; if it doesn't, the mock is wrong. This removes an entire class of fidelity bugs by construction.

- **Determinism is enforced, not hoped for.** The virtual clock is the only time source: every stamp — `created_at`, merge-commit dates, log entries — comes from it, tokens and ids are derived rather than random, and wall-clock reads and hash-ordered collections are compile errors (deny-level clippy bans). Because commit dates come from the clock too, replaying the same inputs reproduces the same SHAs byte-for-byte.

- **The world is a directory.** Everything a world is — SQLite state, bare repos — lives under one root. So a snapshot is a quiesced copy, restore is the reverse copy, fork is a copy somewhere else, and cleanup is `rm -rf`. There is no hidden state anywhere else to disagree with the copy.

- **The diff is the verdict.** Every mutation is attributed to an actor and appended to an action log, and the engine can semantically diff any two world states: PRs opened/merged, labels and review requests, reviews posted, refs moved, teams and memberships, sign-ins (`spec/usage.md` lists every section). "Did the thing actually happen?" is answered from state, never from transcripts or vibes — which is what makes a scenario simultaneously an integration test and a benchmark task.

- **Two doors, one door per operation.** Anything real GitHub has an API for goes through the face, performed by synthetic actors — including CI, since real CI systems are just GitHub Apps posting check runs. The `/_orrery/` control plane holds only what no public API can express: identity, time, snapshots. Nothing is reachable through both.

- **Unknown surface fails loudly.** The face grows strictly on observed consumer demand. An unimplemented endpoint returns a self-describing `501` naming the path — never a plausible `404` that a consumer would mistake for "no data". The two must never blur; the design docs in [`spec/`](spec/README.md) carry the full rulebook and surface inventory.

## Running it

Requires Rust and `git` on `PATH`.

```bash
cargo build --workspace
cargo test --workspace          # the E2E suite drives real git against a served world
```

Serve an instance from a config (see [`orrery.example.toml`](orrery.example.toml)):

```bash
target/debug/orrery serve --config orrery.toml
```

Then mint an actor and act through the face like any GitHub client would:

```bash
# control plane: identity and time
curl -X POST localhost:4180/_orrery/actors -d '{"login":"alice","kind":"user"}'
curl -X POST localhost:4180/_orrery/clock/advance -d '{"seconds":3600}'

# face: real GitHub shapes, real git
curl -X POST localhost:4180/api/v3/user/repos -H "authorization: token $TOKEN" -d '{"name":"webapp"}'
git clone http://x-access-token:$TOKEN@localhost:4180/alice/webapp.git

# engine ops: pause the world, act, ask what happened
curl -X POST localhost:4180/_orrery/snapshots -d '{"name":"before"}'
curl -X POST localhost:4180/_orrery/diff -d '{"from":"before"}'
```

A snapshot, or a stopped instance's world, can seed a whole new instance, and a world can be made without serving it ([`spec/usage.md`](spec/usage.md) has both commands and their `--json` output):

```bash
target/debug/orrery fork --instance ./instance --snapshot before --dest ./instance2
target/debug/orrery fork --world ./instance2/world --dest ./instance3 --secret random
target/debug/orrery create --instance ./instance4 --t0 2026-01-01T00:00:00Z
```
