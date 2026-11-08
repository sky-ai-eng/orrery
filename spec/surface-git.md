# Git surface

Orrery serves git's smart-HTTP protocol by running the real `git http-backend` over bare repositories (rule 2). It reimplements nothing in the pack protocol, so what `git` can do over smart-HTTP against GitHub it can do against Orrery, within the limits below.

## Paths

A repository answers at `/{owner}/{repo}` and `/{owner}/{repo}.git`: exactly two path segments, GitHub's shape. The owner and name are matched against the repository's row, and git is always run against the row's own spelling. Three requests make up the protocol:

| Request | Purpose |
| --- | --- |
| `GET /{owner}/{repo}[.git]/info/refs?service=git-upload-pack` or `?service=git-receive-pack` | Ref advertisement. Any other `service`, or none, is `400`: Orrery serves smart HTTP only. |
| `POST /{owner}/{repo}[.git]/git-upload-pack` | Clone and fetch |
| `POST /{owner}/{repo}[.git]/git-receive-pack` | Push |

An unknown repository is `404`. A request body is capped at 256 MiB.

## Authentication

An actor authenticates with HTTP Basic, its token as the password, the way GitHub documents for tokens (`x-access-token:<token>`; the username is not checked). Without credentials, clone and fetch of a public repository succeed unless the instance sets `anonymous_git_reads = false`, which a world whose faces are on the public internet does. A push, or any request for a private repository, without credentials is `401`, not `404`, so `git` knows credentials would change the answer.

## The bare repository recipe

Each repository is a bare repository at `<world>/repos/<owner>/<repo>.git`, created with its default branch as `HEAD` and configured with:

| Setting | Why |
| --- | --- |
| `uploadpack.allowFilter = true` | Partial clones (`git clone --filter=blob:none`) |
| `uploadpack.allowAnySHA1InWant = true` | A partial clone fetches missing blobs by object id during checkout |
| `http.receivepack = true` | `git http-backend` refuses pushes without it |
| `gc.auto = 0` | Object files are never rewritten, so a snapshot can share them with the live world |

Every `git` the engine runs, `http-backend` included, runs hermetically: `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, and `GIT_TERMINAL_PROMPT=0`, so the operator's own git configuration never changes a world.

## What a push does

`git-receive-pack` runs under a per-repository lock, and Orrery compares the repository's refs before and after it. For each ref that changed it appends one `ref.update` entry to the action log, attributed to the pushing actor, naming the ref and its old and new object ids. Then:

- an open pull request whose head branch moved takes the new tip as its head, GitHub's `synchronize`; a deleted branch leaves its pull requests on their last head;
- the repository's `pushed_at` moves to the world clock; and
- the first push of a branch into an empty repository sets the default branch, as on GitHub.

A commit's author and committer dates come from the client that made it, not from Orrery. A script that commits sets them from the world clock first (`orrery env`, `usage.md`), or replaying it produces different commit ids.

## Not served

- `refs/pull/{n}/head` and `refs/pull/{n}/merge`. GitHub keeps both for every pull request; Orrery doesn't create them yet, so a client that fetches a pull request by number finds no such ref.
- SSH, the `git://` protocol, `git-upload-archive`, and Git LFS.
