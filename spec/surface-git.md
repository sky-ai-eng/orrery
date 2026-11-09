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
| `receive.hideRefs = refs/pull` | A push to a pull request's ref is refused, as on GitHub ("Pull request refs") |
| `gc.auto = 0` | Object files are never rewritten, so a snapshot can share them with the live world |

Every `git` the engine runs, `http-backend` included, runs hermetically: `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_SYSTEM=/dev/null`, and `GIT_TERMINAL_PROMPT=0`, so the operator's own git configuration never changes a world.

## What a push does

`git-receive-pack` runs under a per-repository lock, and Orrery compares the repository's refs before and after it. For each ref that changed, in ref-name order, it appends one `ref.update` entry to the action log, attributed to the pushing actor, naming the ref and its old and new object ids. With them:

- an open pull request whose head branch moved takes the new tip as its head, GitHub's `synchronize`, and its `refs/pull/{n}/head` moves with it; a deleted branch leaves its pull requests on their last head;
- the repository's `pushed_at` moves to the world clock; and
- the first push of a branch into an empty repository sets the default branch, and `HEAD` with it, as on GitHub.

The entries and the state they describe land in one commit, after every ref the push moved is in place, so a reader that can see any of a push's entries sees the repository, its refs, and its pull requests as the push left them. Opening, closing, reopening, and merging a pull request take the same per-repository lock, so the pull requests a push re-pins are the ones open on its branch when it lands.

A commit's author and committer dates come from the client that made it, not from Orrery. A script that commits sets them from the world clock first (`orrery env`, `usage.md`), or replaying it produces different commit ids.

## Pull request refs

Every pull request has a ref, `refs/pull/{n}/head`, at its head, as on GitHub, so a client fetches a pull request by number without knowing its head branch (`git fetch origin pull/7/head`). It always names the pull request's pinned head commit:

- creating the pull request sets it;
- it moves whenever the head does: on a push to the head branch, and on the merge of another pull request into it; and
- closing or merging the pull request, or deleting its head branch, leaves it where it is.

The ref is advertised to fetches, so `git ls-remote` lists it, and hidden from pushes. A push that creates or moves any ref under `refs/pull/` is refused, as GitHub refuses it: `git` reports `deny updating a hidden ref`, and nothing moves.

Orrery moves these refs itself, so they have no `ref.update` entries in the action log. Each one follows from the pull request's `pull.open` entry and from the `ref.update` entries of its head branch, so replaying the log rebuilds it. A diff reads refs from the repositories themselves, so its `ref_changes` lists them with the rest.

## Not served

- `refs/pull/{n}/merge`, GitHub's test merge of a pull request into its base.
- SSH, the `git://` protocol, `git-upload-archive`, and Git LFS.
