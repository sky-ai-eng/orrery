# Platform grammar (build once, engine-wide)

Extracted at TF commit `78c8bfd2` (2026-08-21). These are the cross-cutting behaviors every endpoint implicitly relies on. Per the constraint preamble (README rule 1), grammar is implemented once up front, not per endpoint — and a mock that gets grammar wrong breaks consumers in ways no endpoint test catches.

## GitHub

### Seven clients, different header disciplines

There is no single shared client. A strict mock that requires headers uniformly will break the data plane:

| Client | Auth | Accept | `X-GitHub-Api-Version` | User-Agent |
| --- | --- | --- | --- | --- |
| `internal/github.Client` (main REST+GraphQL data plane) | `Bearer <tok>` — omitted entirely when token empty (deliberate anonymous mode) | `application/vnd.github.v3+json`; `.v3.diff` for raw diffs; EMPTY for GraphQL | **none** | **Go default** |
| `internal/githubapp.Minter` (all `/app/*`) | `Bearer <RS256 JWT>` | `application/vnd.github+json` | `2022-11-28` | `triage-factory-githubapp` |
| `internal/auth` validation | `Bearer <PAT>` | `.v3+json` | none | Go default |
| manifest/import client | none / Basic | `vnd.github+json` | mixed | mixed |
| reachability probe | none | `*/*` | n/a | `triage-factory-reachability`; redirects NOT followed |
| gh injector upstream hop | rewrites to `token <tok>` (not Bearer) | passthrough | passthrough | passthrough (gh's own) |
| git smart-HTTP | `Basic base64("x-access-token:"+tok)` | n/a | n/a | git's own |

No preview media types anywhere. `Content-Type: application/json` only when a body is present.

### Base URLs — three host classes (GHES is first-class)

`internal/github/baseurl.go`: `github.com` → `https://api.github.com`; `*.ghe.com` → `https://api.<tenant>.ghe.com`; everything else → `{base}/api/v3`. GraphQL: `/graphql` resp. `{host}/api/graphql` (NOT `/api/v3/graphql`). Resolution: `org_settings.github_base_url` → `github_url` secret → default; a read failure propagates rather than defaulting (a GHES tenant must never route to the public host). Known divergence: the validation path hand-builds `{base}/api/v3/user` and mishandles `.ghe.com` (see `findings.md`); `internal/db/github_app_backfill.go` holds a private duplicate of the derivation (drift risk).

### Rate limiting (the contract a mock should emit)

- Headers read: `X-RateLimit-Remaining` + `X-RateLimit-Reset` (both required for state to update — absent headers leave state untouched, deliberately supporting GHES with rate limiting off), `X-RateLimit-Used` best-effort.
- Retry triggers: 429 always; 403 only when confirmed rate-limit-shaped by ANY of: `Retry-After` present, `X-RateLimit-Remaining: 0`, or body containing `secondary rate limit` / `abuse detection mechanism` (case-insensitive). **A 403 matching none of these is a permissions error and the drained body is replayed to the caller — the 403 body TEXT is semantically meaningful.**
- Wait: `Retry-After` (delay-seconds or HTTP-date; ≤0 treated as absent) → primary reset → exponential backoff (1s base, doubling); max 3 extra attempts, 5-min wait cap; give-up → typed `ErrRateLimited{ResumeAt}` which the poll cycle treats as a first-class outcome (resume cursor + reschedule).
- Pre-flight throttle: a request is blocked client-side before sending when last-seen remaining is 0; reset >5 min out returns `ErrRateLimited` with no request.
- Idempotency split: GETs + GraphQL queries retry; **mutations get exactly one attempt**.
- 304s don't count against budget. No live `GET /rate_limit` call exists anywhere.
- 5xx is NOT retried; transport errors NOT retried.

### Pagination — two regimes

- Main client: NO Link-header parsing anywhere. Hand-rolled `?per_page=100&page=N` with termination heuristics (`len%100==0` continue; or body `total_count`). Caps: 10 pages general (1,000 items), 100 pages for whole-account repo walks (10,000) — truncation always WARN-logged, and the repo-mirror walkers return `complete=false` and refuse to persist an incomplete answer.
- `internal/githubapp`: real `Link: <url>; rel="next"` parsing, with the next URL pinned same-origin (scheme+host) against the configured API base — cross-origin Link is a hard error (the request carries the App JWT).

### Conditional requests

One shared mechanism (`GetConditional`): `If-None-Match` when an etag is held; 304 → typed empty result, body drained; 200 → new `ETag` captured. Exactly one consumer today: open-PR discovery page 1, etag persisted per-repo in `repositories.pulls_etag`. The apiproxy passes `If-None-Match` through untouched.

### Status-code contracts worth pinning

- `POST .../access_tokens` and `POST /app-manifests/{code}/conversions`: **201 exactly** (200 fails).
- `POST /applications/{client_id}/token`: 401 = bad secret, **404 = valid secret**.
- `GET /app/hook/*`: 404 → GHES-too-old sentinel.
- `CheckRepoAccess`: 200 conclusive-reachable; 403 OR 404 conclusive-unreachable; else indeterminate → fail open.
- Installation-token `expires_at` must be strictly in the future; 5-min expiry guard on the cache.
- GraphQL: 200 with `data` + `errors[]` = partial success; `data` null/absent = failure; `null` nodes skipped.
- OAuth token endpoint: 200 with `error` field on logical failure.

### Credential classes

Org-level, App XOR PAT (never both; decided by `org_settings.github_credential_class`, never inferred). App = installation tokens (indistinguishable from PATs on the wire — same Bearer shape). Scoped mints narrow by bare repo names + permissions; out-of-grant → 422. No per-user GitHub API credential exists (OAuth user tokens are used once for identity capture, then discarded). Suspended installations: never served from token cache; skipped without a request in enumeration.

## Jira

- URL: `{BaseURL}/rest/api/{2|3}/{path}`; version follows the deployment marker (Cloud 3 / DC 2); Cloud enhanced search pins `/rest/api/3/search/jql` unconditionally; `serverInfo` probe pins v2.
- Retry: 429 always (incl. mutations); 5xx + transport errors only for idempotent (GETs and the two read-shaped search POSTs, via an explicit flag); max 3 extra; `Retry-After` (delay-seconds or HTTP-date) honored to a 30s cap — longer surfaces the throttle to the caller; backoff 500ms→1s→2s cap 30s.
- Only `Retry-After` is read — no `X-RateLimit-*` equivalents.
- `Content-Type: application/json` on every request including GETs; no Accept header.
- GET requires exactly 200; PUT/POST accept <300. `StatusError`/`IsNotFound` produced only by GETs (Jira 404s permission-denied to avoid existence disclosure).
- 15s timeout, traced; the validation path uses a separate 10s no-retry client.
- Pagination: DC `/search` startAt offsets (cap 50 pages); Cloud `nextPageToken`+`isLast`; Cloud project search startAt with `isLast`→`total`→short-page precedence; DC project catalog entirely unpaged.

## Webhooks / push channels (inbound receivers that exist)

- GitHub: `POST /api/webhooks/github/{org_id}` — HMAC-SHA256 (`X-Hub-Signature-256`), `X-GitHub-Event` + `X-GitHub-Delivery` required, 25 MiB cap, dedup on `(installation_id, delivery_guid)`, uniform 401 anti-enumeration, 204 on success. App-class orgs only.
- Slack Events API: `POST /api/webhooks/slack/{org_id}` — v0 signing, 5-min replay window, `url_verification` challenge.
- Jira: none. Pure polling.
- Slack Socket Mode: outbound wss (see `surface-jira-slack.md`).
