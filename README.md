# Orrery

*(ORR-uh-ree — a clockwork model of a world: turn the crank and everything advances deterministically, in lockstep, while observers watch.)*

A deterministic mock of the software-development world. Orrery serves a GitHub-faithful API backed by **real git**, all from one snapshotable directory, advanced by a virtual clock. It exists so that the tools that watch and act on that world — pollers, CI reactors, coding agents — can be tested and benchmarked against controlled, replayable conditions instead of live SaaS state.

## Why it's built this way

- **Real tools are the judge of realness.** Clone, fetch, and push are served by the actual `git http-backend` over bare repositories, not a reimplementation of the pack protocol. If unmodified `git` (and eventually `gh`) works against Orrery, the mock is faithful; if it doesn't, the mock is wrong. This removes an entire class of fidelity bugs by construction.

- **Determinism is enforced.** The virtual clock is the only time source: every stamp — `created_at`, merge-commit dates, log entries — comes from it, tokens and ids are derived rather than random, and wall-clock reads and hash-ordered collections are compile errors (deny-level clippy bans). Because commit dates come from the clock too, replaying the same inputs reproduces the same SHAs byte-for-byte.

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
eval "$(target/debug/orrery env)"   # date the next commits at the world clock, not the laptop's

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

## Releases

Each [release](https://github.com/sky-ai-eng/orrery/releases) carries an `orrery` binary for Linux and macOS, on x86_64 and arm64, one `tar.gz` per platform. The Linux binaries are static, so they run on any distribution. `git` must be on `PATH`.

```bash
gh release download --repo sky-ai-eng/orrery \
  --pattern 'orrery_*_linux_amd64.tar.gz' --pattern 'checksums.txt*'
```

**Versions.** Orrery is versioned `0.y.z`. A release that moves `y` (0.3.4 to 0.4.0) breaks something an existing caller relies on: a command or flag, `--json` output or the ready line, a control-plane route, the instance config file, or opening a world an earlier release wrote. Every other release moves only `z`. That includes new face surface and any change that brings a face closer to GitHub, because GitHub's behavior is what a face promises. [`CHANGELOG.md`](CHANGELOG.md) lists each release's changes.

**Verifying a download.** `checksums.txt` holds the sha256 of every archive, and it is signed keylessly with [cosign](https://docs.sigstore.dev/) through GitHub Actions' OIDC identity, so there is no key to fetch first. With the archive, `checksums.txt`, `checksums.txt.sig`, and `checksums.txt.pem` in one directory:

```bash
cosign verify-blob --certificate checksums.txt.pem --signature checksums.txt.sig \
  --certificate-identity-regexp '^https://github\.com/sky-ai-eng/orrery/\.github/workflows/release\.yml@refs/tags/.+$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com checksums.txt
sha256sum --ignore-missing -c checksums.txt
```

The identity names the release workflow and a tag push, not just the repository, so a signature made by any other workflow here fails. Each archive also has an SPDX SBOM beside it, `<archive>.sbom.json`.
