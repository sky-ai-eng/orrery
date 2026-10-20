# GitHub surface

Extracted at TF commit `78c8bfd2` (2026-08-21). File:line references point into the TF tree at that commit. See `grammar.md` for headers/auth/rate-limit/pagination behavior shared across these calls.

Three producer classes reach GitHub, over three transports:

| Producer | Process | Transport |
| --- | --- | --- |
| TF Go code (poller, tracker, reconcile, profiling, server handlers) | orchestrator | `internal/github.Client` — REST + GraphQL, org credential |
| `tfac exec gh` verbs | agent → agenthost daemon → host client | REST only (GraphQL fails closed through the sidecar proxy client, `internal/github/client.go:472-481`) |
| Real `gh` CLI | agent → per-run TLS injector (`internal/ghinjector`) → GitHub | REST + GraphQL, agent-composed |

Base URLs: `https://api.github.com` / GHES `{host}/api/v3` / data-residency `api.<tenant>.ghe.com`; GraphQL at `/graphql` resp. `{host}/api/graphql` (NOT `/api/v3/graphql`). The gh injector presents a GHE-shaped host, so agent traffic arrives as `/api/v3/*` and `/api/graphql`, with `/api/v3` stripped before forwarding.

---

## 1. REST read catalog

Each entry: consumers → params → response fields actually read → pagination/conditional → cadence → credential class. "Fields read" is the consumption contract — the mock may emit more, but these must be right.

### 1.1 `GET /repos/{owner}/{repo}/pulls` — core discovery ⭐

- **Consumers:** tracker Phase 1a (`internal/tracker/tracker.go:767` via `pulls.go:895,920`); reconcile gh-channel PR backstop (`internal/reconcile/reconcile.go:133`, always `etag=""`).
- **Params:** `state=open&per_page=100&page=N`. No sort param — GitHub's default order (created desc) is load-bearing for the ETag fast path.
- **Fields read** (`restPR` → `toDiscoverySnapshot`, `pulls.go:840,963`): `number`, `node_id`, `title`, `state` (upper-cased), `draft`, `html_url`, `created_at`, `updated_at`, `user.login`, `head.sha`, `head.ref`, `head.repo.full_name`, `base.ref`, `requested_reviewers[].login`, `requested_teams[].slug` (rendered `{owner}/{slug}`), `labels[].name`. `base.repo.full_name` is decoded but never read.
- **Conditional:** the ONLY ETag consumer in the poll path. `If-None-Match` on page 1 only (`client.go:284-316`); etag stored in `repositories.pulls_etag`. On 304: stored snapshot fed back as fresh, repo enters the quiet set, suppressing both discovery and the GraphQL refresh for that repo. 304s don't consume rate budget.
- **Pagination:** pages ≥2 unconditional while `len%100==0`, cap 10 pages (1,000 PRs) with WARN.
- **Cadence:** once per tracked repo per poll cycle, errgroup width `TF_POLL_REPO_CONCURRENCY` (default 4, clamp [1,16]).
- **Credential:** either. 403/404 → "repo unreachable, skip"; `ErrRateLimited` pins the round-robin resume cursor.

### 1.2 `GET /repos/{owner}/{repo}/pulls/{number}` — single PR object

Multiple consumers with different field consumption:
- **Stub enrichment** (`tracker.go:650` via `GetPRBasic`, `pulls.go:74`): decoded to map via `prViewFromRaw` (`pulls.go:88`) but tracker consumes only `node_id`, `merged`, `state`. Fires once per snapshot-less stub entity per cycle, forever, until resolved; 404 deliberately NOT terminal.
- **`gh pr view` / delegate run setup** (`GetPR`, `pulls.go:130-207`): full set — `number, node_id, title, body, state, merged, additions, deletions, changed_files, html_url, created_at, updated_at, user.login, head.{ref,sha,repo.clone_url,repo.ssh_url}, base.{ref,sha,repo.clone_url,repo.ssh_url}`. The delegate consumes the clone/ssh URLs + refs.
- **prskeleton header** (`internal/prskeleton/fetch.go:64`): `title, state, draft, merged, created_at, updated_at, user.login, head.ref, head.sha, base.ref, mergeable_state, additions, deletions, changed_files, commits, comments, review_comments`.
- **Live status** (`GetPRStatus`, `status.go:44`, `GET /api/dashboard/prs/.../status`): `mergeable`, `auto_merge` (presence), `mergeable_state`, `head.sha`.
- **Artifact overlays** (`artifacts_handler.go:240,320,581`): `title`, `body` — fires on every artifact GET/PATCH/approve.
- **Claim guards & freshness** (`review_freshness.go:41`, agenthost paths): `head.sha`, `base.ref`.
- **node_id resolution for draft-flip mutations** (`status.go:200-208`): `node_id` only.

### 1.3 `GET /repos/{o}/{r}/pulls/{n}/reviews` — `?per_page=100`, single page

Consumers: `GetPR` (`pulls.go:145` — `id, user.login, state, body, submitted_at`); `GetPRStatus` (`status.go:63` — `user.login, state, submitted_at`; COMMENTED skipped, latest-per-author kept). Errors swallowed (partial result).

### 1.4 `GET /repos/{o}/{r}/pulls/{n}/comments` — `?per_page=100`, single page

`GetPR` (`pulls.go:151`): reads only `pull_request_review_id` (per-review comment counts).

### 1.5 `GET /repos/{o}/{r}/issues/{n}/comments` — `?per_page=100`, single page

`GetPR` (`pulls.go:187`): `id, user.login, body, created_at`.

### 1.6 `GET /repos/{o}/{r}/issues/{n}/timeline` — prskeleton feed

- `internal/prskeleton/fetch.go:88`. `per_page=100`, pages 1..5 (hard cap 500 events), stops on short page; mid-walk failure → partial + truncation marker. Plain `Accept: application/vnd.github.v3+json` — NOT the historical mockingbird-preview type.
- Fields (union `timelineItem`, `fetch.go:213`): always `event`, `created_at`; actors `actor.login`/`user.login`; reviews `id, state, submitted_at`; commits `sha, message, author.name, author.date`; labels `label.name`; review requests `requested_reviewer.login` / `requested_team.slug`; renames `rename.to`; `line-commented` batches `comments[].{created_at, user.login, pull_request_review_id}`.
- Event values mapped (all else dropped): `committed, reviewed, commented, labeled, unlabeled, review_requested, review_request_removed, ready_for_review, convert_to_draft, head_ref_force_pushed, renamed, base_ref_changed, merged, closed, reopened, line-commented`.
- Fires once per delegated GitHub-PR run setup and once per follow-up blueprint step; best-effort.

### 1.7 `GET /repos/{o}/{r}/commits/{sha}/check-runs` — `?per_page=100`, single page

`GetPRStatus` (`status.go:111`): `check_runs[].{id (int64 on purpose), name, status, conclusion, completed_at, details_url}`; `details_url` parsed for `/actions/runs/{id}/`; deduped by name keeping highest id.

### 1.8 `GET /repos/{owner}/{repo}` — repo object, three distinct uses

- **Profile / rename detection** (`GetRepoMeta`, `events.go:280`; `profiler.go:293`): `id, full_name, default_branch, clone_url, ssh_url`. Rename detection depends on GitHub 301-redirecting a renamed repo and answering with the new `full_name` (⚠️ see `findings.md` — inferred, not test-asserted). Also `projects.go:407` (bundle preflight, consumes `clone_url`).
- **Reachability probe** (`CheckRepoAccess`, `events.go:222` — hand-built request, no retry): body drained, status only. 200 → reachable/conclusive; 404 OR 403 → unreachable/conclusive; anything else → indeterminate → fail open. Callers: team-repos cold path (≤20 concurrent, 3s timeout each, probed across every credential tier at once) and the App repo-coverage probe in `ClientForRepo` (positives memoized 5 min).

### 1.9 `GET /repos/{o}/{r}/contents/{path}` — profile docs

`GetFileContent` (`events.go:347`); `profiler.go:290-292` fetches exactly `README.md`, `CLAUDE.md`, `AGENTS.md` (no `?ref=`). Fields: `content` (base64, newlines stripped), `encoding` (must be `"base64"` or hard error). ONLY 404 = absent; any other non-2xx errors the whole repo's profile pass. TTL: 3 days per repo unless forced. No `/languages`, `/git/trees`, `/commits`, or search calls anywhere in repoprofile.

### 1.10 `GET /repos/{o}/{r}/compare/{base}...{head}` — three media shapes

Path built with `url.PathEscape` on base/head; `...` literal.
- JSON (`CompareCommits`, `pulls.go:366`): `ahead_by`, `files[].{filename,status,previous_filename,additions,deletions,patch}`. First page only, deliberately.
- Diff media (`GetCompareDiff`, `pulls.go:312`, `Accept: application/vnd.github.v3.diff`): raw text. 406 = "too large" is a first-class response → falls back to files form.
- Files form (`GetCompareFiles`, `pulls.go:324`): `files[]` only.
Consumers: review freshness annotation (every review-artifact GET, best-effort), finalize frame, `pr add-review-comment` anchoring, `pr diff` staleness probe (`ahead_by` only; errors → 0).

### 1.11 `GET /repos/{o}/{r}/pulls/{n}/files` — `?per_page=100&page=N`

`GetPRFiles` (`pulls.go:239`): `filename, status, previous_filename, additions, deletions, patch`. Paged to `MaxPRFiles = 1000` with WARN. Consumers: `gh pr files`, 406 fallbacks.

### 1.12 PR diff media — `GET /repos/{o}/{r}/pulls/{n}` with `Accept: application/vnd.github.v3.diff`

`GetPRDiff` (`pulls.go:283`): raw diff text; optional client-side single-file filter; 406 → files fallback.

### 1.13 `GET /repos/{o}/{r}/issues/comments/{id}` — thread view

`GetCommentThread` (`pulls.go:406`): `id, user.login, body, created_at`. Despite its doc comment there is NO review-comment fallback, and the verb's `--page` arg is ignored (see `findings.md`).

### 1.14 `GET /repos/{o}/{r}/pulls/{n}/reviews/{review_id}` + `/comments`

`GetReviewDetail` (`pulls.go:453,476`): review `id, user.login, state, body, submitted_at`; comments (`per_page=100`) `id, path, line, start_line, body, user.login, created_at`.

### 1.15 Actions (agent tools only)

- `GET /repos/{o}/{r}/actions/runs?head_sha={sha}&per_page=20&exclude_pull_requests=true` (`actions.go:199-204`): `workflow_runs[].{id,name,event,status,conclusion,head_sha,head_branch,created_at,html_url}`.
- `GET .../actions/runs/{run_id}/jobs?per_page=100` (`actions.go:421`): `total_count`, `jobs[].{id,name,status,conclusion,started_at,completed_at}`.
- `GET .../actions/runs/{run_id}/logs` (`actions.go:630` via `DownloadArtifact`): streamed zip, redirect-following (mock must serve a `Location`-redirected blob), 500 MB cap, 15-min timeout.
- On 404 only, per completed job: `GET .../actions/jobs/{job_id}/logs` — plain text.

### 1.16 Repo enumeration

- `GET /user/repos?sort=pushed&direction=desc&per_page=100&page=N` (`events.go:102`): PAT-tier reachable mirror (`reachcache/refresher.go:265`) + PAT preflight. Fields: `id, full_name, html_url, description, language, pushed_at, private`. Cap 100 pages / 10,000 repos; truncation → `complete=false` → nothing written, previous mirror kept.
- `GET /installation/repositories?per_page=100&page=N` (`events.go:186`): App tier. Envelope `total_count`, `repositories[]` with the same seven fields. Walks until `len >= total_count` or empty page; same cap and refuse-on-incomplete. **Called by two independent walkers back-to-back each cycle** (grant mirror + poll intersection — acknowledged duplication). App installation token ONLY (PAT gets 403). Suspended installations skipped without a request.

### 1.17 Teams

- `GET /orgs/{org}/teams?per_page=100&page=N` (`teams.go:354`): poller group-prune, once per distinct repo owner per cycle. Fields: `slug` (used), `name` (decoded). Cap 20 pages. An error must NOT prune — 403/404 behavior is load-bearing. Orrery models no teams: an org answers `[]` (GitHub's answer for an org that never made one, which the poller skips rather than prunes against), any other login 404.
- `GET /user/teams?per_page=100&page=N` (`teams.go:68,111`): local/PAT only — poll cycle (`slug`, `organization.login`) and the include-membership settings read (`slug, name, members_count, organization.login`). App tokens can't answer this; failure non-fatal. Orrery models no teams: a user is in none (`[]`), and a bot (App) token is refused 403 "Resource not accessible by integration", as GitHub refuses an installation token.
- GraphQL `organization.teams` — see § 3.

### 1.18 `GET /repos/{o}/{r}/branches?per_page={n}&page={n}` — branch picker

`ListBranchesPage` (`events.go:317`; `repos_handler.go:969`): `name` only. Pass-through proxied list; no server-side search (client filters).

### 1.19 Identity & validation

- `GET /user` + `GET /user/emails` (`validate.go:128,183`; also GoTrue, §2.1): `id, login, avatar_url, name`; emails `email, primary, verified` (Orrery serves an actor's stored addresses, primary first; every actor starts with `<login>@orrery.invalid`, and the control plane sets the rest) — first primary+verified wins, else hard error. Status contract: 200 / 401 bad token / 403 missing scopes (`/user/emails` 403 → dedicated error). ⚠️ Builds its own URL, hardcoding `{base}/api/v3/user` for non-github.com — diverges from `github.APIBase` on `.ghe.com` hosts (probable bug; `findings.md`).
- **Bot actors.** A `bot` actor stands for a GitHub App: REST spells its login `<login>[bot]` with `"type": "Bot"`, GraphQL types its author `Bot` with the bare login (TF's `restLogin` reconciles the two spellings), and its token is refused on `GET /user`, `GET /user/emails` and `/user/repos` with 403 "Resource not accessible by integration", as GitHub refuses an installation token there. A PAT is not an actor of its own: it is a user actor's token, a machine user's included, so it renders as that user.
- `GET /users/{login}` (`client.go:254`; `github_app_register.go:715`): App bot user id (`"<slug>[bot]"` path-escaped). **Unauthenticated** — no Authorization header; the mock must accept anonymous requests here. Field: `id`. Failure non-fatal.
- API-root reachability probe (`reachability.go:66`): bare `GET {APIBase(url)}`, UA `triage-factory-reachability`, redirects not followed; ANY HTTP status = reachable.

---

## 2. GitHub App / auth endpoints

All `internal/githubapp` calls carry `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`, `User-Agent: triage-factory-githubapp` — unlike the main client (see `grammar.md`).

| Endpoint | Auth | Contract |
| --- | --- | --- |
| `GET /app/installations?per_page=100` | app JWT | Link-header pagination (`rel="next"`), same-origin pinned (cross-origin Link = hard error). Fields: `id, account.{id,login,type}, created_at, suspended_at, suspended_by.login, repository_selection`. Fires every poll cycle (grant reconcile) + cutover/settings paths. |
| `POST /app/installations/{id}/access_tokens` | app JWT | **201 exactly** (200 fails the mint). Optional body `{"repositories":[bare names],"permissions":{...}}` — narrowing; out-of-grant → 422 expected. Response: `token`, `expires_at` (required, must be strictly future). Cached per installation, 5-min expiry guard; scoped mints bypass cache. The hot path for every credential mint. |
| `GET /app` | app JWT | Fields: `id, slug, client_id, owner.{login,type}, permissions, events`. 401 = bad App-ID/key pair. |
| `GET /app/hook/config` | app JWT | Fields: `url, content_type, secret` (emptiness only). 404 → `ErrHookAPIUnavailable` (GHES < 3.2). 10-min/2-min TTL cache. |
| `GET /app/hook/deliveries` | app JWT | Link-paginated, same-origin pinned. Params vary: `per_page=1` / `per_page=30` (health), `status=failure&per_page=100`, ≤2 pages (replay). Fields: `id, guid, delivered_at, redelivery, status, status_code, event, action, installation_id`. |
| `POST /app/hook/deliveries/{id}/attempts` | app JWT | any 2xx (GitHub sends 202); body ignored. |
| `POST /app-manifests/{code}/conversions` | none (code is the credential) | **201 exactly**. Fields: `id, slug, client_id, client_secret, pem, webhook_secret` (last optional). |
| `POST /applications/{client_id}/token` | Basic `client_id:client_secret` | Body: bogus token. **401 = bad secret, 404 = valid secret**, else inconclusive. Response body never read. |
| `POST {webBase}/login/oauth/access_token` | none | Web host, not API base. Form: `client_id, client_secret, code, redirect_uri`; `Accept: application/json`. Reads `access_token, token_type, error, error_description` — 200 even on logical failure, `error` checked independently. |

JWT shape (mint side, for reference): RS256, claims exactly `{iss: <numeric app id>, iat: now-60s, exp: now+9m}`; PKCS#1 and PKCS#8 keys both accepted.

### 2.1 The OAuth web flow (GoTrue's GitHub provider)

TF's multi mode signs people in through GoTrue, whose GitHub provider runs GitHub's OAuth web flow against `GOTRUE_EXTERNAL_GITHUB_URL`. With that URL set to a GHES-style host, GoTrue sends the browser to `{url}/login/oauth/authorize`, exchanges at `{url}/login/oauth/access_token`, and reads identity at `{url}/api/v3/user` and `{url}/api/v3/user/emails` (§1.19). Observed in GoTrue v2.189.0 (`internal/api/provider/github.go`) and in TF's SSO end-to-end suite (`e2e/sso`).

| Endpoint | Auth | Contract |
| --- | --- | --- |
| `GET {webBase}/login/oauth/authorize` | none (browser) | Query: `client_id, redirect_uri, scope, state` (GoTrue's scope is `user:email`). Unknown `client_id` → 404 page. `redirect_uri` optional, defaulting to the app's callback URL; when given, scheme, host and port must equal the callback's and the path must be the callback's or below it, else a 302 to the callback with `error=redirect_uri_mismatch, error_description, error_uri, state`. GitHub's sub-domain allowance and loopback-port exception are not modeled (refused). |
| `POST {webBase}/login/oauth/authorize` | an actor's login (form) | Orrery's stand-in for GitHub's sign-in and consent pages: form fields as above plus `login`, naming an existing **user** actor (rule 12: no passwords exist). An unknown login or a non-user actor re-renders the page with "Incorrect username or password." Success → 302 to `redirect_uri` with `code` and `state`. |
| `POST {webBase}/login/oauth/access_token` | `client_id` + `client_secret` | Params from the query, a form body or a JSON body; the client pair may instead be HTTP Basic. Response form-encoded unless `Accept: application/json` (`application/xml` is not mocked). Success: `access_token, token_type=bearer, scope`. Refusals are **200** with `error, error_description, error_uri`: `incorrect_client_credentials`, `bad_verification_code` (unknown, spent, or older than 10 minutes on the virtual clock), `redirect_uri_mismatch` (a `redirect_uri` that differs from the one the code was issued for). A code is spent by its first successful exchange. |

Scopes are recorded and echoed, not enforced: no face route checks a token's scopes.

---

## 3. GraphQL surface (TF's own client)

Updated at TF commit `a53a770e` (2026-10-20), where triage-factory's poll end-to-end test (`internal/poller/poll_e2e_test.go`, the `poll-e2e` workflow) found the queries had grown since the extraction: `fullDatabaseId`, backward paging on `reviews`/`comments`, and `rateLimit`.

`PostGraphQL` (`client.go:472-542`): empty Accept header; queries only (never mutations except the two below); refused through the sidecar REST proxy (fails closed). **A 200 carrying both `data` and `errors[]` is partial success** — the mock must be able to produce this (e.g. FORBIDDEN `statusCheckRollup` on one node), and `null` entries in `nodes` are skipped.

### Queries

- **`search(query,type:ISSUE,first:50)` — discovery/backfill** (`graphql.go:198`). Only when a viewer login exists (local/PAT; multi dashboard backfill once per user/host). Four query bases over 30 days, batched ≤3 `repo:` scopes / ≤256 chars each: `is:pr is:merged author:{u} merged:>=`, `is:pr is:merged reviewed-by:{u} merged:>=`, `is:pr is:closed is:unmerged author:{u} closed:>=`, same with `reviewed-by`. Single page, no cursor. Fragment: `prDiscoveryFragment` (below), no `statusCheckRollup`.
- **`nodes(ids:[ID!]!)` — Phase-2 batch refresh** (`graphql.go:287`). Chunks of 20 ids. Open batch uses `prFullFragment`; terminal batch and reconcile use `prDiscoveryFragment`.
- **`node(id:){...on PullRequest}{comments(last:50, before:$before)}`** — the tracker's new-comment read (`pr_comments.go`, query `PRComments`): `totalCount, pageInfo{hasPreviousPage, startCursor}, nodes{fullDatabaseId, createdAt, author{__typename, login}, body}`. Walks backwards from the newest comment through `startCursor` until it reaches what it saw last cycle.
- **`node(id:){...on PullRequest}{reviews(last:50, before:$before, states:[APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED])}`** — the tracker's new-review read (`pr_reviews.go`, query `PRReviews`): `totalCount, pageInfo{hasPreviousPage, startCursor}, nodes{fullDatabaseId, state, submittedAt, author{__typename, login}, body, comments(first:100){totalCount, nodes{fullDatabaseId, path, body}}}`. `totalCount` counts the `states`-filtered set.
- **`rateLimit { cost }`** is selected by every query document, and its cost is recorded per operation (`client.go` `recordQueryCost`). A null `rateLimit` is allowed — a GHES with rate limiting off sends it — and records nothing; Orrery, which presents as that GHES, answers null.
- **`node(id:){...on PullRequestReview}`** (`pending_reviews.go:58`): `state, body, comments(first:100){pageInfo.hasNextPage, nodes{id, path, line, startLine, body}}` — `line`/`startLine` are nullable and null is meaningful (outdated comment), must not collapse to 0. `null` node = absence, not error.
- **Aliased branch existence** (`branches.go:65`): batches of 50 aliases, `r{i}: repository(owner:$o{i}, name:$n{i}){ref(qualifiedName:$q{i}){id}}` with `q = refs/heads/<branch>`. Presence only; a null repository alias or an alias named in `errors[].path[0]` = unknown, never "deleted".
- **`organization.teams`** (`teams.go:159,258`): with/without `userLogins:` filter; cursor-paged, cap 20; `pageInfo.{hasNextPage,endCursor}`, `nodes[].{slug,name,members.totalCount}`; null organization = empty result.

### Fragments (exact consumption)

`prBaseFields` (`graphql.go:15`): `id, number, title, body, author{__typename, login}, state, isDraft, merged, mergeable, headRefName, baseRefName, url, repository.nameWithOwner, headRepository.nameWithOwner, additions, deletions, changedFiles, reviewRequests(first:100){pageInfo.hasNextPage, nodes.requestedReviewer{User.login | Team.slug + Team.organization.login}}, latestReviews(first:20){nodes{id, fullDatabaseId, author{__typename, login}, state, submittedAt, comments.totalCount}}, reviews(last:1, states:[APPROVED, CHANGES_REQUESTED, COMMENTED, DISMISSED]){totalCount, nodes{fullDatabaseId, submittedAt}}`. `fullDatabaseId` is GitHub's `BigInt` scalar, a string on the wire; TF also accepts a number.

`prDiscoveryFragment` (`graphql.go:67`) = base + `commits(last:1){nodes.commit{oid, committedDate}}, labels(first:10){nodes.name}, comments(last:1){totalCount, nodes{fullDatabaseId, createdAt}}, createdAt, updatedAt, mergedAt, closedAt`.

`prFullFragment` (`graphql.go:107`) = base + commits with `statusCheckRollup{contexts(first:100){pageInfo.hasNextPage, nodes{__typename, ...on CheckRun{databaseId, name, status, conclusion, completedAt, detailsUrl, checkSuite.workflowRun.databaseId}}}}` + labels/comments/timestamps + `timelineItems(last:20, itemTypes:[LABELED_EVENT, UNLABELED_EVENT, REVIEW_REQUESTED_EVENT, READY_FOR_REVIEW_EVENT]){nodes{__typename, createdAt, label.name, requestedReviewer{User.login | Team.slug + Team.organization.login}}}`. `StatusContext` union members are decoded then dropped — legacy commit statuses are silently ignored; only CheckRun survives.

### TF's two GraphQL mutations

`markPullRequestReadyForReview` and `convertPullRequestToDraft` (`status.go:198-248`; approval + dashboard drag): each preceded by a REST PR read for `node_id`; response discarded.

---

## 4. Write surface

### 4.1 TF Go direct writes (13 REST sites + the 2 mutations above)

| Call | Site | Body / contract |
| --- | --- | --- |
| `POST /repos/{o}/{r}/pulls` | `CreatePR` (`pulls.go:612`) ← `exec gh pr create` | `{title, head, base, body, draft}` — draft hardcoded `true` by the verb. Response: `number, html_url, node_id`. 422 envelope `message` + `errors[].{code,field,message}` parsed. |
| `PATCH /repos/{o}/{r}/pulls/{n}` | `UpdatePR` (`pulls.go:643`) ← artifact edit + approval footer | `{title, body}` whole-field replace (omitted field pre-read live). |
| `PATCH /repos/{o}/{r}/pulls/{n}` | `ClosePR` (`pulls.go:662`) ← abandon draft | `{"state":"closed"}`; failures logged only. |
| `POST /repos/{o}/{r}/pulls/{n}/reviews` | `SubmitReview` (`pulls.go:541`) ← human approval of staged review + auto-post posture | `{commit_id, event: APPROVE\|COMMENT\|REQUEST_CHANGES, body, comments[]?{path, line, start_line?, body}}`. Response: `id` only; URL composed locally as `<pull url>#pullrequestreview-<id>`. Wire-relevant errors: 422+REQUEST_CHANGES → "own PR"; body containing `must be part of the same hunk` → hunk message. |
| `PUT /repos/{o}/{r}/pulls/{n}/reviews/{id}/dismissals` | `pulls.go:730` ← `exec gh pr review-dismiss` | `{"message": ...}` — TF does NOT send `event: DISMISS` (see `findings.md`). Response ignored. |
| `POST /repos/{o}/{r}/issues/{n}/comments` | `pulls.go:752` ← `exec gh pr add-comment` | `{"body"}`. Response: `id, html_url`. |
| `POST /repos/{o}/{r}/pulls/{n}/comments/{id}/replies` | `pulls.go:767` ← `exec gh pr comment-reply` | `{"body"}`. Response: `id`. |
| `POST .../reactions` (two-path probe) | `pulls.go:782` ← comment-react | Tries `/issues/comments/{id}/reactions`, on ANY error retries `/pulls/comments/{id}/reactions`. `{"content": emoji}`. Response ignored. |
| `PATCH` comment (two-path probe) | `pulls.go:806` ← comment-update (numeric id) | `/issues/comments/{id}` then `/pulls/comments/{id}`. `{"body"}`. |
| `DELETE` comment (two-path probe) | `pulls.go:826` ← comment-delete (numeric id) | Same two paths; no body. |

Affirmatively absent: TF posts no comments/labels/statuses/check-runs of its own accord; no RUN_URL is ever written to GitHub by Go code (`{{RUN_URL}}` is a prompt placeholder only); poller/tracker/reconcile/curator/grantmirror are read-only.

### 4.2 `tfac exec gh` verbs (agent-initiated, host-executed)

Read verbs compose §1 endpoints (view=4 GETs; diff=diff-media+406 fallback+compare; files; thread-view; review-view; actions×2). Write verbs map to the table above. Review staging is TF-side: `start-review` = 1 read (zero writes), `add-review-comment` = reads only (staged into artifact `details_json`), `finalize-review` = 0 writes (approval posture, default) or 1 `POST .../reviews` (auto-post posture). `SubmitReview` exists on the IPC seam but no verb calls it — don't over-provision from the IPC surface.

### 4.3 Real `gh` CLI through the injector

Env contract: `GH_HOST=<injector host>`, `GH_ENTERPRISE_TOKEN=<placeholder>`, `SSL_CERT_FILE=<per-run CA>`, `GH_CONFIG_DIR=<empty dir>`. Injector strips `/api/v3`, swaps auth to `token <real>` upstream (fail-anonymous on resolution failure), deletes forwarding headers, routes `/api/graphql` to the GraphQL upstream. Git smart-HTTP shares the same port-443 listener, routed by path suffix BEFORE the API auth gate.

**Porcelain is GraphQL-first** (pinned-gh conformance, `gh_conformance_test.go`):

| gh command | Wire |
| --- | --- |
| `pr create` | `createPullRequest` — selection is only `pullRequest{id url}`, so no number in the response; parsed from the URL |
| `pr comment` / `--edit-last` / `--delete-last` | `addComment` / `updateIssueComment` / `deleteIssueComment` |
| `pr close` / `reopen` / `edit` | `closePullRequest` / `reopenPullRequest` / `updatePullRequest` |
| `pr edit --add-label/--remove-label` | `addLabelsToLabelable` / `removeLabelsFromLabelable` (separate request) |
| `pr edit --add-reviewer` | TWO requests: REST `POST .../requested_reviewers` AND `updatePullRequest` |
| `pr ready` / `--undo` | `markPullRequestReadyForReview` / `convertPullRequestToDraft` |
| `pr merge --merge` / `--auto` / `--disable-auto` | `mergePullRequest` / `enablePullRequestAutoMerge` / `disablePullRequestAutoMerge` |
| `pr review` | `addPullRequestReview` — **refused by the gate** |
| `pr lock/unlock`, `pr revert`, `pr update-branch` | `lockLockable`/`unlockLockable`, `revertPullRequest`, `updatePullRequestBranch` |
| `issue` family | `createIssue, addComment, closeIssue, reopenIssue, deleteIssue, updateIssue, pinIssue, unpinIssue, transferIssue, createLinkedBranch, lockLockable, unlockLockable` |
| `repo archive/unarchive/edit/fork/delete` | GraphQL archive/unarchive; REST `PATCH /repos`, `POST .../forks`, `DELETE /repos` |
| `repo create` | `createRepository` / `cloneTemplateRepository` — **refused**; `--add-readme` variant is REST `POST /user/repos` or `/orgs/{org}/repos` — **refused** |
| `label create/edit/delete` | REST `POST/PATCH/DELETE /repos/{o}/{r}/labels[/{name}]` |
| `release create/edit` | REST `POST .../releases`, `PATCH .../releases/{id}` |
| inline review comments | ONLY via `gh api` REST — porcelain has no flags for them |

**Gate (refusals, 403 + JSON envelope `{message, triage_factory:{refused, reason, ...}}`, pre-forward):** three families — `review` (`addPullRequestReview`, `submitPullRequestReview`, `addPullRequestReviewThread`, REST `POST .../reviews`), `repo_create` (`createRepository`, `cloneTemplateRepository`, REST `/generate`, `POST /user/repos`, `POST /orgs/{org}/repos`), `unreadable` (GraphQL body over 1 MiB cap, malformed, ambiguous operation, unresolved fragment). Merging is deliberately NOT gated. REST paths are `path.Clean`-normalized before matching.

**Observation (artifact capture):** 2xx POSTs only. GraphQL arm keys on exact `data.createPullRequest` → consumes `pullRequest.{id, url}` (head/base/title stay empty — gh doesn't select them). REST arm recognizes post-rewrite `.../pulls` (fields `number, node_id, html_url, title, body, draft, head.ref, base.ref`) and `.../pulls/{n}/reviews` (dead under the current gate). **Audit** fires for every mutation on any status; REST bodies never read (method+path classification only); GraphQL response read for `errors[]` presence + first `url` at depth ≤6.

**Classification tables** (`internal/ghwrite/classify.go:339-668`, `graphql.go:130-211`) enumerate ~30 REST shapes and ~30 mutations → action names (`pr_created`, `review_submitted`, `label_added`, …). These tables are the ready-made write-action vocabulary for the mock's semantic diff.

**Structural blind spot (accepted):** gh commands following absolute URLs from response bodies (release-asset `upload_url`) leave the channel entirely — not injected, gated, or logged.

---

## 5. Inbound webhooks (what the mock must be able to SEND)

`POST /api/webhooks/github/{org_id}` (`internal/server/github_webhooks.go:53-182`), App-class orgs only:

- Headers required: `X-GitHub-Event` (400 if missing), `X-Hub-Signature-256` (401), `X-GitHub-Delivery` (400; the dedup key).
- Signature: HMAC-SHA256 over the raw body with the org's stored `webhook_secret`; empty secret short-circuits to 401. Body cap 25 MiB.
- Uniform 401 for no-App / PAT-org / bad-signature (anti-enumeration). Dedup on `(installation_id, delivery_guid)` → duplicate = 204. Success = 204.
- Specially handled events: `installation.created/deleted/suspend/unsuspend` (the suspend/delete path invalidates the token cache). Everything else → event bus.

The mock's App face therefore needs: webhook delivery with correct signature/headers, a delivery log to back `GET /app/hook/deliveries`, and redelivery on `POST .../attempts`. Polling remains the primary event channel; webhook fidelity matters for App-credential scenarios and the webhook-health UI.
