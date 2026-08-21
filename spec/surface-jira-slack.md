# Jira + Slack surface

Extracted at TF commit `78c8bfd2` (2026-08-21). The Jira face is deferred from the engine's V1 (a free Jira Cloud site covers that tier meanwhile); this is the complete catalog for when it's built.

## Jira: shape of the client

One client (`internal/jira/client.go`), one URL builder: `{BaseURL}/rest/api/{APIVersion}/{path}`. `APIVersion` is a pure function of deployment: Cloud → 3, Data Center → 2. Both trees must be served, plus `auth.atlassian.com` / `api.atlassian.com` for Cloud OAuth. Deployment is an explicit stored marker (`jira_auth_method` secret; host-shape `*.atlassian.net` suffix match is only the fallback for pre-marker orgs). **Jira is pure polling — zero inbound Jira webhooks exist** (the only inbound Jira HTTP is the OAuth browser callback). Neither poller nor tracker ever writes to Jira.

Auth shapes: `dc_pat` → `Bearer <PAT>` on v2 at the site URL; `cloud_api_token` → `Basic base64(email:token)` on v3; `cloud_oauth` → `Bearer <access token>` on v3 at `https://api.atlassian.com/ex/jira/{cloud_id}`. Per-user credentials never fall back to the org credential; the poller, gates, mirror, and every `exec jira` verb use the org service credential (bot-attributed by design); stock/task claim/done/requeue use the per-user credential.

In multi mode, `exec jira` verbs ride a per-run reverse proxy (placeholder Bearer → real header upstream); path templates are unchanged on the upstream hop.

## Jira reads

| Endpoint | Purpose / consumer | Contract |
| --- | --- | --- |
| `POST /rest/api/2/search` (DC) | discovery + batch refresh (`SearchIssues` DC arm, `client.go:958`) | Body `{jql, maxResults, fields[]}`; `startAt` added only from page 2. Reads `issues[]`, `total`. Offset pagination, cap 50 round trips. 5xx retried (idempotent-flagged POST). |
| `POST /rest/api/3/search/jql` (Cloud) | same, Cloud arm (`client.go:923`) — old `/search` is 410 on Cloud | Body `{jql, maxResults ≤5000, fields[], nextPageToken?}`. Reads `issues[]`, `nextPageToken`, `isLast`. No `total`. `fields` never omitted (Cloud returns ids-only without it). Version pinned to 3 regardless of APIVersion. |
| `GET /rest/api/{v}/issue/{key}` | unreachable-confirm (≤20/cycle, only entities stale >1h; 404 → `jira:issue:unreachable`, changed `key` → moved, re-keyed silently); `exec jira ticket view` | Full `Issue` struct. |
| `GET /rest/api/{v}/issue/{key}?fields=assignee,status` | claim guard (`GetClaimState`) before assign/transition | Reads `status.{name,id}`, `assignee.{accountId,key,name}`; compared to `/myself` by accountId → key → name. Nil on any error ("unknown, proceed"). Query string embedded in the path format — route on prefix. |
| `GET /rest/api/{v}/project/{key}/statuses` | settings statuses read; team-jira-projects write gate; JQL salvage (only after a failed discovery query) | Array of issue types each `{statuses:[{id,name}]}`; deduped by NAME (collisions collapse). |
| `GET /rest/api/3/project/search?startAt&maxResults&query` (Cloud) | project picker + gate visibility walk | Reads `values[]{key,name}`, `isLast`, `total`, `maxResults`. End precedence: isLast → total → short page. |
| `GET /rest/api/2/project` (DC) | same | NO params — whole catalog unpaged; filtering/windowing client-side. The sharpest DC/Cloud divergence. |
| `GET /rest/api/{v}/project/{key}` | `exec jira ticket list-types` | `issueTypes[]{id,name,subtask,iconUrl}`. |
| `GET /rest/api/{v}/priority` | `exec jira ticket list-priorities` | `[{id,name}]`. |
| `GET /rest/api/{v}/field` | Epic Link field discovery (lazy, uncached; DC fallback paths only) | Matches `schema.custom == "com.pyxis.greenhopper.jira:gh-epic-link"` → `id`; not-found = empty, not error. |
| `GET /rest/api/{v}/myself` | client identity (cached per client lifetime; failures not cached); credential validation (org bind, per-user bind, OAuth whoami) | `name, key, accountId`; validation additionally `displayName`, `avatarUrl` or `avatarUrls["48x48"]`. 401 bad token / 403 insufficient perms. |
| `GET {base}/rest/api/2/serverInfo` | reachability probe (no auth, v2 hard-wired) | ANY HTTP status = reachable; response unread; redirects not followed. |
| `GET`+`POST /rest/api/{v}/issue/{key}/transitions` | transition list + execute | GET reads `transitions[]{id,name,to{id,name}}`; POST body `{"transition":{"id"}}`. Matching: target-ID exact, else case-insensitive `to.name`. |

**JQL actually sent** (tracker): `project = "KEY" AND status IN (<terms>) AND assignee IS EMPTY`; `project = "KEY" AND assignee = currentUser() AND status NOT IN (<terms>)`; `key IN (…≤100)`; plus on-demand `parent = KEY ORDER BY created ASC` (and the `cf[<id>] = KEY` Epic Link fallback) and arbitrary agent `--jql`. All-digit status terms are sent as bare ids, others `%q`-quoted. Discovery field list: `summary, description, status, assignee, priority, labels, issuetype, parent, comment, subtasks, created, updated`; batch refresh drops `description`. `maxResults=100`.

**`Issue` fields parsed:** `key`, `self`, and `fields.{summary, description (string on DC, ADF doc on Cloud), status{id,name}, issuetype{name}, priority{name}, assignee{displayName,accountId,key,name}, parent{key}, labels[], comment{total}, created, updated, subtasks[]{key, fields.status{id,name}}}`.

## Jira writes (complete set — all `exec jira` verbs use the org credential; product surfaces use per-user)

| Write | Endpoint | Body notes |
| --- | --- | --- |
| assign to self | `PUT .../issue/{key}/assignee` | `{"accountId"}` when `/myself` returned one, else `{"name"}` — keyed off the RESPONSE, not the configured deployment |
| unassign | same | `{"accountId": null}` / `{"name": null}` |
| transition | `POST .../transitions` | after a GET; by-name matching from agents (they never have ids) |
| comment | `POST .../issue/{key}/comment` | v2: raw wiki-markup string; v3: ADF doc from `wikiToADF`. Response `id` best-effort (unparseable 2xx → empty id, no error) |
| create issue | `POST .../issue` | `{fields:{project{key}, issuetype{name}, summary, description?, priority{name}?, parent{key}?}}`. On error body containing `gh.epic.error` OR `parent`: drops parent, discovers Epic Link field, re-POSTs with `customfield_XXXXX` — the fallback keys on error-BODY substring matching |
| update fields | `PUT .../issue/{key}` | `{fields:{summary/description/priority/issuetype}}` + `{update:{labels:[{add},{remove}]}}` so untouched labels survive |
| set parent / set priority | `PUT .../issue/{key}` | parent has the same Epic Link retry |

Surfaces: stock claim/done, task claim/requeue (per-user); board→Jira mirror + all `exec jira` verbs (org).

## Cloud OAuth (3LO)

- Authorize (browser): `https://auth.atlassian.com/authorize?audience=api.atlassian.com&client_id&scope=read:jira-work write:jira-work read:me offline_access&redirect_uri&state&response_type=code&prompt=consent`.
- `POST https://auth.atlassian.com/oauth/token` (form): code exchange + refresh. Reads `access_token, refresh_token, expires_in, error, error_description`; BOTH tokens required or hard fail. **Refresh rotation is mandatory to model** — every refresh invalidates the presented refresh token. Access token cached in-process, refreshed 60s before expiry, singleflighted per (org,user,host).
- `GET https://api.atlassian.com/oauth/token/accessible-resources`: `[{id (cloud_id), url, name}]`, matched against the org host case-/slash-insensitively.

## Jira mock-relevant quirks

1. `get` requires exactly 200 (a 204 errors); `put/post` accept any <300. `StatusError`/`IsNotFound` only exist for GETs.
2. Retry: 429 always (mutations included — a throttled request was rejected, not processed); 5xx and transport errors only for idempotent; `Retry-After` honored ≤30s (longer surfaces the 429).
3. `Content-Type: application/json` on every request including GETs; no Accept header ever.
4. Validation path uses a separate 10s no-retry client.
5. No `/rest/agile`, `/rest/greenhopper`, or `/rest/auth` usage anywhere.

---

## Slack (ee; light pass)

Base `https://slack.com/api` (test-redirectable var, not user-configurable). Auth: `Bearer xoxb-…` (bot) / `xapp-…` (app-level). Two transports, chosen per workspace at connect: **Socket Mode** (`apps.connections.open` → wss dial → `hello` frame → envelopes `hello|disconnect|events_api`, acked `{"envelope_id"}` within ~3s or Slack redelivers; ping 30s) or **Events API** (`POST /api/webhooks/slack/{org_id}`, Slack v0 signing `v0=HMAC-SHA256(secret, "v0:"+ts+":"+body)`, 5-min replay window, handles `url_verification` challenge / `event_callback` / `app_rate_limited`). Ambiguous config (both creds, no choice) refuses.

Web API methods called: `auth.test` (validation; needs `team_id`+`bot_id`), `bots.info`, `apps.connections.open`, `users.info`, `conversations.info/list/join/replies/history`, `users.conversations`, `chat.postMessage/update/getPermalink`, `reactions.add`, `files.info`, `files.getUploadURLExternal`, plus raw `url_private` file downloads (Bearer, non-JSON). Content type varies: JSON for `chat.*`; form-encoded for `conversations.join`/`reactions.add`; query-string GETs elsewhere. 429: honors `Retry-After`, retries exactly once; second consecutive 429 → typed error. Cursor pagination (`limit=200`) with a page cap and explicit truncation flag.
