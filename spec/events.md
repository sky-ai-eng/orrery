# Events

Every change to a world is an event in its log. An event records which object changed, its values before and after, who made the change, and through which door. `POST /_orrery/log/list` reads the log and `GET /_orrery/log/stream` follows it (`usage.md`). `GET /_orrery/log/schema` serves the events' JSON Schema, which `event.schema.json` beside this file holds and each release archive ships. The engine generates the schema from the types that write the events (`crates/orrery-world/src/events.rs`), so it never describes anything else.

The log is a record of changes, not a vendor's notification stream. A face that sends webhooks one day renders them from these events, the way its vendor sends them.

## One write

A merge through the REST face is one write of four events. Hashes are shortened.

```json
{"seq": 22, "write": {"seq": 22, "events": 4}, "clock_unix": 1767225660,
 "wall_unix_ms": 1794259602054, "actor": {"id": 2, "login": "alice"}, "door": "rest", "app": null,
 "type": "pull:merged",
 "subject": {"type": "pull", "id": 1, "number": 1,
             "repo": {"id": 1, "name": "webapp"}, "owner": {"id": 2, "login": "alice"}},
 "changes": {"state": {"from": "open", "to": "closed"}, "merged": {"from": false, "to": true},
             "merge_commit_sha": {"from": null, "to": "e8bb707…"},
             "merged_by": {"from": null, "to": {"id": 2, "login": "alice"}},
             "closed_unix": {"from": null, "to": 1767225660}}}
{"seq": 23, …, "type": "timeline_item:created",
 "subject": {"type": "timeline_item", "id": 3, "pull": {"id": 1, "number": 1}, "repo": …, "owner": …},
 "object": {"id": 3, "kind": "closed", "actor": {"id": 2, "login": "alice"}, "created_unix": 1767225660,
            "label": null, "ref_name": null, "reviewer": null, "team": null}}
{"seq": 24, …, "type": "repo:pushed",
 "subject": {"type": "repo", "id": 1, "name": "webapp", "owner": …},
 "changes": {"pushed_unix": {"from": 1767225600, "to": 1767225660}}}
{"seq": 25, …, "type": "ref:updated",
 "subject": {"type": "ref", "name": "refs/heads/main", "repo": …, "owner": …},
 "changes": {"sha": {"from": "2406ace…", "to": "e8bb707…"}},
 "forced": false, "commits": ["adb6b6b…", "e8bb707…"], "commit_count": 2}
```

## The rules

1. **Every event has the envelope.** `seq` is the event's position, which only grows and has a gap wherever a restore discarded events. `write` names the write: its first event's `seq` and how many events it appended. `clock_unix` is the world clock, and `wall_unix_ms` the real time the write was appended, which nothing in the world reads. `actor`, `door`, and `app` are rule 6's.
2. **A type is `object:happening`,** lower snake case with a colon and the past tense: `pull:merged`, `review:submitted`. The object part is the subject's type. Outcomes and values are data, never part of a type: an approving review is `review:submitted` with `state: "approved"`. A type has one body shape, so an operation that sometimes creates and sometimes changes, such as setting a membership, has a type for each.
3. **A write's operation comes first.** A write appends its operation's event, then one event for each consequence, in the order the engine made them, in one transaction. A reader holding `write.events` events from `write.seq` holds the whole write. A write that changes nothing appends nothing.
4. **Every event has one subject:** the object it is about, by its stable id, its names at that moment, and the objects that contain it as top-level keys (`owner`, `repo`, `pull`, `team`). A reader can keep one repository's or one account's events without knowing every type, and an event still names its object after the object is renamed. A ref and a commit have no id, so a ref is named by its name and a commit by its sha.
5. **Bodies carry values.** A create carries `object`, every stored field of the new object. A change carries `changes`, each field that changed as `{"from", "to"}`; a field holding a set, such as labels or requested reviewers, is its whole list before and after. A delete carries `object` as it was. Timestamps the engine sets, such as a pull's `updated_unix`, are fields like any other. Every value a write stores is in one of its events, so a reader learns each change from the log without knowing the engine's behavior. No event holds a credential: tokens, client secrets, and codes are derived from the world secret and the numbers the events carry.
6. **Attribution** is who made the change (`actor`), how it reached the world (`door`: `rest`, `graphql`, `git`, `web` for the OAuth web flow, `control`, or `cli`), and the OAuth app the call came through (`app`, or `null`): the one whose token authenticated it, or whose authorize page or client credentials it used. Control-plane and CLI writes are the system actor's. A scenario driver is a client of the faces and the control plane, so it is not a door (`usage.md`).
7. **Shapes only grow.** Once a type ships it keeps its meaning and its fields. New fields and types are added, and none are removed. A reader skips a type it does not know.

## How the rules are checked

- **Completeness.** In a test build, every write reads every table before and after it, and fails if it stored a value under no field of an event about that object (`crates/orrery-world/src/audit.rs`). A column the schema gains fails every write that stores it until the audit lists it. The world also stores how many entries its log holds, so that the whole log's count reads no entries; no event carries that number, because it is the log's own length, and the check is instead that every write leaves it equal to the number of entries the log holds.
- **Shape.** In a test build, every event a write appends is checked against the schema as the log serves it.
- **Determinism.** Replay tests re-execute each write's operation through the door it came through, and require the same events, consequences included, and the same world (rule 5).

## Subjects

| Type | Identity | Containers |
| --- | --- | --- |
| `world` | none: a world has one | none |
| `actor` | `id`, `login`, `kind` | none |
| `oauth_app` | `id`, `client_id`, `name` | none |
| `oauth_authorization` | `id` | `app`, `actor` (who granted it) |
| `repo` | `id`, `name` | `owner` |
| `ref` | `name` | `repo`, `owner` |
| `pull` | `id`, `number` | `repo`, `owner` |
| `timeline_item` | `id` | `pull`, `repo`, `owner` |
| `review` | `id` | `pull`, `repo`, `owner` |
| `comment` | `id` | `pull`, `repo`, `owner` |
| `check_run` | `id`, `name`, `head_sha` | `repo`, `owner` |
| `org_membership` | `member` | `owner` (the organization) |
| `team` | `id`, `slug` | `owner` (the organization) |
| `team_membership` | `member` | `team`, `owner` |
| `ssh_signing_key` | `id` | `owner` (the account that holds it) |
| `commit` | `sha` | `repo`, `owner` |

## Types

| Type | Made by | Body | Consequences in the same write |
| --- | --- | --- | --- |
| `world:created` | creating a world | `object` | `actor:created` for the system actor |
| `world:clock_advanced` | advancing the clock | `changes` | none |
| `world:rekeyed` | forking a world | `rekeyed`: how many of each credential were derived again | none |
| `actor:created` | creating an actor, with its first token and its address | `object` | none |
| `actor:emails_set` | setting an actor's addresses | `changes` | none |
| `actor:token_issued` | issuing a token | `n`, its number | none |
| `oauth_app:created` | registering an app, with its first client secret | `object` | none |
| `oauth_app:secret_issued` | issuing a client secret | `n`, its number | none |
| `oauth_authorization:created` | authorizing an app | `object` | none |
| `oauth_authorization:exchanged` | trading the code for a token | `changes` | none |
| `repo:created` | creating a repository | `object` | none |
| `repo:pushed` | a push, and a consequence of a merge | `changes` | for a push: a ref event per ref it moved, in ref-name order, then what [a push does to pull requests](#what-a-push-records), then a `commit:verified` per commit whose verdict it stores |
| `ref:created` | a consequence of a push | `object`, `commits`, `commit_count` | |
| `ref:updated` | a consequence of a push or a merge | `changes`, `forced`, `commits`, `commit_count` | |
| `ref:deleted` | a consequence of a push | `object`, as it was | |
| `pull:opened` | opening a pull request | `object` | none |
| `pull:edited` | editing its title, body, or base | `changes` | none |
| `pull:closed` | closing it, and a consequence of a push | `changes` | `timeline_item:created` |
| `pull:reopened` | reopening it | `changes` | none |
| `pull:merged` | merging it | `changes` | `timeline_item:created`; `repo:pushed` when `pushed_unix` moves; the base's `ref:updated`; what the base's move does to other pull requests |
| `pull:ready_for_review` | marking it ready | `changes` | `timeline_item:created` |
| `pull:converted_to_draft` | converting it to a draft | `changes` | none |
| `pull:labeled`, `pull:unlabeled` | adding or removing labels | `changes` | `timeline_item:created` per label |
| `pull:review_requested` | requesting reviews | `changes` | `timeline_item:created` per reviewer |
| `pull:review_request_removed` | withdrawing requests; a consequence of a review by a user asked for one, and of a team's deletion | `changes` | `timeline_item:created` per reviewer when withdrawn |
| `pull:head_moved` | a consequence of a push or a merge that moves its head branch | `changes` | |
| `pull:updated` | a consequence of any other write that changes it | `changes` | |
| `timeline_item:created` | a consequence only | `object` | |
| `review:submitted` | submitting a review | `object` | `pull:review_request_removed` when the author was asked for one, or else `pull:updated` when `updated_unix` moves |
| `comment:created` | commenting | `object` | `pull:updated` when `updated_unix` moves |
| `check_run:created` | creating a check run | `object` | none |
| `check_run:updated` | updating one | `changes` | none |
| `org_membership:created`, `org_membership:updated` | setting an organization membership | `object`, or `changes` | none |
| `team:created` | creating a team | `object` | `team_membership:created` when the creator is a member of the organization |
| `team:deleted` | deleting a team | `object`, as it was | `pull:review_request_removed` per pending request, then `team_membership:deleted` per member |
| `team_membership:created`, `team_membership:updated` | setting a team membership | `object`, or `changes` | none |
| `team_membership:deleted` | removing a member, and a consequence of a team's deletion | `object`, as it was | none |
| `ssh_signing_key:created` | registering a signing key | `object` | none |
| `ssh_signing_key:deleted` | deleting one | `object`, as it was | none |
| `commit:verified` | a consequence of a push: a commit it brought in whose signature verifies (`surface-github.md` §1.10) | `object`: the account that signed it, the key, and when | |

### What a push records

A push's ref events come first, then what the moves did to pull requests (`surface-git.md`, "What a push does"):

- `pull:updated` setting `base_moved` on each unmerged pull request whose base branch the push created or moved;
- `pull:head_moved` for each open pull request whose head branch the push moved;
- for a deleted branch, `pull:closed` and its `timeline_item:created` for each open pull request whose head it was, then a `timeline_item:created` recording the deletion on each pull request whose head it was, and for each open pull request whose base it was, a `timeline_item:created` recording the deletion, then `pull:closed` and its `timeline_item:created`;
- for a created branch, a `timeline_item:created` recording its restoration on each closed pull request whose head it is.

A pull request whose `updated_unix` moves without another change gets a `pull:updated` before its item.

Last come the verdicts the push stores: a `commit:verified` for each commit the ref events list, each once, in the order they list them, whose signature is `valid` and that has no stored verdict yet (`surface-github.md` §1.10).

### Ref events

`commits` lists commits oldest first, at most 2048 of them, the newest; `commit_count` counts all of them. For `ref:updated` they are the commits reachable from `sha.to` and not from `sha.from`, and `forced` says whether `sha.from` is not an ancestor of `sha.to`. For `ref:created` they are the commits reachable from the new ref that no ref in the repository reached before the write. A pull request's `refs/pull/{n}/head` has no events of its own: it is the pull request's `head.sha`, which every event that moves it carries.
