#!/usr/bin/env bash
#
# Drive `zig cc` as the C compiler for a Linux musl target.
#
# Why this exists: cc-rs (behind `libsqlite3-sys` / `aws-lc-sys`) insists on a
# compiler that emits musl objects. On Debian/Ubuntu that is `musl-tools`'
# `musl-gcc`, which only covers the host architecture and needs root; a real
# `x86_64-linux-musl-gcc` for both x86_64 and aarch64 means musl.cc's prebuilt
# cross toolchain (~115 MB per architecture). Zig carries a musl sysroot for
# every target it supports, so it can play that role instead — the same trick
# `build-for-release`'s `zig_cc_wrapper` already uses for the cross-arch musl
# releases.
#
# Rust's `*-unknown-linux-musl` std is self-contained (musl is linked
# statically into the binary), so nothing here has to know where the *final*
# libc comes from: this only has to produce target-correct object files.
#
# **The target comes from the file name or `ZIG_CC_TARGET`, never from `$1`.**
# cc-rs runs a family probe (`<cc> -E detect_compiler_family.c`) against the
# same value it uses for real builds, so a wrapper that expects `$1` to be the
# compiler mangles that probe. That is also why `scripts/install-musl-gcc.sh`
# installs *symlinks named after the triple* rather than one script with
# arguments. The same lesson is written up in `scripts/zig-cc-darwin.sh`.
#
# Usage (the installer does this for you):
#
#     ln -s ../../../scripts/zig-cc-musl.sh .toolchains/musl-gcc/bin/x86_64-linux-musl-gcc
#     CC_x86_64_unknown_linux_musl=$PWD/.toolchains/musl-gcc/bin/x86_64-linux-musl-gcc
#
# `ZIG_BIN` exists for the same reason as in `zig-cc-darwin.sh`: zig need not be
# on PATH (`scripts/install-zig.sh` puts it under `.toolchains/`), and a bare
# `zig` would then disagree with the caller's lookup.
#
# `ZIG_GLOBAL_CACHE_DIR` / `ZIG_LOCAL_CACHE_DIR` are passed through if set; zig
# wants a writable cache and its default (`~/.cache/zig`) fails loudly with
# `AccessDenied` under a read-only home, which reads like a build failure.
set -euo pipefail

# `$0` is a symlink named `<arch>-linux-musl-gcc` (or `-g++`): both the tool and
# the target come from that name so that a bare `command -v` hit is enough.
self=$(basename "$0")
tool=cc
case "$self" in
# zig spells the C++ driver `c++` (not `g++` / `cxx`), and only that spelling
# makes it link against libc++ instead of treating the input as C.
*++) tool=c++ ;;
esac
target=${ZIG_CC_TARGET:-${self%-gcc}}
target=${target%-g++}
# Accept the rust spelling too (`x86_64-unknown-linux-musl`), since a caller may
# symlink or set CC by that name; zig wants the short one and rejects the other
# with `UnknownOperatingSystem`.
case "$target" in
*-unknown-linux-musl) target="$(echo "$target" | cut -d- -f1)-linux-musl" ;;
esac
if [ -z "$target" ] || [ "$target" = "$self" ]; then
    echo "$0: cannot tell the target from the file name '$self'; rename it to" >&2
    echo "    <arch>-linux-musl-gcc, or set ZIG_CC_TARGET (e.g. x86_64-linux-musl)" >&2
    exit 2
fi

# Resolve the *real* script, so `..`-relative symlink installs still find the
# repository (and its `.toolchains/zig*`).
script=${BASH_SOURCE[0]}
while [ -L "$script" ]; do
    dir=$(cd -P "$(dirname "$script")" && pwd)
    script=$(readlink "$script")
    case $script in /*) ;; *) script="$dir/$script" ;; esac
done
root=$(cd -P "$(dirname "$script")/.." && pwd)

zig_bin=${ZIG_BIN:-}
if [ -z "$zig_bin" ]; then
    if [ -n "${ZIG:-}" ] && [ -x "${ZIG}" ]; then
        zig_bin=$ZIG
    elif zig_bin=$(command -v zig 2>/dev/null); then
        :
    else
        for candidate in "$root"/.toolchains/zig*/zig "${TDP_HOME:-$HOME/cot}"/lib/zig_*/zig; do
            if [ -x "$candidate" ]; then
                zig_bin=$candidate
                break
            fi
        done
    fi
fi
if [ -z "$zig_bin" ]; then
    echo "$0: cannot find zig (looked at \$ZIG, PATH, $root/.toolchains/zig*/," >&2
    echo "    ${TDP_HOME:-$HOME/cot}/lib/zig_*/). Run scripts/install-zig.sh, or set ZIG_BIN." >&2
    exit 2
fi

# zig's own default is `~/.cache/zig`, which is not writable on a machine whose
# home is read-only (this one included) and fails with `AccessDenied` — a
# message that says nothing about C compilers, so it reads like a repository
# bug. Default to the repository's gitignored `.tmp/` and let an explicit value
# win, exactly like `build-for-release` does for its own wrapper.
if [ -z "${ZIG_GLOBAL_CACHE_DIR:-}" ]; then
    export ZIG_GLOBAL_CACHE_DIR="$root/.tmp/zig-cache/global"
fi
if [ -z "${ZIG_LOCAL_CACHE_DIR:-}" ]; then
    export ZIG_LOCAL_CACHE_DIR="$root/.tmp/zig-cache/local"
fi
mkdir -p "$ZIG_GLOBAL_CACHE_DIR" "$ZIG_LOCAL_CACHE_DIR" 2>/dev/null || true

args=()
for arg in "$@"; do
    case "$arg" in
    # cc-rs appends the *rust* triple (`--target=x86_64-unknown-linux-musl`),
    # which zig rejects (UnknownOperatingSystem); the target is already fixed
    # above, so drop it.
    --target=*) continue ;;
    # rustc passes this GNU-ld workaround for aarch64 (cortex-a53 erratum);
    # zig's lld does not know it and quits.
    -Wl,--fix-cortex-a53-843419) continue ;;
    esac
    args+=("$arg")
done

# zig inserts UBSan checks at -O0, and `aws-lc-sys` compiles jitterentropy at
# -O0 on purpose: without this the final link misses `__ubsan_handle_*`.
exec "$zig_bin" "$tool" -target "$target" -fno-sanitize=undefined "${args[@]}"
