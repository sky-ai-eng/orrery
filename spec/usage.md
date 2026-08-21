# Usage model — the two doors

How a world gets created, populated, and driven. Settled in design discussion 2026-08-22; this is the consumer's mental model and the constraint that shapes the V1 endpoint cut.

## The principle

**Authoring the world is being another user of the world.** Everything a real person or bot could do to real GitHub is done through the vendor face, as a synthetic actor holding a fake token. Everything nobody can do to real GitHub — because reality does it — goes through a small, explicitly-not-GitHub control plane. Two doors, and every operation has exactly one.

## Door one: the face

Repos get content via real `git push`. PRs are authored via `POST /repos/{o}/{r}/pulls` as actor `alice`. Reviews via `POST .../reviews` as actor `dana`. Comments, labels, merges — same. CI fits this door too: real CI systems are GitHub Apps reporting through `POST /repos/{o}/{r}/check-runs`, so a synthetic `ci-bot` actor posting a failing check run through the face is how the real world does it, not a workaround.

What this buys:

- **One vocabulary.** No config schema to learn; any bash script with `git` and `curl` is a world-authoring tool.
- **Fidelity dogfooding.** Our own seed scripts exercise the face's write path constantly, long before any system under test does — the seed suite is a standing conformance test.
- **Third-party readiness.** Future consumers already know this API.
- **Transitions natively.** A declarative fixture describes a state; sequenced API calls *are* transitions, which is what the systems Orrery serves actually consume.

Clarification worth pinning: Triage Factory's credential injector refuses repo creation and review submission *for its agents* — that is TF's policy, enforced in TF's proxy. Orrery mocks GitHub, and real GitHub serves those operations; authoring scripts talk to Orrery directly, with no injector in between.

## Door two: the control plane

Namespaced away from the face (its own path prefix/port) so it can never be mistaken for mocked surface. It holds only what no public GitHub API can express:

- **Identity** — mint actors and their tokens (GitHub cannot create users via API).
- **Time** — set and advance the virtual clock (see below).
- **Fabricated subsystem state** — Actions workflow runs / jobs / logs, when a scenario needs the agent's log-reading tools to work. Real GitHub has no API to create a run in an arbitrary state; Actions itself does it.
- **Engine operations** — snapshot, fork, diff, coverage report.

**One door per operation.** If GitHub has an API for something, the control plane must never duplicate it. CLI conveniences are permitted only as literal aliases that issue the face call (and say so). This killed the early `orrery repo create` idea: repo creation has a real API (`POST /orgs/{org}/repos`, `POST /user/repos`), so that endpoint is the door — and since real GitHub has no push-to-create, it becomes the REST face's first endpoint rather than a control-plane verb that would later need deleting.

## Scenarios and scripts

The scenario driver is a client of the same two doors, not a third mutation path. A scenario file is a declared sequence of calls with the determinism bookkeeping handled (clock discipline, actor management, replay). Consumers who want rigor use scenarios; consumers who want zero learning curve write bash against the doors directly. Both are first-class.

## Walkthrough

```bash
orrery serve --config ci.toml &            # empty world, clock at T0
orrery actor add alice ci-bot              # control plane
curl -u x-access-token:$ALICE -X POST $ORRERY/api/v3/orgs/acme/repos -d '{"name":"webapp"}'
eval "$(orrery env)"                       # exports GIT_AUTHOR_DATE/GIT_COMMITTER_DATE from the world clock
git push https://x-access-token:$ALICE@$HOST/acme/webapp.git main
curl -u x-access-token:$ALICE -X POST .../repos/acme/webapp/pulls -d '{...}'
curl -u x-access-token:$CIBOT -X POST .../check-runs -d '{"conclusion":"failure"}'
orrery clock advance 10m
orrery snapshot before
# ... boot the system under test against it; its poller sees the failing PR,
#     its agent pushes a fix through the same face ...
orrery snapshot after
orrery diff before after                   # → branch pushed by agent, check re-run, PR updated
```

## Time: the clock is an input, not a history mechanism

The virtual clock is one value — what time the world believes it is — consulted whenever the engine stamps something (`created_at`, `merged_at`) or computes staleness. It is a row in `world.db`, part of the state like everything else. Two operations:

- **Set** — only on an empty world (establishing T0).
- **Advance** — forward only, any time. Writes one value, mutates nothing else (rule 4: nothing in the world decays or fires spontaneously with time).

Authoring history is replaying it under the crank: set clock to T0, make the calls (they stamp T0), advance, repeat. Backdated worlds need no backdoor.

**There is no clock rollback in a live world.** Rewinding time over existing state is meaningless. What feels like rollback is a world operation:

- **Coarse:** restore or fork from a named snapshot (the clock value travels inside it).
- **Fine:** replay — rule 5 guarantees seed + action log rebuilds the world at any prefix. The log is the fine-grained history; snapshots are memoized prefixes for speed. "State as of event N" = fork from the nearest earlier snapshot, re-execute the log to N.

Snapshots are therefore taken at explicit moments only — named test points (`before`/`after`), seed caches, the ring's reset point — never per event.

## Snapshots: storage and mechanics

```
<instance>/
├── world/            # the live root: world.db + repos/ — delete = gone (rule 10)
└── snapshots/
    └── <name>/       # full copy of world/ at that moment, clock included
```

- Copy via `cp --reflink=auto`: copy-on-write (milliseconds, near-zero bytes) on XFS/Btrfs; degrades to a real copy on ext4, which at tens of MB is still nothing.
- Every bare repo gets `gc.auto=0` so git object files are effectively append-only — never rewritten out from under a shared copy; safe to snapshot cheaply everywhere.
- Snapshotting quiesces writes: hold the world write-lock, checkpoint SQLite's WAL, copy. Milliseconds, explicit, never racing live traffic.
- Export for sharing = `tar` of a snapshot dir — safe by rule 12 (no real credentials exist to leak).
- Residence: CI → the job's temp dir, discarded with it. Seeds → cached keyed by the hash of the seed definition. Ring → short rotation on the ring host's disk.

## Determinism addenda (discovered via this model)

- **Git dates come from the virtual clock.** Real `git commit` stamps author/committer dates from the *client's* wall clock — a seed script run on a laptop embeds real time into commit objects, and replaying it later produces different SHAs. Anything that creates commits must export `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` from the world clock: the scenario driver does this automatically; bash authors use `orrery env`. This is the "hidden input" class rule 5 exists to catch.

## The V1 cut, revised

"Start with the git surface" plus this usage model means V1 is the git face **plus the thin REST write path that makes worlds authorable** — which arrives before the read surface, because the reads are only testable against worlds that contain state:

1. Git smart-HTTP (clone/fetch/push per `surface-git.md`'s recipe), auth → actor, push observation → action log.
2. `POST /orgs/{org}/repos` + `POST /user/repos` — repo creation (the face's first REST endpoints; no push-to-create, matching real GitHub).
3. The authoring writes: `POST /repos/{o}/{r}/pulls` plus its lifecycle (`PATCH .../pulls/{n}` for edit/close/reopen, `PUT .../pulls/{n}/merge` — a real merge, `git merge-tree` + `commit-tree`, committed at the world clock), `POST .../reviews`, `POST .../issues/{n}/comments`, and `POST`/`PATCH` `.../check-runs`. All but check-runs are already on TF's write inventory; check-runs and create-repo are the only genuinely new surface this model adds.
4. Control plane: actors/tokens, clock set/advance, snapshot/fork/diff, coverage report, `orrery env`.

GraphQL stays out of V1 entirely — authoring uses REST even where `gh` porcelain would use GraphQL (`gh repo create` is GraphQL `createRepository`; script authors use `curl` until the GraphQL face exists). The poller-read endpoints (`surface-github.md` §1) follow once authored worlds exist to serve them, and the TF contract test lands with them.
