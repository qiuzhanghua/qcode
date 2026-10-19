#!/usr/bin/env bash
#
# macOS 签名 + 公证的占位脚本（SPEC §17 M4）。
#
# 真正公证需要 Apple Developer Program 的 Developer ID Application 证书，
# 以及 app-specific password 或 API key。CI/本机没有凭据时不会运行此脚本。
#
# 用法：
#   APPLE_SIGNING_IDENTITY="Developer ID Application: ..." \
#   APPLE_ID="you@example.com" \
#   APPLE_TEAM_ID="ABCDE12345" \
#   APPLE_APP_PASSWORD="xxxx-xxxx-xxxx-xxxx" \
#   ./notarize-macos.sh dist/qcode_0.1.0_darwin_arm64
#
# 只做三件事：codesign（hardened runtime + timestamp）、zip、notarytool submit。
# 裸 Mach-O 不能 stapler；如需离线票据，请把 CLI 包进 .app/.pkg/.dmg 后再扩展本脚本。

set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "notarize-macos.sh: 只能在 macOS 上运行" >&2
    exit 2
fi

: "${APPLE_SIGNING_IDENTITY:?set APPLE_SIGNING_IDENTITY to a Developer ID Application identity}"
: "${APPLE_ID:?set APPLE_ID}"
: "${APPLE_TEAM_ID:?set APPLE_TEAM_ID}"
: "${APPLE_APP_PASSWORD:?set APPLE_APP_PASSWORD (app-specific password)}"

if [[ $# -eq 0 ]]; then
    echo "usage: $0 <artifact>..." >&2
    exit 2
fi

for artifact in "$@"; do
    if [[ ! -f "$artifact" ]]; then
        echo "notarize-macos.sh: not a file: $artifact" >&2
        exit 1
    fi

    echo "==> signing $artifact"
    codesign --force --timestamp --options runtime \
        --sign "$APPLE_SIGNING_IDENTITY" "$artifact"
    codesign --verify --strict --verbose=2 "$artifact"

    zip="${artifact}.zip"
    rm -f "$zip"
    ditto -c -k --keepParent "$artifact" "$zip"

    echo "==> notarizing $zip"
    xcrun notarytool submit "$zip" \
        --apple-id "$APPLE_ID" \
        --team-id "$APPLE_TEAM_ID" \
        --password "$APPLE_APP_PASSWORD" \
        --wait

    echo "notarized: $zip (ticket is stored by Apple; Gatekeeper checks online)"
done
