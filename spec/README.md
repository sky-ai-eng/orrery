# Orrery spec

The design source of truth for Orrery. Code must not contradict it, and a deliberate change to the design lands here in the same pull request as the code that needs it.

## What Orrery is

A deterministic mock of GitHub, for testing and benchmarking software that acts on GitHub. It serves a GitHub-faithful face (REST, GraphQL, git, and the OAuth web flow) over vendor-neutral state kept in one directory, advanced by a virtual clock, with every change recorded as an event, attributed to an actor, in an append-only log. Snapshots, forks, and a semantic diff between any two states of a world come from that design rather than being built on top of it.

A world runs in one of two ways, with the same binary and config:

- **Ephemeral:** one world per test or CI job, seeded, driven by a script, asserted against, and deleted.
- **Long-lived:** one world that stays up for an environment, with a driver producing ongoing activity. Nothing asserts against it, and it is reset from a snapshot on a schedule.

GitHub is the only face. Rule 8 is what lets another vendor's face be added without changing the engine.

## Files

| File | Contents |
| --- | --- |
| [`usage.md`](usage.md) | The two doors (face and control plane), the clock, snapshots, forks, the CLI, what a diff reports, and the determinism rules that follow |
| [`surface-github.md`](surface-github.md) | Every REST route, GraphQL field, and OAuth endpoint the GitHub face serves, how each behaves, and where Orrery chose a behavior it has not observed on GitHub |
| [`surface-git.md`](surface-git.md) | The git smart-HTTP surface and the recipe every bare repository is served with |
| [`grammar.md`](grammar.md) | Behavior every face endpoint shares: base URLs, authentication, pagination, conditional requests, rate limits, and errors |
| [`events.md`](events.md) | The events a world's log records: their envelope, the rules every one follows, and each type, with its JSON Schema in [`event.schema.json`](event.schema.json) |

## The rules

**Honesty**

1. **The surface is a list that grows on demand.** An endpoint is added when a consumer needs it, backed by a call that consumer really makes, never speculatively. An endpoint Orrery doesn't serve fails loudly and names itself (a `501` saying it is not mocked), never a plausible `404`: "known endpoint, no data" and "not implemented" are different answers. State is stored generously, close to GitHub's own representation; endpoints are strict. Behavior every endpoint shares (`grammar.md`) is built once for the engine, not per endpoint.
2. **Real tools judge realness.** Fidelity means real `git`, real `gh`, and consumers' own code working against Orrery unmodified. Orrery's own tests drive real `git` against a served world.
3. **The people in the world are scripted, never AI.** There is no LLM inside the engine. Simulated people, if they are ever wanted, are a driver outside the determinism boundary.

**Determinism**

4. **Only three things change the world:** a scenario step, a driver (scenario steps on a timer), or an outside actor's API call. Nothing changes on its own.
5. **Replay is free.** A seed snapshot plus the log rebuilds the exact world: re-executing each write's operation makes the same events again, consequences included. If it doesn't, something read a hidden input, and that is a bug.
6. **Nothing a world sends can change it.** A world's state changes only through logged calls (rule 4). The one thing a world sends is a webhook delivery, through the egress its instance configures (`usage.md`, "What a world sends"), and what the receiver answers reaches the world only as a result a logged write records. Nothing else leaves a world: no telemetry, no fetching, and no calls to an LLM.

**Shape**

7. **An instance is one config file plus one seed** (faces mounted, seed snapshot, driver schedule).
8. **State is vendor-neutral; vendors are faces.** There is one stored idea of a pull request; GitHub Enterprise Server versus github.com is a difference in how it is served. Adding a face never adds a table.
9. **A long-lived world is not special:** the same binary and config, plus longevity and a reset schedule.
10. **Delete the directory and it's gone.** No state lives outside the world root.

**Judgment**

11. **The engine's verdict is the diff; judgment lives above the engine.** Factual questions are answered from two snapshots and the log, deterministically, and the engine never calls an LLM. A benchmark built on Orrery may add judge rubrics anchored to yes-or-no questions: judges score how well something was done, never whether it happened, and their evidence is the diff, the log, and stored bodies, not agent transcripts.
12. **A snapshot is always safe to share:** every credential in a world is fake by construction.

**Scope**

13. **Benchmark features stop at what the design provides:** snapshot, fork, diff, clock, and log. Task authoring, reward design, and rollout orchestration are outside the engine.
