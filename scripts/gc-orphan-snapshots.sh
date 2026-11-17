#!/usr/bin/env bash
# Reclaim disk space from snapshots whose session no longer exists.
#
# `qc sessions delete` / `gc` remove a session's snapshots along with it, so an
# orphan here means the session file was removed some other way (an older build,
# or a `--data-dir` that changed under it). Nothing can ever read those
# pre-images again: rollback and checkpoints are addressed by session id, and
# there is no session.
#
# Dry run by default; pass --yes to actually delete.
#
#   scripts/gc-orphan-snapshots.sh
#   scripts/gc-orphan-snapshots.sh --yes
#   scripts/gc-orphan-snapshots.sh --data-dir /x/qcode --yes
#
# An entry counts as an orphan only when all of these hold:
#   - snapshots/<id>/ exists,
#   - neither sessions/<id>.jsonl nor sessions/<id>/ exists,
#   - locks/<id>.lock does not exist (a live session keeps its lock),
#   - <id> looks like a ULID (26 chars, Crockford base32).
#
# Meant to be started from a normal login shell, outside the agent's Landlock
# domain: ~/.local/share is not in that domain's write set.

set -uo pipefail

usage() {
    # Print the leading comment block, whatever its length.
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "${BASH_SOURCE[0]}"
    exit "${1:-0}"
}

data_dir=""
assume_yes=0

while [ $# -gt 0 ]; do
    case $1 in
        --yes|-y) assume_yes=1 ;;
        --data-dir) shift; data_dir=${1:?--data-dir needs a path} ;;
        -h|--help) usage 0 ;;
        *) printf 'unknown option: %s\n\n' "$1" >&2; usage 1 ;;
    esac
    shift
done

# Same resolution order as the binary: explicit, env, XDG, else ~/.local/share.
if [ -z "$data_dir" ]; then
    if [ -n "${QCODE_DATA_DIR:-}" ]; then
        data_dir=$QCODE_DATA_DIR
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        data_dir=$XDG_DATA_HOME/qcode
    else
        data_dir=${HOME:?HOME is not set}/.local/share/qcode
    fi
fi

[ -d "$data_dir" ] || { printf 'no such data dir: %s\n' "$data_dir" >&2; exit 1; }
printf 'data dir: %s\n' "$data_dir"

snapshots=$data_dir/snapshots
sessions=$data_dir/sessions
locks=$data_dir/locks

if [ ! -d "$snapshots" ]; then
    printf 'no snapshots directory; nothing to do\n'
    exit 0
fi

human() { du -sh "$1" 2>/dev/null | cut -f1; }

# No `numfmt`: macOS ships bash 3.2 and BSD userland, so keep to awk.
kb_human() {
    awk -v kb="$1" 'BEGIN {
        b = kb * 1024
        split("B KB MB GB TB", unit, " ")
        i = 1
        while (b >= 1024 && i < 5) { b /= 1024; i++ }
        printf (i == 1 ? "%d%s\n" : "%.0f%s\n"), b, unit[i]
    }'
}

# ULIDs are 26 Crockford base32 chars: I, L, O and U are not in the alphabet.
is_ulid() {
    [ ${#1} -eq 26 ] || return 1
    case $1 in
        *[!0-9A-HJKMNP-TV-Z]*) return 1 ;;
    esac
    return 0
}

orphans=()
skipped=()
total_kb=0

for dir in "$snapshots"/*/; do
    [ -d "$dir" ] || continue
    id=$(basename -- "$dir")

    if ! is_ulid "$id"; then
        skipped+=("$id (not a session id)")
        continue
    fi
    # A session that still exists keeps its pre-images.
    if [ -e "$sessions/$id.jsonl" ] || [ -d "$sessions/$id" ]; then
        continue
    fi
    # A lock means the session may be live; leave it for `qc sessions gc`.
    if [ -e "$locks/$id.lock" ]; then
        skipped+=("$id (lock present — session may be running)")
        continue
    fi

    size=$(du -sk -- "$dir" 2>/dev/null | cut -f1)
    total_kb=$((total_kb + ${size:-0}))
    orphans+=("$id|${size:-0}")
done

if [ ${#skipped[@]} -gt 0 ]; then
    printf '\nskipped:\n'
    printf '  %s\n' "${skipped[@]}"
fi

if [ ${#orphans[@]} -eq 0 ]; then
    printf '\nno orphan snapshots\n'
    exit 0
fi

# Biggest first: that is the one worth deleting, and the order the eye wants.
# Sorted through `sort -rn` rather than rewriting the array, because `mapfile`
# is bash 4 and macOS still ships bash 3.2.
printf '\norphan snapshots (%d):\n' "${#orphans[@]}"
printf '%s\n' "${orphans[@]}" | awk -F'|' '{printf "%010d %s\n", $2, $1}' | sort -rn |
    while read -r kb id; do
        printf '  %-28s %s\n' "$id" "$(human "$snapshots/$id")"
    done
printf '\nreclaimable: %s\n' "$(kb_human "$total_kb")"

if [ "$assume_yes" -ne 1 ]; then
    printf '\ndry run — nothing deleted. Re-run with --yes to delete these.\n'
    exit 0
fi

printf '\ndeleting...\n'
failed=0
for entry in "${orphans[@]}"; do
    id=${entry%%|*}
    if rm -rf -- "${snapshots:?}/$id"; then
        printf '  removed %s\n' "$id"
    else
        printf '  FAILED  %s\n' "$id" >&2
        failed=$((failed + 1))
    fi
done

printf '\nremaining: %s in %s\n' "$(human "$snapshots")" "$snapshots"
[ "$failed" -eq 0 ] || exit 1
