# Git protocol surface

Extracted at TF commit `78c8bfd2` (2026-08-21). Bottom line: **plain `git http-backend` over real bare repos is sufficient**, proven by two in-tree fixtures that drive the production code paths (`internal/worktree/blobless_auth_test.go:24-52`, `internal/ghinjector/sharedorigin_git_test.go:36-55`). No go-git/libgit2 anywhere — every operation execs the `git` binary. No submodules, no LFS, no shallow clones (`--depth` appears nowhere), no tags refspecs, no `git-upload-archive`.

## The server recipe

1. **Layout:** `GIT_PROJECT_ROOT=<root>` with bare repos at `<root>/<owner>/<repo>.git`. Must answer both `/owner/repo/...` and `/owner/repo.git/...` (TF's clone URLs carry `.git`; the proxy gate strips it).
2. **Per-repo config, every bare:**
   - `uploadpack.allowFilter = true` — mandatory; every TF clone is `--filter=blob:none`.
   - `uploadpack.allowAnySHA1InWant = true` — mandatory; lazy promisor fetches request blobs by OID during `worktree add` / `checkout` / `reset --hard`.
   - `http.receivepack = true` — mandatory for pushes (`GIT_HTTP_EXPORT_ALL` does not enable it).
   - A meaningful `HEAD` symref — default-branch detection reads the clone's HEAD (`worktree_branch.go:884`); a missing one falls back to literal `"main"`.
3. **`refs/pull/{N}/head` must exist** on the upstream bare for every reviewable PR (seeded by pushing `HEAD:refs/pull/{N}/head`). This is how fork PRs are fetched without touching the fork.
4. **Fork PRs:** the head repo needs its own path with `http.receivepack=true` — the push remote points at `headCloneURL`.
5. **Auth:** accept `Authorization: Basic base64("x-access-token:" + <token>)`. Behind TF's gitproxy the mock's own auth is irrelevant (the proxy injects).
6. **URL shape constraints (hardcoded in TF):** exactly two path segments `/{owner}/{repo}[.git]` (the proxy gate rejects anything else — no sub-path mounting); owner/repo match `[A-Za-z0-9._-]+`; `gitproxy.Upstream` must be scheme+host only and https unless loopback. Host is fully configurable via org `GitHubBaseURL`; path layout is not.

## Operations the server will see

| Op | Command shape | When |
| --- | --- | --- |
| Bare clone | `git clone --bare --filter=blob:none <cloneURL> <dir>` (`worktree.go:553`) | first need per repo (run setup, curator seed, bootstrap warm, restore, bundle import) |
| PR fetch | `git fetch origin +refs/pull/N/head:refs/heads/triagefactory/<rootKey>/pr-N +refs/pull/N/head:refs/remotes/origin/...` (`worktree_pr.go:138`) | every PR-run materialization |
| Base refresh | `git fetch origin +refs/heads/<base>:refs/remotes/origin/<base>` (`worktree_pr.go:156`, best-effort) | PR materialization with a known base |
| Branch fetch | same refspec shape into `refs/remotes/origin/*` (5 sites: branch/checkout worktrees, curator pinned ×2) | workspace add, every curator turn (local) / 0→1 reader (multi shared) |
| Promisor fetches | git-initiated `fetch` during `worktree add` / `checkout` / `reset --hard` (≈10 sites) | constantly; auth rides the inherited `GIT_CONFIG_*` env |
| ls-remote | `git ls-remote --exit-code --heads origin <head>` (`cmd/exec/gh/pr.go:1020`) | every `pr create`; non-zero exit hard-fails |
| Push | agent-originated only — **TF Go never pushes** | through the proxy (multi + managed local) or gated by the pre-push hook (unmanaged local) |

## Credential delivery (host side)

Never argv, never persisted into `remote.origin.url` — always process-env `GIT_CONFIG_COUNT/KEY_n/VALUE_n`:
- Token mode: `http.https://<host>/.extraHeader = Authorization: Basic base64("x-access-token:"+token)` (no-op for non-https URLs → SSH remotes silently get no injection).
- Proxy mode: `url.<proxy>/.insteadOf = <upstream>/` + `http.<proxy>/.extraHeader = ... base64("x-run:"+placeholder)`.
- `GIT_TERMINAL_PROMPT=0` forced; `GIT_ASKPASS`/`SSH_ASKPASS` stripped; `HOME` inherited (operator `~/.gitconfig` IS read by host-side git — see below).

## The per-run git proxy (`internal/gitproxy`)

- Path allowlist: only `GET .../info/refs`, `POST .../git-upload-pack`, `POST .../git-receive-pack` on two-segment paths; all else 403 pre-token.
- Inbound auth: per-run token as the Basic password (username conventionally `x-run`), constant-time.
- Outbound: sets `Authorization: Basic base64("x-access-token:"+real)`; strips forwarding headers. CONNECT → 501 (agent remote URLs are plain `http://` at the proxy address, or ride the shared TLS origin).
- Receive-pack gate: pkt-line command block buffered (64 KiB cap, overflow fails closed), every ref checked against `Decision.AllowedRefs` (= `refs/heads/<PushTargetBranch>` minus `pushpolicy.ProtectedFor` — always main+master + profile default/base branch, modulated by team `BaseBranchPushPolicy`), **all deletes rejected**, body reconstructed byte-identically, upstream status recorded per ref.
- Multi mode: the proxy handler is re-mounted behind the gh injector's port-443 TLS listener so one fake-GHE origin serves REST + GraphQL + git (routing: `/api/v3`+`/api/graphql` prefixes win, then smart-HTTP path suffixes, before API auth).

## Ambient escapes (local mode only — mock design should route around them by always configuring HTTPS clone URLs)

1. Local curator pinned-repo refresh is fully uncredentialed (`cloneTokenFor` returns `""` in local) — an `ssh_url` row means real ambient SSH.
2. Local delegated clone falls back to SSH URLs when the managed git channel isn't up.
3. Local bootstrap warm injects the raw PAT directly (not proxied) and goes ambient on SSH URLs; project-bundle import clones with no auth options at all.
4. `PreflightSSH` diagnostics run real `ssh -T` with `StrictHostKeyChecking=accept-new` (writes operator `known_hosts`).

One synthesized clone URL exists (`workspace_snapshot.go:657`): `<orgBase>/<owner>/<repo>.git` — host configurable, path shape hardcoded.
