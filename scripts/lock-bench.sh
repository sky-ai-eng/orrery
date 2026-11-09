#!/usr/bin/env bash
# Run the repository-lock benchmark (crates/orrery-bench, `lock-bench`): real
# git clones, pushes, and merges against `orrery serve`, measuring how long a
# fetch request holds a repository's lock and what that costs the writes
# behind it. See docs/contributors/benchmarks/lock-bench.md for the scenarios
# and recorded results.
#
# With no --engine, it builds this tree's `orrery` in release and benchmarks
# that. To compare builds, build each and name it:
#   ./scripts/lock-bench.sh --engine main=/tmp/orrery-main --engine this=target/release/orrery
#
# Usage:
#   ./scripts/lock-bench.sh
#   ./scripts/lock-bench.sh --scenario writers --payloads src40,src168 --rounds 40
#   ./scripts/lock-bench.sh --scenario hold --payloads tiny --runs 3
#
# Payloads are built once and kept under target/lock-bench/. The report
# prints to stdout and is archived in ./bench-results/.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo build --release -q -p orrery -p orrery-bench

case " $* " in
  *" --engine"*) ;;
  *) set -- --engine "this=$PWD/target/release/orrery" "$@" ;;
esac

OUT_DIR="$PWD/bench-results"
mkdir -p "$OUT_DIR"

STAMP=$(date +%Y%m%d-%H%M%S)
OUT="$OUT_DIR/lock-bench-$STAMP.txt"

echo "==> running lock bench (report: bench-results/lock-bench-$STAMP.txt)"
./target/release/lock-bench --work "$PWD/target/lock-bench" "$@" | tee "$OUT"
