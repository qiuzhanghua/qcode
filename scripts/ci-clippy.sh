#!/usr/bin/env bash
# Run clippy and surface each diagnostic as a GitHub annotation, so a failing
# CI run is diagnosable without downloading the raw log.
#
# Two paths used to leave a red run mute:
#   * the raw line was spliced into `::error title=clippy::$line` unescaped.
#     A workflow command that does not parse is dropped without a trace, so the
#     "annotations replace the log" promise quietly stopped holding.
#   * a failure with no `path:line:col` diagnostic — a dependency download, a
#     build script, the linker — matched neither `*error:*` nor `*warning:*`
#     and so emitted nothing at all.
# Both now end up in the annotations, and in the job summary so the run page
# alone is enough to read the reason.
set -o pipefail

log="${RUNNER_TEMP:-/tmp}/qcode-clippy.log"
cargo clippy --workspace --all-targets --message-format=short -- -D warnings 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

# Workflow commands treat `%`, CR and LF specially in both the property list and
# the message body; an unescaped `%` silently invalidates the whole command.
escape() {
    local text=${1//'%'/%25}
    text=${text//$'\r'/%0D}
    text=${text//$'\n'/%0A}
    printf '%s' "$text"
}

emit() {
    local level=$1 file=$2 line=$3 col=$4 message=$5
    if [[ -n $file ]]; then
        printf '::%s file=%s,line=%s,col=%s,title=clippy::%s\n' \
            "$level" "$(escape "$file")" "$line" "$col" "$(escape "$message")"
    else
        printf '::%s title=clippy::%s\n' "$level" "$(escape "$message")"
    fi
    if [[ -n ${GITHUB_STEP_SUMMARY:-} ]]; then
        if [[ -n $file ]]; then
            printf -- '- `%s` %s:%s:%s — %s\n' "$level" "$file" "$line" "$col" "$message"
        else
            printf -- '- `%s` %s\n' "$level" "$message"
        fi >>"$GITHUB_STEP_SUMMARY"
    fi
}

if [[ $status -ne 0 ]]; then
    [[ -n ${GITHUB_STEP_SUMMARY:-} ]] && printf '### clippy\n\n' >>"$GITHUB_STEP_SUMMARY"

    located=0
    while IFS= read -r line; do
        # `--message-format=short` puts the location first. Match the Windows
        # drive letter separately: a bare `([^:]+)` would stop at the `C:` and
        # a greedy `(.+)` would instead split on a `:1:2: error:` quoted inside
        # the message.
        if [[ $line =~ ^([A-Za-z]:[^:]*):([0-9]+):([0-9]+):\ (error|warning)(\[[A-Za-z0-9_]+\])?:\ (.*)$ ]] ||
            [[ $line =~ ^([^:]+):([0-9]+):([0-9]+):\ (error|warning)(\[[A-Za-z0-9_]+\])?:\ (.*)$ ]]; then
            emit "${BASH_REMATCH[4]}" "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" \
                "${BASH_REMATCH[3]}" "${BASH_REMATCH[6]}"
            located=$((located + 1))
        fi
    done <"$log"

    # Nothing to point at means the reason is in the prose: a failed download, a
    # `pkg-config` probe, a linker error. Surface the tail rather than exit 101
    # with no explanation.
    if ((located == 0)); then
        while IFS= read -r line; do
            [[ -z ${line//[[:space:]]/} ]] && continue
            emit error "" "" "" "$line"
        done < <(tail -n 40 "$log")
    fi
fi

exit "$status"
