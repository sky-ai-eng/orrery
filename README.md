<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/imgs/lockup-display-on-dark.svg">
    <img src="docs/imgs/lockup-display-on-light.svg" alt="Orrery" width="360">
  </picture>
</h1>

<p align="center"><em>(ORR-uh-ree — a clockwork model of a world: turn the crank and everything advances deterministically, in lockstep, while observers watch.)</em></p>

<p align="center">A local GitHub for testing software that acts on GitHub.</p>

Orrery serves GitHub's REST and GraphQL APIs and real git from one directory, on a clock you control. Point a bot, a poller, a CI integration, or a coding agent at it and it behaves like GitHub, except that you can snapshot the world, fork it, advance time by an hour in one call, and ask exactly what changed.

- **Real git.** Clone, fetch, and push are served by `git http-backend` over bare repositories, so unmodified `git` works against it.
- **Same inputs, same world.** Every timestamp comes from the world's clock and every id is derived rather than random, so replaying the same calls reproduces the same commit SHAs.
- **The diff is the result.** Snapshot, run the thing under test, then diff: pull requests opened and merged, reviews posted, labels added, refs moved, each attributed to an actor. Whether something happened is answered from the world's state, so the same scenario works as an integration test or as an agent benchmark.

Orrery implements the parts of GitHub its users have needed so far, and grows as they need more. An endpoint it doesn't implement returns `501` naming the path instead of a `404`, and [contributions are welcome](CONTRIBUTING.md). A world never makes a network call of its own.

## Quick start

Download the binary for your platform from the [latest release](https://github.com/sky-ai-eng/orrery/releases); [Install](docs/users/install.md) covers every platform and how to verify the download. `git` must be on `PATH`.

```bash
gh release download --repo sky-ai-eng/orrery --pattern 'orrery_*_linux_amd64.tar.gz'
tar xzf orrery_*_linux_amd64.tar.gz orrery
```

Serve a world. An instance is one config file and the directory it names; [`orrery.example.toml`](orrery.example.toml) lists every setting.

```bash
printf '[instance]\ndir = "./instance"\n' > orrery.toml
./orrery serve --config orrery.toml
```

In another terminal, make an actor, take a snapshot, act through GitHub's API and git, and ask what changed:

```bash
# control plane: an actor and its token, then a snapshot to diff against
TOKEN=$(curl -s -X POST localhost:4180/_orrery/actors -d '{"login":"alice","kind":"user"}' | jq -r .token)
curl -s -X POST localhost:4180/_orrery/snapshots -d '{"name":"before"}'

# face: GitHub's REST API and real git
curl -s -X POST localhost:4180/api/v3/user/repos -H "authorization: token $TOKEN" -d '{"name":"webapp"}'
git clone http://x-access-token:$TOKEN@localhost:4180/alice/webapp.git && cd webapp
eval "$(../orrery env)"   # date commits at the world's clock, not your machine's
git commit --allow-empty -m 'first commit' && git push origin HEAD:main

# control plane: everything that happened since the snapshot
curl -s -X POST localhost:4180/_orrery/diff -d '{"from":"before"}'
```

## How it works

Orrery has two doors, and every operation goes through exactly one of them.

- **The face** is GitHub: the REST API under `/api/v3`, GraphQL at `/api/graphql`, git at `/{owner}/{repo}.git`, and the OAuth web flow under `/login/oauth`. Orrery presents itself as a GitHub Enterprise Server, so a client that takes a GitHub base URL needs nothing else. Every call is made by an actor holding a token, including CI, which on GitHub is an App posting check runs.
- **The control plane**, under `/_orrery`, holds what GitHub has no API for: creating actors and issuing their tokens, setting their email addresses, registering OAuth apps, the clock, snapshots, restore, and diff.

A world is a directory: a SQLite database and bare git repositories. A snapshot is a copy of it, `orrery fork` turns a snapshot into a new instance, and deleting the directory deletes the world. Every credential in a world is fake, so a snapshot is safe to share.

## Documentation

- [Documentation index](docs/README.md): installing, verifying a release, and contributing
- [Usage model](spec/usage.md): the two doors, the clock, snapshots, forks, and what a diff reports
- [GitHub surface](spec/surface-github.md): every endpoint and GraphQL field Orrery serves, and how it behaves
- [Design rules](spec/README.md): the rules every change to Orrery follows

## License

Source-available under the [Orrery License 1.0](LICENSE): free to use, copy, and modify for your own internal business purposes, including running it to develop and test software you ship, but not to redistribute, to offer as a hosted or managed service, or to build any part of it into a product you provide to others. See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution terms.

## Disclaimer

Orrery is provided "as is", without warranty of any kind; see the [LICENSE](LICENSE) for the full terms.
