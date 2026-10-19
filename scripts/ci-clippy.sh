#!/usr/bin/env bash
# Run clippy and surface each diagnostic as a GitHub annotation, so a failing
# CI run is diagnosable without downloading the raw log.
set -o pipefail

log="${RUNNER_TEMP:-/tmp}/qcode-clippy.log"
cargo clippy --workspace --all-targets --message-format=short -- -D warnings 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

if [[ $status -ne 0 ]]; then
    while IFS= read -r line; do
        case "$line" in
            *"error:"* | *"warning:"*)
                # The message may contain `::`; GitHub escapes the rest.
                echo "::error title=clippy::$line"
                ;;
        esac
    done < "$log"
fi

exit "$status"
