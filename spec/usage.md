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

- **Identity** — mint actors and their tokens (GitHub cannot create users via API); issue another token for an existing actor (`POST /_orrery/actors/{login}/tokens`); set an actor's email addresses, which is verified and which is primary (`PUT /_orrery/actors/{login}/emails` — a verification link and the settings page do this, no API); register OAuth apps (`POST /_orrery/oauth-apps` — GitHub creates them only in its web settings) and issue another client secret for one (`POST /_orrery/oauth-apps/{client_id}/secrets` — its settings page generates one); list a world's actors with their addresses (`POST /_orrery/actors/list`) and its OAuth apps (`POST /_orrery/oauth-apps/list`), which is how a caller holding a world it did not build learns which actors and apps it may issue credentials for; and read one actor (`GET /_orrery/actors/{login}`, the login compared without case) or one OAuth app (`GET /_orrery/oauth-apps/{client_id}`), answered as its item in the list, for a caller that already holds the key. These reads are the control plane's, not a copy of GitHub's `GET /users` or `GET /users/{username}`: actors and apps are created here, the REST face needs a token, and the caller of a re-keyed world holds none until it issues one. No read returns a stored token or secret, as on GitHub, so issuing a new one is the way in to an actor or app someone else made. Every one issued stays valid. Signing in and authorizing an app are the face's (`/login/oauth/*`, `surface-github.md` §2).
- **GitHub Apps** — register one, give it a key, and install it, each of which GitHub does only in its web pages (`surface-github.md` §1.11 has what the App then does as itself).
  - `POST /_orrery/apps` registers an App: `slug`, `name`, `owner` (the login of a user or an organization), `callback_urls` (1 to 10), and optionally `permissions` (each permission's name to `read`, `write`, or `admin`) and `events`. Its bot is made with it, under the slug as its login. A `bot` actor that already holds the slug and has no App becomes the App's bot, and any other holder of the login is `409 ALREADY_EXISTS` on `slug`. The answer carries the App's `id`, its `client_id`, and its first `client_secret`, which no read returns again. An App is an OAuth app too, so it is in the list of OAuth apps, and `POST /_orrery/oauth-apps/{client_id}/secrets` issues it another client secret.
  - `POST /_orrery/apps/{slug}/keys` adds a key the App's JWTs are checked against: `public_key`, one PEM block labelled `PUBLIC KEY` or `RSA PUBLIC KEY`, holding an RSA key of 2048 to 8192 bits. A world holds public keys only, so that a snapshot stays safe to share (rule 12): the caller makes the pair, keeps the private half, and sends the public half. A private key is `400 INVALID_ARGUMENT` on `public_key`, and a key the App already holds is `409 ALREADY_EXISTS`. It answers the key as `{id, public_key}`, with `public_key` one PEM block labelled `RSA PUBLIC KEY`, which is how every read of an App answers its keys and a form this route takes.
  - `POST /_orrery/apps/{slug}/installations` installs the App on `account`, a user or an organization. `repositories` names the account's repositories the installation covers, and absent or null covers every repository the account owns, those created later included. An App is installed on an account once: again is `409 ALREADY_EXISTS` on `account`. The installation takes the App's permissions and events as they are then.
  - `GET /_orrery/apps/{slug}` and `POST /_orrery/apps/list` read Apps, each with its keys and never a secret. `GET /_orrery/apps/{slug}/installations/{id}` and `POST /_orrery/apps/{slug}/installations/list` read an App's installations.
- **Time** — read the clock, advance a `manual` world's, and change its mode (see below).
- **Fabricated subsystem state** — Actions workflow runs, jobs, and logs, when a scenario needs a consumer's log-reading code to work. Real GitHub has no API to create a run in an arbitrary state; Actions itself does it. None is served yet.
- **Engine operations** — snapshots and restore, listing snapshots (`POST /_orrery/snapshots/list`), diff, reading the log of events (`POST /_orrery/log/list`, `events.md`), following it as it grows (`GET /_orrery/log/stream`, below), and reading the events' JSON Schema (`GET /_orrery/log/schema`). The log read is oldest first or, with `"sort_key": "seq", "sort_dir": "desc"`, newest first. `after_seq` narrows the read to what followed a position another call reported, such as a diff's `log.shared_seq` or a snapshot's `last_seq`. As for the stream, a position is 0 or the `seq` of an event in the log, and `after_seq` defaults to 0; any other number is `404 NOT_FOUND` on `after_seq`, with the log's newest `seq` in the message, because the events above a position a restore discarded did not follow it. The log is paged as every list is (**Lists**, below). A token resumes from the last event it served. If a restore has discarded that event, the token is `400 INVALID_ARGUMENT` on `page_token`, and the walk starts again from the first page. A `seq` is issued once, so a log that still holds that event holds the same events up to it, and a walk across a restore that kept it goes on. Forking a world is the CLI's `orrery fork`, below, because it acts on a world no process is serving.

**Following the log.** Every change to a world appends to its log, clock advances included (rule 4), so a caller that reacts to a world's changes follows the log rather than polling it. `GET /_orrery/log/stream` answers `text/event-stream`: a plain `GET` that `curl -N` reads, and that a browser's `EventSource` reconnects and resumes without help. The GitHub face sends no webhook deliveries yet (**What a world sends**, below), so a caller that wants them runs a relay that follows this stream. A caller that reads lines rather than server-sent events runs `orrery log follow` (below), which prints one entry per line.

- **Where it starts.** The position is the query parameter `after_seq`, or the `Last-Event-ID` header, which an SSE client sends when it reconnects. When the header is present it wins, because an `EventSource` reconnects to the URL it opened, whose `after_seq` is stale by then. With neither, the answer is `400 INVALID_ARGUMENT` on `after_seq`. The query is read strictly: an unknown parameter, a repeated `after_seq`, or a value that isn't a decimal integer is `400 INVALID_ARGUMENT` naming the parameter, and a negative value is `400 OUT_OF_RANGE`. A malformed header gets the same reasons, with no `field` and a message naming the header. A position is 0 or the `seq` of an event in the log; any other number is `404 NOT_FOUND`, on `after_seq` or, for the header, with no `field`, and its message gives the log's newest `seq`. That is how a caller learns its position was never issued, or belongs to history a restore discarded.
- **What it sends.** Every event whose `seq` is greater than the position, exactly once each, in ascending `seq` order: the events already in the log, then each one as it is appended, with no gap and no repeat between the two. Each is one server-sent event, with `id: <seq>`, `event: entry`, and as `data:` the object `POST /_orrery/log/list` returns for it. A write's events are appended together, so a stream sends them together. While nothing else is sent, a `: keepalive` comment goes out every 15 seconds, so proxies keep an idle stream open. Events are read from the world at the caller's pace: a slow caller falls behind and catches up without losing any. Between reads a stream holds neither the world write gate nor the database, so it never holds up a write, a snapshot, or a restore.
- **How a restore ends it.** When a snapshot restore completes, every open stream sends `event: restored` with `data: {"newest_seq": <n>}`, the restored log's newest `seq`, and no `id`, then ends. The caller re-reads what it shows and opens a new stream from a position in the restored log. A stream opened from a position the restore discarded is the `404` above.
- **How stopping the world ends it.** When `orrery serve` is stopped (**Stopping a world**, below), every open stream ends after the events it has already read, with no closing event. A caller that reconnects from the last `id` it received, as an `EventSource` does, resumes once the world is served again.

**Lists.** The control plane's six lists, of actors, OAuth apps, GitHub Apps, an App's installations, snapshots, and the log, are read the same way: a `POST` whose body fields are all optional. Each sorts by one key, ascending: an actor's `login` (compared without case), an OAuth app's `client_id`, a GitHub App's `slug` (compared without case), an installation's `id`, a snapshot's `name`, and an event's `seq`. The log alone also reads newest first, with `"sort_key": "seq", "sort_dir": "desc"`; the other lists refuse `"desc"`, and a `sort_dir` without a `sort_key` is refused. It is answered `{items, next_page_token, total_count}`, where `total_count` is how many items the list holds under the body's filters. The next page is read by sending the same body with `next_page_token` as `page_token`. A token is opaque and holds the position of the last item served, so a walk neither repeats nor skips an item while the list grows. It is refused, `400 INVALID_ARGUMENT` on `page_token`, by any other list and for any other filter or order. `next_page_token` is absent on the last page. A page holds 100 items unless `page_size` names another number from 1 to 500, and `page_size: 0` answers only `total_count`.

**Calls the control plane doesn't serve.** It is Orrery's own API, not mocked surface, so it answers in its error shape (`{"errors": [{"reason", "message", "field"}]}`), never with a face's `501`. A path under `/_orrery` that names no route is `404 NOT_FOUND`. A route called with a method it doesn't take is `405 METHOD_NOT_ALLOWED`, with an `Allow` header listing the methods it does. Issuing a token or a client secret, restoring a snapshot, and deleting one take no body fields: each accepts an empty body or `{}`, and refuses any field.

**One door per operation.** If GitHub has an API for something, the control plane must never duplicate it. CLI conveniences are permitted only as literal aliases that issue the face call (and say so). This killed the early `orrery repo create` idea: repo creation has a real API (`POST /orgs/{org}/repos`, `POST /user/repos`), so that endpoint is the door — and since real GitHub has no push-to-create, it becomes the REST face's first endpoint rather than a control-plane verb that would later need deleting.

**Where the doors are.** A caller that starts a world learns its addresses from the world. Once the world is open, its repositories' refs agree with its log (`surface-git.md`, "Starting a world"), and every listener is bound, and before it serves a request, `orrery serve` prints one line of JSON to stdout:

```json
{"base_url":"http://127.0.0.1:43211","listen":"127.0.0.1:43211","control":"127.0.0.1:43211"}
```

`listen` and `control` are the bound addresses, `host:port` with the real port or `unix:/absolute/path`. A listener can also be a listening Unix socket a supervisor made and passed to `orrery serve` as an open descriptor, under the protocol of systemd's socket activation (`fd:<name>` in the config); it is reported by its path, read from the socket, as `unix:/absolute/path`. `control` is `listen` unless `server.control_listen` is set; either way the control plane is under `/_orrery` on it. `base_url` is the configured one, or the one derived from `listen`. So `listen = "127.0.0.1:0"` gets a free port and reports it, and a caller starting many worlds never picks ports in advance. Nothing else is written to stdout; logs go to stderr.

**Stopping a world.** SIGTERM or SIGINT stops `orrery serve`, sent to its process alone or to its process group, as Ctrl-C in a terminal sends it. It stops accepting connections, ends every log stream, and gives the requests it is answering, and the git processes it started, 5 seconds to finish. A git request still running then is killed, and a push it cut off moves back what it moved (`surface-git.md`, "Starting a world"). Then the process exits with status 0, dropping any connection still open. A push in flight when the signal arrives is therefore recorded, or moved back.

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

`orrery log follow --after-seq <n>` is `GET /_orrery/log/stream?after_seq=<n>` (**Following the log**, above) for a caller that reads lines rather than server-sent events: `orrery log follow --after-seq 0 | jq …`, or a program that starts the command and reads its stdout. For each `entry` event it prints the event's `data`, the object `POST /_orrery/log/list` returns for that entry, as one line of compact JSON, and flushes it, so a reader at the other end of a pipe sees each entry as it arrives. Nothing else goes to stdout: keepalives are read and not printed. `--after-seq` is required, because the route requires a position. The command never reconnects. It runs until the stream ends, and every way the stream ends tells the caller where to resume, so a wrapper can run it again with `--after-seq`. Its exit status says which way it ended:

- **0**, never. Interrupting it (Ctrl-C) ends it the way the signal ends any process.
- **1**, when the world answers an error, such as the `404` for a position that names no entry, and stderr carries the world's answer and the URL; when the answer is not `text/event-stream`, so the URL is not a world's control listener; or when the connection can't be made, drops, ends without a `restored` event (as stopping the world ends it), or sends nothing at all for 45 seconds, and stderr names the URL and the last `seq` printed, or the starting position if none was. The stream sends a keepalive every 15 seconds, so 45 seconds of silence means the connection is gone. No other timeout applies, since an idle world can send nothing but keepalives for hours.
- **2**, when `--after-seq` is missing or is not a non-negative integer: the usage error every command exits with for a bad argument, its message on stderr.
- **3**, when a snapshot restore ends the stream, and stderr names the restored log's newest `seq`.

## What the diff reports

`orrery diff` (`POST /_orrery/diff`) answers what happened between two world states, read from the states themselves (rule 11). Every section is present in every diff, empty when nothing happened. Entries name things by stable identity — a login, `owner/repo` with a pull number, `org/slug` — never by row id.

| Area | Sections |
| --- | --- |
| Actors and identity | `actors_added` (with the addresses each holds), `emails_changed` (an existing actor's whole address set before and after, each address with `primary` and `verified`), `oauth_apps_added` (name and client id), `authorizations_issued`, `authorizations_exchanged` (per app and actor, with the scope), `apps_added` (slug, client id, and owner; an App's bot and its OAuth app are in `actors_added` and `oauth_apps_added` too), `installations_added` (the App, the account, and the repositories it lists) |
| Repositories | `repos_created`, `repos_deleted`, `default_branch_changes`, `ref_changes` |
| Pulls | `pulls_opened`, `pulls_closed`, `pulls_reopened`, `pulls_merged`, `pulls_synchronized`, `pulls_edited` (title, body, base), `pulls_ready_for_review` (with who marked it), `pulls_converted_to_draft`, `labels_added`, `labels_removed` |
| Review | `review_requests_added`, `review_requests_removed` (each naming `{"user": login}` or `{"team": "org/slug"}`), `reviews_added`, `comments_added` |
| Checks | `check_runs_added`, `check_runs_updated` |
| Organizations | `org_memberships_added`, `org_memberships_removed`, `org_roles_changed`, `teams_created`, `teams_deleted`, `team_memberships_added`, `team_memberships_removed`, `team_roles_changed` |
| Log | `log` — the newest `seq` both sides' logs hold (`shared_seq`), and how many of `to`'s events follow it (`appended`), counted, since `seq` has gaps. Each side's own newest is its `last_seq`, beside its name and `clock_unix` in `from` and `to` |

A few rules follow from reading states rather than events:

- Something that began and finished within the interval reports both facts: a pull opened and merged, an authorization issued and exchanged.
- A review request is removed however it ended: withdrawn, cleared by the reviewer's review, or deleted with its team. A deleted team's memberships are removed with it.
- A team deleted and a new one created under its slug is a deletion and a creation, not an unchanged team.
- `pulls_ready_for_review` comes from the pull's timeline, so it shows a pull marked ready and converted back within the interval. The world keeps no timeline item for a conversion, so `pulls_converted_to_draft` compares the two states, and that round trip does not appear there.
- Credentials never appear: no client secret, authorization code or token.
- The two sides' logs are the same up to `shared_seq` and differ after it, since a `seq` is issued once per world. When `to` carries on from `from`, `shared_seq` is `from`'s `last_seq`. When `from` carries on from `to`, as in a diff back to an earlier snapshot, it is `to`'s, and `appended` is 0. When a restore between the two sides discarded `from`'s newest events, it is older than both, and `appended` counts only the events of `to`'s that followed it. On a world whose log is `to`'s, as the live world's is when `to` is absent, `POST /_orrery/log/list` with `after_seq: shared_seq` reads exactly the `appended` events.

## Time: the clock is an input, not a history mechanism

The clock is what time the world holds it is, consulted whenever the engine stamps something (`created_at`, `merged_at`) or computes staleness. It is stored in `world.db`, part of the state like everything else, with its mode, which says how it moves:

- **`manual`**, the default: the clock moves only when a caller advances it. Two runs of one script against a manual world make the same log, which is what a scripted test needs.
- **`real`**: the clock is the host's time, and nobody advances it. This is for a world that stands in for GitHub in front of a running application, which compares the times the world stamps with its own clock, and sends the world times it made from its own clock. Two runs of one script against a real world make different logs, and choosing `real` accepts that.

A caller's own clock is the host's in a real world, and anything in a manual one. So a manual world compares no time a caller made from its own clock to the world's: it checks the signature on a GitHub App's JWT and who issued it, and not whether it has expired or how long it lasts, since GitHub holds both only to its own clock (`surface-github.md` §1.11). A time the world made itself, such as an installation token's expiry, is held to the world's clock in both modes, so in a manual world a token expires when the clock is advanced past it.

The operations:

- **Create** — `orrery create --clock manual|real`, or `clock` in the instance config beside `t0`, `manual` when absent. A manual world's clock starts at `t0` (`orrery create --t0`, or `t0` in the config). A real world's starts at the host's time, so a `t0` with `real` is refused. Like `t0`, the config's `clock` is read only when `serve` creates the world.
- **Read** — `GET /_orrery/clock` answers `clock_unix`, the same instant as `rfc3339`, and `mode`.
- **Advance** — a manual world's, forward only, any time (`POST /_orrery/clock/advance` with `seconds`, answered as a read). Writes one value, mutates nothing else (rule 4: nothing in the world decays or fires spontaneously with time). The clock stops at 9999-12-30T22:00:00Z, the latest time the faces can render; an advance past it is `400 OUT_OF_RANGE` on `seconds` and moves nothing. A real world's clock is never advanced by hand: the call is `409 CLOCK_IS_REAL` and moves nothing.
- **Change the mode** — `PATCH /_orrery/clock` with `{"mode": "manual"}` or `{"mode": "real"}`, answered as a read. An absent `mode` keeps the mode, and `null` is `400 INVALID_ARGUMENT`. A change to `real` first advances the clock to the host's time. Since the clock never goes back, a world whose clock is ahead of the host's time cannot become real: the change is `409 CLOCK_AHEAD` until the host's time passes the clock. A change to `manual` first advances the clock to the present, and the clock stays there. Each change is a logged write whose event carries the new mode, after the advance, which is a write of its own (`events.md`).

**How a real world keeps time.** Before a write, the world reads the host's time once. If it is ahead of the stored clock, the world first records an advance to it, a `world:clock_advanced` event as a manual advance makes, as a write of its own; the write then stamps from the stored clock, as in a manual world. A write that dates something with a time a read answered just before it, as a merge dates its merge commit and a check run its default `started_at` and `completed_at`, advances to that time instead, since the host's time may have passed it by the write. So every time a write used is in the log, and a seed snapshot plus the log still rebuilds the world (rule 5). A write that fails, or that appends nothing, keeps no advance. An idle world writes nothing: there is no timer and no event per second.

A read of the clock, such as `GET /_orrery/clock` or `GET /_orrery/healthz`, or the check of whether an installation token has expired, answers the later of the host's time and the stored clock, and writes nothing. That is a time no event records, and it is outside what replay rebuilds, because replay re-executes writes, not reads.

The stored clock never goes back. When the host's time is behind it, because the host's clock was stepped back or the world came from a machine whose clock was ahead, reads answer it and writes stamp it until the host's time passes it. A read records nothing, so a host clock stepped back after a read can make a later read answer an earlier time than that one did, though never one earlier than the stored clock.

**Snapshots, restores, and forks** carry the mode with the clock. Taking a snapshot of a real world first records an advance to the host's time, so the snapshot's `clock_unix` is the time it was taken. A restored or forked world holds the snapshot's older clock, so its first read answers the present and its first write records the advance to it: a pull request untouched for 30 days in the snapshot is 30 days old the moment the snapshot is restored. Re-keying a fork is its first write, so `orrery fork` of a real world records the advance, through the CLI, and reports the present as `clock_unix`.

Authoring history is replaying it under the crank: set clock to T0, make the calls (they stamp T0), advance, repeat. Backdated worlds need no backdoor.

**There is no clock rollback in a live world.** Rewinding time over existing state is meaningless. What feels like rollback is a world operation:

- **Coarse:** restore or fork from a named snapshot (the clock value travels inside it).
- **Fine:** replay — rule 5 guarantees seed + log rebuilds the world at any prefix. The log is the fine-grained history; snapshots are memoized prefixes for speed. "State as of event N" = fork from the nearest earlier snapshot, re-execute each write's operation through its door up to N; the engine makes the rest of each write's events again. A replay cannot re-execute the advances a real world logged, since a real world refuses `POST /_orrery/clock/advance`, and it must not let the replaying host's time in: it gives the world, as the host's time, the clock each logged write carries, so the world records the same advances again itself. Credentials replay too: each is derived under the world's secret, which is never in the log, and a world is re-keyed only at its creation (one `world:rekeyed` event, when it is made from another world's state). So every event after that was made under the secret the world holds now, and a replay reads it from the live world and re-executes under it rather than re-keying. One write needs what no world holds: an installation token is issued to a JWT its App signed, and a world holds the App's public keys only, so a replay re-executes that write with the private half of one of them, from the caller that keeps it. The token is derived like every other, so the replay issues the same one.

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
- A sequence number is issued once per world. Restoring a snapshot replaces the log with the snapshot's, and the next event's `seq` is greater than every `seq` the world issued before the restore, including the events it discarded. So a `seq` a caller holds never comes to name a different event, and the log has a gap wherever a restore discarded events: nothing may assume `seq` is contiguous.
- Export for sharing = `tar` of a snapshot dir — safe by rule 12 (no real credentials exist to leak).
- Residence: in CI, the job's temp dir, discarded with it; for seeds, a cache keyed by the hash of the seed definition; for a long-lived world, a short rotation on its host's disk.

## Making worlds without serving them

Two commands make an instance's world and exit, for anything that supervises many worlds or builds them ahead of time. Neither is a control-plane alias: no world is running for them to call.

- `orrery create --instance <dir> [--t0 <rfc3339>] [--clock manual|real] [--secret local|random]` creates the world at `<dir>/world`, exactly as `serve` does on first run with the same `t0`, `clock`, and `secret`, and writes no config file. `t0` defaults to 2020-01-01T00:00:00Z, `clock` to `manual`, and `secret` to `local`. A `t0` with `--clock real` is refused, since a real world's clock starts at the host's time (**Time**, above). An instance that already holds a world is refused.
- `orrery fork (--instance <dir> --snapshot <name> | --world <dir>) --dest <dir> [--secret local|random]` copies a world into a new instance and re-keys it under `secret` (default `local`), so the fork accepts none of its source's credentials, except that two local worlds derive the same ones. An App's keys are not credentials a world derives: each is the public half of a pair its caller holds, so a fork holds the keys its source held, and a JWT that authenticates an App to the source authenticates it to the fork. The source is `<instance>/snapshots/<name>`, or any world directory, such as a stopped instance's `world/` or a built template. It is only read, so any number of forks may copy it at once. A world that `serve` is running can change mid-copy, so it is never a `--world` source: snapshot it and fork the snapshot.

With `--json`, each prints exactly one object on stdout. Success is `{"instance": "<dest>", "world": "<dest>/world", "clock_unix": <n>}`, with absolute paths. Failure is the control plane's error shape, `{"errors": [{"reason", "message", "field"}]}`, and exit status 1. `field` names the flag the failure is about (`t0`, `snapshot`, `world`, `instance`, or `dest`), and is absent for `INTERNAL`.

| `reason` | When |
| --- | --- |
| `INVALID_ARGUMENT` | a malformed snapshot name or `t0`, or a `t0` with `--clock real` |
| `NOT_FOUND` | no such snapshot or world directory |
| `ALREADY_EXISTS` | the destination already holds a world |
| `INTERNAL` | anything else |

A malformed command line (an unknown flag, a missing source, or a `--secret` that is neither choice) is the argument parser's usage error, with or without `--json`: a message on stderr and exit status 2.

**The build commit.** `orrery --version` prints `orrery <version> (<commit>)`, and `GET /_orrery/healthz` reports the same commit as `build`. It is the full SHA of the commit the binary was built from: `ORRERY_BUILD_COMMIT` at build time if set, else `git rev-parse HEAD`, else `unknown`. There is no dirty-tree marker.

**The world schema.** `orrery --version` prints a second line, `world schema <N>`, and `GET /_orrery/healthz` reports the same number as `world_schema`. It is the version this binary writes of everything a world stores: its tables, and the events it logs. A binary migrates a world to its own version when it opens it, and refuses a world at a higher version. So before moving a world from one binary to another, a caller compares the two numbers: a world the first has opened won't open in a binary whose number is lower.

**The config digest.** `serve` reads its config file once, at start, so a process started before the file changed keeps the settings it read, such as the `base_url` in every clone and HTML URL, until it is restarted. `GET /_orrery/healthz` reports which file it read as `config_sha256`: the SHA-256 of the file's bytes exactly as `serve` read them, as 64 lowercase hex characters. It is computed once and never again, so editing the file while the world runs does not change it. A caller that holds the file compares the digest of its bytes with `config_sha256`, and a difference means the running process has not read that file. The digest is of the raw bytes, not of the parsed settings, so any edit changes it, a comment or whitespace included, and a caller needs no TOML parser to compare. healthz reports the digest and none of the settings, so a key added to the config later is covered and none of the file's contents reaches the control plane. A world served by embedding `orrery-server`, with no config file, reports `"config_sha256": null`.

## What a world sends

A world sends one thing: a webhook delivery, a POST from the world to a receiver's URL (rule 6). The GitHub face sends none yet (`surface-github.md` §5). Each delivery is one attempt:

- **One request, one answer.** The attempt POSTs the delivery's headers and body, adding `Host` and `Content-Length` from the URL and the body, and no other header. It is never retried and never follows a redirect, because GitHub does neither for a delivery: a `3xx` is the answer. The attempt keeps the status, the response headers, the first 64 KiB of the body as the receiver sent it, whether the body went on past that, and how long the attempt took. A response head longer than 64 KiB fails it.
- **10 seconds for the whole attempt,** as GitHub gives a receiver: from the lookup and the connection, through TLS and the request, to the last byte of the answer it keeps. An answer still arriving then is a timeout, however recently its last byte came.
- **A failure says which step failed:** the URL is not an http or https URL with a host; no connection was made, because it was refused, the address was unreachable, or the name did not resolve; the egress proxy opened no tunnel; the TLS handshake failed, as it does for a certificate no trusted authority issued; the time ran out; or the exchange failed after the connection was made.
- **TLS** verifies the receiver's certificate against Mozilla's root certificates, which are built into the binary, and against the authorities in `outbound.extra_ca_certs`.

How the connection leaves the machine is `outbound.egress` in the instance config:

- `direct`, the default: the engine looks up the receiver's host name and connects to it.
- The address of an HTTP `CONNECT` proxy: an IP address and port, as in `127.0.0.1:3128`, or `unix:/absolute/path` for a Unix socket. The engine connects to the proxy and sends `CONNECT <host>:<port>`, naming the receiver as its URL does. It looks up no name itself, so the proxy does the lookup and can refuse an address, and for the same reason the proxy's own address is never a host name. A `2xx` answer opens the tunnel and any other answer refuses it. TLS and the request then run through the tunnel, end to end with the receiver. A supervisor that runs a world with no network of its own gives it egress this way.

The egress is the config's alone: proxy variables in `serve`'s environment, such as `HTTPS_PROXY` and `NO_PROXY`, change nothing.

`outbound.extra_ca_certs` names a PEM file of certificate authorities to trust as well, such as the one that issued a local receiver's self-signed certificate. `serve` reads both settings when it starts. An egress it can't parse, or a certificate file it can't read or that holds no certificate, stops it before it opens or creates the world. A proxy is reached only when there is something to send, so it need not be listening when the world starts.

## Determinism addenda (discovered via this model)

- **Git dates come from the virtual clock.** Real `git commit` stamps author/committer dates from the *client's* wall clock — a seed script run on a laptop embeds real time into commit objects, and replaying it later produces different SHAs. Anything that creates commits must export `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE` from the world clock: a scenario driver does this for every step it runs, and a shell script uses `orrery env`. This is the "hidden input" class rule 5 exists to catch.

  `orrery env` reads `GET /_orrery/clock` and prints its `clock_unix` in git's internal date format, which every git version reads as that exact instant:

  ```
  export GIT_AUTHOR_DATE='@<clock_unix> +0000'
  export GIT_COMMITTER_DATE='@<clock_unix> +0000'
  ```

  A script runs `eval "$(orrery env)"` before each commit, and again after advancing the clock. In a `real` world, `orrery env` reads the present, so the commits it dates carry the real time.

- **A signature is part of its commit.** A signed commit's id covers its signature, so a replay makes the same commit only if the signature comes out the same. `ssh-keygen` signs the same payload with the same Ed25519 or RSA key the same way every time, and with an ECDSA or security key differently each time, so a scenario that signs commits uses an Ed25519 or RSA key. The verdict on a signature depends on the commit and the world alone (`surface-github.md` §1.10).

- **An event records the real time it was appended at.** Beside `clock_unix`, the world's time, each event carries `wall_unix_ms`: the real time, in Unix milliseconds, at which its write was appended, so a caller showing a world's activity can say when something happened. Nothing in the world reads it, so it is not an input to replay (rule 5): a replay records the real time it re-executes each write at, and the diff does not compare it. Snapshots, restores, and forks copy the log whole, so an event a world inherits keeps the time it was first appended at.
