#!/usr/bin/env bash
#
# Build an unsigned (ad-hoc signed) macOS .app for local P0 use.
#
# No Apple Developer account, certificate, or notarization is required. The
# bundle runs on the machine that built it; distributing it to other machines
# will still trigger Gatekeeper unless users remove the quarantine bit or the
# app is later Developer ID signed/notarized.
#
# Usage:
#   ./scripts/build-macos-app.sh            # release build -> dist/QCode.app
#   ./scripts/build-macos-app.sh --debug    # debug build (faster)
#   QCODE_APP_NAME=QCode ./scripts/build-macos-app.sh
#
# Output:
#   dist/QCode.app
#   dist/QCode.app/Contents/MacOS/QCode      (the binary itself; ADR-0086)

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: this script only builds macOS .app bundles" >&2
    exit 1
fi

PROFILE=release
PROFILE_FLAG=--release
for arg in "$@"; do
    case "$arg" in
        --debug)
            PROFILE=debug
            PROFILE_FLAG=
            ;;
        --release)
            PROFILE=release
            PROFILE_FLAG=--release
            ;;
        -h|--help)
            sed -n '2,25p' "$0"
            exit 0
            ;;
        *)
            echo "error: unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

if [[ -z "${CARGO_HOME:-}" && -d "$ROOT/.cargo-home" ]]; then
    export CARGO_HOME="$ROOT/.cargo-home"
fi

# Login shells in this repo often get cargo from `c`; non-interactive callers
# may not have it on PATH.
CARGO_BIN=$(command -v cargo || true)
if [[ -z "$CARGO_BIN" ]]; then
    for candidate in "$HOME/.cargo/bin/cargo" "/opt/homebrew/bin/cargo"; do
        if [[ -x "$candidate" ]]; then
            CARGO_BIN="$candidate"
            break
        fi
    done
fi
if [[ -z "$CARGO_BIN" ]]; then
    echo "error: cargo not found; activate the rust toolchain or set PATH" >&2
    exit 1
fi

APP_NAME="${QCODE_APP_NAME:-QCode}"
APP="$ROOT/dist/$APP_NAME.app"
BIN="$ROOT/target/$PROFILE/qc"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ -z "$VERSION" ]]; then
    echo "error: cannot read workspace version from Cargo.toml" >&2
    exit 1
fi

echo "==> building qcode with the gui feature ($PROFILE)"
"$CARGO_BIN" build ${PROFILE_FLAG:+"$PROFILE_FLAG"} -p qcode --features gui

if [[ ! -x "$BIN" ]]; then
    echo "error: built binary not found: $BIN" >&2
    exit 1
fi

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Dock / Finder icon (the menu-bar template is embedded in the binary).
cp "$ROOT/crates/qcode/res/icons/macos/qcode.icns" "$APP/Contents/Resources/qcode.icns"

# 一份可执行文件（ADR-0086）。以前这里放两份：`qc-gui`（其实就是 `qc` 的副本，
# 只是改了名）加一个启动器脚本，脚本负责 `cd $HOME` 再 `exec ... gui`。现在两件事
# 都由 `qc` 自己做：
#
#   - **cwd 兜底**：Finder 启动时 cwd 是 `/`，`qc` 判出来会退到家目录（ADR-0085；
#     实测 `open -a` 给的就是 `/`）。
#   - **前端选择**：`CFBundleExecutable` 指向 `qc`，`Info.plist` 的
#     `LSEnvironment` 传内部标记，于是「双击」这件事本身就是判据（ADR-0086）。
#     没有启动器脚本可改、可绕过，也就没有「脚本与二进制谁说了算」的问题。
#
# 名字仍用 $APP_NAME（QCode），这样 Dock 里显示的是产品名而不是命令名。
cp "$BIN" "$APP/Contents/MacOS/$APP_NAME"
chmod +x "$APP/Contents/MacOS/$APP_NAME"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>qcode</string>
  <key>CFBundleDisplayName</key><string>qcode</string>
  <key>CFBundleIdentifier</key><string>com.qcode.desktop</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>$APP_NAME</string>
  <key>CFBundleIconFile</key><string>qcode.icns</string>
  <!-- ADR-0086：双击/点图标这条路上，唯一的入口就是我们自己生成的 plist，
       所以这里挂内部标记。它比任何平台信号都硬——判定不必猜。 -->
  <key>LSEnvironment</key>
  <dict>
    <key>QCODE_LAUNCHED_FROM_GUI</key><string>1</string>
  </dict>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHumanReadableCopyright</key><string>qcode contributors</string>
</dict>
</plist>
PLIST

printf 'APPL????' > "$APP/Contents/PkgInfo"
plutil -lint "$APP/Contents/Info.plist" >/dev/null

echo "==> ad-hoc signing (no Apple account)"
# Ad-hoc identity "-" makes Apple Silicon accept the bundle locally. It is not
# Developer ID and will not pass `spctl`/notarization for distribution.
codesign --force --deep --sign - "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

echo
echo "built: $APP"
echo "run:   open \"$APP\""
echo "note:  unsigned local build; distribution would require Developer ID + notarization."
