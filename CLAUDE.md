# CLAUDE.md

Guidance for Claude Code in this repository. Orrery is a deterministic mock of the software-development world — a GitHub-faithful API and real git served from one snapshotable directory, advanced by a virtual clock. The design source of truth is `spec/` (start with `spec/README.md` for the 13 rules and `spec/usage.md` for the two-door usage model); code must not contradict the spec, and a deliberate spec change lands in `spec/` in the same PR as the code that needs it.

**Write everything here as if this repository were public.** Some of Orrery's consumers are private. Tickets may describe them; nothing committed here may: not code, comments, docs, tests, commit messages, or PR bodies. A feature a private consumer needs is described in engine terms, as something the engine does for any caller ("a supervisor running many worlds", "a world whose faces are on the public internet"), never by naming who wants it or why.

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
- **Control-plane endpoints follow house style, strictly.** See "The control plane's API" below.
- **Unknown face surface fails loudly.** An unmatched face path returns the 501 NOT-MOCKED envelope naming the path and pointing at the surface inventory. A matched route with no data returns the vendor-faithful 404. Never let these two blur — a plausible 404 for unimplemented surface is how consumers silently break.
- **Determinism is enforced, not hoped for.** `clippy.toml` bans wall-clock reads and hash-ordered collections at deny level. The world's only time source is `World::clock`. Every mutation is attributed to an actor and recorded in the action log. Tokens and synthetic ids are derived, never random. Subprocess git runs hermetically: `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, `GIT_TERMINAL_PROMPT=0` — the operator's `~/.gitconfig` must never leak into world behavior.
- **The engine never touches the network** beyond its own listener, and never calls an LLM. No outbound HTTP, no telemetry, no fetching anything.

## The control plane's API

A core principle of engineering in the era of AI is that the entire product must be drivable purely from the API. To that end, every `/_orrery/` route must follow these rules. Faces don't: a face copies its source (like GitHub), including its lax decoding and its error bodies. A route that doesn't meet a rule yet is converged when it is next touched; never add a new deviation.

- **One intent per route.** A route does one nameable thing with a fixed body schema. No discriminator field that changes which other fields are meaningful, and no field whose *emptiness* selects a second behavior (blank-means-delete, blank-means-keep). If two operations need different required fields, validation, or response shapes, they are two routes. Verb routes (`POST …/snapshots/{name}/restore`) are only for effects a field write can't express.
- **Bad calls fail.** Strict body decoding — unknown fields are rejected. Invalid values are rejected with the field named — never clamped, defaulted, case-folded, or silently dropped. A malformed filter is rejected and must never *widen* a result set by falling back. Never report success for work that didn't happen.
- **Uniform reads.** A resource is read by `GET …/<resource>/{key}` (a snapshot by its name) and listed by `POST …/<resource>/list`. Lists are keyset-paginated over a total order, with a visible cursor named for its key: the request takes `page_size` and `after_<key>` (`after_seq`, `after_name`), and the answer is `{items, next_after_<key>}`, where `next_after_<key>` is null on the last page. An out-of-range `page_size` is 400, never clamped. No unbounded fetch-the-world reads, and no hidden limit that truncates silently. Reads are side-effect-free. This differs from Triage Factory's opaque `page_token` and `total_count` on purpose: a world's log position and a snapshot's name are meaningful values a caller can hold, compare, and pass to other routes.
- **A statistic is its own read.** Numbers *about* rows (counts, a distribution, coverage) get their own read-only `GET` with a fixed schema of named cuts — never a `view=`/`group_by=` parameter that changes a response's shape. A different derivation is a different path.
- **One error shape.** `{"errors": [{"reason": "SCREAMING_SNAKE_CODE", "message": "prose", "field": "optional"}]}` — always a list; validation reports *every* failing field, not the first. `reason` is a stable machine code; `field` appears only for payload-field faults. No plain-text bodies, and no 200 with an error.
- **Updates are PATCH on the resource,** field by field: absent = keep, explicit `null` = clear. No full-replace PUT that is really a partial write, and no verb route for what is a field write. A PUT replaces the whole value it names (`PUT …/actors/{login}/emails` sets the full list).
- **The API is the whole product; the CLI is one client.** Every route is also driven by scripts, agents, tests, and other services, so no rule may exist only in a client. A CLI command is a literal alias for one documented call (`spec/usage.md`), and the route itself enforces anything the command checks before calling it. The exceptions are `create` and `fork`, which act on world directories no process is serving, so there is no route to call.

## Database rules

- All persistence goes through `orrery-world`; no other crate opens the SQLite file. One process, one writer — do not grow a repository-pattern layer or a second dialect.
- The schema is a baseline plus an ordered list of forward migrations (`orrery-world/src/schema.rs`). A world migrates when it opens; one newer than the binary, or older than the baseline, is refused. World databases outlive the binary that wrote them, so every schema change is a new forward migration with a fixture in `orrery-world/tests/fixtures/` (see its README), and the baseline never changes. Tables are `STRICT`.
- **A single-row write returns the row it persisted**, sourced from `RETURNING` on the write statement itself — never a follow-up read — projecting the same columns the point read uses. A caller must never re-read to learn what the write already returned. Conformance tests assert returned-row == point-read for every such method.
- The action log is append-only: no UPDATE or DELETE ever targets `action_log`. Its `seq` is the total order every replay and diff hangs off.

## Prose style

Mannered prose substitutes metaphor and flourish for direct statement. Instead of "a parameter worth varying," the mannered writer produces "a dial worth turning." Instead of "this point still matters," they write "this point earns its keep." The phrases exist to display the writer, not to convey the idea, and readers can tell. That is why mannered prose irritates: it makes the reader work harder so the writer can perform. It is also imprecise. Metaphors drag in connotations the writer did not choose and cannot control. The fix is to say what you mean. When a literal phrase is available, use it.

Every list takes the serial comma: "faces, the control plane, and the CLI".

## Comments and deferred work

- **Comments explain why; they never restate what the code does or assert what other code does.**
- **Comments describe what ships in this commit — never the history of how it got there.** When you remove or change something, just change it. No "formerly", no "renamed from", no "this replaced X", no note that an alias or fallback once existed and was deleted: a comment narrating code the reader cannot see sends them hunting for something that does not exist. The story of a change is told once, in the commit message and PR body. The one look-backward that belongs in source is a constraint that still binds ("stored worlds carry this value"), stated as the constraint it is.
- **No ticket references in code comments — with exactly one exception,** the deferral marker below. Ticket ids belong in commit messages, PR bodies, and branch names.
- **Deferred work is tracked in code or it does not exist.** Any work knowingly left undone — a case not handled, a shape not covered, a follow-up someone intends to do — is marked at the place it is missing:

  ```rust
  // TODO(ORR-NN): <what is missing, and what depends on it>
  // TODO(#NN): <the same, when a GitHub issue in this repository is the tracker>
  ```

  Either form is fine, and one is enough: the Linear team (ORR) is contributor-only, so an outside contributor's tracker is a GitHub issue here. Two conditions, both required: the tracker must **already exist and already describe this specific gap**, and the marker goes at the site of the gap, not in a PR description. The alternative to a marker is doing the work now. If nothing covers it, file it first or say so — a `TODO` pointing somewhere that does not describe the gap is worse than no `TODO` at all, because it looks tracked.

## Commits and pull requests

- **Titles,** for commits and PRs alike, are conventional commits: `type(scope): imperative summary (ORR-NN)`, with the ticket suffix only when there is a ticket. Types: `feat`, `fix`, `perf`, `refactor`, `ci`, `docs`, `chore`, `test`. Scopes name the area: `world`, `face`, `github`, `snapshot`, `serve`, `spec`.
- **Bodies** say what was wrong, what changed, and how it was checked, then stop. The diff already says how; don't restate it, and don't narrate the path that led here or the alternatives passed over. Call out anything that could be missing or problematic.

## Coding conventions

- Errors are typed (`thiserror`) in libraries; `anyhow` only in the binary. Map domain conflicts to typed variants (e.g., repo-exists), never string-match error text.
- Tests exercise real tools where the spec's rule 2 applies: the git face's tests run real `git` against the served endpoint (the `blobless` clone recipe from `spec/surface-git.md`), not simulated requests.
- Keep crate boundaries honest: `orrery-world` must never grow an HTTP or vendor-format dependency; faces must never open the DB directly.
