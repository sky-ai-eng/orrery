# CLAUDE.md

Guidance for Claude Code in this repository. Orrery is a deterministic mock of the software-development world — a GitHub-faithful API and real git served from one snapshotable directory, advanced by a virtual clock. The design source of truth is `spec/` (start with `spec/README.md` for the 13 rules and `spec/usage.md` for the two-door usage model); code must not contradict the spec, and a deliberate spec change lands in `spec/` in the same PR as the code that needs it.

## Build, lint, test

```bash
cargo build --workspace
cargo clippy --workspace --all-targets   # deny-level determinism tripwires live in clippy.toml
cargo test --workspace                   # E2E tests exec the real `git` binary; git must be on PATH
cargo fmt --all
```

## Architecture in one paragraph

`orrery-world` is the kernel: world-root layout, SQLite state, the virtual clock, actors/tokens, the collaboration objects (pulls, reviews, comments, check runs), and the append-only action log. It has no HTTP dependencies and no vendor vocabulary — stored state is neutral (`approved`, not `APPROVED`); faces render vendor casing at the wire. Faces adapt vendors onto the kernel: `orrery-face-git` (bare-repo store, git smart-HTTP via CGI to real `git http-backend`, push observation, the merge machinery) and `orrery-face-github` (the GitHub REST surface, growing strictly on demand per `spec/surface-github.md`). `orrery-snapshot` treats a snapshot as a world, paused: quiesced directory copies under `<instance>/snapshots/`, restore via the kernel's connection swap, fork to a new instance, and the semantic diff between any two world states. `orrery-server` hosts faces plus the `/_orrery/` control plane and owns cross-cutting HTTP behavior (auth middleware, the NOT-MOCKED fallback, the world write gate that keeps snapshot copies exact). `crates/orrery` is the thin CLI binary (`serve`, `fork`).

## The rules that shape every change

- **Two doors, one door per operation** (`spec/usage.md`). Anything real GitHub has an API for is served by the face and only the face. The control plane (`/_orrery/`) holds only what no public API can express: actors, the clock, engine ops. Never add a control-plane verb that duplicates a face capability.
- **Face fidelity overrides house API style.** Face endpoints mimic real GitHub — its URL shapes, status codes, error bodies, and its *lax* body decoding (GitHub ignores unknown fields; so do we). The judge of face behavior is real `git`/`gh`/consumer code running unmodified, not our own taste. When we deliberately can't support a behavioral request field, reject it loudly with an Orrery-flavored error naming the field — never silently ignore something a caller meant.
- **Control-plane endpoints follow house style, strictly.** One intent per route; strict body decoding (unknown fields rejected); invalid values rejected with the field named, never clamped or defaulted; the one error shape `{"errors":[{"reason","message","field?"}]}`; list reads are keyset-paginated with a deterministic total order.
- **Unknown face surface fails loudly.** An unmatched face path returns the 501 NOT-MOCKED envelope naming the path and pointing at the surface inventory. A matched route with no data returns the vendor-faithful 404. Never let these two blur — a plausible 404 for unimplemented surface is how consumers silently break.
- **Determinism is enforced, not hoped for.** `clippy.toml` bans wall-clock reads and hash-ordered collections at deny level. The world's only time source is `World::clock`. Every mutation is attributed to an actor and recorded in the action log. Tokens and synthetic ids are derived, never random. Subprocess git runs hermetically: `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, `GIT_TERMINAL_PROMPT=0` — the operator's `~/.gitconfig` must never leak into world behavior.
- **The engine never touches the network** beyond its own listener, and never calls an LLM. No outbound HTTP, no telemetry, no fetching anything.

## Database rules

- All persistence goes through `orrery-world`; no other crate opens the SQLite file. One process, one writer — do not grow a repository-pattern layer or a second dialect.
- The schema is one definition (`orrery-world/src/schema.rs`) applied when a world is created. No world is deployed, so a schema change edits it in place and bumps `SCHEMA_VERSION`; a world written under another version is refused at open, not migrated. Tables are `STRICT`.
- **A single-row write returns the row it persisted**, sourced from `RETURNING` on the write statement itself — never a follow-up read — projecting the same columns the point read uses. A caller must never re-read to learn what the write already returned. Conformance tests assert returned-row == point-read for every such method.
- The action log is append-only: no UPDATE or DELETE ever targets `action_log`. Its `seq` is the total order every replay and diff hangs off.

## Coding conventions

- Comments explain **why**, never what or how; they never narrate the history of a change ("formerly", "renamed from") — the codebase reads as a description of itself at this commit. Change stories live in commit messages and PR bodies.
- No ticket references in code comments, with one exception: deferred work is marked `// TODO(#NN): <what is missing>` against an existing GitHub issue that actually describes the gap. If nothing tracks it, either do it now or file the issue first. A TODO pointing nowhere is worse than none.
- Errors are typed (`thiserror`) in libraries; `anyhow` only in the binary. Map domain conflicts to typed variants (e.g., repo-exists), never string-match error text.
- Tests exercise real tools where the spec's rule 2 applies: the git face's tests run real `git` against the served endpoint (the `blobless` clone recipe from `spec/surface-git.md`), not simulated requests.
- Keep crate boundaries honest: `orrery-world` must never grow an HTTP or vendor-format dependency; faces must never open the DB directly.
