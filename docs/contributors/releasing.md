# Releasing

Releases are cut by [release-please](https://github.com/googleapis/release-please) from the pull request titles that land on `main`, and built by [GoReleaser](https://goreleaser.com/) when the release's tag is pushed. Nobody edits a version number or the changelog by hand.

## From a title to a release

1. A pull request merges to `main`. Its title, a conventional commit (`feat(face): serve GET /repos/{o}/{r}/branches`), is the commit release-please reads.
2. `release-please.yml` keeps one Release PR open and updates it on every push to `main`. It bumps `workspace.package.version` in `Cargo.toml` (every crate inherits it), the workspace's own entries in `Cargo.lock`, and `.github/release-please-manifest.json`, and adds the new titles to `CHANGELOG.md`.
3. Merging the Release PR creates the GitHub release and pushes its `vX.Y.Z` tag.
4. The tag runs `release.yml`. It runs the dependency license gate (`scripts/license-check.sh`) first, then GoReleaser builds `orrery` for Linux and macOS on x86_64 and arm64, writes an SPDX SBOM per archive, signs `checksums.txt` with cosign, and attaches everything to the release. The same tag runs `linear-release.yml`, which records the release in Linear and builds nothing.

`feat`, `fix`, and `perf` titles appear in the changelog. `chore`, `ci`, `docs`, `refactor`, and `test` titles don't, and a run of only those doesn't produce a Release PR.

## Which number moves

While Orrery is `0.y.z`, a title with `!` after its scope (`feat(serve)!: …`) moves `y`, and every other title moves `z`. A change takes `!` when it breaks an existing caller of:

- a command or flag,
- `--json` output or the ready line `orrery serve` prints,
- a control-plane route,
- the instance config file, or
- a world an earlier release wrote.

New GitHub surface, and any change that brings the face closer to GitHub, is not breaking: GitHub's behavior is what the face promises. [Install](../users/install.md#versions) states the same promise for users.

`.github/release-please-config.json` holds the rules: `bump-minor-pre-major` and `bump-patch-for-minor-pre-major` produce the behavior above, and `initial-version` set the first release at 0.1.0. Orrery reaches 1.0.0 only when that file gains `"release-as": "1.0.0"`.

The release type is `simple`, not `rust`, because release-please's `rust` type treats the root `Cargo.toml` as a package and refuses a virtual workspace. The config's `extra-files` bump the two versions above instead.

## The release token

`release-please.yml` authenticates with the `RELEASES_TOKEN` secret, a personal access token, rather than the workflow's own `GITHUB_TOKEN`. GitHub doesn't start workflows from events a `GITHUB_TOKEN` causes, so a tag pushed with it would never run `release.yml`.

The token must be:

- a fine-grained token whose resource owner is `sky-ai-eng`, with access to this repository,
- granted **Contents** and **Pull requests**, both read and write, and
- approved by an organization owner, under Organization settings → Third-party Access → Personal access tokens → Pending requests. Editing the token's permissions or repositories sends it back for approval.

The account that owns the token also needs the **Write** role on this repository. A fine-grained token can't do more than its account can, and the read access every organization member has isn't enough.

A misconfigured token fails in one of two ways:

| Error in the release-please run | Cause |
| --- | --- |
| `Missing required manifest config: .github/release-please-config.json` | The token can't see the repository. GitHub answers 404 for a repository a token can't access, and release-please reports a 404 as a missing file. The token is unapproved, owned by the wrong resource owner, or not granted this repository. |
| `Resource not accessible by personal access token` on `POST …/git/refs` | The token can read but not write: it lacks Contents read and write, or its account lacks Write on the repository. |

## Tagging by hand

If release-please can't run, pushing a `vX.Y.Z` tag runs `release.yml` and `linear-release.yml` all the same. A hand-pushed tag doesn't update `Cargo.toml`, `Cargo.lock`, the manifest, or `CHANGELOG.md`, so land a pull request that sets them to the tagged version before the next Release PR, or it proposes the same number again.
