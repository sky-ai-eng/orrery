# GitHub surface

Everything the GitHub face serves, and how it behaves. Behavior every route shares (where the face is mounted, authentication, errors, pagination, conditional requests, and rate limits) is in `grammar.md`.

The surface grows when a consumer needs it (rule 1). A route not listed here answers `501` naming itself. A request field GitHub defines but Orrery does not implement is refused with a `422` naming the field, never ignored, because ignoring it would leave the caller's world silently different from what it asked for. Section 4 lists where Orrery chose a behavior it has not observed on GitHub, and section 5 what is not served.

## 1. REST

Every route is under `/api/v3` and needs a token, except the routes a GitHub App calls as itself, which take a JWT the App signed (§1.11).

### 1.0 Objects

Routes answer with these shapes. Fields GitHub sends that are not listed are absent, never null.

| Object | Fields |
| --- | --- |
| Nested user | `login, id, node_id, type, site_admin` (`false`). `type` is `User`, `Organization`, or `Bot`; a bot's `login` is `<slug>[bot]`. |
| User | A nested user plus `name` (the login), `avatar_url`, `url`, `html_url` (`{base}/apps/{slug}` for a bot), `created_at`, `updated_at` |
| Organization | A nested user plus `url, repos_url, members_url, description` (null), `avatar_url, html_url, created_at, updated_at` |
| Team | `id, node_id, url, html_url, name, slug, description, privacy` (`closed`), `notification_setting, permission` (`pull`), `members_url, repositories_url, parent` (null). A full team adds `members_count, repos_count` (0), `created_at, updated_at, organization`. |
| Repository | `id, node_id, name, full_name, owner, private, visibility, description, fork` (false), `html_url, url, clone_url, git_url, ssh_url` (both on the base URL's host without its port, since Orrery serves neither protocol), `default_branch, language` (null), `archived, disabled` (false), `created_at, updated_at, pushed_at` (null until a push or merge) |
| Pull request | `id, node_id, number, state` (`open` or `closed`), `locked` (false), `title, body, draft, user, html_url, url, issue_url, merged, mergeable` (null in a list and in the review-request answers; see below), `merge_commit_sha, merged_by, created_at, updated_at, closed_at, merged_at, head, base` (each `label, ref, sha, user, repo`), `labels, requested_reviewers, requested_teams, assignees` (empty), `milestone` (null) |
| Review | `id, node_id, user, body, state` (`APPROVED`, `CHANGES_REQUESTED`, or `COMMENTED`), `commit_id, html_url, pull_request_url, submitted_at, author_association` (`NONE`) |
| Issue comment | `id, node_id, user, body, html_url, url, issue_url, created_at, updated_at, author_association` (`NONE`) |
| Label | `name, color` (`ededed`, what GitHub gives a label created on first use), `default` (false), `description` (null). No `id`. |
| Check run | `id, node_id, name, head_sha, status, conclusion, external_id, details_url, url, html_url, started_at, completed_at, output, check_suite` (`{id}`), `app` (the App of the bot that created it, as `GET /app` answers it; for a bot that has no App, `slug` and `name`, both the bot's bare login) |
| App | `id, slug, client_id, node_id, owner` (a nested user), `name, description` (null), `external_url` and `html_url` (both `{base}/apps/{slug}`), `created_at, updated_at, permissions` (each permission's name to `read`, `write`, or `admin`), `events` |
| Installation | `id, account` (a nested user), `repository_selection` (`all` or `selected`), `access_tokens_url, repositories_url, html_url, app_id, app_slug, client_id, target_id` (the account's id), `target_type` (`User` or `Organization`), `permissions, events, created_at, updated_at` (both with milliseconds, `2020-01-01T00:00:00.000Z`, as GitHub writes an installation's times alone), `single_file_name` (null), `has_multiple_single_files` (false), `single_file_paths` (empty), `suspended_by, suspended_at` (null) |
| SSH signing key | `id, key` (its type and base64 blob, without a comment), `title, created_at` |
| Commit | `sha, url, html_url, commit, parents`. `commit` is `url, author, committer, message, tree, verification`: `author` and `committer` are `name, email, date` as the commit object holds them, `message` is the commit's without its final newlines, `tree` is `sha, url`, and `verification` is `verified, reason, signature, payload, verified_at` (§1.10). `parents` is `[{sha, url, html_url}]`. `files`, `stats`, and the accounts GitHub matches to the author and committer (the top-level `author` and `committer`) are absent. |

A pull request's `labels` are ordered by name, ignoring case, and its `requested_reviewers` and `requested_teams` are in request order. Its `head.repo` and `base.repo` are always the same repository.

A single read and the create and edit answers add `mergeable_state` and `rebaseable`, and fill in `mergeable` from a test merge of the head onto the base branch's tip. The first row that applies decides:

| Pull request | `mergeable` | `mergeable_state` | `rebaseable` |
| --- | --- | --- | --- |
| Merged | null | `unknown` | null |
| Base branch gone | `false` | `dirty` | `false` |
| Its test merge is stale | null | `unknown` | null |
| The test merge conflicts | `false` | `dirty` | `false` |
| The test merge is clean | `true` | `clean` | `true` while open, else `false` |

GitHub works mergeability out in the background and answers null until it has. On github.com in November 2026:

- Opening a pull request, reopening it, and moving it to another base each start a test merge. The answer of the request that did it is null, and the test merge finishes a second or two later with no read.
- A move of the base branch, by a push or a merge, starts none, and neither does a push to the head branch. Each leaves the last test merge stale for as long as nothing reads the pull request's mergeability, which was five minutes in the longest case observed.
- A read that carries mergeability and finds the last test merge stale starts one, and answers null. Those reads are the REST single read, the answer of a REST edit, and GraphQL `mergeable`, whether in a single read, in `nodes`, or in a connection. A REST list and git start none.
- A closed pull request whose head branch is gone gets no new one, so once its last test merge is stale there is nothing to report, however often it is read.
- Nothing records when the answer arrives: `updated_at` does not move.

Orrery stores whether each pull request's last test merge is stale, and the writes above set and clear that as GitHub does. A read that carries mergeability and finds it stale starts a test merge and answers null, and the next read answers. Starting one is a write to the world, so these face reads can change it: the write is a `pull:test_merge_started` event by the reader, through the door the read came through (`events.md`), and the test merges a client's reads cause are counted from the log. A closed pull request whose head branch is gone starts none. A draft reads as any other pull request.

A list carries no test merge: `mergeable` stays null and the other two are absent, so the list's `ETag` does not change when a base branch moves.

### 1.1 Users and organizations

| Route | Behavior |
| --- | --- |
| `GET /user` | The caller as a user object. An organization's token answers the organization. |
| `GET /user/emails` | `[{email, primary, verified, visibility}]`, the primary first, then by address. `visibility` is `private` on the primary and null otherwise. Every actor starts with `<login>@orrery.invalid`, primary and verified; the control plane replaces the set. |
| `GET /users/{username}` | A user or organization by login, or a bot as `<slug>[bot]`. A bot's bare slug, or a login nobody holds, is `404`. |
| `GET /user/{account_id}` | An account by its id, answered as `GET /users/{username}` answers it under the login it holds now: a user, an organization, or a bot as `<slug>[bot]`. An id no account has is `404`. GitHub serves other endpoints beside an account id, such as `/user/keys`, so a segment that isn't an integer is surface Orrery doesn't serve, and answers `501`. |
| `GET /user/teams` | The caller's teams in every organization, as full teams, paged. |
| `GET /orgs/{org}` | The organization object, or `404` for any login that is not an organization. |

A bot's token is refused on `GET /user`, `/user/emails`, `/user/teams`, `/user/repos`, and `/user/ssh_signing_keys` with `403` `{"message": "Resource not accessible by integration", ...}`, as GitHub refuses an installation token.

### 1.2 Organization membership and teams

| Route | Behavior |
| --- | --- |
| `GET /orgs/{org}/memberships/{username}` | The membership, as the `PUT` answers it, or `404` for an organization or a user that does not exist, an account that is not an organization, and a user who is not a member, as on GitHub. |
| `PUT /orgs/{org}/memberships/{username}` | Body optional; `role` is `member` (the default) or `admin`, else `422`. Answers `{url, state, role, organization_url, organization, user}`. A member is added at once, so `state` is always `active` (§4). Only a user can be a member; an unknown organization or user is `404`. |
| `GET /orgs/{org}/teams` | Teams oldest first, paged; `[]` for an organization with none, `404` for a login that is not an organization. |
| `POST /orgs/{org}/teams` | `{name, description?, privacy?}`. A missing, null, or blank `name` is `422 missing_field`; a non-string `name`, or one with no ASCII letter or digit, is `422 invalid`. The slug is the name lower-cased with each run of other characters turned into one hyphen. A name or slug already in the organization, ignoring case, is `422 already_exists`. A creator who is a member of the organization becomes the team's first maintainer. `maintainers`, `repo_names`, `parent_team_id`, and `privacy: secret` are not mocked. Answers `201` with the full team. |
| `DELETE /orgs/{org}/teams/{team_slug}` | `204`. The team's memberships and pending review requests go with it. Team ids are never reused. |
| `PUT /orgs/{org}/teams/{team_slug}/memberships/{username}` | Body optional; `role` is `member` (the default) or `maintainer`, else `422`. Answers `{url, role, state: "active"}`. A login that is not a user is `404`; a user who is not a member of the organization is a `422`. |
| `DELETE /orgs/{org}/teams/{team_slug}/memberships/{username}` | `204`, including for a user not on the team. |

### 1.3 Repositories

| Route | Behavior |
| --- | --- |
| `POST /user/repos`, `POST /orgs/{org}/repos` | `{name, description?, private?, visibility?}`; `visibility` (`public` or `private`) overrides `private`. A name is 1 to 100 characters from `[A-Za-z0-9._-]`, not `.` or `..`, and not ending in `.git`; a missing or invalid name, or one the owner already has (ignoring case), is GitHub's `422` "Repository creation failed." envelope. `auto_init`, `gitignore_template`, `license_template`, `is_template`, and `team_id` are not mocked, whatever their value. The repository starts empty with `default_branch: main` until its first push (`surface-git.md`). Answers `201`. |
| `GET /repos/{owner}/{repo}` | The repository, or `404`. |
| `GET /repositories/{repository_id}` | The repository by its id, answered as `GET /repos/{owner}/{repo}` answers it, or `404`. A world neither renames nor deletes a repository (§4), so it answers every repository under the name it was created with. |
| `GET /user/repos` | The repositories the caller owns, paged. `sort` is `pushed` (the default); any other `sort` is not mocked. `direction` is `desc` (the default) or `asc`, else `422`. Never-pushed repositories sort last. |

### 1.4 Pull requests

| Route | Behavior |
| --- | --- |
| `GET /repos/{o}/{r}/pulls` | `state` is `open` (the default), `closed` (merged included), or `all`, else `422`. Newest first, paged, with an `ETag` on every page (`grammar.md`). `head`, `base`, `sort`, and `direction` are ignored. |
| `POST /repos/{o}/{r}/pulls` | `{title, head, base, body?, draft?}`. `head` is a branch, or `owner:branch` naming the repository's own owner. Missing fields are `422 missing_field`; a branch that doesn't exist is `422 invalid`. No commits between base and head, or an open pull request with the same head and base, is GitHub's `422` with its message. A cross-repository `head`, `issue`, and `head_repo` are not mocked. The head and base commits are pinned at creation, and the repository's `refs/pull/{n}/head` is set to the head (`surface-git.md`). Answers `201`. |
| `GET /repos/{o}/{r}/pulls/{n}` | The pull request, with its mergeability (§1.0), or its diff in the diff media type (below). |
| `PATCH /repos/{o}/{r}/pulls/{n}` | `title` (trimmed), `body` (null clears), `state` (`open` or `closed`; any other value is ignored, as on GitHub), and `base` (moving to another branch pins that branch's tip as the base commit). A blank `title` is `422 missing_field`, as on create, and is refused before anything is written. Otherwise the request is written in GitHub's order, and a refusal keeps every write before it: the `body`; then, unless the reopen is refused, the `title` and the state change; then the `base`, judged against the state the pull request is left in and moved after a reopen. Reopening a pull request moves its head to its head branch's tip, and its `refs/pull/{n}/head` with it (`surface-git.md`), in the same commit as the reopen. A refused reopen keeps only the `body`, and is GitHub's `422`, a `custom` error on `state`, unless a `base` was sent beside it: that base is judged against the pull request left closed, and its refusal is the answer. The reopen checks run in this order: for a merged pull request, "state cannot be changed. The pull request cannot be reopened.", whatever its branches hold; for a base or head branch that is gone, "state cannot be changed. The {branch} branch has been deleted.", and for both, "state cannot be changed. The {base} and {head} branches have been deleted."; while another pull request is open from the same head into the same base, the rule that also refuses a create, "state cannot be changed. There is already an open pull request from {head} to {base}."; and for a head branch whose tip does not descend from the head (a force-push, or a branch deleted and recreated elsewhere), "state cannot be changed. The {branch} branch was force-pushed or recreated." A refused `base` is GitHub's `422`, an `invalid` error on `base`, and keeps the request's other writes, in this order: on a pull request that is closed once the state change lands, whatever branch it names, "Cannot change the base branch of a closed pull request."; for a branch that does not exist, "Proposed base branch '{base}' was not found"; for a branch that already holds the head's commits, the head branch among them, "There are no new commits between base branch '{base}' and head branch '{head}'"; and for a base another open pull request from the same head already targets, "A pull request already exists for base branch '{base}' and head branch '{head}'". |
| `PUT /repos/{o}/{r}/pulls/{n}/merge` | A real merge: `git merge-tree` and `commit-tree`, a two-parent commit authored and committed by `<login> <login@orrery.invalid>` at the world clock, with GitHub's default message unless `commit_title` or `commit_message` is given. A pull request that is not open, is a draft, or conflicts with its base is `405` "Pull Request is not mergeable" (a draft: "Pull Request is still a work in progress"); a `sha` that is not the head is `409`. A `merge_method` other than `merge` is not mocked. The base branch moves by compare-and-swap, and open pull requests whose head is the base branch take its new tip, their `refs/pull/{n}/head` with it. The merge is one write: `pull:merged` and its timeline item, then the base branch's move recorded as a push of it would be, with `repo:pushed`, `ref:updated`, and an event for each pull request the move changes (`events.md`). The write lands in one commit, after every ref the merge moves is in place, as a push's does (`surface-git.md`). A merge whose record fails moves its refs back, appends nothing, and answers `500`. Answers `{sha, merged: true, message}`. |
| `GET /repos/{o}/{r}/pulls/{n}/files` | `[{filename, status, additions, deletions, changes, patch?, previous_filename?}]` in path order, paged, from the pull request's diff (below). `status` is `added`, `removed`, `modified`, `renamed`, `copied`, or `changed`. `additions` and `deletions` count the lines the file adds and removes, 0 for a binary file, and `changes` is their sum. `patch` is the file's part of the diff from its first `@@` line, without the final newline; text that is not UTF-8 reads with each invalid sequence replaced by U+FFFD. A type change is two files of one name, the one `removed` and then the one `added`. `patch` is absent, never null, where the file has no hunk (a binary file, a pure rename, a mode change, or an empty file), where it is 100 KiB (102,400 bytes) or larger, which leaves the file's counts in place, and past a response's limits (below). `sha` and the URLs are absent. |

The changed files of one response, a page of these or a compare's (§1.8), share two limits, taken in the order they are listed. A file's patch counts its lines, `@@` lines included, and its bytes plus one. Once the next file's patch would take the response past 20,000 lines or past 1 MiB (1,048,576 bytes), that file and every file after it read 0 for `additions`, `deletions`, and `changes`, and have no `patch`. A patch left out for its size counts toward neither limit. The limits are per response, so a smaller `per_page` can carry patches a larger one leaves out.

A pull request's diff is the output of `git diff -M` between the base and the head: against the base branch's tip while the pull request is open (or the pinned base if the branch is gone), and against the pinned base once it is closed. Its files and GraphQL's diff fields (§3.3) come from the same comparison.

`GET …/pulls/{n}` answers the diff when `Accept` lists the diff media type: `application/vnd.github.v3.diff` or `application/vnd.github.diff`, either one with or without `+json`. The diff wins over anything `Accept` lists beside it, the patch media type and JSON included. The answer is `200` with `Content-Type` naming the diff media type `Accept` listed, plus `; charset=utf-8`, and an empty body when the diff is. Its `index` lines abbreviate object ids to 7 hex digits, or more where 7 would be ambiguous. A diff of more than 300 files is GitHub's `406`, with nothing else limiting it:

```json
{"message": "Sorry, the diff exceeded the maximum number of files (300). Consider using 'List pull requests files' API or locally cloning the repository instead.",
 "errors": [{"resource": "PullRequest", "field": "diff", "code": "too_large"}]}
```

GitHub's body also carries `documentation_url` and `status`, which no face error does. The patch media type (`application/vnd.github.v3.patch`) is refused (`grammar.md`), as is the diff media type on `…/pulls/{n}/files`.

### 1.5 Reviews, review requests, and comments

| Route | Behavior |
| --- | --- |
| `GET /repos/{o}/{r}/pulls/{n}/reviews` | Oldest first, paged. |
| `POST /repos/{o}/{r}/pulls/{n}/reviews` | `{event, body?, commit_id?}`. `event` is `APPROVE`, `REQUEST_CHANGES`, or `COMMENT`, else `422`; an absent `event` (a pending review) and `comments` are not mocked. The author approving or requesting changes on their own pull request is GitHub's `422`. `commit_id` defaults to the head. A review removes its author's pending request; a team's request stays. |
| `POST /repos/{o}/{r}/pulls/{n}/requested_reviewers` | `{reviewers?, team_reviewers?}`, each a list of names, and a list may be empty. Every name is checked before anything is written, and the refusals are GitHub's `422`s, a `message` alone, in GitHub's order: a team not in the organization that owns the repository is "Reviews may only be requested from collaborators. One or more of the teams you specified is not a collaborator of the {owner}/{repo} repository."; the author's login is "Review cannot be requested from pull request author."; and an organization's login is the first message with "users or teams" for "teams". A login that names nobody, a bot's login, and a reviewer already pending are skipped. Logins match whatever their case. Answers `201` with the pull request. A body the schema refuses is `422` "Invalid request.\n\n" and then: with neither list, "No subschema in \"anyOf\" matched.\n\"reviewers\" wasn't supplied.\n\"team_reviewers\" wasn't supplied."; with a list that is not one, "For 'properties/{field}', {value} is not an array.", where a null is `nil`; with a name that is not a string, "For 'items', {value} is not a string."; and with no body, "For 'links/0/schema', nil is not an object." |
| `DELETE /repos/{o}/{r}/pulls/{n}/requested_reviewers` | `{reviewers, team_reviewers?}`, `reviewers` required and possibly empty: without it, `422` "Invalid request.\n\n\"reviewers\" wasn't supplied." The other schema refusals are the request's, and no body names `links/2/schema`. A name with no pending request is skipped. Answers `200` with the pull request. |
| `GET /repos/{o}/{r}/pulls/{n}/comments` | Always `[]`: Orrery has no inline review comments. |
| `GET /repos/{o}/{r}/issues/{n}/comments` | Oldest first, paged. `{n}` is a pull request; the world has no issues. |
| `POST /repos/{o}/{r}/issues/{n}/comments` | `{body}`, non-empty, else `422 missing_field`. Answers `201`. |

### 1.6 Labels

Orrery keeps labels per pull request, not per repository, unique on the pull request ignoring case. There is no repository label API.

| Route | Behavior |
| --- | --- |
| `POST /repos/{o}/{r}/issues/{n}/labels` | `{"labels": [names]}` or a bare array of names. A label object (`{"name": ...}`) is not mocked; an empty name, or one longer than 50 characters, is `422`. A name already on the pull request is skipped. Answers `200` with all its labels. |
| `DELETE /repos/{o}/{r}/issues/{n}/labels/{name}` | Matches ignoring case. Answers `200` with the labels left, or `404` "Label does not exist". |

### 1.7 Check runs

| Route | Behavior |
| --- | --- |
| `POST /repos/{o}/{r}/check-runs` | `{name, head_sha, status?, conclusion?, started_at?, completed_at?, details_url?, external_id?, output?}`. `status` is `queued` (the default), `in_progress`, or `completed`; `conclusion` is one of GitHub's eight values, and implies `completed`. `started_at` defaults to the world clock, and `completed_at` to the clock once the run completes. `output` is stored as sent. `actions` is not mocked. Answers `201`. |
| `PATCH /repos/{o}/{r}/check-runs/{id}` | The same fields, each kept when absent. Moving a run to another `head_sha`, and `actions`, are not mocked. |
| `GET /repos/{o}/{r}/commits/{ref}/check-runs` | `{total_count, check_runs}` for a full sha or a branch, newest first, paged; anything else is `422` "No commit found for SHA: {ref}". A sha names its commit in upper case or lower, here, on `…/check-suites`, and as a new check run's `head_sha`, which is answered in lower case. It lists the newest run of each name in each suite, and with `filter=all` every run; any other `filter` is the default. `check_name`, `status`, and `app_id` each keep the runs that match, and a value that matches none keeps none. |
| `GET /repos/{o}/{r}/check-runs/{id}` | The check run, or `404` for one that is not this repository's. |
| `GET /repos/{o}/{r}/check-suites/{id}` | The suite: `id, node_id, head_branch, head_sha, status, conclusion, url, before, after, pull_requests, app, created_at, updated_at, rerequestable, runs_rerequestable, latest_check_runs_count, check_runs_url, repository`. `404` for one that is not this repository's. |
| `GET /repos/{o}/{r}/check-suites/{id}/check-runs` | The suite's check runs, with the same filters. GitHub lists the newest of each name oldest first, and with `filter=all` every run newest first, and so does a world. |
| `GET /repos/{o}/{r}/commits/{ref}/check-suites` | `{total_count, check_suites}` for a full sha or a branch, oldest first, paged; anything else is `404`. `app_id` keeps one App's. With `check_name`, GitHub answers a suite once for each of its check runs of that name, so a suite with two is listed twice, and so does a world. |

Only a bot creates or updates check runs; any other actor gets a `403` naming the bot to use. On GitHub only an App can.

**Check suites.** A suite is the check runs one App made on one commit, and each check run is in one. What follows was observed on github.com on 2026-11-19, with a scratch App on a scratch repository.

- **When one is made.** A push that brings a commit to a branch's tip makes a suite there for each App installed on the repository with write access to checks, before any check run exists: `queued`, with `head_branch` the branch, `before` what the branch pointed at (forty zeros for a new branch), and `after` the commit. A merge is such a push. An App's first check run on a commit where it has no suite makes one whose `head_branch`, `before`, and `after` are null.
- **What it comes to.** A suite's `status` and `conclusion` follow its check runs, counting the newest run of each name: `in_progress` while any is, `queued` while any is queued or it has none, and otherwise `completed`, with the first of these conclusions any run holds: `action_required`, `timed_out`, `failure`, `cancelled`, `success`, `neutral`, `stale`, `skipped`. GitHub's API refuses `stale` as a check run's conclusion and a world accepts it, so its place is a world's own. GitHub takes a few seconds to bring a suite up to date, and a world does it in the write.
- `latest_check_runs_count` counts the newest run of each name. `pull_requests` are the open pull requests whose head is the commit, as a check run lists them. `rerequestable` and `runs_rerequestable` are true.

### 1.8 Branches, refs, compare, and contents

| Route | Behavior |
| --- | --- |
| `GET /repos/{o}/{r}/branches` | `[{name, commit: {sha}, protected: false}]` ordered by name, paged. |
| `GET /repos/{o}/{r}/git/ref/heads/{branch}` | `{ref, url, object: {sha, type: "commit", url}}`, with an `ETag` derived from the body, so it changes exactly when the tip does (`grammar.md`). A branch name's `/` stays a path separator. A missing branch is `404`. |
| `GET /repos/{o}/{r}/compare/{base}...{head}` | `{status, ahead_by, behind_by, total_commits, commits, files}`. Each end is anything `git rev-parse` resolves to a commit (a sha, a branch, a tag, a ref); `owner:branch`, the two-dot form, and an end that doesn't resolve are `404`. `status` is `identical`, `ahead`, `behind`, or `diverged`. `commits` is `[{sha, parents: [{sha}]}]` oldest first, at most 250; `files` is as on pull request files, from `git diff -M base...head`, in path order, its first 300 with nothing marking the rest left out, and under the same per-response limits (§1.4). The diff media type answers the comparison's diff, as on a pull request (§1.4), with no limit at all. |
| `GET /repos/{o}/{r}/contents/{path}` | A file at `ref` (default `HEAD`): `{type: "file", encoding: "base64", name, path, sha, size, content, url, html_url, download_url: null}`. `content` is not line-wrapped. `application/vnd.github.object` answers the same object. A directory, a missing file, or an empty repository is `404`. |

### 1.9 Meta

| Route | Behavior |
| --- | --- |
| `GET /meta` | `{verifiable_password_authentication: false, installed_version: "3.17.0"}`. No actor has a password to verify (rule 12). GitHub's description of 3.17 also lists `packages` and `domains`, the addresses of services Orrery does not run, so both are absent. |

Clients read `installed_version` to decide which requests to send, so Orrery reports the release whose requests it serves. 3.17 is the first release without classic projects, which Orrery does not have, and the last before advanced issue search, whose syntax (parentheses and `OR`) the search in §3.2 does not parse. gh 2.89, for one, asks for a pull request's classic project cards below 3.17, and sends advanced search syntax from 3.18.

Under github.com's layout, `GET /meta` is the not-mocked `501`, through any host (`grammar.md`, "Where the face is"): github.com's `/meta` names no release, and lists network addresses and keys Orrery does not have.

### 1.10 Commit signing

An account signs commits with an SSH key it registers as a signing key, and a commit read answers GitHub's verdict on its signature.

| Route | Behavior |
| --- | --- |
| `GET /users/{username}/ssh_signing_keys` | The account's signing keys, oldest first, paged. The account is found as `GET /users/{username}` finds it, so a login nobody holds, and a bot's bare slug, are `404`. A bot holds none, since its token is refused on `POST /user/ssh_signing_keys`. |
| `GET /user/ssh_signing_keys` | The caller's signing keys, as above. |
| `POST /user/ssh_signing_keys` | `{key, title?}`. `key` is an OpenSSH public key, `<type> <base64> [comment]`, of type `ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, `ecdsa-sha2-nistp384`, `ecdsa-sha2-nistp521`, `sk-ssh-ed25519@openssh.com`, or `sk-ecdsa-sha2-nistp256@openssh.com`, and is stored without its comment. A missing, null, or blank `key` is `422 missing_field`. Any other string that is not such a key is a `custom` error on `key`, "key is invalid. You must supply a key in OpenSSH public key format", and so is a key any account already holds, "key is already in use". A `title` left out is null. Answers `201` with the key. |
| `DELETE /user/ssh_signing_keys/{ssh_signing_key_id}` | `204`. A key the caller does not hold is `404`, whoever holds it. Key ids are never reused. A verdict a push stored for a commit the key signed stays. |
| `GET /repos/{o}/{r}/commits/{ref}` | The commit (§1.0). `ref` is anything `git rev-parse` resolves to a commit, as a compare's ends are; anything else is `422` "No commit found for SHA: {ref}". |

**The verdict.** `verification.verified` is true exactly when `reason` is `valid`. The first row that applies decides `reason`:

| The commit | `reason` |
| --- | --- |
| has no signature | `unsigned` |
| has a PGP or an X.509 signature. No account in a world holds a GPG key or a certificate, since nothing registers one. | `unknown_key` |
| has an SSH signature that does not parse | `malformed_signature` |
| has an SSH signature by a key no account holds as a signing key | `unknown_key` |
| has one that does not verify with that key | `invalid` |
| is committed under an address the key's account holds, verified | `valid` |
| is committed under an address the key's account holds, unverified | `unverified_email` |
| is committed under an address only other accounts hold | `bad_email` |
| is committed under an address no account holds | `no_user` |

Addresses compare ignoring case. Whether an SSH signature verifies is judged by `git verify-commit` (rule 2), which runs `ssh-keygen -Y verify` against an allowed-signers file holding only the key the signature names, and hands it the commit's own committer date as the time to verify at. So that answer depends on the commit alone, and which account holds the key is the world's. A world that verifies needs `ssh-keygen` on `PATH` (`docs/users/install.md`). Without it, a read of an SSH-signed commit whose verdict was not stored is `500`, and so is a push that brings one in, which moves back what it moved (`surface-git.md`, "What a push does").

**When the verdict is reached.** A push reaches the verdict of each commit its ref events list (`events.md`, "Ref events") that carries a signature, against the world as its write finds it, and stores each one that is `valid` as a `commit:verified` event in its write. A stored verdict answers every later read: `valid`, with `verified_at` the world clock at the push, after the key is deleted or the address changes too. A failing verdict is not stored, so a read reaches it again from the world as it is then, and a commit whose fault has since been fixed, by registering its key or verifying its address, reads `valid` with `verified_at` null. A commit no push listed, such as one past a ref event's 2048, has its verdict reached on every read. `signature` is the armored signature and `payload` the commit without it, which is what the signature signs; both are null for an unsigned commit.

### 1.11 GitHub Apps

A GitHub App is registered, given a public key, and installed on the control plane (`usage.md`), or by a person on the pages GitHub does each on (§2.2). Its bot is the `bot` actor whose login is its slug. An App is an OAuth app too: it has a client id, client secrets, and callback URLs, and the OAuth web flow takes its client id (§2).

**As the App.** These routes take a JWT the App signed, as `Authorization: Bearer <jwt>`, in place of a token.

| Route | Behavior |
| --- | --- |
| `GET /app` | The App, plus `installations_count`. |
| `GET /app/installations` | The App's installations, newest first, paged, with a `Link` header (below). |
| `GET /app/installations/{installation_id}` | One installation. An id that is not one of the App's installations is `404`. |
| `POST /app/installations/{installation_id}/access_tokens` | Issues an installation token, answering `201` `{token, expires_at, permissions, repository_selection}`. With no body, `null`, or `{}`, as GitHub's clients send when they have no options, the token reaches what the installation covers. `repositories`, a list of repository names, narrows it to those: `repository_selection` is then `selected`, and `repositories` lists them as repositories. A name the installation does not cover is `422` "There is at least one repository that does not exist or is not accessible to the parent installation." `permissions` narrows it to the levels it gives, plus `metadata: read` when the installation holds `metadata`. A permission the installation does not hold is `422` "The permissions requested are not granted to this installation.", one it holds at a lower level is `422` "The level of access for permissions requested are not granted to this installation.", and a level other than `read`, `write`, or `admin` is `422` "There is at least one permission action that is not supported. It should be one of: \"read\", \"write\" or \"admin\"." The body is read laxly, as GitHub reads it: an empty list or object, a field of the wrong type, and an unknown field narrow nothing. A body that is not JSON is `400`. `repository_ids` is not mocked. |
| `PUT /app/installations/{installation_id}/suspended` | Suspends the installation: `204`, and `204` again while it is suspended. A read then answers `suspended_at`, a time with no milliseconds, and `suspended_by`, the App's bot, and `updated_at` moves to the suspension, as it does again when the suspension is lifted. While it is suspended, a request for a token is `403` "This installation has been suspended", a token issued earlier is `403` "This GitHub App installation is currently suspended." on every REST and GraphQL route, and over git `401` "Invalid username or token. Password authentication is not supported for Git operations.", and the installation is sent no event about its repositories. |
| `DELETE /app/installations/{installation_id}/suspended` | Lifts the suspension: `204`, and `204` again. Tokens issued earlier work again until they expire. |
| `DELETE /app/installations/{installation_id}` | Removes the installation: `204`, then `404`. Its tokens are `401` "Bad credentials" from then on, and the App can be installed on the account again, as a new installation with a new id. |
| `GET /app/hook/config`, `PATCH /app/hook/config`, `GET /app/hook/deliveries`, `POST /app/hook/deliveries/{delivery_id}/attempts` | The App's webhook (§1.12). |

**With a person's token.** `GET /user/installations` answers `{total_count, installations}`: the installations of the token's App that its user reaches, newest first, each the object `GET /app/installations/{id}` answers, paged with `per_page` and `page` and the `Link` header below. A person reaches the installation on their own account and the ones on organizations they are a member of. GitHub answers it only to a token a person gave an App (§2): any other person's token is `403` "You must authenticate with an access token authorized to a GitHub App in order to list installations", and a bot's token is `403` "Resource not accessible by integration", as on the other `/user` routes (§1.1).

**The `Link` header.** `GET /app/installations`, `GET /user/installations`, and `GET /installation/repositories` send GitHub's `Link` header when the list is more than the page asked for. It names, in this order, `prev`, `next`, `last`, and `first`, each when there is such a page and it is not the page asked for; a page past the end names the last page as both `prev` and `last`. Each is a URL on the base URL carrying `page`, and `per_page` when the request carried it.

GitHub serves more to an App's JWT than Orrery does: every other path under `/app`, and the lookups of the installation on a repository, an organization, or a user (`GET /repos/{o}/{r}/installation`, `/orgs/{org}/installation`, `/users/{username}/installation`). Each answers the `501` of surface Orrery does not serve, to a valid JWT alone, so a caller whose JWT is good is never told it is bad.

**The JWT** is RS256 over `iss`, `iat`, and `exp`, checked against the App's keys. Each answer here, and each answer in this section that is GitHub's own, was observed on github.com on 2026-11-13, with a scratch App installed on one repository of a user and on every repository of an organization.

- `iss` is the App's id, as a number or a string of digits, or its client id.
- `iat` is a number, with or without a fraction, or a string of one. `exp` is an integer, or a string of one.
- A `real` world holds both to its clock, as GitHub does, allowing a minute between its clock and the signer's: `exp` is refused from 60 seconds after it passes, and when it is more than 10 minutes ahead, and `iat` when it is more than 60 seconds ahead. Only `exp` is held to 10 minutes: a JWT issued an hour ago that expires in 5 minutes is accepted.
- A `manual` world compares neither to its clock, which is not the signer's (`usage.md`, "Time").

| Fault | Answer |
| --- | --- |
| No `Authorization` header | `401` "Requires authentication" |
| A scheme other than `Bearer`, or one of the world's tokens, such as an installation token | `401` "A JSON web token could not be decoded" |
| A value that is not three segments, an algorithm other than RS256, or no `iss` | `401` "Bad credentials" |
| An `iss` that is a number, or digits, no App has | `404` "Integration not found" |
| An `iss` that is any other string and no App's client id | `401` "'Issuer' claim ('iss') must be an Integer" |
| A signature none of the App's keys made | `401` "A JSON web token could not be decoded" |
| No `iat` | `401` "Missing 'issued at' claim ('iat') in assertion" |
| No `exp` | `401` "Bad credentials" |
| An `exp` with a fraction | `401` "'Expiration time' claim ('exp') must be a numeric value representing the future time at which the assertion expires" |
| In a `real` world, an `exp` that passed a minute ago, or an `iat` more than a minute ahead | `401` "Bad credentials" |
| In a `real` world, an `exp` more than 10 minutes ahead | `401` "'Expiration time' claim ('exp') is too far in the future" |

The signature is checked before `iat` and `exp`. GitHub's order between the two was not observed.

**An installation token** is a token like any other (`grammar.md`), prefixed `orrfake_` and derived from the world's secret. It acts as the App's bot, so the routes that refuse a bot refuse it (§1.1), and a write it makes records the App. It expires an hour after it is issued, on the world's clock, and is then `401` "Bad credentials", with no allowance: on github.com a token was accepted 6 seconds before its `expires_at` and refused 4 seconds after it.

It reaches the repositories its installation covers: every repository the account owns, those created later included, or the ones the installation lists. A narrowed token reaches the repositories it was narrowed to.

- A private repository it does not reach is `404` on every route under `/repos/{o}/{r}`, and on `GET /repositories/{id}`.
- A public repository it does not reach is read as anyone reads it. Any other method on it is `403` `{"message": "Resource not accessible by integration", ...}`.
- Git answers the same way: `404` "Repository not found." for the private one, and for the public one a fetch succeeds and a push is `403` "Permission to {owner}/{repo}.git denied to {slug}[bot]." (`surface-git.md`).
- GraphQL shows it the same repositories (§3.2).
- An organization other than the account it is installed on is read as anyone reads it. Any other method on a route under `/orgs/{org}` is the same `403`, so it creates no repository and no team there. A name no organization has is the route's `404`, as it is for any caller.

Permissions are recorded and echoed, not checked: no route reads a token's permissions, as none reads an OAuth token's scopes.

| Route | Behavior |
| --- | --- |
| `GET /installation/repositories` | `{total_count, repository_selection, repositories}`: the repositories the caller's installation token reaches, by id, paged, with a `Link` header (above). `repository_selection` is `selected` for a narrowed token. Any other token is `403` "You must authenticate with an installation access token in order to list repositories for an installation." |

**Client credentials.** One route takes an OAuth client's id and secret as HTTP Basic, for an App's client and any other OAuth app's. Every other path under `/applications/{client_id}`, and every other method, answers the `501` of surface Orrery does not serve, to a client id and secret that name an OAuth app.

| Route | Behavior |
| --- | --- |
| `POST /applications/{client_id}/token` | `{access_token}`. No `Authorization` header is `401` "Requires authentication", and a client id and secret that do not name an OAuth app are `401` "Bad credentials". A token the client did not issue, an installation token among them, is `404`, and so is a path that names another client. That `404` is how a caller holding a client secret learns that it is right. A token the client did issue is not mocked: GitHub answers the authorization it stands for. |

### 1.12 Webhooks

An App's webhook is set on the control plane (`usage.md`): its URL, whether it is active, and its secret, which the world derives. While it is active, the world makes a delivery for each event below and sends it (`usage.md`, "What a world sends"). What this section says GitHub does was observed on github.com on 2026-11-14, on a scratch App whose webhook pointed at a host that resolves nowhere, by reading each delivery back from `GET /app/hook/deliveries/{delivery_id}`. What the delivery list says of a receiver's answer was observed by sending the App's `ping` again to a public test service that answers with a chosen status.

**Which App is sent an event.** An `installation` event goes to the installation's App, whatever it subscribes to. Any other event goes to each App that has an installation which covers the repository, lists the event among its `events`, and is not suspended.

| Event | Actions | Payload |
| --- | --- | --- |
| `installation` | `created`, `deleted`, `suspend`, `unsuspend` | `action, installation, sender`. `installation` is the object `GET /app/installations/{id}` answers. `created` and `deleted` add `repositories`, each `{id, node_id, name, full_name, private}`, and `created` adds `requester` (null). `sender` is who made the change: the App's bot for a suspension and a removal, as on GitHub when the App makes them, and for an installation, which the control plane makes, the account, where GitHub names the person who installed it. |
| `pull_request` | `opened`, `edited`, `closed`, `reopened`, `ready_for_review`, `converted_to_draft`, `labeled`, `unlabeled`, `review_requested`, `review_request_removed`, `synchronize` | `action, number, pull_request, repository, sender`. `pull_request` is the object `GET …/pulls/{n}` answers, with `mergeable` null and `mergeable_state` `unknown`, which is what GitHub sends before it has computed them. `edited` adds `changes`, the earlier value of each of `title`, `body`, and `base` that changed, as `{"title": {"from": …}}`. `labeled` and `unlabeled` add `label` and are sent once for each label. The review-request actions add `requested_reviewer` or `requested_team` and are sent once for each reviewer the call added or withdrew, so a call that changes nothing sends nothing. `synchronize`, sent when a push moves the head, adds `before` and `after`. A merge sends `closed` with `merged` true. |
| `pull_request_review` | `submitted` | `action, review, pull_request, repository, sender`. `review` is the object REST answers, with `state` in lower case (`commented`, `approved`, `changes_requested`), where REST spells it in upper, and with `updated_at` and `_links` (`html` and `pull_request`, each `{href}`). `pull_request` is the object a list carries, without `merged`, `mergeable`, and `merged_by`. |
| `issue_comment` | `created` | `action, issue, comment, repository, sender`. `issue` is the pull request as GitHub's issue object, whose `pull_request` field holds its `url`, `html_url`, `diff_url`, `patch_url`, and `merged_at`. `comment` is the object REST answers, plus `reactions`, `performed_via_github_app` (null), and `minimized` (null). `issue.reactions` and `comment.reactions` count none. |
| `check_suite` | `completed` | `action, check_suite, repository, sender`. `check_suite` is the object REST answers without its `repository`. Sent each time a write to a check run changes a suite's status or conclusion and leaves it complete: when its last check run completes, and again whenever a complete suite comes to another conclusion. A check run that leaves a complete suite as it was, such as one created already complete beside others of the same conclusion, sends none, as on GitHub. `sender` is as for `check_run`. |
| `check_run` | `created`, `completed` | `action, check_run, repository, sender`. `sender` is whoever made the push the run's suite came from, whichever App wrote the check, and for a suite no push made it is GitHub's `ghost` (`{login: "ghost", id: 10137, node_id: "MDQ6VXNlcjEwMTM3", type: "User"}`), which is no actor of the world. `check_suite` is the suite, as `id, node_id, head_branch, head_sha, status, conclusion, url, before, after, pull_requests, app, created_at, updated_at`. `check_run` is the object REST answers, plus `pull_requests`: the open pull requests whose head is the run's commit, each `{url, id, number, head, base}` with `head` and `base` as `{ref, sha, repo: {id, url, name}}`. GitHub lists them once it has tied the commit to its pull requests, which takes it a moment after the commit becomes a head: a run made at once listed none, and a run made seconds later listed them. A world lists them from the start. `completed` is sent when an update completes the run, and again when an update changes a complete run's conclusion. No other update sends anything, as on GitHub. A run created already complete sends `created` and then `completed`, as on GitHub. |
| `dependabot_alert` | `created`, `dismissed`, `reopened`, `fixed`, `reintroduced`, `auto_dismissed`, `auto_reopened` | `action, alert, repository, sender`. `alert` is the alert object (§1.13) as the move left it. `sender` is the person who dismissed or reopened it, and for every move GitHub makes itself it is GitHub's own organization, `{login: "github", id: 9919, node_id: "MDEyOk9yZ2FuaXphdGlvbjk5MTk=", type: "Organization"}`, which is no actor of the world and carries GitHub's ids, not a world's. Recording what the updater did sends nothing. |
| `push` | none | `ref, before, after, created, deleted, forced, base_ref, compare, commits, head_commit, pusher, repository, sender`, for a branch or a tag that was created, moved, or deleted, by a push or by a merge. `before` or `after` is 40 zeros for a ref that was created or deleted. `commits` are the ones the move added, oldest first, at most 2048: each `{id, tree_id, distinct, message, timestamp, url, author, committer, added, removed, modified}`, with the files compared with the commit's first parent, and every file a commit with no parent holds listed as `added`, in path order. `author` and `committer` are `{name, email, date}`, plus `username`, the login of the account that holds the address, when one does. `distinct` is whether no other branch or tag reaches the commit: a merge's push lists the merged branch's commits with `distinct` false and the merge commit with true. `head_commit` is the commit the ref names now, with `distinct` true whether or not the push added it, or null for a deleted ref. `base_ref` is null, except for a ref created at a commit that a branch names: then it is that branch, the first in name order when several name it. `pusher` is `{name, email}`, the login and primary address of who pushed. `repository` carries `created_at` and `pushed_at` as Unix seconds, and `master_branch`, `stargazers`, `owner.name`, and `owner.email`, as GitHub's does in a push alone. |

Every payload but `installation`'s also carries `installation`, `{id, node_id}`, and, for a repository an organization owns, `organization`, as `login, id, node_id, url, repos_url, events_url, hooks_url, issues_url, members_url, public_members_url, avatar_url, description` (null). A user's repository has none. The other objects in a payload are the ones REST answers (§1.0), with the fields listed there.

**The request** is a POST of the payload as JSON, with these headers, in this order: `Accept: */*`, `Content-Type: application/json`, `User-Agent: GitHub-Hookshot/0000000` (GitHub's ends in seven hex digits of its build), `X-GitHub-Delivery` (the delivery's GUID, a UUID), `X-GitHub-Event`, `X-GitHub-Hook-ID` and `X-GitHub-Hook-Installation-Target-ID` (both the App's id), `X-GitHub-Hook-Installation-Target-Type: integration`, and, for a webhook that has a secret, `X-Hub-Signature` (`sha1=` and the HMAC-SHA1 of the body under the secret, in hex) and `X-Hub-Signature-256` (`sha256=` and the HMAC-SHA256). A GUID is derived under the world's secret, so two worlds with different secrets never send a receiver the same one, and two with the same secret and the same history send the same ones.

| Route | Behavior |
| --- | --- |
| `GET /app/hook/config` | `{content_type: "json", insecure_ssl: "0", secret: "********", url}`. `secret` is absent for a webhook that has been issued none, and `url` is null for an App with no webhook. |
| `PATCH /app/hook/config` | `{url?, content_type?, insecure_ssl?}`: points the webhook at `url` and answers `200` with the configuration. The change is the App's bot's, through the App. GitHub checks the body against its schema first, each a `422` "Invalid request.\n\n" and then: a key it does not take, "\"{key}\" is not a permitted key."; a value that is not a string, "For 'properties/{key}', {value} is not a string."; a body that is not an object, "For 'links/2/schema', {value} is not an object."; and a URL holding a space, "{url} is not a valid uri." Then the URL, each a `400` whose `message` is a list: an empty one is `["Config attribute records value can't be blank", "Url cannot be blank"]`, one with no scheme `["Url is missing a scheme"]`, one whose scheme is not `http` or `https` `["Url scheme is invalid"]`, and one with no host `["Url host is invalid"]`. A world sends JSON, checks a receiver's certificate, and derives a webhook's secret, so `content_type` other than `json`, `insecure_ssl` other than `"0"`, and any `secret` are refused as not mocked (`422`), naming the field. |
| `GET /app/hook/deliveries` | The deliveries whose attempt was made, newest first: each `{id, guid, delivered_at, redelivery, duration, status, status_code, event, action, installation_id, repository_id, url, throttled_at}`. `delivered_at` is the world's clock when the result was recorded, `duration` is in seconds, `url` is `""`, `throttled_at` is null, and `installation_id` is null for an `installation` event, as on GitHub. `status` and `status_code` are, as GitHub gives them: `OK` and the status for a `2xx` answer; the status and a space (`500 `, `502 `) for those two; `Invalid HTTP Response: {status}` and the status for any other; `failed to connect to host` and `502` for an attempt that reached no receiver; and `timed out` and `504` for one that ran out of time. `per_page` is 30 unless given, at most 100, and digits alone: anything else is `422` "Invalid request.\n\n{value} does not match /^\\d+$/." `status` keeps `success`, an answer with a status below 400, or `failure`, any other attempt, and any other value is `422` "Invalid request.\n\n{value} is not a member of [\"failure\", \"success\"]." A page that is not the last sends a `next` link, `<…?{the request's parameters}&cursor=v1_{id}>; rel="next"`, where the id is the next page's first delivery, and that cursor starts a page at that delivery. A page reached by a cursor also sends a `prev` link, after the `next`, whose cursor is `v1_p{id}l{per_page}` with the id of the page's own first delivery: such a cursor starts a page that many deliveries before that one, at most 100, and an empty page with no link when none is before it. A cursor of any other form is `422` "Error parsing cursor parameter", and any other parameter is `422` "Invalid request.\n\n\"{name}\" is not a permitted key." |
| `POST /app/hook/deliveries/{delivery_id}/attempts` | `202` `{}`. The delivery is sent again as a new delivery with the same GUID, event, and body, and `redelivery` true, to the URL the webhook has now. An id that is not one of the App's deliveries is `404`, as is one whose attempt was never made, which the list does not hold either: a delivery still pending, or one a restore or a fork dropped. An id that is not an integer is `422` "The delivery_id parameter must be an integer." |

### 1.13 Dependabot alerts

A repository's vulnerable dependencies. GitHub finds them by scanning, which a world does not do, so an alert is made on the control plane, and so is every move GitHub itself makes of one (`usage.md`). What a person does, and every read, is here. What this section says GitHub does was observed on github.com on 2026-11-19, on a private repository holding manifests with known vulnerable dependencies.

**The switch.** A repository has alerts only while they are on, and one starts with them off, as a private repository does on GitHub.

| Route | Behavior |
| --- | --- |
| `GET /repos/{o}/{r}/vulnerability-alerts` | `204` while alerts are on, and `404` "Vulnerability alerts are disabled." while they are off. |
| `PUT /repos/{o}/{r}/vulnerability-alerts` | Turns them on: `204`, and `204` again. |
| `DELETE /repos/{o}/{r}/vulnerability-alerts` | Turns them off: `204`, and `204` again. The repository's alerts are kept, and are read again, under the same numbers, once alerts are on, as GitHub's were. |

While alerts are off, every route below is `403` "Dependabot alerts are disabled for this repository.", which is how a caller tells a repository with no alerts to read from a credential that may not read them.

A repository's alerts are not public, even where its code is. An installation token reads them, and whether they are on, only where its installation covers the repository: anywhere else it is `403` "Resource not accessible by integration", by the repository's name or by its id.

**The alert object.** `number` (from 1 within the repository), `state` (`open`, `dismissed`, `fixed`, or `auto_dismissed`), `dependency` (`package: {ecosystem, name}`, `manifest_path`, `version_requirement`, `scope`, `relationship`), `security_advisory`, `security_vulnerability` (`package`, `severity`, `vulnerable_version_range`, and `first_patched_version`, `{identifier}` or null), `url`, `html_url`, `created_at`, `updated_at`, `dismissed_at`, `dismissed_by` (a user), `dismissed_reason`, `dismissed_comment`, `fixed_at`, `auto_dismissed_at`, and, as GitHub sends them for an alert nobody has acted on, `dismissal_request` (null), `assignees` (`[]`), and `reachability` (`{verdict: "disabled"}`). `severity` is `low`, `medium`, `high`, or `critical`.

A world keeps what it was given of the advisory: its `ghsa_id`, `cve_id`, `summary`, `description`, and `severity`. The rest of `security_advisory` is rendered from that: `identifiers` are the two ids, `vulnerabilities` is the alert's one, `published_at` and `updated_at` are the alert's `created_at`, `withdrawn_at` and `epss` are null, `references` and `cwes` are empty, every score is `{vector_string: null, score: 0.0}`, and `classification` is `general`. `relationship` is always `unknown`, which is what GitHub gives a dependency it cannot place.

| Route | Behavior |
| --- | --- |
| `GET /repos/{o}/{r}/dependabot/alerts` | The alerts, newest first, 30 to a page. Filters, each a comma-separated list: `state` and `scope` (`runtime`, `development`), in any case, where a value GitHub does not know matches no alert; `severity` and `ecosystem`, where one it does not know is no filter, and an ecosystem matches in any case; and `package` and `manifest`, matched exactly. `sort` is `created` (the default) or `updated`, and `direction` is `desc` (the default) or `asc`; any other value of either is the default, as on GitHub. `per_page` is 0 to 100: more is `400` ":per_page must be between 0 and 100", and a value that is not a number is 0. `page` is `400` "Pagination using the `page` parameter is not supported." Any other parameter is ignored, `first` and `last` among them, except what a world keeps nothing for, which GitHub applies and a world refuses as not mocked: the filters `epss_percentage`, `has`, and `assignee`, and `sort=epss_percentage`. |
| `GET /repositories/{id}/dependabot/alerts` | The same list, at the path its `Link` header names. |
| `GET /repos/{o}/{r}/dependabot/alerts/{alert_number}` | The alert. A number no alert has is `404` "No alert found for alert number {n}", and one that is not an integer is `422` "The alert_number parameter must be an integer." |
| `PATCH /repos/{o}/{r}/dependabot/alerts/{alert_number}` | A person dismisses an open alert (`{state: "dismissed", dismissed_reason, dismissed_comment?}`) or reopens a dismissed one (`{state: "open"}`), and is answered the alert. `dismissed_reason` is `fix_started`, `inaccurate`, `no_bandwidth`, `not_used`, or `tolerable_risk`. The refusals are below. |

**Paging.** A page that is not the last sends `Link: <{base}/api/v3/repositories/{id}/dependabot/alerts?{the request's parameters}&after={cursor}>; rel="next"`, and a page with alerts ahead of it sends, after that, the same with `before={cursor}` and `rel="prev"`. A cursor is opaque. One that is not a cursor is `400` ":after does not appear to be a valid cursor" (or ":before"). A page of size 0 is empty and names itself as the next, without a cursor, as GitHub's does.

**`ETag`.** The list answers a strong `ETag`, and a request that sends it back as `If-None-Match` is `304` with no body while the page would be the same. The tag covers the page's links with its alerts, so a page that gains a next page is not the same, though its own alerts are: a caller told otherwise would never read the page it gained.

**What `PATCH` refuses,** the body's faults first, each `422` "Invalid request.\n\n" and then:

- a body that is not an object: "Invalid input: `{value}` is not of type `object`.";
- no `state`: "Invalid input: data matches no possible input. See `documentation_url`.";
- a key GitHub does not take: that, and "\nInvalid input: \"{key}\" is not a permitted key.";
- a value that is not a string: "Invalid property /{key}: `{value}` is not of type `string`.", or, for a null, "Invalid property /{key}: data cannot be null.";
- another `state`: "Invalid property /state: `{value}` is not a possible value. Must be one of the following: dismissed, open.", and likewise for `dismissed_reason`, naming its five values;
- a `dismissed_comment` of more than 280 characters: "Invalid property /dismissed_comment: Only 280 characters are allowed; {n} were supplied."

GitHub also takes `assignees` here, which a world keeps none of and refuses as not mocked. Then:

- dismissing with no reason is `400` "Setting an alert to 'dismissed' requires a 'dismissed_reason'";
- reopening with one is `400` "Can't set 'dismissed_reason' when setting an alert to 'open'";
- an alert already there is `409` "Alert {n} is already open", or "is already dismissed";
- a fixed alert is `409` "Fixed alerts cannot be dismissed or reopened".

## 2. Web pages

The pages a person's browser is sent to, on the web host, without an `Authorization` header. A world has no browser sessions (rule 12), so each is Orrery's stand-in for GitHub's page, and its form signs the person in.

**Signing in.** A form carries `login`, a user's login. A world set to ask for one (`page_sign_in = "token"`; `usage.md`, "Door two: the control plane") shows a `password` field too, which must be a token the control plane issued that user: any other value signs in nobody, a token an app was given for them included. A form that signs in nobody is answered with the page again, `200`, and "Incorrect username or password.", and changes nothing. A page shown again keeps the login and never the token. Below, "signed in" means this.

### 2.1 OAuth web flow

GitHub's OAuth web flow. An OAuth app is registered on the control plane (`POST /_orrery/oauth-apps`). A GitHub App is an OAuth app too (§1.11): the flow takes its client id, a `redirect_uri` may match any of its callback URLs, and the first of them is the default.

| Route | Behavior |
| --- | --- |
| `GET /login/oauth/authorize` | `client_id, redirect_uri?, scope?, state?`. An unknown `client_id` is a `404` page. `redirect_uri` defaults to the app's callback; when given, its scheme, host, and port must equal the callback's and its path must be the callback's or below it, else a `302` to the callback with `error=redirect_uri_mismatch`, `error_description`, `error_uri`, and `state`. GitHub's sub-domain allowance and loopback-port exception are not modeled. Answers Orrery's stand-in for GitHub's sign-in and consent pages: a form asking for a login. |
| `POST /login/oauth/authorize` | The same fields as a form, signed in. Success is a `302` to `redirect_uri` with `code` and `state`. |
| `POST /login/oauth/access_token` | `client_id, client_secret, code, redirect_uri?` from the query, a form body, or a JSON body; the client pair may instead be HTTP Basic. Success is `access_token` (a new token for the actor who authorized), `token_type=bearer`, and `scope`, form-encoded unless `Accept: application/json`; `application/xml` is not mocked. Refusals are `200` with `error, error_description, error_uri`: `incorrect_client_credentials`, `bad_verification_code` (unknown, spent, or older than 10 minutes on the world clock), and `redirect_uri_mismatch` (a `redirect_uri` that differs from the one the code was issued for). A code is spent by its first successful exchange. |

Scopes are recorded and echoed, not enforced: no route checks a token's scopes. The token reads the identity routes in §1.1.

### 2.2 Registering and installing a GitHub App

**From a manifest.** A caller sends a person's browser to GitHub with the App it wants as a manifest, and is handed back a code it trades for the App's credentials.

| Route | Behavior |
| --- | --- |
| `POST /settings/apps/new?state=`, `POST /organizations/{org}/settings/apps/new?state=` | A form with one field, `manifest`, the App as JSON. Answers the page a person confirms on, which registers nothing: a form holding the manifest, the App's name, and the sign-in. Posting that form signed in registers the App, owned by that user or by `{org}`, and is a `302` to the manifest's `redirect_url` with `code` and, when the URL carried one, `state`. With no `redirect_url` it answers a page. An `{org}` that is not an organization is a `404` page. The page is shown again with `422` for a name that is blank ("Name can't be blank"), longer than 34 characters ("Name is too long (maximum is 34 characters)"), or taken ("Name is already in use"), for a manifest with no `url` ("Url can't be blank"), for a `redirect_url` that is not an `http` or `https` URL a redirect can carry, for `request_oauth_on_install` with no `callback_urls`, and for anything the control plane refuses of an App. A refused manifest registers nothing. A `manifest` that is not JSON is a `422` page. |
| `POST /app-manifests/{code}/conversions` | No credential but the code, and no body. `201` with the App as `GET /app` answers it, without `installations_count`, plus `client_secret`, `webhook_secret`, and `pem`, the App's private key as PKCS#1 PEM (`BEGIN RSA PRIVATE KEY`), 2048 bits. A code is traded once, within an hour of the world's clock: a code that is unknown, traded, or older is `404` "Not Found". |

The manifest's fields: `name`, `url`, `redirect_url`, `callback_urls`, `setup_url`, `request_oauth_on_install`, `default_permissions`, `default_events`, and `hook_attributes` (`url`, and `active`, true unless given). The App's slug is its name in lower case with every run of other characters a hyphen. `description`, `public`, and `setup_on_update` are accepted and kept nowhere. The App is registered when the person confirms, by them through the `web` door, and it is the App `POST /_orrery/apps` registers. It has a webhook secret whether or not its webhook is active, as on GitHub.

The private key is derived from the world's secret when the code is traded, and the world keeps its public half as the App's first key: a world never holds a private key, and the same world gives the same key on replay. A fork derives a code it inherited again, and a code traded in the fork gives the fork's key. A fork under another secret also derives again the key a code traded in its source gave (`usage.md`, "Making worlds without serving them").

**Installing.** A caller links a person to the App's install page.

| Route | Behavior |
| --- | --- |
| `GET /apps/{slug}/installations/new?state=` | The install page: a form with the sign-in, the account to install on (the person's own unless named), all of the account's repositories or the ones named, and whether to install or only ask to. An unknown App is a `404` page. |
| `GET /apps/{slug}/installations/new/permissions?target_id=&state=` | The same page with the account chosen: `target_id` is a user's or an organization's id, and any other is a `404` page. |
| `POST` to either | Signed in, installs the App on the account, by the person through the `web` door, and sends them on (below). An account or a repository that does not exist shows it again with `422`. An account that already has the App is a `302` to its installation's page. |
| `GET /settings/installations/{id}`, `GET /organizations/{org}/settings/installations/{id}` | A page naming the installation, at the path its `html_url` gives. Any other id is a `404` page. |

Where a person is sent once they have installed the App, each a `302`:

- An App with `request_oauth_on_install` sends them to its first callback URL with `code`, an authorization code for them as the OAuth flow issues one (§2.1), then `installation_id`, `setup_action=install`, and the `state` the install URL carried.
- An App with a `setup_url` sends them there with `installation_id`, `setup_action=install`, and `state`.
- Any other App sends them to the installation's page.

`setup_action=request` on the form is what GitHub does for a person who may not install on the account: nothing is installed, and they are sent on the same way with `setup_action=request` and no `installation_id`. An App that asks for authorization still gets a `code`. With neither a callback nor a setup URL to go to, the answer is a page.

## 3. GraphQL

### 3.1 Requests and errors

`POST /api/graphql` with a token, taking `{query, variables?, operationName?}`. A body that isn't one such object is `400` "Problems parsing JSON"; everything else is `200` with `data` and `errors`. A field resolver's error is a partial result: its parent is null, the error carries a `path`, and `data` holds the rest, as on GitHub. Introspection is on. The root types are named `QueryRoot` and `MutationRoot`.

A selection naming a field, type, or argument Orrery lacks fails the whole document. Its message starts `orrery (not mocked):` and carries `extensions.orrery.not_mocked: true`, so it can't be mistaken for GitHub's own validation error.

### 3.2 Root fields and mutations

- `node(id:)` and `nodes(ids:)`: a pull request (`PullRequest`) or a review (`PullRequestReview`). Any other id is `null`.
- `repository(owner:, name:)` and `organization(login:)`: `null` when there is none.
- `search(query:, type: ISSUE, first:)`: pull requests, newest first, as `{pageInfo, nodes}`. The query takes `is:pr`, `is:issue` (matches nothing), `is:open`, `is:closed`, `is:merged`, `is:unmerged`, `author:`, `reviewed-by:`, `repo:` (repeatable, any of), `merged:>=`, and `closed:>=` (a date or a timestamp; a merge is a close). Values compare ignoring case. Any other qualifier fails the field as not mocked, because answering a query Orrery half understands would return wrong results.
- `rateLimit`: always `null`, as a GitHub Enterprise Server with rate limiting off answers (`grammar.md`, "Rate limits").
- Mutations: `markPullRequestReadyForReview` and `convertPullRequestToDraft`, each taking `input: {pullRequestId, clientMutationId?}` and answering `{pullRequest, clientMutationId}`. A pull request that is closed, or already in the requested state, is an error.

An installation token (§1.11) sees the repositories it reaches and every public one. A private repository it does not reach reads as one that does not exist: `repository` is `null`, a `node` in it is `null`, a search leaves its pull requests out, and a mutation on one of them is the error of an id that does not resolve. A mutation on a pull request in a public repository it does not reach is an error with the message "Resource not accessible by integration".

### 3.3 Types

| Type | Fields |
| --- | --- |
| `PullRequest` | `id, number, title, body, author, state, isDraft, merged, mergedBy, mergeable, headRefName, headRefOid, baseRefName, url, repository, headRepository, additions, deletions, changedFiles, files, reviewRequests, latestReviews, reviews, commits, labels, comments, createdAt, updatedAt, mergedAt, closedAt, timelineItems` |
| `PullRequestReview` | `id, fullDatabaseId, author, state, submittedAt, body, comments` (always empty) |
| `IssueComment` | `fullDatabaseId, author, body, createdAt` |
| `User`, `Bot` | `login, databaseId`. `Bot` carries the bare slug. An organization authoring something renders as `User`. |
| `Organization` | `databaseId, login, teams(first, after, userLogins)` |
| `Team` | `databaseId, slug, name, organization, members { totalCount }` |
| `Repository` | `nameWithOwner, ref(qualifiedName:)` (`Ref { id }`, or null), `vulnerabilityAlerts(states, first, after)` |
| `RepositoryVulnerabilityAlert` | `number, dependabotUpdate` (`{pullRequest, error: {errorType, title}}`, or null) |
| `Commit` | `oid, committedDate, statusCheckRollup` (null with no check runs, else `contexts`, whose only member is `CheckRun`) |
| `CheckRun` | `databaseId, name, status, conclusion, completedAt, detailsUrl, checkSuite`. A rollup's contexts hold the newest run of each name in each suite, as GitHub's do. |
| `CheckSuite` | `databaseId, status, conclusion, workflowRun` (always null, as for a check GitHub Actions did not make) |

`databaseId` on `User`, `Bot`, `Team`, and `Organization` is the id REST gives the same account or team. `fullDatabaseId` is GitHub's `BigInt`, a string on the wire. A pull request's `body` is `""` when unset, as GitHub's non-null `String!`.

- `mergeable` is REST's `mergeable` (§1.0) in GraphQL's terms, from the same decision: `MERGEABLE` for `true`, `CONFLICTING` for `false`, and `UNKNOWN` for null. Reading it is a read that carries mergeability, so one query starts a test merge for each pull request it reads `mergeable` on whose last one is stale, and answers `UNKNOWN` for each of them.
- `additions`, `deletions`, and `changedFiles` come from the same diff as REST pull request files, and a rename counts once. So does a type change, which REST lists as two files. `files` lists `{path, changeType}` in path order; `changeType` is `ADDED`, `DELETED`, `RENAMED`, `COPIED`, `MODIFIED`, or `CHANGED`, and there is no previous path, as on GitHub.
- `vulnerabilityAlerts` lists a repository's Dependabot alerts (§1.13), oldest first, as `{pageInfo, nodes}`; `states` keeps the alerts in any of `OPEN`, `FIXED`, `DISMISSED`, and `AUTO_DISMISSED`. A repository whose alerts are off lists none, as on GitHub, and so does one an installation token does not reach. `dependabotUpdate` is what Dependabot's updater did about the alert, as the control plane recorded it: null for nothing, both fields null for an attempt that has started, `pullRequest` for the pull request it opened, and `error` for what it reported.
- `reviewRequests` lists pending requests, users then teams, each in request order. A request's `requestedReviewer` is a `User` or a `Team`.
- `latestReviews` is each author's newest review. `reviews` takes `states`, and its `totalCount` counts the filtered set.
- `commits(last:)` has one node, the head commit.
- `labels` are ordered by name, ignoring case.
- `mergedBy` is the actor who merged, typed like an author, and null until a merge.
- `timelineItems` takes `itemTypes` and lists, oldest first: `LabeledEvent` and `UnlabeledEvent` from the label routes, `ReviewRequestedEvent` and `ReviewRequestRemovedEvent` from the review-request routes, `ReadyForReviewEvent` from the mutation, `ClosedEvent` (with its `actor`) from a close or a merge, since GitHub records a close beside every merge, and `HeadRefDeletedEvent` (with `actor` and `headRefName`), `BaseRefDeletedEvent` (with `actor` and `baseRefName`), and `HeadRefRestoredEvent` (with `actor`) from a push that deletes or restores a branch (`surface-git.md`, "Deleting and restoring a branch"). A request whose team has since been deleted keeps its item with a null `requestedReviewer`, even after a new team takes the slug. Reopening a pull request and converting it to a draft add no item.

### 3.4 Connections

A connection takes exactly one of `first` and `last`, from 0 to 100, and refuses otherwise with GitHub's messages: "You must provide a `first` or `last` value to properly paginate the `X` connection.", "Passing both `first` and `last` to paginate the `X` connection is not supported.", and "Requesting N records on the `X` connection exceeds the `first` limit of 100 records." `first` pages forward with `after`, and `last` backward with `before`, where the connection takes them: `reviews`, `comments`, and `timelineItems` take `before`; `files` and `teams` take `after`. A cursor Orrery didn't issue is refused. `commits(last:)` and a review's `comments(first:)` are not bounded.

## 4. Behaviors Orrery chose

Where GitHub's behavior has not been observed, or Orrery deliberately differs, this is what Orrery does. Each is a candidate to check against GitHub when a consumer depends on it.

**Not yet observed on GitHub:**

- A team member's review leaves the team's pending request in place.
- Of Dependabot alerts (§1.13): the `reintroduced`, `auto_dismissed`, and `auto_reopened` deliveries, which follow GitHub's documentation, where `created`, `dismissed`, `reopened`, and `fixed` were observed; whether a person can reopen an alert a rule dismissed, which a world allows; and what GitHub answers an installation token for a repository it is not installed on, which a world answers as it answers that token anything else it may not do. GitHub marked every open alert fixed, with a `fixed` delivery each, when alerts were turned off, and had them open again once they were on: a world leaves them as they are and sends nothing.
- Everything a browser session is needed to see of §2.2. Observed on github.com: the manifest form's redirect carries `code` and `state` and nothing else; a conversion is `201`, a second is `404`, and so is an unknown code; the conversion's fields, its PKCS#1 key, and its webhook secret for an inactive webhook; `GET /user/installations` refused to a token no App was given; and the membership read. Not observed: the parameters an install redirects with (`installation_id`, `setup_action`, and `code`), which follow GitHub's documentation; what a request to install redirects with; what the manifest page answers a manifest it refuses, and its messages, including whether GitHub refuses a manifest with no `hook_attributes` or with a permission or an event it does not know, each of which a world accepts; an expired code; `GET /user/installations` with a token an App was given, including which installations GitHub counts as reached, and with an installation token.
- The `pull_request` payload's `requested_team`, which follows GitHub's documentation. Every other payload a world sends was observed.
- A team that is not in the organization that owns the repository is refused with the message GitHub gave for a team asked on a user's repository.
- GitHub sent `synchronize` a second time, with the same `before` and `after`, when a pull request's base was changed just after a push to its head, and not when the base was changed back. A world sends it once, for the push.
- The list's `status` for a receiver that answers `504`, which a world lists as `Invalid HTTP Response: 504`.
- Whether a push commit's `timestamp` is the date it was committed, which is what a world sends, or the date it was written. The two were equal in every commit observed.
- Adding a user who is not a member of the organization to a team is a `422` with Orrery's own message.
- Deleting a team with a pending request does not move the pull request's `updated_at`.
- A review request on a merged pull request is accepted, as GitHub accepted one on a closed pull request.
- `DELETE …/teams/{team_slug}/memberships/{username}` for a user not on the team is `204`; GitHub documents only `204` and `403`.
- Team slugs keep ASCII letters and digits only, so "Équipe" becomes `quipe` and "日本" has no slug (`422 invalid` on `name`), where GitHub may transliterate. Two names with one slug ("Café" and "Caf") collide as `already_exists` on `name`. No length limit is applied.
- An id `node` or `nodes` can't resolve is `null` with no entry in `errors`.
- A merge into a pull request's head branch leaves its test merge stale, as a push to its head does.
- A closed pull request whose test merge conflicts reads as an open one does. Every closed pull request observed had a clean test merge.
- A closed pull request whose head branch is deleted before its base branch moves has no mergeability once the base moves. It was observed only with the base moving first, or in the same push.
- One push that deletes several branches, or a branch that is the head of some pull requests and the base of others, handles the branches in ref-name order, and each branch's head pull requests before its base pull requests.
- The message for a negative `first` or `last` on a GraphQL connection: "`first` on the `X` connection cannot be less than zero."
- The order of the verdict's checks when a commit has more than one fault, and `bad_email` for an SSH-signed commit whose committer address only accounts other than the key's hold.
- GitHub storing a passing verdict at push and reaching a failing one again on every read.
- A signing key one account holds is refused to every other account. The messages for a key already in use and for one that is not a key, and a `title` left out reading null.
- `GET …/commits/{ref}` for a `ref` that names no commit is `422` "No commit found for SHA: {ref}".
- The fields `GET /meta` answers on a GitHub Enterprise Server 3.17, which are from GitHub's published description of that release's REST API, not from a server's answer.
- How a type change counts toward a pull request diff's 300 files, and in GraphQL's `changedFiles` and `files`. Orrery counts it as the two files REST lists toward the 300, and as one `CHANGED` file in GraphQL.
- The 20,000-line limit on a page of a pull request's files. It was observed on a compare, where it also leaves out every file after the one that crosses it, and the 1 MiB limit was observed on both. Whether a response holding exactly 20,000 lines keeps the file that reaches it.
- Whether the patch-size and response limits count bytes or characters: every patch observed was ASCII. Orrery counts the bytes of the patch it serves, after replacing invalid UTF-8.
- Limits on a diff past those observed. GitHub served a pull request's diff of 87,234 lines and 2.9 MB, and a compare's of 3,985 files, 1.16 million changed lines, and 49.6 MB.

**Deliberate differences:**

- There are no permissions. Any authenticated actor may read any repository, create repositories in any organization, label, request reviews, review, and merge. Any user may be asked for a review, where GitHub refuses one who is not a collaborator on the repository. The one limit is the repositories an installation token reaches (§1.11). An App's, an installation's, and a token's `permissions` are recorded and echoed, and no route checks them.
- A `manual` world compares neither `iat` nor `exp` of an App's JWT to its clock, so it accepts one that has expired, and one that expires more than 10 minutes ahead (`usage.md`, "Time").
- `repository(owner:, name:)` for a repository that does not exist, or that an installation token does not see, is `null` with no error. GitHub adds an error whose `type` is `NOT_FOUND`: "Could not resolve to a Repository with the name '{owner}/{name}'."
- A bot's own token, the one its actor was created with, reaches every repository. Only an installation token is limited.
- An App registered from a manifest that names no callback URL is given its `redirect_url`, or else its `url`, as one, since an App here holds at least one. A manifest that asks for authorization on install must name a callback URL of its own, because that is where the person is sent with a code. The manifest page's refusals for a `redirect_url` and a missing callback URL are Orrery's words. A request to install records nothing, where GitHub keeps it for an owner to approve. An account that already has the App is shown its installation, where GitHub offers to change it. An App takes a setup URL or authorization on install, not both, which is what GitHub's settings page allows.
- An App holds at least one callback URL, where GitHub lets an App have none. Its `description` is null and its `external_url` is its `html_url`, since Orrery keeps neither.
- `repository_ids` on a request for an installation token is not mocked, where GitHub narrows the token by it. `GET /app/installations` refuses `since` and `outdated` as not mocked (`422`), where GitHub filters the list by them.
- A path under `/app` that Orrery does not serve is `501` to a valid JWT, where GitHub answers a path it does not serve `401` "Bad credentials".
- GitHub's error bodies in §1.11 carry `documentation_url` and `status` beside `message`. Orrery's carry `message`, as its other routes' do.
- A webhook's deliveries (§1.12) differ from GitHub's in these ways. A world sends `check_suite` only as `completed`: GitHub also sends `requested` to an App with write access to checks when a push makes its suite, and `rerequested`. A suite's `head_commit` is absent. It has no inline review comments, so it sends no `pull_request_review_comment`. An installation's `created_at` ends in `.000Z` in every payload, where GitHub's `created` payload gave it with the installer's own offset from UTC (`…T22:19:33.000-06:00`). It sends no `ping`, `create`, `delete`, `label`, or `installation_repositories` event, no `installation` event for accepted permissions, and no `issue_comment` or `pull_request_review` action but `created` and `submitted`. GitHub's delivery ids are large numbers, and a world's count from 1. GitHub's GUIDs are time-based UUIDs, and a world's are derived. An App receives every App's check runs. The objects in a payload carry the fields REST answers (§1.0), which are fewer than GitHub's: a pull request, for one, has no `commits`, `additions`, `deletions`, `changed_files`, `comments`, or `_links`.
- A redelivery asked of an App whose webhook is inactive is `422` with an Orrery message, since an inactive webhook sends nothing (`usage.md`, "What a world sends").
- `PATCH /app/hook/config` matches a URL's scheme in lower case, where GitHub took `HTTPS://`. GitHub stores any `content_type` and `insecure_ssl` it is sent; a world refuses the values it does not act on. What GitHub answers a `secret` was not observed.
- An installation token is `orrfake_` and 32 hex digits. GitHub's is `ghs_` and three segments joined by dots, 383 characters when observed.
- An installation token that creates a repository for its own account (`POST /user/repos`) is `403` "Resource not accessible by integration", as on the other routes that refuse a bot. GitHub answered that call `403` "Rate Limit Exceeded".
- An installation's `suspended_at` and `suspended_by` are always null: nothing suspends one yet (§5).
- An installation token's `expires_at` is on the world's clock. In a `manual` world that clock is not the caller's: a client that compares `expires_at` to its own clock sees a token from a world in the past as expired and asks for another before every call, and a token outlives its hour for a client that waits, until the clock is advanced. A world that such a client talks to is `real`, or is created with its clock at the present.
- A repository name outside `[A-Za-z0-9._-]` is refused rather than slugified.
- Repositories are never renamed or deleted, so nothing redirects, and `GET /repositories/{repository_id}` never answers under a later name.
- A file's `content` is not line-wrapped, and a directory's contents are `404`, not a listing.
- `GET /user/repos` sorts by `pushed` by default (GitHub: `full_name`) and lists only the caller's own repositories.
- A bot that has no App is installed nowhere, so no push makes it a suite: its suite comes from its first check run on a commit, records no push, and names `ghost` as the sender of its deliveries. A suite made by the migration that introduced suites, for a check run a world already held, records no push either.
- Whether GitHub makes a suite at a push for an App that may only read checks, or for a suspended installation: a world makes none for either. And what a suite of only `neutral` check runs comes to, which a world answers `neutral`.
- A check run's `output` is stored and answered as sent, so it has no `annotations_count` or `annotations_url` unless the creator sent them, and `GET …/check-runs/{id}/annotations` is not mocked.
- Only a bot creates or updates a check run, and a check run's `head_sha` and a review's `commit_id` are not checked against the repository. GitHub refuses a check run on a commit the repository does not hold with `422` "No commit found for SHA: {sha}", and refuses a person's token with `403` "You must authenticate via a GitHub App.".
- A test merge finishes by the next read, not after a time. GitHub takes a second or two, so a client that reads again sooner than that gets null more than once on GitHub and once on Orrery. A test merge that a write started has finished by the first read after it, where a read made at once on GitHub is still null.
- For about a second after a push returns, GitHub still answers from the test merge the push left stale. Orrery answers null from the first read.
- In a repository with an Actions workflow on `pull_request`, a push to a head branch starts a test merge with no read. Orrery has no Actions, so it starts none.
- `merge_commit_sha` is null until a pull request is merged. GitHub's names the test merge commit, which a test merge after a move of the head replaces, and is null while the test merge conflicts. A test merge after a move of the base was observed to keep a commit made minutes earlier and to replace one made more than 12 hours earlier.
- REST `mergeable_state` reads no check runs and no branch protection, so it is never `unstable`, `blocked`, or `behind`. `rebaseable` is the test merge's answer, not a replay of the head's commits one at a time, so a head whose merge is clean but whose rebase would stop on a conflict reads `true`.
- A read of a repository's refs waits for a write under way (a push, a merge, opening a pull request, or an edit of one that sets its `state` or `base`), and sees the refs before it or after its record (`surface-git.md`, "What a fetch sees"). Two fields of one GraphQL query can still see two states.
- Labels belong to a pull request, not to a repository.
- A merge commit's author and committer address is `<login>@orrery.invalid`, not the actor's stored primary address.
- A merge commit Orrery makes is unsigned, so it reads `unsigned`. GitHub signs the merges it makes with its own key, and they read `valid`.
- Orrery parses no PGP signature, so a malformed one reads `unknown_key` where GitHub answers `malformed_signature`.
- A verdict a read reaches as `valid` has a null `verified_at`. GitHub stores the verdict then.
- `GET /meta` needs a token, as every REST route does, where github.com answers it without one (`grammar.md`).
- No response carries `X-RateLimit-*` headers, where github.com sends them on every response, and GraphQL `rateLimit` is `null`, which github.com never answers (`grammar.md`, "Rate limits").
- Setting a user's organization membership adds them at once and answers `state: active`. github.com sends an invitation and answers `pending` until the user accepts it, and a GitHub Enterprise Server adds them at once. Invitations are not modeled.
- A diff's `index` lines abbreviate object ids to 7 hex digits. GitHub's length grows with the repository, as git's default does (7 digits were observed on a small repository and 9 on a larger one), but git picks that length from how the repository's objects are packed, which a replayed world need not share.
- A hunk header's trailing context is the one git picks, by default the nearest line above the hunk that starts with a letter, `_`, or `$`. GitHub picks it by the file's language, so in an HTML file it names the nearest heading.
- `Accept` media types match ignoring case, and every `Accept` header counts. GitHub answers JSON to `application/vnd.github.v3.DIFF`, and reads only the first `Accept` header.
- A diff's `Content-Type` names the diff media type the request listed, where GitHub's repeats everything `Accept` lists.
- `…/pulls/{n}/files` refuses the diff media type, where GitHub answers JSON.

## 5. Not served

Every route here answers `501` until a consumer needs it:

- issues, and an issue's or pull request's REST timeline;
- reading a single comment or review, dismissing a review, review comment replies, reactions, and editing or deleting a comment;
- inline review comments (a review with `comments`, or `POST …/pulls/{n}/comments`);
- repository labels, branch protection, `GET …/branches/{branch}`, tags, and refs other than branches;
- listing commits, a commit's `files` and `stats`, trees, and statuses;
- GPG keys and SSH authentication keys (`/user/gpg_keys`, `/user/keys`), and a single signing key (`GET /user/ssh_signing_keys/{id}`);
- Actions: workflow runs, jobs, and logs;
- for GitHub Apps: one delivery with its request and response (`GET /app/hook/deliveries/{delivery_id}`), repository and organization webhooks (`/repos/{o}/{r}/hooks`, `/orgs/{org}/hooks`), the lookups of the installation on a repository, an organization, or a user, updating an installation from its page and the pages of an App's own settings, the installation requests an App has received (`GET /app/installation-requests`), changing an installation's repositories, `DELETE /installation/token`, and revoking or resetting a token or a grant (`/applications/{client_id}/…`);
- REST search, organization member listings, and `/rate_limit`.
