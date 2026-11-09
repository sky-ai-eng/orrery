# Usage model — the two doors

How a world gets created, populated, and driven: the model a caller works with, and the constraint it puts on which endpoints exist.

## The principle

**Authoring the world is being another user of the world.** Everything a real person or bot could do to real GitHub is done through the vendor face, as a synthetic actor holding a fake token. Everything nobody can do to real GitHub — because reality does it — goes through a small, explicitly-not-GitHub control plane. Two doors, and every operation has exactly one.

## Door one: the face

Repos get content via real `git push`. PRs are authored via `POST /repos/{o}/{r}/pulls` as actor `alice`. Reviews via `POST .../reviews` as actor `dana`. Comments, labels, merges — same. CI fits this door too: real CI systems are GitHub Apps reporting through `POST /repos/{o}/{r}/check-runs`, so a synthetic `ci-bot` actor posting a failing check run through the face is how the real world does it, not a workaround.

What this buys:

- **One vocabulary.** No config schema to learn; any bash script with `git` and `curl` is a world-authoring tool.
- **Fidelity dogfooding.** Our own seed scripts exercise the face's write path constantly, long before any system under test does — the seed suite is a standing conformance test.
- **Third-party readiness.** Future consumers already know this API.
- **Transitions natively.** A declarative fixture describes a state; sequenced API calls *are* transitions, which is what the systems Orrery serves actually consume.

## Door two: the control plane

Namespaced away from the face (its own path prefix/port) so it can never be mistaken for mocked surface. It holds only what no public GitHub API can express:

- **Identity** — mint actors and their tokens (GitHub cannot create users via API); issue another token for an existing actor (`POST /_orrery/actors/{login}/tokens`); set an actor's email addresses, which is verified and which is primary (`PUT /_orrery/actors/{login}/emails` — a verification link and the settings page do this, no API); register OAuth apps (`POST /_orrery/oauth-apps` — GitHub creates them only in its web settings) and issue another client secret for one (`POST /_orrery/oauth-apps/{client_id}/secrets` — its settings page generates one); list a world's actors with their addresses (`POST /_orrery/actors/list`, paged by `after_login`) and its OAuth apps (`POST /_orrery/oauth-apps/list`, paged by `after_client_id`), which is how a caller holding a world it did not build learns which actors and apps it may issue credentials for; and read one actor (`GET /_orrery/actors/{login}`, the login compared without case) or one OAuth app (`GET /_orrery/oauth-apps/{client_id}`), answered as its item in the list, for a caller that already holds the key. These reads are the control plane's, not a copy of GitHub's `GET /users` or `GET /users/{username}`: actors and apps are created here, the REST face needs a token, and the caller of a re-keyed world holds none until it issues one. No read returns a stored token or secret, as on GitHub, so issuing a new one is the way in to an actor or app someone else made. Every one issued stays valid. Signing in and authorizing an app are the face's (`/login/oauth/*`, `surface-github.md` §2).
- **Time** — set and advance the virtual clock (see below).
- **Fabricated subsystem state** — Actions workflow runs, jobs, and logs, when a scenario needs a consumer's log-reading code to work. Real GitHub has no API to create a run in an arbitrary state; Actions itself does it. None is served yet.
- **Engine operations** — snapshots and restore, diff, and reading the action log, either oldest first (`POST /_orrery/log/list`, paged by `after_seq`) or newest first (`POST /_orrery/log/recent`, paged by `before_seq`, for a caller that wants the latest entries without reading the whole log). Forking a world is the CLI's `orrery fork`, below, because it acts on a world no process is serving.

**Calls the control plane doesn't serve.** It is Orrery's own API, not mocked surface, so it answers in its error shape (`{"errors": [{"reason", "message", "field"}]}`), never with a face's `501`. A path under `/_orrery` that names no route is `404 NOT_FOUND`. A route called with a method it doesn't take is `405 METHOD_NOT_ALLOWED`, with an `Allow` header listing the methods it does. Issuing a token or a client secret, restoring a snapshot, and deleting one take no body fields: each accepts an empty body or `{}`, and refuses any field.

**One door per operation.** If GitHub has an API for something, the control plane must never duplicate it. CLI conveniences are permitted only as literal aliases that issue the face call (and say so). This killed the early `orrery repo create` idea: repo creation has a real API (`POST /orgs/{org}/repos`, `POST /user/repos`), so that endpoint is the door — and since real GitHub has no push-to-create, it becomes the REST face's first endpoint rather than a control-plane verb that would later need deleting.

**Where the doors are.** A caller that starts a world learns its addresses from the world. Once the world is open and every listener is bound, and before it serves a request, `orrery serve` prints one line of JSON to stdout:

```json
{"base_url":"http://127.0.0.1:43211","listen":"127.0.0.1:43211","control":"127.0.0.1:43211"}
```

`listen` and `control` are the bound addresses, `host:port` with the real port or `unix:/absolute/path`. `control` is `listen` unless `server.control_listen` is set; either way the control plane is under `/_orrery` on it. `base_url` is the configured one, or the one derived from `listen`. So `listen = "127.0.0.1:0"` gets a free port and reports it, and a caller starting many worlds never picks ports in advance. Nothing else is written to stdout; logs go to stderr.

## Scenarios and scripts

The scenario driver is a client of the same two doors, not a third mutation path. A scenario file is a declared sequence of calls with the determinism bookkeeping handled (clock discipline, actor management, replay). Consumers who want rigor use scenarios; consumers who want zero learning curve write bash against the doors directly. Both are first-class.

## Walkthrough

```bash
orrery serve --config ci.toml &                    # empty world, clock at T0
C=http://127.0.0.1:4180/_orrery                    # control plane
ALICE=$(curl -s -X POST $C/actors -d '{"login":"alice","kind":"user"}' | jq -r .token)
CIBOT=$(curl -s -X POST $C/actors -d '{"login":"ci-bot","kind":"bot"}' | jq -r .token)
curl -u x-access-token:$ALICE -X POST $ORRERY/api/v3/user/repos -d '{"name":"webapp"}'
eval "$(orrery env)"                               # exports GIT_AUTHOR_DATE/GIT_COMMITTER_DATE from the world clock
git push http://x-access-token:$ALICE@$HOST/alice/webapp.git main fix
curl -u x-access-token:$ALICE -X POST $ORRERY/api/v3/repos/alice/webapp/pulls -d '{...}'
curl -u x-access-token:$CIBOT -X POST $ORRERY/api/v3/repos/alice/webapp/check-runs -d '{...,"conclusion":"failure"}'
curl -X POST $C/clock/advance -d '{"seconds":600}'
curl -X POST $C/snapshots -d '{"name":"before"}'
# ... start the system under test against the world; it sees the failing check,
#     and pushes a fix through the same face ...
curl -X POST $C/snapshots -d '{"name":"after"}'
curl -X POST $C/diff -d '{"from":"before","to":"after"}'   # → branch pushed, check run added, pull synchronized
```

`orrery env`, and any command that talks to a running world, addresses it by its control listener's origin, without `/_orrery`: `--control-url <url>`, else `$ORRERY_CONTROL_URL`, else `http://127.0.0.1:4180`, `serve`'s default listen. Only `http` and `https` are accepted, because seed scripts reach a world over TCP: `git` cannot push to a Unix socket. A world that can't be reached, or that answers with an error, fails the command with a non-zero exit that names the URL. A command's output is its stdout alone; logs go to stderr.

## What the diff reports

`orrery diff` (`POST /_orrery/diff`) answers what happened between two world states, read from the states themselves (rule 11). Every section is present in every diff, empty when nothing happened. Entries name things by stable identity — a login, `owner/repo` with a pull number, `org/slug` — never by row id.

| Area | Sections |
| --- | --- |
| Actors and identity | `actors_added` (with the addresses each holds), `emails_changed` (an existing actor's whole address set before and after, each address with `primary` and `verified`), `oauth_apps_added` (name and client id), `authorizations_issued`, `authorizations_exchanged` (per app and actor, with the scope) |
| Repositories | `repos_created`, `repos_deleted`, `default_branch_changes`, `ref_changes` |
| Pulls | `pulls_opened`, `pulls_closed`, `pulls_reopened`, `pulls_merged`, `pulls_synchronized`, `pulls_edited` (title, body, base), `pulls_ready_for_review` (with who marked it), `pulls_converted_to_draft`, `labels_added`, `labels_removed` |
| Review | `review_requests_added`, `review_requests_removed` (each naming `{"user": login}` or `{"team": "org/slug"}`), `reviews_added`, `comments_added` |
| Checks | `check_runs_added`, `check_runs_updated` |
| Organizations | `org_memberships_added`, `org_memberships_removed`, `org_roles_changed`, `teams_created`, `teams_deleted`, `team_memberships_added`, `team_memberships_removed`, `team_roles_changed` |
| Log | `log` — the span of action-log sequence numbers between the two sides |

A few rules follow from reading states rather than events:

- Something that began and finished within the interval reports both facts: a pull opened and merged, an authorization issued and exchanged.
- A review request is removed however it ended: withdrawn, cleared by the reviewer's review, or deleted with its team. A deleted team's memberships are removed with it.
- A team deleted and a new one created under its slug is a deletion and a creation, not an unchanged team.
- `pulls_ready_for_review` comes from the pull's timeline, so it shows a pull marked ready and converted back within the interval. The world keeps no timeline item for a conversion, so `pulls_converted_to_draft` compares the two states, and that round trip does not appear there.
- Credentials never appear: no client secret, authorization code or token.

## Time: the clock is an input, not a history mechanism

The virtual clock is one value — what time the world believes it is — consulted whenever the engine stamps something (`created_at`, `merged_at`) or computes staleness. It is a row in `world.db`, part of the state like everything else. Two operations:

- **Set** — once, when the world is created: `t0` in the instance config, or `orrery create --t0`.
- **Advance** — forward only, any time (`POST /_orrery/clock/advance` with `seconds`). Writes one value, mutates nothing else (rule 4: nothing in the world decays or fires spontaneously with time). The clock stops at 9999-12-30T22:00:00Z, the latest time the faces can render; an advance past it is `400 OUT_OF_RANGE` on `seconds` and moves nothing.

Authoring history is replaying it under the crank: set clock to T0, make the calls (they stamp T0), advance, repeat. Backdated worlds need no backdoor.

**There is no clock rollback in a live world.** Rewinding time over existing state is meaningless. What feels like rollback is a world operation:

- **Coarse:** restore or fork from a named snapshot (the clock value travels inside it).
- **Fine:** replay — rule 5 guarantees seed + action log rebuilds the world at any prefix. The log is the fine-grained history; snapshots are memoized prefixes for speed. "State as of event N" = fork from the nearest earlier snapshot, re-execute the log to N. Credentials replay too: each is derived under the world's secret, which is never in the log, and a world is re-keyed only at its creation (one `world.rekey` entry, when it is made from another world's state). So every entry after that was made under the secret the world holds now, and a replay reads it from the live world and re-executes under it rather than re-keying.

Snapshots are therefore taken at explicit moments only — named test points (`before`/`after`), seed caches, a long-lived world's reset point — never per event.

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
- Residence: in CI, the job's temp dir, discarded with it; for seeds, a cache keyed by the hash of the seed definition; for a long-lived world, a short rotation on its host's disk.

## Making worlds without serving them

Two commands make an instance's world and exit, for anything that supervises many worlds or builds them ahead of time. Neither is a control-plane alias: no world is running for them to call.

- `orrery create --instance <dir> [--t0 <rfc3339>] [--secret local|random]` creates the world at `<dir>/world`, exactly as `serve` does on first run with the same `t0` and `secret`, and writes no config file. `t0` defaults to 2020-01-01T00:00:00Z, and `secret` to `local`. An instance that already holds a world is refused.
- `orrery fork (--instance <dir> --snapshot <name> | --world <dir>) --dest <dir> [--secret local|random]` copies a world into a new instance and re-keys it under `secret` (default `local`), so the fork accepts none of its source's credentials, except that two local worlds derive the same ones. The source is `<instance>/snapshots/<name>`, or any world directory, such as a stopped instance's `world/` or a built template. It is only read, so any number of forks may copy it at once. A world that `serve` is running can change mid-copy, so it is never a `--world` source: snapshot it and fork the snapshot.

With `--json`, each prints exactly one object on stdout. Success is `{"instance": "<dest>", "world": "<dest>/world", "clock_unix": <n>}`, with absolute paths. Failure is the control plane's error shape, `{"errors": [{"reason", "message", "field"}]}`, and exit status 1. `field` names the flag the failure is about (`t0`, `snapshot`, `world`, `instance`, or `dest`), and is absent for `INTERNAL`.

| `reason` | When |
| --- | --- |
| `INVALID_ARGUMENT` | a malformed snapshot name or `t0` |
| `NOT_FOUND` | no such snapshot or world directory |
| `ALREADY_EXISTS` | the destination already holds a world |
| `INTERNAL` | anything else |

A malformed command line (an unknown flag, a missing source, or a `--secret` that is neither choice) is the argument parser's usage error, with or without `--json`: a message on stderr and exit status 2.

**The build commit.** `orrery --version` prints `orrery <version> (<commit>)`, and `GET /_orrery/healthz` reports the same commit as `build`. It is the full SHA of the commit the binary was built from: `ORRERY_BUILD_COMMIT` at build time if set, else `git rev-parse HEAD`, else `unknown`. There is no dirty-tree marker.

## Determinism addenda (discovered via this model)

- **Git dates come from the virtual clock.** Real `git commit` stamps author/committer dates from the *client's* wall clock — a seed script run on a laptop embeds real time into commit objects, and replaying it later produces different SHAs. Anything that creates commits must export `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` from the world clock: a scenario driver does this for every step it runs, and a shell script uses `orrery env`. This is the "hidden input" class rule 5 exists to catch.

  `orrery env` reads `GET /_orrery/clock` and prints its `clock_unix` in git's internal date format, which every git version reads as that exact instant:

  ```
  export GIT_AUTHOR_DATE='@<clock_unix> +0000'
  export GIT_COMMITTER_DATE='@<clock_unix> +0000'
  ```

  A script runs `eval "$(orrery env)"` before each commit, and again after advancing the clock.
