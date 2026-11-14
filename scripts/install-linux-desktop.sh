#!/usr/bin/env bash
#
# Install the Linux desktop entry and theme icons for QCode.
#
# Why this exists: GNOME Shell (and the KDE/xfce app menus) draw an application's
# icon from the .desktop file whose name matches the window's WM_CLASS. A window
# that has neither resolves to the stock `application-x-executable` icon — the
# gear — no matter what the process puts in `_NET_WM_ICON`. So `WM_CLASS` (set by
# `crates/qcode-gui/src/window_icon.rs`) and this file have to agree:
#
#     WM_CLASS / app_id   qcode
#     .desktop basename   qcode.desktop
#     Icon=               qcode     (found at hicolor/<size>/apps/qcode.png)
#
# Changing one of those three without the others puts the gear back.
#
# Usage:
#   ./scripts/install-linux-desktop.sh              # this user only (no root)
#   ./scripts/install-linux-desktop.sh --system     # /usr/local, needs root
#   ./scripts/install-linux-desktop.sh --uninstall  # remove the user install
#   ./scripts/install-linux-desktop.sh --check      # report what is installed
#   ./scripts/install-linux-desktop.sh --exec /path/to/qc
#
# `Exec=` names whatever `qc` is on PATH. When it is not there — a checkout you
# run as `./target/release/qc --launched-from-gui`, say — pass `--exec` so the entry still starts
# the thing it points at instead of a stale or missing one.
#
# A user install goes under XDG_DATA_HOME, which is on the shell's search path by
# default, so no `update-desktop-database` and no logout are needed: both the
# .desktop directories and the icon theme are monitored at runtime. `--check`
# tells you whether the running shell can see it, rather than assuming.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

APP_ID=qcode
DESKTOP_SRC=crates/qcode/res/linux/qcode.desktop
ICON_SRC=crates/qcode/res/icons/linux/hicolor
SIZES=(16 22 24 32 48 64 128 256)

mode=user
exec_bin=
while [[ $# -gt 0 ]]; do
    case "$1" in
        --system) mode=system ;;
        --uninstall) mode=uninstall ;;
        --check) mode=check ;;
        --exec)
            [[ $# -ge 2 ]] || { echo "--exec needs a path" >&2; exit 2; }
            exec_bin=$2
            shift
            ;;
        *) echo "usage: $0 [--system|--uninstall|--check] [--exec /path/to/qc]" >&2; exit 2 ;;
    esac
    shift
done

# Whatever the entry should launch: the override, or `qc` if the shell can find it.
if [[ -n "$exec_bin" ]]; then
    [[ -x "$exec_bin" ]] || { echo "not executable: $exec_bin" >&2; exit 2; }
    # Absolute, because the entry is run by the shell from an arbitrary cwd.
    exec_cmd=$(cd "$(dirname "$exec_bin")" && printf '%s/%s' "$PWD" "$(basename "$exec_bin")")
elif command -v qc >/dev/null 2>&1; then
    exec_cmd=qc
else
    echo "warning: qc is not on PATH; the entry will not start anything." >&2
    echo "         re-run with --exec /path/to/qc to point it at a build." >&2
    echo "         note the entry will also be *ignored*, not just fail to start:" >&2
    echo "         an unresolvable Exec= invalidates the whole desktop file, so the" >&2
    echo "         window falls back to the default icon." >&2
    exec_cmd=qc
fi

# `TryExec` must resolve too, and for a different reason than `Exec`: GLib drops a
# desktop file whose TryExec is not found, and a dropped file means the shell
# cannot match the window at all — it draws its fallback icon (the gear), so the
# failure looks like a missing icon rather than a broken entry. Verified against
# libgio: `Exec=<abs> TryExec=qc` with qc off PATH parses to NULL. Keep the two in
# lockstep: the try-exec target is always whatever exec_cmd names.
try_exec=$exec_cmd

if [[ "$mode" == system ]]; then
    data_dir=/usr/local/share
else
    data_dir=${XDG_DATA_HOME:-$HOME/.local/share}
fi

desktop_dir=$data_dir/applications
icon_root=$data_dir/icons/hicolor

check() {
    local failures=0
    echo "  desktop file: $desktop_dir/$APP_ID.desktop"
    if [[ -f "$desktop_dir/$APP_ID.desktop" ]]; then
        echo "    present"
    else
        echo "    MISSING"
        failures=$((failures + 1))
    fi

    local missing=()
    for size in "${SIZES[@]}"; do
        [[ -f "$icon_root/${size}x${size}/apps/$APP_ID.png" ]] || missing+=("$size")
    done
    echo "  icons under:  $icon_root/<size>/apps/$APP_ID.png"
    if [[ ${#missing[@]} -eq 0 ]]; then
        echo "    all ${#SIZES[@]} sizes present"
    else
        echo "    MISSING sizes: ${missing[*]}"
        failures=$((failures + 1))
    fi

    # The shell resolves by name, so ask the same question it asks. `Icon=` is the
    # answer that matters: a file on disk that the theme lookup cannot reach is
    # still a gear.
    if command -v gtk4-icon-browser >/dev/null 2>&1; then :; fi
    if [[ -f "$desktop_dir/$APP_ID.desktop" ]]; then
        local declared
        declared=$(sed -n 's/^Icon=//p' "$desktop_dir/$APP_ID.desktop" | head -1)
        echo "  Icon= in the desktop file: ${declared:-(none)}"
        if [[ "$declared" != "$APP_ID" ]]; then
            echo "    does not match the icon files above; the shell will fall back"
            failures=$((failures + 1))
        fi
        local wm
        wm=$(sed -n 's/^StartupWMClass=//p' "$desktop_dir/$APP_ID.desktop" | head -1)
        echo "  StartupWMClass: ${wm:-(none)}"
        if [[ "$wm" != "$APP_ID" ]]; then
            echo "    does not match the app id the window sets; the shell will fall back"
            failures=$((failures + 1))
        fi

        # The entry is only real if GLib can load it, and GLib silently drops a file
        # whose Exec or TryExec names a program it cannot find. A dropped file gives
        # the window no app to belong to, which surfaces as the default icon — so
        # this has to be checked, or "all files present" reads as success while the
        # dock still shows the gear.
        local key value program
        for key in Exec TryExec; do
            value=$(sed -n "s/^$key=//p" "$desktop_dir/$APP_ID.desktop" | head -1)
            [[ -n "$value" ]] || continue
            program=${value%% *}
            if [[ "$program" == /* ]]; then
                if [[ -x "$program" ]]; then
                    echo "  $key: $value  (ok)"
                else
                    echo "  $key: $value  MISSING — GLib ignores the whole entry"
                    failures=$((failures + 1))
                fi
            elif command -v "$program" >/dev/null 2>&1; then
                echo "  $key: $value  (ok, via PATH)"
            else
                echo "  $key: $value  NOT ON PATH — GLib ignores the whole entry"
                echo "    re-run with --exec /path/to/qc to point it at a build"
                failures=$((failures + 1))
            fi
        done
    fi

    if command -v xprop >/dev/null 2>&1; then
        # Every running instance, not just the oldest: a stale binary looks exactly
        # like a broken build, and reporting the first window would hide it.
        local pids
        pids=$(pgrep -x qc || true)
        if [[ -z "$pids" ]]; then
            echo "  (no running qc; start it to verify WM_CLASS)"
        else
            local pid win class
            while read -r pid; do
                win=$(xdotool search --pid "$pid" 2>/dev/null | head -1 || true)
                [[ -n "$win" ]] || continue
                class=$(xprop -id "$win" WM_CLASS 2>/dev/null | sed -n 's/.*= *//p')
                echo "  pid $pid window $win: WM_CLASS ${class:-(not set)}"
                if [[ "$class" != *"\"$APP_ID\""* ]]; then
                    echo "    window does not advertise $APP_ID; the shell cannot match it"
                    failures=$((failures + 1))
                fi
            done <<< "$pids"
        fi
    fi

    if [[ $failures -eq 0 ]]; then
        echo "OK: everything the shell needs is in place."
    else
        echo "$failures problem(s) above." >&2
        return 1
    fi
}

case "$mode" in
    check)
        check
        ;;
    uninstall)
        rm -f "$desktop_dir/$APP_ID.desktop"
        for size in "${SIZES[@]}"; do
            rm -f "$icon_root/${size}x${size}/apps/$APP_ID.png"
        done
        echo "removed $APP_ID from $data_dir"
        ;;
    user | system)
        mkdir -p "$desktop_dir"
        # Rewrite Exec= and TryExec=; every other key stays exactly as it ships, so
        # the committed file remains the source of truth for what the app looks like
        # on the desktop.
        sed -e "s|^Exec=.*|Exec=$exec_cmd --launched-from-gui|" \
            -e "s|^TryExec=.*|TryExec=$try_exec|" \
            "$DESKTOP_SRC" > "$desktop_dir/$APP_ID.desktop"
        chmod 644 "$desktop_dir/$APP_ID.desktop"
        for size in "${SIZES[@]}"; do
            src=$ICON_SRC/${size}x${size}/apps/$APP_ID.png
            [[ -f "$src" ]] || { echo "missing source icon: $src" >&2; exit 1; }
            dir=$icon_root/${size}x${size}/apps
            mkdir -p "$dir"
            install -m 644 "$src" "$dir/$APP_ID.png"
        done
        # Not required by GNOME Shell, which watches these directories itself, but
        # other shells and `gio` read the cached index.
        if command -v update-desktop-database >/dev/null 2>&1; then
            update-desktop-database "$desktop_dir" 2>/dev/null || true
        fi
        for cache in gtk4-update-icon-cache gtk-update-icon-cache; do
            if command -v "$cache" >/dev/null 2>&1; then
                "$cache" -f -t "$icon_root" 2>/dev/null || true
                break
            fi
        done
        echo "installed $APP_ID into $data_dir"
        echo "  Exec=$exec_cmd --launched-from-gui"
        check
        ;;
esac
