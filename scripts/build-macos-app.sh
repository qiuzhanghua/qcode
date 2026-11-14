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
#   ./scripts/build-macos-app.sh --debug    # debug build (faster) -> dist/QCode-debug.app
#   QCODE_APP_NAME=QCode ./scripts/build-macos-app.sh
#
# 两个 profile 落到**两个不同的 bundle**，因为体积差 5 倍：release 走 Cargo.toml 的
# `[profile.release]`（thin LTO + 1 CGU + strip = "symbols"），本地实测约 36M；dev
# profile 不优化、留着调试信息与符号表，同一份代码约 200M（__TEXT 86M + __LINKEDIT
# 116M）。早先 `--debug` 也写 dist/QCode.app，于是「跑一次 debug 包」会把手里能用的
# release 包静默换成 200M 的那份，双击起来又慢又不像成品。现在按 profile 分名，
# 谁也不覆盖谁。
#
# Output:
#   dist/QCode.app           (release)
#   dist/QCode-debug.app     (--debug)
#   <bundle>/Contents/MacOS/qc         (the binary itself; ADR-0086)
#
# 可执行文件名是 `qc`（不是 bundle 名），与 Linux 的 `qc`、Windows 的 `qc.exe` 对齐；
# Dock 显示名来自 Info.plist 的 CFBundleDisplayName，不受影响。

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
            sed -n '2,29p' "$0"
            exit 0
            ;;
        *)
            echo "error: unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

# 不碰 CARGO_HOME：用调用者环境里那份。
#
# 这里原来是「未设置就回退到仓库内的 $ROOT/.cargo-home」。那个回退是坏的——仓库里的
# .cargo-home 通常没有 config.toml（没有换源镜像），在拉不动 crates.io 的网络下
# cargo 会直接 `unable to update registry crates-io`。而且这个回退本来是为了绕开
# 「文件沙箱只允许写工作区」，而 run_shell 现在已通过 OutsideRoots::BuildCaches 直接
# 授予 CARGO_HOME 写权限（crates/qcode-tools/src/sandbox/macos.rs），绕法不再需要。

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

# release 与 debug 各占一个 bundle 名，免得 200M 的 debug 包顶掉 36M 的 release 包。
# 显式给了 QCODE_APP_NAME 就完全听调用方的。
if [[ -n "${QCODE_APP_NAME:-}" ]]; then
    APP_NAME="$QCODE_APP_NAME"
elif [[ "$PROFILE" == debug ]]; then
    APP_NAME="QCode-debug"
else
    APP_NAME="QCode"
fi
# bundle 里的可执行文件名，与 bundle 名分开：
#
#   dist/QCode.app/Contents/MacOS/qc
#
# Dock / Finder 显示的名字来自 Info.plist 的 CFBundleDisplayName，跟这个文件名无关，
# 所以叫 `qc` 不会让图标底下写出命令名。这样安排是为了**三端一致**——终端里想敲哪个
# 名字，三端都是同一个答案：
#
#   macOS    dist/QCode.app/Contents/MacOS/qc
#   Linux    <tar.gz>/qc            （.desktop 里 Exec=qc / TryExec=qc）
#   Windows  <zip>/qc.exe           （scoop bin: qc.exe，winget 指向同一个）
#
# 以前这里用的是 bundle 名（`QCode`），于是同一个程序在 mac 上叫 `QCode`、在另外两
# 端叫 `qc`/`qc.exe`，终端里敲 `qc` 的习惯到 mac 上就断了。
EXEC_NAME="${QCODE_EXEC_NAME:-qc}"

APP="$ROOT/dist/$APP_NAME.app"
BIN="$ROOT/target/$PROFILE/qc"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ -z "$VERSION" ]]; then
    echo "error: cannot read workspace version from Cargo.toml" >&2
    exit 1
fi

# 没有 `--features gui` 了（ADR-0086）：窗口是这个二进制的固有部分，不是可选项。
echo "==> building qcode ($PROFILE)"
"$CARGO_BIN" build ${PROFILE_FLAG:+"$PROFILE_FLAG"} -p qcode

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
cp "$BIN" "$APP/Contents/MacOS/$EXEC_NAME"
chmod +x "$APP/Contents/MacOS/$EXEC_NAME"

# 体积守卫。release 该 strip 过（[profile.release] strip = "symbols"），正常 36M
# 上下；突然上百兆说明 cargo 没按那个 profile 走，属于打包错了，不该悄悄产出。
# debug 侧不拦、只报数——200M 是调试的正常代价，但得让人看见。
BUNDLE_MB=$(( $(du -sk "$APP" | cut -f1) / 1024 ))
if [[ "$PROFILE" == release ]] && (( BUNDLE_MB > 100 )); then
    echo "error: release bundle is ${BUNDLE_MB}M; expected ~36M." >&2
    echo "       strip/LTO 没生效？检查 Cargo.toml 的 [profile.release]。" >&2
    exit 1
fi
if [[ "$PROFILE" == debug ]]; then
    echo "note:  debug bundle is ${BUNDLE_MB}M (unstripped, includes debug info)."
fi

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
  <!-- 必须与实际文件名一致，否则 LaunchServices 找不到可执行文件（双击直接失败）。 -->
  <key>CFBundleExecutable</key><string>$EXEC_NAME</string>
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
echo "size:  ${BUNDLE_MB}M"
echo "run:   open \"$APP\""
echo "note:  unsigned local build; distribution would require Developer ID + notarization."
