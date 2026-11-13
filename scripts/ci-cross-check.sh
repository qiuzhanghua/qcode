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
# Arguments override this list, which is for a developer machine that has the
# cross compilers installed. CI does not run this script: covering the Windows
# cfg gates means compiling the bundled C dependencies (`libsqlite3-sys`,
# `aws-lc-sys`) for a foreign target, which needs the llvm-mingw toolchain, and
# `windows-build` on a real Windows runner covers the same gates natively.
# The per-target preflight below says which compiler is missing instead of
# letting a build script panics about it.
#
# Note on the aarch64 triples: a C build script (`cc-rs`) needs the matching
# cross compiler, e.g. `aarch64-w64-mingw32-clang` for windows-gnullvm. The
# release environment has llvm-mingw (`build-for-release` requires it).
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

# The C compiler a target's build scripts will ask `cc-rs` for, when it needs one
# that the host's own compiler is not. Only the mingw triples have a name this
# script can predict; anything else is left to fail on its own terms.
#
# Without this preflight, `cargo check` dies deep inside a build script:
#
#   thread 'main' panicked at aws-lc-sys-0.46.0/builder/cc_builder.rs:722:
#   Failed to compile jitterentropy: ToolNotFound: failed to find tool
#   "x86_64-w64-mingw32-clang": No such file or directory (os error 2)
#
# That message names one C library out of several and says nothing about the
# toolchain being missing, which reads like a bug in this repository.
cross_cc() {
    case "$1" in
        *-pc-windows-gnullvm) printf '%s-w64-mingw32-clang' "${1%%-*}" ;;
        *) return 1 ;;
    esac
}

status=0
checked=0
# Two reasons to skip, kept apart because the fix differs: one needs
# `rustup target add`, the other needs a compiler on PATH.
missing_std=()
missing_cc=()

for target in "${TARGETS[@]}"; do
    if [[ $target == "$native" ]]; then
        continue
    fi
    # Without the target's std, cargo fails with a wall of text about
    # `can't find crate for core`. Detect it directly and say what to run.
    if [[ ! -d $sysroot/lib/rustlib/$target ]]; then
        missing_std+=("$target")
        continue
    fi
    # A target whose C build scripts need a cross compiler we do not have would
    # fail on the first bundled C library — `libsqlite3-sys`, `aws-lc-sys` — in
    # a message that does not mention the toolchain. Say it here instead.
    if cc=$(cross_cc "$target") && ! command -v "$cc" >/dev/null 2>&1; then
        missing_cc+=("$target (needs $cc)")
        continue
    fi
    echo "==> cargo check --target $target"
    if ! cargo check --workspace --all-targets --target "$target"; then
        status=1
    fi
    checked=$((checked + 1))
done

if ((${#missing_std[@]} > 0)); then
    printf '\nnot checked (target std not installed): %s\n' "${missing_std[*]}"
    printf 'install with: rustup target add %s\n' "${missing_std[*]}"
    # No rustup here is not an accident to report: the development machines use
    # a plain distribution (cot), where the std has to be unpacked into the
    # sysroot by hand. Point at the script that does it instead of leaving the
    # `rustup` line above as the only answer.
    if ! command -v rustup >/dev/null 2>&1; then
        printf 'this machine has no rustup; use scripts/install-rust-std.sh <triple> <rust-std-*.tar.xz>\n'
    fi
fi

if ((${#missing_cc[@]} > 0)); then
    printf '\nnot checked (no cross C compiler on PATH):\n'
    printf '  %s\n' "${missing_cc[@]}"
    printf 'install llvm-mingw and put its bin/ on PATH — the bundled C\n'
    printf 'dependencies (`libsqlite3-sys`, `aws-lc-sys`) are compiled with it.\n'
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
