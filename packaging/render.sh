#!/usr/bin/env bash
#
# 从 dist/ 里的发布归档渲染安装清单（Homebrew / Scoop / winget）。
#
#   ./packaging/render.sh                 # 读 dist/，写到 dist/manifests/
#   DIST=dist ./packaging/render.sh       # 换目录
#   OUT=dist/manifests ./packaging/render.sh
#
# 模板里的占位符只有两种：@@VERSION@@ 与 @@SHA_<平台>@@。版本来自 workspace
# Cargo.toml（与 build-for-release 同一个来源），sha256 来自 dist/ 里的归档本身——
# 这样清单里的哈希永远等于我们真正上传的字节，而不是某次手工抄写的结果。
#
# 归档缺失直接失败：清单里留空哈希比不生成更糟（用户会装到一个无法校验的包）。

set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)
DIST=${DIST:-$ROOT/dist}
OUT=${OUT:-$DIST/manifests}

die() {
    echo "error: $*" >&2
    exit 1
}

VERSION=$(grep -m1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
[ -n "$VERSION" ] || die "读不出 workspace Cargo.toml 的版本"

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "需要 sha256sum 或 shasum"
    fi
}

# 归档名与 build-for-release 的 artifact_name() 一致。
archive_hash() {
    local name="$1" path="$DIST/$1"
    [ -f "$path" ] || die "缺少 ${path}（先跑 build-for-release --archive -r）"
    sha256_of "$path"
}

render() {
    local template="$1" target="$2" sed_args=(-e "s/@@VERSION@@/$VERSION/g")
    local key value
    for key in LINUX_AMD64 LINUX_ARM64 DARWIN_AMD64 DARWIN_ARM64 WINDOWS_AMD64 WINDOWS_ARM64; do
        case "$key" in
        LINUX_*) os=linux ext=tar.gz ;;
        DARWIN_*) os=darwin ext=tar.gz ;;
        WINDOWS_*) os=windows ext=zip ;;
        esac
        case "$key" in
        *_AMD64) arch=amd64 ;;
        *_ARM64) arch=arm64 ;;
        esac
        value=$(archive_hash "qcode_${VERSION}_${os}_${arch}.${ext}")
        sed_args+=(-e "s/@@SHA_${key}@@/$value/g")
    done
    mkdir -p "$(dirname "$target")"
    sed "${sed_args[@]}" "$template" > "$target"
    echo "  rendered: ${target#"$ROOT"/}"
}

echo "==> manifests for $VERSION"
rm -rf "$OUT"
render packaging/homebrew/qc.rb.in "$OUT/homebrew/qc.rb"
render packaging/scoop/qc.json.in "$OUT/scoop/qc.json"
render packaging/winget/Qiuzhanghua.qcode.yaml.in "$OUT/winget/Qiuzhanghua.qcode.yaml"
render packaging/winget/Qiuzhanghua.qcode.installer.yaml.in "$OUT/winget/Qiuzhanghua.qcode.installer.yaml"
render packaging/winget/Qiuzhanghua.qcode.locale.en-US.yaml.in "$OUT/winget/Qiuzhanghua.qcode.locale.en-US.yaml"
echo "done: 5 manifest(s) in ${OUT#"$ROOT"/}"
