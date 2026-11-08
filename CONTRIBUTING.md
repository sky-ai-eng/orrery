# Contributing to Orrery

Thanks for contributing.

## Before You Start

By contributing to this repository, you agree that your contributions may be
licensed under the repository's [Orrery License 1.0](LICENSE), as set out in
the contributor license agreement in [`docs/contributors/CLA.md`](docs/contributors/CLA.md),
which all contributions must be covered by.

The repository is source-available. Under the CLA you grant the maintainer
broad rights to use, relicense, and distribute your contributions, including
under separate commercial terms. You keep copyright to your work; contributing
does not create any claim to revenue from the project or any say over how the
maintainer licenses or sells it.

For a change to how Orrery behaves, rather than a fix, please open an issue to
discuss it first.

## Contributor License Agreement

This project uses an individual Contributor License Agreement so contributors
keep copyright to their work while granting the maintainer broad rights to use,
relicense, and distribute contributions as part of the project.

For pull requests submitted through GitHub, the expected workflow is:

1. Open a pull request.
2. If prompted by CLA Assistant, review [`docs/contributors/CLA.md`](docs/contributors/CLA.md) and sign the agreement in the
   PR flow.
3. Wait for the CLA status check to pass before requesting review or merge.

If you are contributing on behalf of an employer or client, make sure you have
permission to submit the work under the terms in [`docs/contributors/CLA.md`](docs/contributors/CLA.md).

## Pull Requests

Keep pull requests focused and easy to review. Build, lint, and test commands,
and the rules every change follows, are in [CLAUDE.md](CLAUDE.md); the design
rules are in [`spec/`](spec/README.md), and a change that contradicts the spec
updates it in the same pull request.

- **Title:** a conventional commit (`type(scope): summary`). Titles become the
  changelog and choose the next version, so read
  [Releasing](docs/contributors/releasing.md) before you write one.
- **Body:** what was wrong, what changed, and how you checked it, plus anything
  that could be missing or problematic.
- **Deferred work:** anything you knowingly leave undone is marked in the code
  where it is missing, with a `TODO(#NN)` naming a GitHub issue that describes
  it.

## Code of Conduct

Be respectful, constructive, and professional in issues and pull requests.
