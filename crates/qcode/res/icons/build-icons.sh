#!/usr/bin/env bash
#
# Build the qcode icon set from the SVG masters in src/.
#
# Requires: rsvg-convert, python3, and (on macOS) iconutil.
#
#   ./build-icons.sh
#
# Outputs (all committed, so a normal build does not need this script):
#   macos/qcodeTemplate.pdf|png|@2x.png   menu-bar template (black + alpha)
#   macos/qcode.icns                      Dock / app icon
#   windows/qcode.ico                     exe + taskbar app icon
#   windows/qcode-tray.ico                notification-area mark (brand blue)
#   linux/hicolor/<size>/apps/qcode.png   tray / launcher icons
#   linux/symbolic/apps/qcode-symbolic.svg
#
# The mark is drawn on a square canvas with ~6% padding per side, so rendering
# to N x N never distorts it. Sizes up to 24 use the optical "small" master
# (bolder, no snow spray); larger sizes use the full master.

set -euo pipefail
cd "$(dirname "$0")"

SMALL=src/qcode-mark-small.svg
LARGE=src/qcode-mark.svg
APP=src/qcode-app.svg
BRAND_BLUE='#2F6BFF'

for tool in rsvg-convert python3; do
    command -v "$tool" >/dev/null || { echo "error: missing $tool" >&2; exit 1; }
done

rm -rf build
mkdir -p build macos windows linux/symbolic/apps

# Brand-coloured copies for the platforms that do not tint icons themselves.
sed "s|#000|$BRAND_BLUE|g" "$SMALL" > build/mark-small-blue.svg
sed "s|#000|$BRAND_BLUE|g" "$LARGE" > build/mark-blue.svg

render() { # svg size out
    rsvg-convert -w "$2" -h "$2" "$1" -o "$3"
}
pick() { # size -> small master below 32, full master above
    if [ "$1" -le 24 ]; then echo "$SMALL"; else echo "$LARGE"; fi
}

# --- Windows ---------------------------------------------------------------
# App icon: every size Windows may request (16..256), see
# https://learn.microsoft.com/windows/apps/design/iconography/app-icon-construction
win_args=()
for size in 16 20 24 32 40 48 64 128 256; do
    render "$APP" "$size" "build/app-$size.png"
    win_args+=("$size:build/app-$size.png")
done
python3 ico.py windows/qcode.ico "${win_args[@]}"

# Tray mark: same sizes, brand blue so it stays visible on light and dark taskbars.
win_args=()
for size in 16 20 24 32 40 48 64 128 256; do
    if [ "$size" -le 24 ]; then src=build/mark-small-blue.svg; else src=build/mark-blue.svg; fi
    render "$src" "$size" "build/tray-$size.png"
    win_args+=("$size:build/tray-$size.png")
done
python3 ico.py windows/qcode-tray.ico "${win_args[@]}"

# --- macOS -----------------------------------------------------------------
# Menu-bar template: black + alpha, the system tints it for light/dark mode.
render "$SMALL" 16 macos/qcodeTemplate.png
render "$LARGE" 32 macos/qcodeTemplate@2x.png
rsvg-convert -f pdf -o macos/qcodeTemplate.pdf "$LARGE"

# Dock icon.
icns_sizes=(16 32 128 256 512)
if command -v iconutil >/dev/null; then
    iconset=build/qcode.iconset
    mkdir -p "$iconset"
    for size in "${icns_sizes[@]}"; do
        render "$APP" "$size" "$iconset/icon_${size}x${size}.png"
        render "$APP" "$((size * 2))" "$iconset/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$iconset" -o macos/qcode.icns
else
    echo "warning: iconutil not found, skipping macos/qcode.icns" >&2
fi

# --- Linux -----------------------------------------------------------------
# hicolor theme PNGs; name the file after the SNI IconName ("qcode").
for size in 16 22 24 32 48 64 128 256; do
    if [ "$size" -le 24 ]; then src=build/mark-small-blue.svg; else src=build/mark-blue.svg; fi
    dir="linux/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    render "$src" "$size" "$dir/qcode.png"
done
cp "$SMALL" linux/symbolic/apps/qcode-symbolic.svg

echo "done:"
find macos windows linux -type f | sort | sed 's/^/  /'
