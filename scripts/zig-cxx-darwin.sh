#!/usr/bin/env bash
#
# The C++ half of `zig-cc-darwin.sh`, separate only so `CC_*` and `CXX_*` can
# point at different language front ends: cc-rs invokes each of them without a
# leading "cc"/"c++" argument, so the choice cannot ride on `$1`.
#
# Whether it is reached at all depends on the dependency: none of the bundled C
# libraries in this workspace have C++ sources today, so this exists to keep
# `CXX_<target>` honest rather than because a build currently needs it.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
ZIG_CC_TOOL=c++ exec "$here/zig-cc-darwin.sh" "$@"
