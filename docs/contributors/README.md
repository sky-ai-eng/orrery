# Contributing

Start with [CONTRIBUTING.md](../../CONTRIBUTING.md) at the repository root: the contribution terms, the CLA, and what a pull request needs. Build, lint, and test commands, and the rules every change follows, are in [CLAUDE.md](../../CLAUDE.md). The design rules and the reference for the GitHub surface are in [`spec/`](../../spec/README.md); a change that contradicts the spec updates the spec in the same pull request.

- [Contributor License Agreement](CLA.md): the agreement every contributor signs. `.github/workflows/cla.yml` checks for signatures, records them on the `cla-signatures` branch, and lists the identities that don't sign. Each signature records the version of this file it was made against.
- [Releasing](releasing.md): how a pull request's title becomes a changelog entry and a version, and how a release is built and signed

Pull request titles are conventional commits, and they become the changelog: read [Releasing](releasing.md) before you title one.
