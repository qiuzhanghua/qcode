#!/usr/bin/env bash
#
# Drive `zig cc` as the C compiler for an Apple target.
#
# Why this exists: `cargo check --target *-apple-darwin` on a Linux host fails in
# `libsqlite3-sys` / `aws-lc-sys` before any Rust is type-checked, because their
# build scripts want a compiler that can emit Mach-O. No macOS SDK is needed for
# that -- zig ships libSystem stubs and headers -- so this wrapper turns the
# bundled C dependencies into something a Linux box can build.
#
# It also undoes one thing cc-rs does on purpose: on seeing what looks like
# clang it appends `--target=arm64-apple-macosx` (Apple's spelling). Zig accepts
# the `aarch64` spelling only and rejects the other with a list of valid
# architectures, which reads like a bug in this repository rather than a naming
# mismatch. The argument is dropped here; the target comes from `ZIG_CC_TARGET`.
#
# **The compiler must not be a positional argument.** `cc-rs` runs a family
# probe (`<cc> -E detect_compiler_family.c`) against the same value it will use
# for real builds, so a wrapper expecting `$1` to be `cc` mangles that probe. The
# language therefore comes from `ZIG_CC_TOOL` (see `zig-cxx-darwin.sh`).
#
# Usage, from `scripts/ci-cross-check.sh`:
#
#     ZIG_CC_TARGET=aarch64-macos CC_aarch64_apple_darwin=…/zig-cc-darwin.sh …
#
# `ZIG_GLOBAL_CACHE_DIR` / `ZIG_LOCAL_CACHE_DIR` are passed through if set; zig
# wants a writable cache and defaults to `~/.cache/zig`.
set -euo pipefail

tool=${ZIG_CC_TOOL:-cc}
target=${ZIG_CC_TARGET:?ZIG_CC_TARGET is not set (e.g. aarch64-macos)}

args=()
for arg in "$@"; do
    case "$arg" in
        --target=*) continue ;;
    esac
    args+=("$arg")
done

exec zig "$tool" -target "$target" "${args[@]}"
