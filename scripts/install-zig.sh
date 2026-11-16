#!/usr/bin/env bash
#
# Put a pinned `zig` where `make check-platforms` can find it.
#
# Why zig and not a real cross toolchain: the darwin triples need a C compiler
# that emits Mach-O for `libsqlite3-sys` / `aws-lc-sys`, and no Linux package
# provides one. Zig ships the macOS SDK (libSystem stubs and headers) and is
# enough for `cargo check`, which stops at metadata and never links. See
# ADR-0095 for what that does and does not cover.
#
#   scripts/install-zig.sh              # install/verify, print the bin dir
#   scripts/install-zig.sh --check      # report only, never touch the network
#   ZIG_VERSION=0.17.0 scripts/install-zig.sh
#   ZIG_SHA256=<hex> scripts/install-zig.sh   # pin the checksum explicitly
#
# Output contract: the **directory containing `zig`** goes to stdout and nothing
# else, so callers can `export PATH="$(scripts/install-zig.sh):$PATH"`. Progress
# and errors go to stderr.
#
# Trust anchor: by default the tarball's sha256 is taken from ziglang.org's own
# `download/index.json` over TLS — the same origin as the tarball. That detects
# a corrupted or truncated download but not a compromised origin, which is why
# `ZIG_SHA256` exists: set it to pin the value in a caller that has one (CI can
# hold it in a repository variable once someone has verified it by hand).
#
# Installs into `.toolchains/zig-<version>/`, already gitignored, and the same
# place `build-for-release`'s `find_zig` looks — one convention, not two.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ZIG_VERSION=${ZIG_VERSION:-0.17.0}
INDEX_URL=${ZIG_INDEX_URL:-https://ziglang.org/download/index.json}

say() { printf '%s\n' "$*" >&2; }

host_platform() {
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) echo x86_64-linux ;;
        Linux-aarch64 | Linux-arm64) echo aarch64-linux ;;
        Darwin-arm64) echo aarch64-macos ;;
        Darwin-x86_64) echo x86_64-macos ;;
        *)
            say "error: no zig build for $(uname -s)-$(uname -m); install zig yourself and set ZIG"
            return 1
            ;;
    esac
}

# A zig that is already here and already the pinned version: say where and stop.
# `zig version` is the only honest test — a directory on disk is not proof that
# the binary runs.
usable() {
    local bin=$1
    [[ -x $bin ]] || return 1
    [[ $("$bin" version 2>/dev/null) == "$ZIG_VERSION" ]]
}

existing() {
    local candidate
    if [[ -n ${ZIG:-} ]] && usable "$ZIG"; then
        dirname "$ZIG"
        return 0
    fi
    if candidate=$(command -v zig 2>/dev/null) && usable "$candidate"; then
        dirname "$candidate"
        return 0
    fi
    if usable "$ROOT/.toolchains/zig-$ZIG_VERSION/zig"; then
        echo "$ROOT/.toolchains/zig-$ZIG_VERSION"
        return 0
    fi
    return 1
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
    sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,2\}//' >&2
    exit 0
fi

check_only=0
if [[ ${1:-} == "--check" ]]; then
    check_only=1
fi

if dir=$(existing); then
    say "zig $ZIG_VERSION already available: $dir/zig"
    echo "$dir"
    exit 0
fi

if ((check_only)); then
    say "zig $ZIG_VERSION is not available (would install into $ROOT/.toolchains/zig-$ZIG_VERSION)"
    exit 1
fi

platform=$(host_platform)
tarball="zig-$platform-$ZIG_VERSION.tar.xz"
url="https://ziglang.org/download/$ZIG_VERSION/$tarball"

# Checksum: explicit pin wins, otherwise read it from the official index. A
# download without a checksum is not worth doing — a truncated tarball fails as
# "unexpected end of file" three layers deep in a build script.
sha=${ZIG_SHA256:-}
if [[ -z $sha ]]; then
    say "fetching checksums from $INDEX_URL"
    if ! sha=$(curl -fsSL "$INDEX_URL" |
        python3 -c '
import json, sys
version, platform = sys.argv[1], sys.argv[2]
try:
    index = json.load(sys.stdin)
except Exception as error:
    sys.exit(f"cannot parse the zig download index: {error}")
entry = index.get(version, {}).get(platform)
if not entry or "shasum" not in entry:
    sys.exit(f"the zig download index has no {platform} entry for {version}")
print(entry["shasum"])
' "$ZIG_VERSION" "$platform"); then
        say "error: could not obtain a checksum for zig $ZIG_VERSION ($platform)."
        say "       Set ZIG_SHA256 to the sha256 of $url and re-run."
        exit 1
    fi
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say "downloading $url"
# `-f` makes curl exit non-zero on 4xx/5xx; without this guard the only thing a
# caller sees is `curl: (22) The requested URL returned error: 403`, which says
# nothing about what to do. A proxy or firewall between the runner and
# ziglang.org is the usual cause.
if ! curl -fsSL --retry 3 --retry-delay 2 -o "$work/$tarball" "$url"; then
    say "error: could not download $url"
    say "       if this machine reaches the internet only through a proxy, allow ziglang.org,"
    say "       or pre-install zig and set ZIG=/path/to/zig"
    exit 1
fi

actual=$(sha256sum "$work/$tarball" | cut -d' ' -f1)
if [[ $actual != "$sha" ]]; then
    say "error: checksum mismatch for $tarball"
    say "  expected $sha"
    say "  actual   $actual"
    exit 1
fi

say "checksum ok, unpacking into .toolchains/zig-$ZIG_VERSION"
mkdir -p "$ROOT/.toolchains"
tar -xJf "$work/$tarball" -C "$work"
# The tarball unpacks to `zig-<platform>-<version>/`; the destination is named
# after the version alone so `find_zig`'s `zig*/zig` glob keeps working when
# several platforms share a checkout.
rm -rf "$ROOT/.toolchains/zig-$ZIG_VERSION"
mv "$work/zig-$platform-$ZIG_VERSION" "$ROOT/.toolchains/zig-$ZIG_VERSION"

dir=$ROOT/.toolchains/zig-$ZIG_VERSION
if ! usable "$dir/zig"; then
    say "error: $dir/zig did not report version $ZIG_VERSION"
    say "       got: $("$dir/zig" version 2>&1 || true)"
    exit 1
fi

say "installed zig $ZIG_VERSION: $dir/zig"
echo "$dir"
