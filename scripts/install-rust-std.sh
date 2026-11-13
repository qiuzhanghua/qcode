#!/usr/bin/env bash
# Install a target's `rust-std` into the sysroot `rustc` actually uses.
#
# `rustup target add <triple>` is the usual answer, and this script runs it when
# rustup is on PATH. The development machines here use the cot toolchain
# (`/home/q/<user>/cot/lib/rust_<ver>_<os>_<arch>_<libc>`), which is a plain
# unpacked distribution with no rustup at all — so `cargo check --target
# aarch64-apple-darwin` fails with "can't find crate for `core`" and
# `make check-platforms` skips the two Darwin triples it was written for.
#
# What rustup does, and therefore what this does: unpack
# `rust-std-<ver>-<triple>.tar.xz` so that `lib/rustlib/<triple>/lib/` ends up
# next to the other targets' std. Nothing else is involved — no linker, no SDK,
# no C compiler — because `cargo check` stops at metadata.
#
#   scripts/install-rust-std.sh aarch64-apple-darwin rust-std-1.99.1-aarch64-apple-darwin.tar.xz
#   scripts/install-rust-std.sh aarch64-apple-darwin /path/to/unpacked/rust-std
#
# The std has to be the *same version* as the compiler: metadata from another
# version is rejected at load time with a message that names neither the version
# nor this script. So the version is checked before anything is unpacked, and
# the result is accepted only after a real `--emit=metadata` compile for the
# target (which is also what validates the triple's metadata).
#
# Getting the tarball: from a machine that has rustup, either
#   rustup target add aarch64-apple-darwin   # then copy ~/.rustup/toolchains/*/lib/rustlib/aarch64-apple-darwin
# or download it from https://static.rust-lang.org/dist/<release-date>/ (the
# release date for the running compiler is in `rustc -vV`'s `commit-date`).
set -euo pipefail

usage() {
    sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,2\}//' >&2
    exit 2
}

force=0
sysroot_override=""
args=()
while (($# > 0)); do
    case "$1" in
        -h | --help) usage ;;
        -f | --force)
            force=1
            shift
            ;;
        --sysroot)
            # Only for this script's own tests: they install into a scratch
            # sysroot instead of the machine's toolchain.
            sysroot_override=$2
            shift 2
            ;;
        *)
            args+=("$1")
            shift
            ;;
    esac
done

if ((${#args[@]} < 1)); then
    usage
fi
triple=${args[0]}
source_path=${args[1]:-}

if [[ -z ${sysroot_override} ]]; then
    sysroot=$(rustc --print sysroot)
else
    sysroot=$sysroot_override
fi
version=$(rustc -vV | sed -n 's/^release: //p')
dest="$sysroot/lib/rustlib/$triple"

# The acceptance test: a real compile for the target, which fails if the std is
# for another triple or another compiler version. Defined before anything is
# installed so both the "already there" and the fresh-install paths use it.
verify() {
    local work
    work=$(mktemp -d)
    trap 'rm -rf "$work"' RETURN
    printf 'pub fn probe() -> usize {\n    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)\n}\n' \
        >"$work/probe.rs"
    # `--sysroot` only when overridden, so a normal run uses the default one.
    local sysroot_args=()
    if [[ -n ${sysroot_override} ]]; then
        sysroot_args=(--sysroot "$sysroot")
    fi
    if ! rustc --edition 2021 --crate-type lib --emit=metadata \
        "${sysroot_args[@]}" --target "$triple" \
        -o "$work/probe.rmeta" "$work/probe.rs" 2>"$work/err"; then
        echo "error: rustc does not accept the std now installed for $triple:" >&2
        sed 's/^/  /' "$work/err" >&2
        return 1
    fi
    rm -rf "$work"
}

if [[ -d $dest/lib ]] && ((!force)); then
    echo "$triple: already installed in $dest"
    verify
    echo "$triple: std out of $sysroot type-checks"
    exit 0
fi

# With rustup, let it do the work — it resolves the version against the
# toolchain, which no manual copy can.
if [[ -z ${source_path} ]] && command -v rustup >/dev/null 2>&1; then
    echo "rustup target add $triple"
    rustup target add "$triple"
    verify
    echo "$triple: std out of $sysroot type-checks"
    exit 0
fi

if [[ -z ${source_path} ]]; then
    cat >&2 <<EOF
error: $triple has no std in $sysroot and there is no rustup to fetch one.

This machine's toolchain is a plain distribution (cot), so the version has to be
supplied by hand: this compiler is $version, and the std must be the same.

Pass the component as an argument:
  $0 $triple rust-std-$version-$triple.tar.xz
or an already-unpacked directory:
  $0 $triple /path/to/rustlib/$triple

EOF
    exit 1
fi

[[ -e $source_path ]] || {
    echo "error: $source_path does not exist" >&2
    exit 1
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

case "$source_path" in
    *.tar.xz | *.txz) tar -xJf "$source_path" -C "$work" ;;
    *.tar.gz | *.tgz) tar -xzf "$source_path" -C "$work" ;;
    *.tar.zst) zstd -dc "$source_path" | tar -xf - -C "$work" ;;
    *)
        # Not an archive: a directory holding `lib/libcore-*.rlib`, either the
        # plain `<triple>/` from a toolchain or its parent.
        if [[ -d $source_path/lib ]]; then
            mkdir -p "$work/plain"
            cp -R "$source_path"/. "$work/plain"/
        elif [[ -d $source_path/lib/rustlib/$triple/lib ]]; then
            cp -R "$source_path"/lib/rustlib/"$triple"/. "$work/plain"/
        fi
        ;;
esac

# Find the directory that is the component root, however the input was wrapped:
# the official `rust-std-<ver>-<triple>.tar.xz` nests it two levels deep, a
# toolchain copy has it at `lib/rustlib/<triple>`, and a bare copy is flat.
component=$(find "$work" -type f -name 'libcore-*.rlib' -path '*/lib/*' -printf '%h\n' 2>/dev/null |
    sed 's|/lib$||' | sort -u | head -1)
if [[ -z ${component} ]]; then
    echo "error: no lib/libcore-*.rlib inside $source_path — not a rust-std component" >&2
    exit 1
fi

# The version check has to happen before the copy: a mismatched std unpacks
# without complaint and fails much later, inside rustc, naming neither side.
#
# The label is read out of the path rather than by counting directories up to a
# known depth: the official archive nests it as
# `rust-std-<ver>-<triple>/rust-std-<triple>/lib`, a toolchain copy has no label
# at all, and a test fixture can be either. Matching the pattern finds it at any
# depth. (Counting was wrong here first — two levels up from `lib` is the work
# directory for the official layout, so the check silently never fired.)
# Only a labelled archive can be checked; a plain toolchain copy has no label, so
# an empty `found` means "nothing to compare", not an error. `|| true` because
# `set -o pipefail` + a non-matching `grep` would otherwise end the script here —
# silently, since a failed assignment has nothing to print.
found=$(printf '%s\n' "$component" | grep -o 'rust-std-[0-9][^-]*-' | head -1 | sed 's/^rust-std-//; s/-$//' || true)
if [[ -n ${found} && ${found} != "$version" ]]; then
    echo "error: that component is for $found, but this rustc is $version" >&2
    echo "  metadata from another version fails to load, naming neither side" >&2
    exit 1
fi

echo "$triple: installing into $dest"
rm -rf "$dest"
mkdir -p "$dest"
cp -R "$component"/. "$dest"/

verify
echo "$triple: std out of $sysroot type-checks"
