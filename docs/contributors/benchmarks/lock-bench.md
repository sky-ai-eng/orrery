# Repository lock benchmark

`lock-bench` (`crates/orrery-bench`) measures what the per-repository lock costs. Git reads hold it for read: a fetch's requests, and the GitHub face's reads of a repository's refs. Writes hold it for write: a push, a merge, opening a pull request, and an edit of one that sets its `state` or `base`. Any other edit of a pull request, such as one of only its title or body, holds it for read. So a write waits for the reads in progress, and a read waits for a write in progress (`spec/surface-git.md`, "What a fetch sees"). The bench measures both waits with real `git`, so a change to the lock, or to what runs under it, can be judged by numbers. Run it with `./scripts/lock-bench.sh`.

It is not part of any workflow and nothing runs it automatically. Run it by hand when you change the lock, the git face's request handling, or the work a write does under the lock.

## What it runs

Each `--engine` is an `orrery` binary, and each scenario runs every engine in turn, one at a time. Each engine gets a world of its own:

- `orrery serve` on a free port;
- one actor, `alice`; and
- her public repository, `alice/bench`, seeded by pushing the payload's `main`.

All traffic is real `git` against the served world, plus REST calls to open and merge pull requests.

A payload is a repository generated from `--seed`, so its name means the same bytes on every machine. Payloads are built once and kept under `target/lock-bench/payloads/`.

| Payload | Contents |
| --- | --- |
| `tiny` | One README. |
| `src<MB>` | One commit of that many MB of source-like text, in files of 2 to 64 KiB, 64 to a directory. It compresses about as well as real source. |
| `bin<MB>` | One commit of that many 1 MiB files of random bytes, which don't compress. |
| `src<MB>+bin<MB>` | Both, as two commits. |

The scenarios:

- **`hold`**: one bare clone at a time. It reports each clone's wall time and its pack request time. The pack request time is the time the clone held the repository's lock for read.
- **`writers`**: rounds of three writes, first with nothing else running, then while `--cloners` clients clone back to back. Each round:
  1. pushes a one-file branch off the seeded `main`;
  2. opens a pull request for it; and
  3. merges it.

  Each round starts after a seeded pause of up to 300 ms, so the writes land at every point of a concurrent clone. The busy phase's writes are the ones that can wait for a clone.
- **`fanout`**: `--parallel` clones started together. It reports the time until all of them have finished. If reads waited on each other, this would grow with the number of clones.
- **`big-push`**: `git ls-remote` back to back while one push of `--push-mb` MB of random data runs. It reports the push's time and each `ls-remote`'s, which shows how long a read waits behind a large write.

A clone's pack request time is read from `GIT_TRACE_CURL`. It runs from the moment the upload-pack POST goes out to the response's status line. The git face buffers `git http-backend`'s answer whole before sending it, so that span is:

- the time the server spent under the lock;
- the time spent sending the request, which is small for a clone; and
- any wait for a write to finish.

With no writes running, as in `hold`, it is the time the clone held the lock.

## Running it

```bash
./scripts/lock-bench.sh
```

```bash
./scripts/lock-bench.sh --scenario writers --payloads src40,src168 --rounds 40
```

```bash
./scripts/lock-bench.sh --scenario hold --payloads tiny --runs 3
```

With no `--engine`, the script builds this tree's `orrery` in release and benchmarks it alone. Each report prints to stdout and is archived under `bench-results/`. See `cargo run -p orrery-bench --bin lock-bench -- --help` for every flag.

The useful flags:

- **`--engine NAME=PATH`**: an engine build to compare. Repeat it to compare builds.
- **`--payloads`**: comma-separated payload names. The default is `tiny,src5,src40,src168,src168+bin150`.
- **`--runs`, `--rounds`, `--cloners`, `--parallel`, `--push-mb`**: the size of each scenario.
- **`--seed`**: the payloads' contents and the writers' pauses.

To compare against another commit, build it in a worktree and name both builds:

```bash
git worktree add /tmp/orrery-main origin/main
```

```bash
(cd /tmp/orrery-main && cargo build --release -p orrery)
```

```bash
./scripts/lock-bench.sh --engine main=/tmp/orrery-main/target/release/orrery --engine this=target/release/orrery
```

## Reading the report

Times are milliseconds, given as p50 / p90 / max, each the nearest rank, except where a column says otherwise.

- **`repo MB`**: the size of the served repository's object store after seeding.
- **`pack request`**: in `hold`, the time a clone held the lock for read. A write that arrives while a clone runs waits for at most the rest of it. In `writers`, it also counts the clones' own waits behind writes, which is why that column says "with waits". Fetch requests share the read side, so the longest one in progress is what a write waits for, not their sum.
- **`push busy`, `open busy`, `merge busy`**: the writes while clients clone. Compared with `push idle`, they show what the waits cost. The p50 barely moves, because most writes don't land during a pack request. The max is about one pack request.
- **`all done`**: for `fanout`, the wall time until the last clone finished.
- **`ls-remote during push`**: a read's wait behind a push. Only the server's `git-receive-pack` run holds the lock. The request body arrives before the lock is taken, so a client's own packing time doesn't count.

## Recorded results

Recorded 2026-11-09 on a 4-CPU Linux VM with 15 GB of memory and git 2.43.0. The client and the server ran on the same machine. Defaults throughout: seed 1, 5 runs, 20 rounds, 2 cloners, 4 parallel clones, and a 150 MB push.

The three engines:

- **`main`**: e73434d. Fetches take no lock, and writes hold it exclusively.
- **`this`**: the commit that adds this bench. Git reads hold the lock for read, and writes hold it for write.
- **`exclusive`**: `this` with every git request holding the lock for write, the alternative to a read-write lock. Its `serve` takes `lock.write()` for every request.

These are single runs on a machine other work shares, so differences under about 20% between engines are noise. Request and byte counts aren't recorded; times are indicative.

### One clone at a time (`hold`)

| Payload | Repo MB | Clone p50 | Pack request p50 / p90 / max (`this`) |
| --- | --- | --- | --- |
| `tiny` | 0.0 | 34 | 7 / 7 / 7 |
| `src5` | 1.4 | 107 | 15 / 20 / 20 |
| `src40` | 11.4 | 525 | 64 / 83 / 83 |
| `src168` | 47.9 | 2237 | 314 / 334 / 334 |
| `src168+bin150` | 197.9 | 4451 | 971 / 1290 / 1290 |

The pack request time is the same in all three engines, to within noise. It is a quarter or less of a clone's wall time; the rest is the client indexing what it received, with no lock held.

### Writes while two clients clone back to back (`writers`)

Push latency, p50 / p90 / max. An idle push's p50 is 45 to 55 ms in every engine.

| Payload | `main` | `this` | `exclusive` |
| --- | --- | --- | --- |
| `tiny` | 51 / 55 / 62 | 51 / 64 / 74 | 59 / 73 / 74 |
| `src5` | 52 / 56 / 68 | 52 / 61 / 75 | 61 / 82 / 93 |
| `src40` | 50 / 57 / 59 | 53 / 117 / 142 | 60 / 132 / 168 |
| `src168` | 56 / 65 / 89 | 61 / 228 / 408 | 59 / 107 / 319 |
| `src168+bin150` | 59 / 83 / 216 | 56 / 189 / 1365 | 55 / 938 / 1209 |

- With the read-write lock, a push's p50 doesn't move.
- Its max is about one pack request plus the push itself:
  - 408 ms on `src168`, whose pack requests take 314 to 334 ms alone (`hold`), and an idle push about 55 ms; and
  - 1365 ms on `src168+bin150`, whose pack requests take 971 to 1290 ms alone.
- Opening and merging a pull request wait the same way. On `this`, the worst was 44 ms to open (`src5`) and 160 ms to merge (`src168+bin150`).
- `main`'s writes wait for nothing. Its tail is the machine's CPUs, shared with the clones.

### Clones started together (`fanout`)

Time until all 4 have finished, ms:

| Payload | `main` | `this` | `exclusive` |
| --- | --- | --- | --- |
| `tiny` | 65 | 73 | 112 |
| `src5` | 159 | 165 | 201 |
| `src40` | 673 | 641 | 880 |
| `src168` | 2707 | 2682 | 2882 |
| `src168+bin150` | 5936 | 5832 | 8231 |

Reads that share the lock cost nothing here. `exclusive` makes the clones' pack requests take turns, and all four finish 7% to 53% later.

### Reads during a large push (`big-push`)

One push of 150 MB of random data, with `git ls-remote` back to back. The push took 11.3 to 11.9 s end to end in all three engines; most of that is the client compressing.

| | `main` | `this` | `exclusive` |
| --- | --- | --- | --- |
| `ls-remote` p50 / p90 / max | 19 / 46 / 203 | 20 / 46 / 1974 | 21 / 47 / 2008 |

- The read side waits for the server's `git-receive-pack` of the received pack: about 2 s for 150 MB.
- On `main`, reads didn't wait for writes at all.

### What the numbers decide

A write waits for at most about one fetch request, not for a whole clone:

- The lock covers only the server's own work, since the git face buffers every answer whole before sending it.
- Fetch requests share the read side.
- A fetch request that arrives behind a waiting write waits for it.

At these sizes, that wait is under 0.5 s for 48 MB of packed source and about 1.5 s for 200 MB that is mostly binaries. Medians don't move. The read side's new wait behind a push is about the server's `git-receive-pack` time.

Both waits are acceptable for the repository sizes the git face is built for: it caps a push's body at 256 MB and holds each answer in memory. Repositories of several GB, where a pack takes many seconds to generate, would need a different design.
