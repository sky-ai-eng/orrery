# Install

Each [release](https://github.com/sky-ai-eng/orrery/releases) carries an `orrery` binary for Linux and macOS, on x86_64 and arm64, one `tar.gz` per platform:

| Platform | Archive |
| --- | --- |
| Linux, x86_64 | `orrery_<version>_linux_amd64.tar.gz` |
| Linux, arm64 | `orrery_<version>_linux_arm64.tar.gz` |
| macOS, Intel | `orrery_<version>_darwin_amd64.tar.gz` |
| macOS, Apple silicon | `orrery_<version>_darwin_arm64.tar.gz` |

The Linux binaries are static, so they run on any distribution. `git` must be on `PATH` wherever Orrery runs, because it serves git with the real `git http-backend`. A world that is pushed SSH-signed commits also needs `ssh-keygen` from OpenSSH 8.2p1 or later on `PATH`, because it verifies each signature with `git verify-commit`, which runs `ssh-keygen`. Without it, such a push fails with `500` and moves nothing, and so does a read of such a commit that its push stored no verdict for. On Debian and Ubuntu it is in `openssh-client`; macOS ships it.

```bash
gh release download --repo sky-ai-eng/orrery \
  --pattern 'orrery_*_linux_amd64.tar.gz' --pattern 'checksums.txt*'
tar xzf orrery_*_linux_amd64.tar.gz orrery
./orrery --version
```

`orrery --version` prints the version and the full commit the binary was built from.

Each archive also holds `spec/event.schema.json`, the JSON Schema of the events a world's log records, for validating what `log/list` and `log/stream` serve without a world running. A served world answers the same document at `GET /_orrery/log/schema`.

## Verifying a download

`checksums.txt` holds the sha256 of every archive. It is signed keylessly with [cosign](https://docs.sigstore.dev/) through GitHub Actions' OIDC identity, so there is no key to fetch first. With the archive, `checksums.txt`, `checksums.txt.sig`, and `checksums.txt.pem` in one directory:

```bash
cosign verify-blob --certificate checksums.txt.pem --signature checksums.txt.sig \
  --certificate-identity-regexp '^https://github\.com/sky-ai-eng/orrery/\.github/workflows/release\.yml@refs/tags/.+$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com checksums.txt
sha256sum --ignore-missing -c checksums.txt
```

The identity names the release workflow and a tag push, not just the repository, so a signature made by any other workflow in the repository fails. Each archive also has an SPDX SBOM beside it, `<archive>.sbom.json`.

## Versions

Orrery is versioned `0.y.z`. A release that moves `y` (0.3.4 to 0.4.0) breaks something an existing caller relies on: a command or flag, `--json` output or the ready line `orrery serve` prints, a control-plane route, the instance config file, or opening a world an earlier release wrote. Every other release moves only `z`. That includes new GitHub surface and any change that brings the face closer to GitHub's behavior, because GitHub's behavior is what the face promises. [`CHANGELOG.md`](../../CHANGELOG.md) lists each release's changes.

## Building from source

Orrery builds with stable Rust:

```bash
cargo build --release -p orrery
```

The binary is `target/release/orrery`. [Contributor docs](../contributors/README.md) cover the tests and the rest of the toolchain.
