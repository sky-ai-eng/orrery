# GitHub surface

Everything the GitHub face serves, and how it behaves. Behavior every route shares (where the face is mounted, authentication, errors, pagination, conditional requests, and rate limits) is in `grammar.md`.

The surface grows when a consumer needs it (rule 1). A route not listed here answers `501` naming itself. A request field GitHub defines but Orrery does not implement is refused with a `422` naming the field, never ignored, because ignoring it would leave the caller's world silently different from what it asked for. Section 4 lists where Orrery chose a behavior it has not observed on GitHub, and section 5 what is not served.

## 1. REST

Every route is under `/api/v3` and needs a token.

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
| Check run | `id, node_id, name, head_sha, status, conclusion, external_id, details_url, url, html_url, started_at, completed_at, output, check_suite` (null), `app` (`slug` and `name`, the bot's bare login) |
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
| `PUT /orgs/{org}/memberships/{username}` | Body optional; `role` is `member` (the default) or `admin`, else `422`. Answers `{url, state, role, organization_url, organization, user}`. Orrery presents as a GitHub Enterprise Server, which adds a member directly, so `state` is always `active`. Only a user can be a member; an unknown organization or user is `404`. |
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
| `POST /repos/{o}/{r}/pulls/{n}/requested_reviewers` | `{reviewers?, team_reviewers?}`; neither, or both empty, is `422 missing_field`. Every name is checked before anything is written. The author is refused with GitHub's "Review cannot be requested from pull request author." A login that is not a user, or a team not in the organization that owns the repository, is GitHub's `422` "Reviews may only be requested from collaborators…". A pending reviewer is skipped. Answers `201` with the pull request. |
| `DELETE /repos/{o}/{r}/pulls/{n}/requested_reviewers` | `{reviewers, team_reviewers?}`, `reviewers` required and possibly empty. A name with no pending request is skipped. Answers `200` with the pull request. |
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
| `GET /repos/{o}/{r}/commits/{ref}/check-runs` | `{total_count, check_runs}` for a full sha or a branch, oldest first, paged; anything else is `422` "No commit found for ref". Every run on the commit is listed. `check_name`, `status`, `filter`, and `app_id` are ignored. |

Only a bot creates or updates check runs; any other actor gets a `403` naming the bot to use. On GitHub only an App can.

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

## 2. OAuth web flow

GitHub's OAuth web flow on the web host, without a token. An OAuth app is registered on the control plane (`POST /_orrery/oauth-apps`).

| Route | Behavior |
| --- | --- |
| `GET /login/oauth/authorize` | `client_id, redirect_uri?, scope?, state?`. An unknown `client_id` is a `404` page. `redirect_uri` defaults to the app's callback; when given, its scheme, host, and port must equal the callback's and its path must be the callback's or below it, else a `302` to the callback with `error=redirect_uri_mismatch`, `error_description`, `error_uri`, and `state`. GitHub's sub-domain allowance and loopback-port exception are not modeled. Answers Orrery's stand-in for GitHub's sign-in and consent pages: a form asking for a login. |
| `POST /login/oauth/authorize` | The same fields as a form, plus `login`, naming a user actor (rule 12: no passwords exist). Any other login re-renders the page with "Incorrect username or password." Success is a `302` to `redirect_uri` with `code` and `state`. |
| `POST /login/oauth/access_token` | `client_id, client_secret, code, redirect_uri?` from the query, a form body, or a JSON body; the client pair may instead be HTTP Basic. Success is `access_token` (a new token for the actor who authorized), `token_type=bearer`, and `scope`, form-encoded unless `Accept: application/json`; `application/xml` is not mocked. Refusals are `200` with `error, error_description, error_uri`: `incorrect_client_credentials`, `bad_verification_code` (unknown, spent, or older than 10 minutes on the world clock), and `redirect_uri_mismatch` (a `redirect_uri` that differs from the one the code was issued for). A code is spent by its first successful exchange. |

Scopes are recorded and echoed, not enforced: no route checks a token's scopes. The token reads the identity routes in §1.1.

## 3. GraphQL

### 3.1 Requests and errors

`POST /api/graphql` with a token, taking `{query, variables?, operationName?}`. A body that isn't one such object is `400` "Problems parsing JSON"; everything else is `200` with `data` and `errors`. A field resolver's error is a partial result: its parent is null, the error carries a `path`, and `data` holds the rest, as on GitHub. Introspection is on. The root types are named `QueryRoot` and `MutationRoot`.

A selection naming a field, type, or argument Orrery lacks fails the whole document. Its message starts `orrery (not mocked):` and carries `extensions.orrery.not_mocked: true`, so it can't be mistaken for GitHub's own validation error.

### 3.2 Root fields and mutations

- `node(id:)` and `nodes(ids:)`: a pull request (`PullRequest`) or a review (`PullRequestReview`). Any other id is `null`.
- `repository(owner:, name:)` and `organization(login:)`: `null` when there is none.
- `search(query:, type: ISSUE, first:)`: pull requests, newest first, as `{pageInfo, nodes}`. The query takes `is:pr`, `is:issue` (matches nothing), `is:open`, `is:closed`, `is:merged`, `is:unmerged`, `author:`, `reviewed-by:`, `repo:` (repeatable, any of), `merged:>=`, and `closed:>=` (a date or a timestamp; a merge is a close). Values compare ignoring case. Any other qualifier fails the field as not mocked, because answering a query Orrery half understands would return wrong results.
- `rateLimit`: always `null`, as a GitHub Enterprise Server with rate limiting off answers.
- Mutations: `markPullRequestReadyForReview` and `convertPullRequestToDraft`, each taking `input: {pullRequestId, clientMutationId?}` and answering `{pullRequest, clientMutationId}`. A pull request that is closed, or already in the requested state, is an error.

### 3.3 Types

| Type | Fields |
| --- | --- |
| `PullRequest` | `id, number, title, body, author, state, isDraft, merged, mergedBy, mergeable, headRefName, headRefOid, baseRefName, url, repository, headRepository, additions, deletions, changedFiles, files, reviewRequests, latestReviews, reviews, commits, labels, comments, createdAt, updatedAt, mergedAt, closedAt, timelineItems` |
| `PullRequestReview` | `id, fullDatabaseId, author, state, submittedAt, body, comments` (always empty) |
| `IssueComment` | `fullDatabaseId, author, body, createdAt` |
| `User`, `Bot` | `login, databaseId`. `Bot` carries the bare slug. An organization authoring something renders as `User`. |
| `Organization` | `databaseId, login, teams(first, after, userLogins)` |
| `Team` | `databaseId, slug, name, organization, members { totalCount }` |
| `Repository` | `nameWithOwner, ref(qualifiedName:)` (`Ref { id }`, or null) |
| `Commit` | `oid, committedDate, statusCheckRollup` (null with no check runs, else `contexts`, whose only member is `CheckRun`) |
| `CheckRun` | `databaseId, name, status, conclusion, completedAt, detailsUrl, checkSuite` (always null) |

`databaseId` on `User`, `Bot`, `Team`, and `Organization` is the id REST gives the same account or team. `fullDatabaseId` is GitHub's `BigInt`, a string on the wire. A pull request's `body` is `""` when unset, as GitHub's non-null `String!`.

- `mergeable` is REST's `mergeable` (§1.0) in GraphQL's terms, from the same decision: `MERGEABLE` for `true`, `CONFLICTING` for `false`, and `UNKNOWN` for null. Reading it is a read that carries mergeability, so one query starts a test merge for each pull request it reads `mergeable` on whose last one is stale, and answers `UNKNOWN` for each of them.
- `additions`, `deletions`, and `changedFiles` come from the same diff as REST pull request files, and a rename counts once. So does a type change, which REST lists as two files. `files` lists `{path, changeType}` in path order; `changeType` is `ADDED`, `DELETED`, `RENAMED`, `COPIED`, `MODIFIED`, or `CHANGED`, and there is no previous path, as on GitHub.
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
- Adding a user who is not a member of the organization to a team is a `422` with Orrery's own message.
- The `422` bodies for requesting a review from the pull request's author, and from someone who is not a collaborator.
- A review request and a withdrawal move the pull request's `updated_at`; deleting a team with a pending request does not.
- A review request on a closed or merged pull request is accepted.
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

- There are no permissions. Any authenticated actor may read any repository, create repositories in any organization, label, request reviews, review, and merge.
- A repository name outside `[A-Za-z0-9._-]` is refused rather than slugified.
- Repositories are never renamed or deleted, so nothing redirects, and `GET /repositories/{repository_id}` never answers under a later name.
- A file's `content` is not line-wrapped, and a directory's contents are `404`, not a listing.
- `GET /user/repos` sorts by `pushed` by default (GitHub: `full_name`) and lists only the caller's own repositories.
- `GET …/commits/{ref}/check-runs` lists every run on the commit; GitHub's default `filter=latest` keeps the newest run per name.
- Only a bot creates or updates a check run, and a check run's `head_sha` and a review's `commit_id` are not checked against the repository.
- A test merge finishes by the next read, not after a time. GitHub takes a second or two, so a client that reads again sooner than that gets null more than once on GitHub and once on Orrery. A test merge that a write started has finished by the first read after it, where a read made at once on GitHub is still null.
- For about a second after a push returns, GitHub still answers from the test merge the push left stale. Orrery answers null from the first read.
- In a repository with an Actions workflow on `pull_request`, a push to a head branch starts a test merge with no read. Orrery has no Actions, so it starts none.
- `merge_commit_sha` is null until a pull request is merged. GitHub's names the test merge commit, which a test merge after a move of the head replaces and one after a move of the base keeps, and is null while the test merge conflicts.
- REST `mergeable_state` reads no check runs and no branch protection, so it is never `unstable`, `blocked`, or `behind`. `rebaseable` is the test merge's answer, not a replay of the head's commits one at a time, so a head whose merge is clean but whose rebase would stop on a conflict reads `true`.
- A read of a repository's refs waits for a write under way (a push, a merge, opening a pull request, or an edit of one that sets its `state` or `base`), and sees the refs before it or after its record (`surface-git.md`, "What a fetch sees"). Two fields of one GraphQL query can still see two states.
- Labels belong to a pull request, not to a repository.
- A merge commit's author and committer address is `<login>@orrery.invalid`, not the actor's stored primary address.
- A merge commit Orrery makes is unsigned, so it reads `unsigned`. GitHub signs the merges it makes with its own key, and they read `valid`.
- Orrery parses no PGP signature, so a malformed one reads `unknown_key` where GitHub answers `malformed_signature`.
- A verdict a read reaches as `valid` has a null `verified_at`. GitHub stores the verdict then.
- `GET /meta` needs a token, as every REST route does, where github.com answers it without one (`grammar.md`).
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
- GitHub Apps and installations (`/app`, `/app/installations`, installation tokens), and webhook delivery;
- REST search, organization member listings, and `/rate_limit`.
