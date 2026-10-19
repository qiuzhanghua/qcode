#!/usr/bin/env bash
# Release gate for the fixture workspace.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
