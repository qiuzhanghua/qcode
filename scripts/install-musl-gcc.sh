#!/usr/bin/env bash
#
# Put a `*-linux-musl-gcc` on the box without root, using zig.
#
# Why: `cargo check/build --target x86_64-unknown-linux-musl` dies inside
# `libsqlite3-sys` / `aws-lc-sys` before any Rust is type-checked —
#
#     error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc"
#
# — because cc-rs looks for a musl-targeted C compiler and the usual sources are
# closed here: `apt install musl-tools` needs root (and covers only the host
# architecture), and musl.cc's prebuilt cross toolchains are ~115 MB per
# architecture. Zig is already on this box for the darwin/A-BI cross checks and
# carries a musl sysroot for every target, which is exactly what compiling these
# C dependencies needs: the *object files* must be musl-flavoured, the final
# musl libc comes from Rust's self-contained `*-unknown-linux-musl` std.
#
# What it writes: `.toolchains/musl-gcc/bin/<triple>-gcc`, each a symlink to
# `scripts/zig-cc-musl.sh`. The symlink *name* is the interface — cc-rs looks the
# compiler up by that name — so nothing has to be passed on the command line
# (see the header of `zig-cc-musl.sh` for why that matters).
#
#   scripts/install-musl-gcc.sh              # install/verify, print the bin dir
#   scripts/install-musl-gcc.sh --check      # report only, never write
#
# Output contract: the **directory containing the symlinks** goes to stdout and
# nothing else, so callers can `export PATH="$(scripts/install-musl-gcc.sh):$PATH"`.
# Progress and errors go to stderr. Same contract as `install-zig.sh`.
#
# `build-for-release` finds this directory on its own (`find_musl_cc`), so
# `make linux-musl-amd64` / `make linux-musl-arm64` need no environment at all.
# Plain `cargo` does not, because cc-rs only searches PATH: export it, or point
# `CC_<triple>` at the file, e.g.
#
#     export CC_x86_64_unknown_linux_musl=$PWD/.toolchains/musl-gcc/bin/x86_64-linux-musl-gcc
#
# The symlinks are absolute-free and relative to the repository, so the checkout
# can move; re-run this script after moving it (`--check` will say so).
set -euo pipefail

# The names cc-rs looks for: the rust triple with `-unknown` dropped, i.e.
# `x86_64-unknown-linux-musl` -> `x86_64-linux-musl-gcc`. `build-for-release`'s
# `musl_cross_cc` computes the same spelling, and it is zig's own triple too.
TARGETS=(
    x86_64-linux-musl
    aarch64-linux-musl
)

ROOT=$(cd -P "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN="$ROOT/.toolchains/musl-gcc/bin"
WRAPPER="$ROOT/scripts/zig-cc-musl.sh"

CHECK=0
case "${1:-}" in
--check) CHECK=1 ;;
-h | --help)
    sed -n '3,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
"") ;;
*)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

find_zig() {
    local candidate
    if [ -n "${ZIG:-}" ] && [ -x "${ZIG}" ]; then
        echo "$ZIG"
        return 0
    fi
    if candidate=$(command -v zig 2>/dev/null); then
        echo "$candidate"
        return 0
    fi
    for candidate in "$ROOT"/.toolchains/zig*/zig "${TDP_HOME:-$HOME/cot}"/lib/zig_*/zig; do
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

zig=""
need_zig() {
    if [ -z "$zig" ]; then
        zig=$(find_zig) || {
            echo "$0: zig not found (looked at \$ZIG, PATH, $ROOT/.toolchains/zig*/, ${TDP_HOME:-$HOME/cot}/lib/zig_*/)" >&2
            echo "     run scripts/install-zig.sh, or install a real musl cross toolchain:" >&2
            echo "     only zig's shim can supply what a C-only toolchain (musl-tools) lacks — a C++ driver." >&2
            exit 1
        }
        echo "zig: $zig" >&2
    fi
}

# A trivial compile is the only check that proves the whole chain (symlink name
# -> wrapper -> zig -> musl sysroot), and it is what the failing build script
# would have done first anyway.
smoke_test() {
    local cc=$1 src out err rc=0
    # The `.c` suffix is load-bearing: zig decides the language from it, and a
    # suffix-less temp file is treated as a *linker* input — the compile "succeeds"
    # with `warning: 'linker' input unused` and writes no object at all. That is
    # how this check first passed on a broken chain.
    src=$(mktemp --suffix=.c)
    out=$src.o
    err=$src.err
    printf 'int qcode_musl_probe(void) { return 0; }\n' >"$src"
    if ! "$cc" -c "$src" -o "$out" 2>"$err"; then
        echo "    smoke test failed:" >&2
        sed 's/^/    /' "$err" >&2
        rc=1
    elif [ ! -f "$out" ]; then
        echo "    smoke test produced no object file at $out" >&2
        sed 's/^/    /' "$err" >&2
        rc=1
    elif command -v file >/dev/null 2>&1 && ! file "$out" | grep -q 'ELF'; then
        echo "    smoke test produced a non-ELF object: $(file "$out")" >&2
        rc=1
    fi
    rm -f "$src" "$out" "$err"
    return "$rc"
}

missing=0
for target in "${TARGETS[@]}"; do
    for tool in gcc g++; do
        # `-g++` 不是多余的：musl 交叉构建里 freetype-sys 在 pkg-config 失败时退回
        # 编自带的 C++ 源码，而 cc-rs 是按名字找 `x86_64-linux-musl-g++` 的。
        name="$target-$tool"
        cc="$BIN/$name"

        # 真的交叉工具链（musl-tools 的 `x86_64-linux-musl-gcc`、musl.cc 的整套）
        # 一律优先：它不是替身，能链接、也有目标平台的 .pc 与库。只在它缺席时补。
        if real=$(command -v "$name" 2>/dev/null) &&
            [ "$(readlink -f "$real")" != "$(readlink -f "$WRAPPER")" ]; then
            echo "有真的 $name（$real），不装替身" >&2
            rm -f "$cc"
            continue
        fi

        if [ "$CHECK" = 1 ]; then
            if [ ! -e "$cc" ]; then
                echo "not installed: $name" >&2
                missing=1
                continue
            fi
            if [ "$(readlink -f "$cc")" != "$(readlink -f "$WRAPPER")" ]; then
                echo "stale (symlink does not point at scripts/zig-cc-musl.sh): $cc" >&2
                echo "    re-run $0" >&2
                missing=1
                continue
            fi
            echo "ok: $name" >&2
            [ "$tool" = gcc ] && { smoke_test "$cc" || missing=1; }
            continue
        fi
        mkdir -p "$BIN"
        need_zig
        # `ln -sfn` with a relative target: keep the checkout movable.
        ln -sfn ../../../scripts/zig-cc-musl.sh "$cc"
        echo "installed: $cc -> $(readlink "$cc")" >&2
        if [ "$tool" = gcc ]; then
            smoke_test "$cc" || {
                echo "    the symlink is there but the toolchain is not usable" >&2
                missing=1
            }
        fi
    done
done

if [ "$CHECK" = 1 ] && [ "$missing" = 1 ]; then
    exit 1
fi
if [ "$missing" = 1 ]; then
    exit 1
fi

# stdout: the directory to put on PATH, nothing else.
echo "$BIN"
