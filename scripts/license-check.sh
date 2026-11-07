#!/usr/bin/env bash
# Orrery — Rust dependency license gate.
#
# A release distributes the `orrery` binary, which statically links its whole
# crate graph. This gate fails if any crate in that graph carries a license
# outside deny.toml's allow list, or no license cargo-deny can read. It's the
# single source of truth for the license check: run by
# .github/workflows/dependency-licenses.yml on PRs + main, and as a hard
# prerequisite of the release job (.github/workflows/release.yml) so a tag
# can't ship binaries without it passing.
#
#   ./scripts/license-check.sh            # run the gate (exit non-zero on violation)
#   ./scripts/license-check.sh --report   # list every license and the crates under it
#
# --report is the on-demand inventory: run it any time you need the current list
# of crates in the binary and their licenses. There's deliberately no checked-in
# inventory file — it would drift from Cargo.lock and churn commit history; this
# gate plus --report keep the source of truth in the code, not a doc.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

CARGO_DENY_VERSION=0.20.2

command -v cargo >/dev/null 2>&1 || { echo "cargo not found on PATH" >&2; exit 1; }

if [ "$(cargo deny --version 2>/dev/null || true)" != "cargo-deny $CARGO_DENY_VERSION" ]; then
  echo "installing cargo-deny@$CARGO_DENY_VERSION..."
  cargo install --locked "cargo-deny@$CARGO_DENY_VERSION"
fi

# The binary's own manifest, not the workspace, so the graph is what ships.
DENY=(cargo deny --config deny.toml --manifest-path crates/orrery/Cargo.toml)

if [ "${1:-}" = "--report" ]; then
  "${DENY[@]}" list --layout license
  exit 0
fi

"${DENY[@]}" check licenses
