<!--
TITLE (set the PR title, not here) — conventional-commit format. The title
becomes the changelog entry and picks the next version number
(docs/contributors/releasing.md):
  With a ticket:     type(scope): imperative summary (ORR-NN)
                     e.g. feat(world): read one actor and one OAuth app by key (ORR-36)
  Without a ticket:  type(scope): imperative summary        (no ticket suffix)
                     e.g. feat(github): serve PullRequest.headRefOid
  Types:   feat · fix · perf · refactor · ci · docs · chore · test
           feat, fix, and perf show up in the changelog
  Scopes:  world · face · github · snapshot · serve · spec
  Breaking: a change that breaks an existing caller of a command or flag,
           `--json` output or the ready line, a control-plane route, the
           config file, or a world an earlier release wrote takes `!` after
           the scope, e.g. feat(serve)!: … (releasing.md, "Which number moves")

BODY — keep it short. Say what was wrong, what you changed, and how you
checked it, then stop. The diff already says how; don't restate it, and don't
narrate the path that led here or the alternatives you passed over. Call out
anything that could be missing or problematic with this PR in the same body.
-->

## Problem

<!--
Why this change exists, for a reviewer who has NOT read the issue. A few
sentences: the symptom, the root cause, and the blast radius only when it
changes how carefully this should be reviewed.
-->

## Change

<!--
What you did. One sentence for the shape of the change, then a short bullet
per decision a reviewer could disagree with. Skip anything the diff shows on
its own. Use code fences for new payloads / API shapes / data structures.
Call out anything deliberately left OUT of scope.

Work knowingly left undone is marked in code at the place it is missing,
against a tracker that already describes that gap:
  // TODO(#NN): <what is missing, and what depends on it>    (a GitHub issue)
  // TODO(ORR-NN): <the same, for contributors with Linear access>
Name it here too, but the marker is what tracks it.
-->

<details>
<summary><strong>Scope / verification</strong></summary>

<!--
Required. The commands you ran and what passed, then what the new tests cover
in a line or two. Baseline (CLAUDE.md, "Build, lint, test"):
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace          # E2E tests run the real `git`; it must be on PATH
  ./scripts/license-check.sh      # when you add or bump a dependency
CHANGELOG is left to release-please — don't hand-edit it.
-->

</details>

<!-- ───────────────────────────────────────────────────────────────────────
Optional sections — add one only when it says something the sections above
don't, delete the rest:

## Spec and docs   — what in spec/ or docs/ you updated (a behavior change
                     updates spec/ in the same PR)
## Behavior notes  — edge cases, before/after, explicit out-of-scope items
## Tests           — call out coverage separately when it's substantial

──────────────────────────────────────────────────────────────────────── -->

<!--
FOOTER — keep exactly ONE of the lines below:
  • A GitHub issue:  the Resolves line with its number
  • A Linear ticket: the Resolves line with the ORR id (fix the ID + URL)
  • Neither:         the literal word "Unticketed"
-->

Resolves #NN
Resolves [ORR-NN](https://linear.app/sky-ai-eng/issue/ORR-NN)
Unticketed
