#!/usr/bin/env bash
#
# Drop snapshot pre-images for sessions that still exist.
#
# This is not `sessions gc`. The session logs stay; what goes is the ability to
# roll those sessions back — `qc sessions rollback` then reports nothing to do
# and `qc sessions checkpoints` is empty. That is why it needs --yes.
#
# It exists because of a specific, measured mess. Before the ADR-0101 tree-
# snapshot fixes, a frontend started in `$HOME` walked the whole home directory
# instead of the session's root: one machine held 22.3 GB of pre-images, 90.9%
# of it `.WebUI` (a browser profile), `.bun/install/cache` and `.anaconda_backup`,
# and in 250 of its turns the 64 MB budget ran out before a single file of the
# checkout was recorded. Those bytes cannot restore anything worth having, so
# deleting them costs nothing that was not already worthless.
#
# It refuses to run while a frontend is alive, because an old build writes
# snapshots as fast as this deletes them — measured at 297 MB back in five
# minutes. Pass --stop/--start to have it manage the frontend itself.
#
#   scripts/reset-snapshots.sh                          # dry run, all sessions
#   scripts/reset-snapshots.sh --yes                    # delete all
#   scripts/reset-snapshots.sh --yes <id> [<id>...]     # only these sessions
#   scripts/reset-snapshots.sh --yes --stop --start     # stop, delete, reopen
#
# Options:
#   --yes            actually delete (default is a dry run)
#   --data-dir DIR   data directory (default: $HOME/.local/share/qcode)
#   --stop           stop qcd and the frontend first
#   --start          open the app bundle afterwards (macOS)
#   -h, --help       this text
#
# After it finishes, check that the fix holds: run one turn that uses
# `run_shell`, then
#
#   find "$DATA_DIR/snapshots" \( -name .WebUI -o -name Library -o -name .bun \) | head
#
# An empty result means the snapshot followed the session root. Anything printed
# means something is still walking the home directory — check that the running
# binary is the rebuilt one (`ls -l dist/QCode.app/Contents/MacOS/qc`, compare
# its timestamp with the process start time in `ps -o lstart= -p <pid>`).

set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
APP="$ROOT/dist/QCode.app"
DATA_DIR="${HOME}/.local/share/qcode"
YES=0
STOP=0
START=0
IDS=()

usage() {
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
    --yes) YES=1 ;;
    --stop) STOP=1 ;;
    --start) START=1 ;;
    --data-dir)
        shift
        DATA_DIR=${1:?--data-dir needs a value}
        ;;
    -h | --help) usage 0 ;;
    -*) echo "unknown option: $1" >&2; usage 1 ;;
    *) IDS+=("$1") ;;
    esac
    shift
done

SNAPSHOTS="$DATA_DIR/snapshots"

if [ ! -d "$SNAPSHOTS" ]; then
    echo "no snapshots directory at $SNAPSHOTS — nothing to do"
    exit 0
fi

# The frontends that could be writing *this* data directory.
#
# `qcd` carries `--data-dir`, so it names its directory outright. The GUI's own
# process does not, and it only matters when we are operating on the directory
# that GUI uses — otherwise a test run against a scratch `--data-dir` would be
# blocked by an unrelated window.
DEFAULT_DATA_DIR="${HOME}/.local/share/qcode"
frontends() {
    {
        pgrep -f "qcd --data-dir ${DATA_DIR}" 2>/dev/null
        if [ "$DATA_DIR" = "$DEFAULT_DATA_DIR" ]; then
            pgrep -f "${APP}/Contents/MacOS/qc" 2>/dev/null
        fi
    } | sort -u
}

wait_gone() {
    local waited=0
    while [ -n "$(frontends)" ]; do
        if [ "$waited" -ge 50 ]; then
            return 1
        fi
        sleep 0.2
        waited=$((waited + 1))
    done
    return 0
}

size_of() {
    du -sk "$1" 2>/dev/null | awk '{ printf "%.1f", $1 / 1024 }'
}

human() {
    # $1 = KiB
    awk -v k="$1" 'BEGIN {
        if (k >= 1048576) printf "%.1f GB", k / 1048576;
        else if (k >= 1024) printf "%.0f MB", k / 1024;
        else printf "%.0f KB", k;
    }'
}

# Which sessions are we touching? Every directory under snapshots/ that looks
# like a session id, or only the ones named on the command line.
targets() {
    local entry name
    for entry in "$SNAPSHOTS"/*; do
        [ -d "$entry" ] || continue
        name=$(basename "$entry")
        if [ ${#IDS[@]} -eq 0 ]; then
            echo "$name"
        else
            for want in "${IDS[@]}"; do
                if [ "$want" = "$name" ]; then
                    echo "$name"
                fi
            done
        fi
    done
}

TARGET_LIST=$(targets)
if [ -z "$TARGET_LIST" ]; then
    echo "nothing matched under $SNAPSHOTS"
    if [ ${#IDS[@]} -gt 0 ]; then
        echo "named: ${IDS[*]}"
        echo "present:"
        for entry in "$SNAPSHOTS"/*; do [ -d "$entry" ] && echo "  $(basename "$entry")"; done
    fi
    exit 1
fi

# A session that still has a log but no snapshot tree is fine; a session we are
# about to reset should still be listed here, or the user is deleting a tree
# whose session is already gone (that is `gc-orphan-snapshots.sh`'s job).
echo "data dir:  $DATA_DIR"
echo "sessions:  $(echo "$TARGET_LIST" | wc -l | tr -d ' ')"
echo
total_kb=0
while IFS= read -r id; do
    kb=$(du -sk "$SNAPSHOTS/$id" 2>/dev/null | awk '{print $1}')
    kb=${kb:-0}
    total_kb=$((total_kb + kb))
    turns=$(find "$SNAPSHOTS/$id" -maxdepth 1 -type d ! -path "$SNAPSHOTS/$id" 2>/dev/null | wc -l | tr -d ' ')
    log="no session log"
    if [ -f "$DATA_DIR/sessions/$id.jsonl" ]; then
        log="log kept"
    fi
    printf '  %s  %8s  %4s turn(s)  %s\n' "$id" "$(human "$kb")" "$turns" "$log"
done <<<"$TARGET_LIST"
echo
echo "total: $(human "$total_kb")"

if [ "$YES" -eq 0 ]; then
    echo
    echo "dry run. Re-running with --yes deletes these pre-images for good:"
    echo "rollback of these sessions will find nothing."
    exit 0
fi

# Delete only after the writers are gone: otherwise an old build recreates the
# tree while we walk it, and the result is neither empty nor consistent.
if [ -n "$(frontends)" ]; then
    if [ "$STOP" -eq 0 ]; then
        echo >&2
        echo "a qcode frontend is running (pid $(frontends | tr '\n' ' '))." >&2
        echo "An old build will recreate these snapshots as fast as we delete them." >&2
        echo "Re-run with --stop (and --start to reopen afterwards)." >&2
        exit 1
    fi
    echo
    echo "stopping frontend(s): $(frontends | tr '\n' ' ')"
    # shellcheck disable=SC2046
    kill $(frontends) 2>/dev/null
    if ! wait_gone; then
        echo >&2 "still running after 10s; not deleting. Stop it by hand first."
        exit 1
    fi
    echo "stopped"
fi

echo
freed=0
while IFS= read -r id; do
    kb=$(du -sk "$SNAPSHOTS/$id" 2>/dev/null | awk '{print $1}')
    kb=${kb:-0}
    if rm -rf "$SNAPSHOTS/$id"; then
        freed=$((freed + kb))
        printf '  removed %s\n' "$id"
    else
        printf '  FAILED to remove %s\n' "$id" >&2
    fi
done <<<"$TARGET_LIST"
echo
echo "freed $(human "$freed")"

if [ "$START" -eq 1 ]; then
    if [ -d "$APP" ]; then
        echo "opening $APP"
        open "$APP"
    else
        echo >&2 "no app bundle at $APP; start it however you normally do"
    fi
fi

echo
echo "verify with one \`run_shell\` turn, then:"
echo "  find \"$SNAPSHOTS\" \\( -name .WebUI -o -name Library -o -name .bun \\) | head"
echo "nothing printed = the snapshot followed the session root."
