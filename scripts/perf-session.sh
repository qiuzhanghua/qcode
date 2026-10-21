#!/usr/bin/env bash
# Reproduce the 100k-event v2.2 performance baseline.
#
# Both tests are `#[ignore]` so normal `cargo test` stays fast; this script
# runs only those two and prints their timings.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

echo "== session store: load / list =="
cargo test -p qcode-core --test session_perf -- --ignored --nocapture

echo
echo "== UI state: replay / TUI open cost =="
cargo test -p qcode-ui-state --test replay_perf -- --ignored --nocapture
