# Findings from the extraction

Side discoveries from the 2026-08-21 surface sweep at commit `78c8bfd2`: TF documentation drift, dead code, probable bugs, and the list of behaviors that need live-wire verification before the mock hardcodes them. None of these block the mock design; the TF-repo items are candidate cleanup tickets.

## Doc drift (`docs/concepts/tracked-events.md` vs code)

1. **`github:pr:mentioned` has no producer.** The doc says it fires for PRs discovered via `mentions:{user}` search; no `mentions:` query exists anywhere in the codebase. The event type is registered and routable but nothing can emit it. Fix the doc (or build the producer) — do NOT build a mock endpoint for it. TF has since built the producer: the tracker reads @-mentions out of the pull's body and its new comments and reviews (`internal/tracker/mentions.go`), which Orrery already serves, so it still needs no endpoint.
2. **Search-based discovery is described but gone.** GitHub discovery is per-repo REST enumeration with ETags; the only surviving search is the GraphQL dashboard backfill (terminal history, no events). A mock built from the doc would implement `GET /search/issues` and never see a call.
3. **`GET /user/teams` reads as universal but is local/PAT-only.** Multi/App orgs resolve reviewer known-ness from the DB (`team_github_groups` + `GET /orgs/{org}/teams`).
4. **The snapshot field list is incomplete.** Missing but used: `node_id` (the batch-refresh key), `head_committed_at`, `created_at`, `merged_at`, `closed_at`, the transient `Timeline`; also missing several diff-emitted event types (`new_commits`, `label_added/removed`, `review_request_removed`, `closed`).

## Dead code (do not mock; candidate deletions)

- `Client.SearchReviewRequested` (`pulls.go:999`), `Client.SearchUserPRs` (`status.go:251`), `Client.GetDashboardStats` (`dashboard.go:33`) — all REST `/search/issues`, zero non-test callers. Probably why the doc still describes search discovery.
- The gh injector's REST review observation arm (`injector.go:1011-1028`) cannot fire under the current gate (that shape is refused pre-forward). Kept deliberately per its comment.
- `ghAPI.SubmitReview` / `IPCClient.GithubSubmitReview` exist on the exec seam but no verb calls them (submission happens host-side via approval or auto-post posture).
- `ListUserReposPage`/`ListInstallationReposPage` doc comments describe a paginated repo-picker proxy that no longer exists (the picker reads the DB mirror).

## Probable bugs in TF found during extraction

1. **`.ghe.com` mishandled in credential validation.** `auth.ValidateGitHub` / `CaptureGitHubIdentity` hand-build `{base}/api/v3/user` instead of using `github.APIBase`, so a data-residency host would get `https://tenant.ghe.com/api/v3/user` instead of `https://api.tenant.ghe.com/user`. Main client is correct; validation diverges. Reading, not confirmed by test.
2. **`gh pr review-dismiss` omits `event: DISMISS`** (`pulls.go:731`) — real GitHub tolerates message-only; worth confirming on the wire (a strict mock would 422 it).
3. **`gh pr thread-view --page` is parsed and ignored**, and the promised review-comment fallback doesn't exist — only `/issues/comments/{id}` is ever requested.
4. **Double pagination walk per cycle**: the grant reconcile and the poll dispatch each independently walk `GET /installation/repositories` per installation, back to back, every cycle (acknowledged in a code comment). Not a bug, but 2× traffic the mock will observe — and a real optimization candidate.
5. **`internal/db/github_app_backfill.go` duplicates the base-URL derivation** to dodge an import cycle — drift risk noted in `baseurl.go` comments.

## Mock-relevant behavioral quirks (correct code, easy to get wrong in a mock)

- Two-path probe verbs (comment react/update/delete) retry the sibling REST path on ANY error from the first, not just 404 — a mock 500 on `/issues/comments/...` produces a second request to `/pulls/comments/...`.
- The tracker treats a 200 GraphQL response carrying `errors[]` as success when `data` is present; `null` node entries are skipped silently.
- `StatusContext` (legacy commit statuses) in the check rollup are decoded and dropped — a mock emitting them sees them ignored.
- REST `state` arrives lowercase (`"closed"`) and is upper-cased by the tracker; GraphQL states are already uppercase. Both forms are guarded.
- A permanently unreadable stub entity costs one `GET /pulls/{n}` every cycle forever (deliberate).
- The in-memory poll cursor resets on process restart — request ordering across restarts is not stable; don't assert on it.
- `gh pr create` hardcodes `draft: true` regardless of its `--draft` flag.
- Timeline is served under plain v3 JSON Accept (not the old preview media type).
- Repo renames rely on 301 + new `full_name` (see verification list).

## Live-wire verification list (behaviors the mock must copy from reality, not from TF's code)

Code reading tells us what TF *sends and reads*; these upstream behaviors need one capture session against real GitHub/Jira (the fake-GHE spike recipe: self-signed cert, `GH_HOST`, log method+path+body):

1. **Repo rename semantics**: 301 redirect behavior on `GET /repos/{old}`, and what `full_name` the followed response carries; same-host Authorization retention in Go's client.
2. **Review dismissal without `event`**: confirm real GitHub accepts `{"message"}` alone.
3. **GraphQL partial-error shapes**: capture a real FORBIDDEN `statusCheckRollup` response and a real `null` node in `nodes(ids:)` to fixture the exact envelope.
4. **`/installation/repositories` envelope** for large grants (does `total_count` count post-filter?), and installation-token 403 shape for PAT misuse.
5. **Actions log redirects**: the exact `Location` dance on `GET .../runs/{id}/logs` (302 to a blob store) and the zip layout the agent tooling unpacks.
6. **Secondary-rate-limit 403 body text** as currently emitted (TF matches substrings).
7. **Jira Cloud `/search/jql`**: real `nextPageToken`/`isLast` interplay and the 410 body of legacy `/search`.
8. **Jira ADF**: minimal ADF document Jira accepts for v3 comment/description bodies (TF's `wikiToADF` output should round-trip).
9. **timeline event payloads**: real shapes for `line-commented`, `head_ref_force_pushed`, `base_ref_changed` (sparsely documented).
10. **`gh` version drift**: re-run the pinned-gh conformance suite against the mock once it exists (rule 2's contract test covers this permanently).
11. **Teams and review requests** (Orrery's behavior below is chosen, not observed):
    - whether a team member's review clears the team's pending request (Orrery keeps it);
    - what GHES answers for adding someone outside the org to a team (Orrery refuses 422 with its own message);
    - the exact 422 bodies for requesting the pull's author and a non-collaborator;
    - whether deleting a team, or withdrawing a request, bumps the pull's `updated_at` (Orrery bumps it on a withdrawal and a request, not on a team deletion), which decides whether TF's tracker re-reads the pull;
    - whether GitHub refuses a review request on a closed or merged pull (Orrery accepts it);
    - `DELETE …/teams/{slug}/memberships/{username}` for a user not on the team: GitHub's reference lists only 204 and 403, so Orrery answers 204;
    - team slugs for names with non-ASCII letters: Orrery keeps ASCII letters and digits only, so "Équipe" becomes `quipe` and "日本" has no slug (422 `invalid` on `name`), where GitHub may transliterate; and two names with one slug ("Café", "Caf") collide as `already_exists` on `name`. No length limit is applied;
    - the error message for a negative `first`/`last` on a GraphQL connection (Orrery's wording is its own).
