#!/usr/bin/env bash
# Type-check the platform-gated code this host cannot compile natively.
#
# Why this exists: a change to a shared signature (`Sandbox::with_roots` grew an
# `OutsideRoots` parameter) silently broke `crates/qcode/tests/sandbox_roots.rs`
# — a file behind `#![cfg(windows)]`. On Linux that file is not compiled at all,
# so `cargo test --workspace` stayed green and the breakage was pushed. The
# cross-check below caught it on the first run.
#
# What this is *not*: a replacement for the `macos-latest` and `windows-latest`
# CI jobs. Those compile (and, on macOS, run) the real backends. This is the
# cheap local gate that fails in seconds instead of waiting for a runner, and
# the reason it is worth having locally is that agents and developers work on
# one OS and push to all three.
#
# `cargo check` needs no linker, so a foreign target only needs its std
# installed (`rustup target add <triple>`). The triples below are the ones
# `build-for-release` actually ships, so this checks the real thing rather than
# a lookalike: there are no `cfg(target_env)` gates in this workspace, which is
# what would make `windows-gnullvm` differ from `windows-msvc` here.
set -o pipefail

# Shipping triples, from `build-for-release` (windows) and the release matrix
# (darwin). Order matters only for readability.
#
# Arguments override this list, which is what CI uses: it installs only the two
# Windows triples, so passing them explicitly keeps the "not checked" line
# meaning "this one is missing here" instead of naming targets the caller never
# intended to check.
SHIPPED=(
    x86_64-pc-windows-gnullvm
    aarch64-pc-windows-gnullvm
    aarch64-apple-darwin
    x86_64-apple-darwin
)

if (($# > 0)); then
    TARGETS=("$@")
else
    TARGETS=("${SHIPPED[@]}")
fi

sysroot=$(rustc --print sysroot)
native=$(rustc -vV | sed -n 's/^host: //p')

status=0
checked=0
skipped=()

for target in "${TARGETS[@]}"; do
    if [[ $target == "$native" ]]; then
        continue
    fi
    # Without the target's std, cargo fails with a wall of text about
    # `can't find crate for core`. Detect it directly and say what to run.
    if [[ ! -d $sysroot/lib/rustlib/$target ]]; then
        skipped+=("$target")
        continue
    fi
    echo "==> cargo check --target $target"
    if ! cargo check --workspace --all-targets --target "$target"; then
        status=1
    fi
    checked=$((checked + 1))
done

if ((${#skipped[@]} > 0)); then
    printf '\nnot checked (target std not installed): %s\n' "${skipped[*]}"
    printf 'install with: rustup target add %s\n' "${skipped[*]}"
fi

if ((checked == 0)); then
    # A silently-empty loop is the failure mode this whole script exists to
    # avoid: a green line that tested nothing. Say so and fail, because "I
    # checked the other platforms" must not be an unfalsifiable claim.
    echo "error: no non-native target was checked; nothing was verified" >&2
    exit 1
fi

echo
if ((status == 0)); then
    echo "cross-check: $checked target(s) type-check"
else
    echo "cross-check: failures above" >&2
fi
exit "$status"
