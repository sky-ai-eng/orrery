# Orrery — spec

*Orrery* (ORR-uh-ree, /ˈɒrəri/): a clockwork mechanical model of a world — turn the crank and everything advances deterministically, in lockstep, while observers watch. Named for the Earl of Orrery, who commissioned the first one (~1704).

**Status:** surface inventory complete (2026-08-21, extracted from triage-factory at commit `78c8bfd2`). Design pass pending. This folder becomes the Orrery repo; the spec lives here from day one so it is never committed into triage-factory.

## What this is

A standalone service that mocks the external world Triage Factory consumes, so TF instances can be tested against a controlled, deterministic, snapshot-able environment. One engine, two deployment shapes:

- **Ephemeral** — booted per CI job / per test, seeded, driven by a scenario script, torn down. Deterministic; assertions live here.
- **Persistent per environment ring** — dev/test/staging each run one long-lived instance with a churn driver producing ambient events. Nothing asserts against it; weekly reset from a seed snapshot.

V1 face: GitHub (REST + GraphQL + git smart-HTTP over real bare repos), presenting as a GHES host so TF's existing base-URL config is the integration seam. Jira/Slack faces deferred (a free Jira Cloud site + free Slack workspace cover those tiers meanwhile).

Designed with benchmark/RL affordances that fall out free of the architecture — snapshot, fork, semantic diff, virtual clock, actor-attributed action log — without productizing any of that.

## Files

| File | Contents |
| --- | --- |
| `surface-github.md` | Every GitHub API call TF makes or proxies: REST reads, GraphQL, the three write producers, App/auth endpoints, the inbound webhook contract |
| `surface-git.md` | The git protocol surface and the exact server recipe (`git http-backend` config) |
| `surface-jira-slack.md` | The full Jira surface (both `/rest/api/2` and `/3` trees, OAuth, writes) and the Slack method list |
| `grammar.md` | Cross-cutting platform grammar built once, engine-wide: auth shapes, base URLs, rate limiting, retries, pagination regimes, ETags |
| `findings.md` | TF doc drift, dead code, probable bugs discovered during extraction, and the live-wire verification list |

## Provenance and method

Extracted by six parallel read-only code sweeps over the triage-factory tree at commit `78c8bfd2` (2026-08-21): poll-path reads, non-poll reads, the write surface (exec verbs + gh injector + TF Go), git protocol, Jira, and cross-cutting grammar. `file:line` references throughout point into the **triage-factory** repo at that commit. Items that code reading cannot settle (upstream behavior TF merely relies on) are listed in `findings.md` § "Live-wire verification" and must be confirmed against real GitHub/Jira before Orrery hardcodes them.

## The constraint preamble (13 rules)

Settled in design discussion 2026-08-21; the design pass elaborates these.

**Honesty**
1. **The surface is a list that grows on demand.** Every endpoint enters the inventory when a consumer needs it, backed by an observed real call — never speculatively. Unknown endpoints fail loudly and self-describe ("not mocked: add X to the inventory"), never a plausible 404 — "known endpoint, no data" and "not implemented" are distinct answers. The state model stores objects generously (near GitHub's own representation: fields liberal, endpoints strict). Cross-cutting grammar (`grammar.md`) is built once engine-wide, not per endpoint.
2. **Real tools judge realness.** Fidelity = real `git`, real `gh`, and TF's real poller working unmodified. A contract-test job in triage-factory's CI runs the poller against Orrery.
3. **The people in the world are scripted, never AI.** No LLM inside the engine; chatty NPCs, if ever, are a driver plugin outside the determinism boundary.

**Determinism**
4. **Only three things change the world:** a scenario step, a driver (scenario steps on a timer), or an outside actor's API call. No spontaneous mutation.
5. **Replay is free** — seed snapshot + action log rebuilds the exact world, or something snuck in a hidden input (bug).
6. **The world never touches the internet.**

**Shape**
7. **An instance is one config file plus one seed** (faces mounted, seed snapshot, driver schedule).
8. **State is vendor-neutral; vendors are faces.** One stored idea of "a pull request"; GHES vs GHEC is serving-layer variation. Adding a face never adds a table.
9. **The persistent ring is not special** — same binary, same config, plus longevity and a reset schedule.
10. **Delete the directory and it's gone** — no state outside the world root.

**Judgment**
11. **The engine's verdict is the diff; judgment lives above the engine.** Factual questions resolve from two snapshots + the action log, deterministically; the engine never calls an LLM. Benchmarks authored on top may add anchored binary-question judge rubrics — judges score *how well*, never settle *whether*, and their evidence is the diff/log/stored bodies, not agent transcripts.
12. **A snapshot is always safe to share** — every credential in the world is fake by construction.

**Scope**
13. **Triage Factory is the only customer until TF's integration suite runs green against Orrery.** The RL affordances kept are exactly the free ones (snapshot, fork, diff, clock, log); task authoring, reward design, rollout orchestration are out of scope until a real second customer exists.

## Prior art in the triage-factory repo

- `internal/pollbench/ghfake.go` — a 432-line GHES-shaped fake GitHub (`httptest`) with ETag/304 and rate-limit-header support, plus `dataset.go`/`harness.go`. Serves 3 of the poll path's 8 endpoints. The closest existing starting point.
- `internal/worktree/blobless_auth_test.go` and `internal/ghinjector/sharedorigin_git_test.go` — working `git http-backend` fixtures that already drive the production clone/fetch/push paths (see `surface-git.md`).
- `internal/ghinjector/gh_conformance_test.go` — pinned-`gh` wire conformance suite; the source of the porcelain→mutation table in `surface-github.md`.
