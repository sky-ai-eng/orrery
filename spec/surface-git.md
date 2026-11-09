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
- a deleted branch closes the open pull requests on it, and a deleted head branch pushed back is recorded on the closed ones (below);
- the repository's `pushed_at` moves to the world clock; and
- the first push of a branch into an empty repository sets the default branch, and `HEAD` with it, as on GitHub.

The entries and the state they describe land in one commit, after every ref the push moved is in place, so a reader that can see any of a push's entries sees the repository, its refs, and its pull requests as the push left them. Opening, closing, reopening, and merging a pull request take the same per-repository lock, so the pull requests a push re-pins are the ones open on its branch when it lands.

A push that fails before it is recorded moves back every ref it moved, appends nothing, and answers `500`, whether its record fails, `git http-backend` fails after `git-receive-pack` moved refs (it exits with an error, or its output has no CGI header block), or Orrery panics while serving the push. Each branch the push moved, created, or deleted returns to where it was, `HEAD` to the default branch if the push set it, and each pull request ref it re-pinned to the head the pull request still has. A retry of the push then finds the refs the failed push found, and lands and is recorded as if the failed push had not happened. A ref that cannot be moved back is logged. So is a push whose refs cannot be read after `git-receive-pack` runs, which moves nothing back, because which refs it moved is unknown.

A push that `git http-backend` answers with a status other than `200` is passed through and not recorded. http-backend sends its status before it starts `git-receive-pack`, so such a push never reached it and moved nothing.

A commit's author and committer dates come from the client that made it, not from Orrery. A script that commits sets them from the world clock first (`orrery env`, `usage.md`), or replaying it produces different commit ids.

## What a fetch sees

Every other git request, a fetch's ref advertisement and pack in particular, holds the same per-repository lock for read. A push, a merge, opening a pull request, and an edit of one that sets `state` to `open` or `closed`, or sets `base`, hold it for write, even when the edit leaves the state or base as it was. A fetch request therefore sees the repository as it was before a write or after the write's record commits, never in between: not a write's refs ahead of its entries, and not the refs that a failed push, or a merge whose record failed, moves back. Any other edit of a pull request, such as one of only its title or body, reads and moves no ref and holds the lock for read, as a fetch request does: fetch requests and the edit don't wait on each other, but the edit waits for a write that runs or waits, so it never lands between a write's read of the pull request and that write's record. A merge's commit message therefore carries the title the pull request had when it merged.

- Fetch requests to a repository don't wait on each other.
- A write waits for the fetch requests already running, and a fetch request that arrives while a write runs or waits waits for it.
- The lock is held while `git http-backend` runs, not while its answer is sent, since the face buffers the answer whole.
- A push that has started runs to its end, holding the lock and recording what it moved or moving it back, even when its client goes away.

A fetch is several requests (the ref advertisement, then the pack, and with protocol v2, `ls-refs` and `fetch` as separate POSTs), and a write can land between two of them. `uploadpack.allowAnySHA1InWant` is on, so a fetch that asks for a commit a ref has since moved off still gets it.

The GitHub face's reads of a repository's refs hold the same lock for read, and see the refs the same way. They are:

- the REST reads of branches, a branch's ref, a compare, contents, check runs for a branch name, a pull request, and a pull request's files; and
- the GraphQL fields `Repository.ref`, and a pull request's `mergeable`, `additions`, `deletions`, `changedFiles`, and `files`.

A REST read and a GraphQL query also hold the world write gate for read for the whole request, as git traffic does, so a snapshot restore never replaces the repository under one. A read that also reads a pull request reads it under the same hold as the branch tips it uses, so they come from one state: the REST pull request, its files, and each of those pull request fields. A GraphQL field holds the lock only for its own reads, so a write can land between two fields of one query, as between two requests of a fetch.

## Deleting and restoring a branch

A push that deletes a branch changes the pull requests on it as GitHub does when the deletion is a push:

- Every open pull request whose head is the branch closes. Then every pull request whose head it is, closed and merged ones included, records the deletion. So the timeline of each pull request it closes gains a close and then a head deletion, and across the pull requests every close comes first.
- Every open pull request whose base is the branch records the deletion and then closes, one pull request at a time. A closed pull request whose base it is records nothing.
- Nothing is retargeted, even when the branch was the head of a merged pull request. GitHub documents a retarget onto the merged pull request's base, but it does not happen when the deletion is a push.
- A pull request keeps its head and base branches and their shas, and a draft stays a draft. Its `refs/pull/{n}/head` stays where it was.

A push that creates a branch records its restoration on every closed pull request whose head it is, merged ones included. They stay closed, and a reopen is then judged by the rules in `surface-github.md`.

When one push deletes or creates several branches, they are handled in ref-name order, and a deleted branch's head pull requests before its base pull requests. Every close and every timeline item is attributed to the pusher and moves the pull request's `updated_at`. Each close appends a `pull.close` entry, after all of the push's `ref.update` entries, in the order the pull requests close. The timeline items have no entries of their own: each follows from a `ref.update` entry and the states of the pull requests on its branch, so replaying the log rebuilds them. They land in the push's commit with its entries. GraphQL serves them as `timelineItems` (`surface-github.md` §3.3).

## Pull request refs

Every pull request has a ref, `refs/pull/{n}/head`, at its head, as on GitHub, so a client fetches a pull request by number without knowing its head branch (`git fetch origin pull/7/head`). It always names the pull request's pinned head commit:

- creating the pull request sets it;
- it moves whenever the head does: on a push to the head branch, on the merge of another pull request into it, and on reopening the pull request, which takes the head branch's tip as its head (`surface-github.md`); and
- closing or merging the pull request, or deleting its head branch, leaves it where it is.

The ref is advertised to fetches, so `git ls-remote` lists it, and hidden from pushes. A push that creates or moves any ref under `refs/pull/` is refused, as GitHub refuses it: `git` reports `deny updating a hidden ref`, and nothing moves.

Orrery moves these refs itself, so they have no `ref.update` entries in the action log. Each one follows from the pull request's `pull.open` and `pull.reopen` entries and from the `ref.update` entries of its head branch, so replaying the log rebuilds it. A diff reads refs from the repositories themselves, so its `ref_changes` lists them with the rest.

## Not served

- `refs/pull/{n}/merge`, GitHub's test merge of a pull request into its base.
- SSH, the `git://` protocol, `git-upload-archive`, and Git LFS.
