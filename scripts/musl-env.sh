#!/usr/bin/env bash
#
# Print the environment `cargo check` / `cargo build` needs for a *-linux-musl
# target, so a bare `cargo` (not `./build-for-release`) can be used:
#
#     eval "$(scripts/musl-env.sh)"                    # host arch (x86_64)
#     eval "$(scripts/musl-env.sh aarch64-unknown-linux-musl)"
#     scripts/musl-env.sh                              # just look at it
#
# Contract: **only `export …` lines on stdout**, nothing else, so `eval` is the
# whole interface. Everything that is not an env line (what was found, what is
# missing) goes to stderr.
#
# Why four separate things are needed — each one is a failure mode that reads
# like a repository bug:
#
# 1. `CC_<triple>` / `CXX_<triple>`: cc-rs looks the compiler up **by name**
#    (`<arch>-linux-musl-gcc`, `<arch>-linux-musl-g++`), so the host `cc` cannot
#    stand in — it emits glibc objects, and it does not find them at all, which
#    surfaces as
#
#        error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc"
#
#    deep inside `libsqlite3-sys` / `aws-lc-sys`. `musl-tools` provides the C
#    driver for the host architecture only and ships **no** `g++`, which
#    `freetype-sys` needs when pkg-config cannot answer (always, when cross
#    compiling) — hence `scripts/install-musl-gcc.sh`, which fills the gaps
#    with zig.
#
# 2. `RUST_FONTCONFIG_DLOPEN=1`: `yeslogic-fontconfig-sys` looks fontconfig up
#    through pkg-config; cross compiling makes pkg-config refuse
#    ("pkg-config has not been configured to support cross-compilation") and the
#    build script panics. The flag switches it to `dlopen("libfontconfig.so.1")`
#    at run time, which is also what `build-for-release` does for its
#    cross-compiled Linux releases. `./build-for-release --list` calls a target
#    "ready" only for targets it can actually produce, and musl is not one of
#    them (see below).
#
# 3. `CARGO_TARGET_<TRIPLE>_LINKER`: rustc's default linker for this target is
#    `cc` = the host driver, which then fails on `-lxcb`/`-lc++` (those are
#    glibc-world libraries, looked up in glibc prefixes). Rust's own
#    `rust-lld` is what produces the static musl binary, and it lives in the
#    sysroot rather than on PATH.
#
# 4. `RUSTFLAGS=-Lnative=…`: `rust-lld` does not read the musl driver's spec
#    file, so it does not know where `musl-tools` keeps its libraries
#    (`/usr/lib/x86_64-linux-musl`, which holds `libdl.a` among others).
#    Without it: `rust-lld: error: unable to find library -ldl`.
#
# What this does **not** give you: a musl `qc` (the GUI/TUI binary). gpui
# hard-links `xcb`, `xkbcommon`, `xkbcommon-x11` — the linker line asks for
# them by name — and there are no musl builds of those libraries on a Debian
# host (`rust-lld: error: unable to find library -lxkbcommon`). `qc` therefore
# stays a glibc artifact; `qcd` (headless, no gpui) does build and run as a
# static-pie musl binary. See "musl" in docs/DEVELOPMENT.md.
set -euo pipefail

ROOT=$(cd -P "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

usage() {
    sed -n '3,6p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

TRIPLE=""
for arg in "$@"; do
    case "$arg" in
    -h | --help) usage ;;
    -*) usage ;;
    *)
        [ -z "$TRIPLE" ] || usage
        TRIPLE=$arg
        ;;
    esac
done

host=$(rustc -vV | sed -n 's/^host: //p')
if [ -z "$TRIPLE" ]; then
    case "$host" in
    x86_64-*) TRIPLE=x86_64-unknown-linux-musl ;;
    aarch64-*) TRIPLE=aarch64-unknown-linux-musl ;;
    *)
        echo "$0: cannot guess the musl triple for host $host; pass one" >&2
        exit 1
        ;;
    esac
fi
case "$TRIPLE" in
*-unknown-linux-musl) ;;
*)
    echo "$0: expected a *-unknown-linux-musl triple, got '$TRIPLE'" >&2
    exit 2
    ;;
esac
arch=${TRIPLE%%-*}

sysroot=$(rustc --print sysroot)
if [ ! -d "$sysroot/lib/rustlib/$TRIPLE/lib" ]; then
    echo "$0: sysroot has no std for $TRIPLE — rustup target add $TRIPLE" >&2
    exit 1
fi

lines=()

# 1. C / C++ drivers. A real `<triple>-gcc` on PATH (musl-tools, musl.cc, a
# distro cross package) always wins; the zig shim fills what is missing.
find_tool() {
    local name=$1 candidate
    if candidate=$(command -v "$name" 2>/dev/null); then
        echo "$candidate"
        return 0
    fi
    for candidate in "${MUSL_GCC_DIR:-}/$name" "$ROOT/.toolchains/musl-gcc/bin/$name"; do
        [ -n "$candidate" ] && [ -x "$candidate" ] || continue
        echo "$candidate"
        return 0
    done
    return 1
}

cc_name="$arch-linux-musl-gcc"
cxx_name="$arch-linux-musl-g++"
key=${TRIPLE//-/_}
if cc=$(find_tool "$cc_name"); then
    echo "C compiler:   $cc" >&2
    lines+=("export CC_$key=$cc")
else
    echo "C compiler:   MISSING ($cc_name) — run scripts/install-musl-gcc.sh" >&2
fi
if cxx=$(find_tool "$cxx_name"); then
    echo "C++ compiler: $cxx" >&2
    lines+=("export CXX_$key=$cxx")
else
    echo "C++ compiler: MISSING ($cxx_name) — run scripts/install-musl-gcc.sh" >&2
    echo "              (freetype-sys compiles bundled C++ sources when pkg-config cannot cross)" >&2
fi

# 2. fontconfig through dlopen instead of pkg-config.
lines+=("export RUST_FONTCONFIG_DLOPEN=1")

# 3. Rust's own linker (static musl), not the host `cc`.
lld="$sysroot/lib/rustlib/$host/bin/rust-lld"
if [ -x "$lld" ]; then
    lines+=("export CARGO_TARGET_$(echo "$TRIPLE" | tr '[:lower:]-' '[:upper:]_')_LINKER=$lld")
else
    echo "linker:       MISSING ($lld)" >&2
fi

# 4. Where the musl C libraries live, for rust-lld. musl-tools uses
# /usr/lib/<triple>; musl.cc and friends keep them next to the compiler.
ldflags=()
for dir in "/usr/lib/$arch-linux-musl" "/usr/lib/$TRIPLE" \
    "${MUSL_GCC_DIR:-}/../$arch-linux-musl/lib" "$ROOT/.toolchains/musl-gcc/$arch-linux-musl/lib"; do
    [ -n "$dir" ] && [ -d "$dir" ] || continue
    ldflags+=("-Lnative=$dir")
    echo "C library dir: $dir" >&2
done
if [ ${#ldflags[@]} -gt 0 ]; then
    # Appended, not replaced: the caller may already have RUSTFLAGS of their own,
    # and silently dropping them would be the class of surprise this script
    # exists to avoid.
    lines+=("export RUSTFLAGS=\"\${RUSTFLAGS:-} ${ldflags[*]}\"")
fi

printf '%s\n' "${lines[@]}"
