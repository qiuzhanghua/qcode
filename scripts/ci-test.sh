#!/usr/bin/env bash
# Run tests and annotate failures so platform-specific breakage is visible in
# the checks API without downloading the raw log.
set -o pipefail

log="${RUNNER_TEMP:-/tmp}/qcode-test.log"
cargo test --workspace --no-fail-fast 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

if [[ $status -ne 0 ]]; then
    while IFS= read -r line; do
        case "$line" in
            *" FAILED"* | *"panicked at"* | *"assertion failed"*)
                echo "::error title=test::$line"
                ;;
        esac
    done < "$log"
fi

exit "$status"
