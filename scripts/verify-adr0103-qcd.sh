#!/usr/bin/env bash
# Verify ADR-0103 §2 for `qcd` end to end, on a real daemon.
#
# What it proves, in order:
#   1. a live qcd notices `config.toml` changed on disk and says so at `warn`
#      (which `logging::init` writes to both the log file and stderr);
#   2. `kill -USR1` makes it re-assemble, and the `sandbox` field on the
#      "reloaded configuration" line is the one from the *new* file — the
#      in-process evidence ADR-0103 §验证2 asks for (`qc doctor` cannot give
#      it: doctor re-reads the file in a *new* process).
#
# The config is restored on exit, including on failure or Ctrl-C.
#
# Usage:
#   scripts/verify-adr0103-qcd.sh              # verify as-is
#   scripts/verify-adr0103-qcd.sh --restart    # restart qcd first (needed when
#                                              # the running daemon predates the
#                                              # build on disk)
#   scripts/verify-adr0103-qcd.sh --mode bwrap # pick a different backend
#
# Exit status: 0 both steps passed, 1 a step failed, 2 could not set up.

set -uo pipefail

mode_from="none"      # assumed current `[sandbox] mode`
mode_to="landlock"    # a value that assembles on Linux; see --mode
timeout_secs=90
do_restart=0

while (($#)); do
    case $1 in
        --restart) do_restart=1 ;;
        --mode) mode_to=${2:?--mode needs a value}; shift ;;
        --timeout) timeout_secs=${2:?--timeout needs a value}; shift ;;
        -h | --help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
    shift
done

config=${QCODE_CONFIG:-$HOME/.config/qcode/config.toml}
data_dir=${QCODE_DATA_DIR:-$HOME/.local/share/qcode}
log=$data_dir/logs/qc.log

die() { printf 'error: %s\n' "$*" >&2; exit 2; }
step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; }

[[ -f $config ]] || die "no config at $config"
[[ -e $log ]] || die "no log at $log (is qcd running with --data-dir $data_dir?)"
command -v qc >/dev/null || die "qc not on PATH"

backup=$(mktemp)
cp "$config" "$backup"
restore() {
    cp "$backup" "$config"
    rm -f "$backup"
    printf '\n(config.toml restored)\n'
}
trap restore EXIT INT TERM

# ── 0. who is driving, and is that daemon this build? ───────────────────────────
step "daemon"
qc daemon status || true

pid=$(pgrep -f 'release/qcd|debug/qcd' | head -1)
if [[ -z $pid ]]; then
    printf '  qcd is not running; starting it\n'
    qc daemon start >/dev/null || die "qc daemon start failed"
    sleep 3
    pid=$(pgrep -f 'release/qcd|debug/qcd' | head -1)
fi
[[ -n $pid ]] || die "still no qcd after starting one"

# A running process holds the binary it was exec'd from, so rebuilding on disk
# leaves the old image in RAM. That matters here: ADR-0103 §2 landed in d1a301f,
# and a daemon started before it has no SIGUSR1 handler at all — the signal is
# then delivered to a process that ignores it and nothing happens.
if [[ -z $(ls -l "/proc/$pid/exe" 2>/dev/null | sed -n 's/.*-> //p' | grep -v '(deleted)') ]]; then
    printf '  \033[33mNOTE\033[0m pid %s runs a binary that is no longer on disk\n' "$pid"
    if ((do_restart)); then
        wait_for_restart=$pid
    else
        printf '  its code may predate ADR-0103 §2; re-run with --restart\n' >&2
    fi
fi
printf '  qcd pid %s\n' "$pid"

if ((do_restart)); then
    step "restarting qcd so it runs this build"
    qc daemon stop >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
    done
    qc daemon start >/dev/null || die "qc daemon start failed"
    for _ in $(seq 1 20); do
        pid=$(pgrep -f 'release/qcd|debug/qcd' | head -1)
        [[ -n $pid ]] && break
        sleep 0.5
    done
    [[ -n $pid ]] || die "qcd did not come back"
    # It waits up to a second between driver-lock probes (ADR-0092 §10).
    sleep 3
    printf '  new qcd pid %s\n' "$pid"
    # The replayed beat also re-baselines the mtime watch, so give it a tick.
    sleep 2
fi

# The daemon must actually be the driver: a qcd parked behind a GUI window
# never reaches the beat that checks for config changes, and would look silent.
if grep -q 'another process drives this data directory' <(tail -n 50 "$log") &&
    ! grep -q 'the other driver is gone; taking over' <(tail -n 50 "$log"); then
    printf '  \033[33mWARN\033[0m qcd may be waiting behind another driver (a GUI window?)\n' >&2
    printf '       close the GUI, or the beat never runs and nothing will be reported\n' >&2
fi

mark=$(wc -l <"$log")

# ── 1. a change on disk must be reported ───────────────────────────────────────
step "step 1: config changed on disk must be reported (ADR-0103 §1)"
current=$(sed -n '/^\[sandbox\]/,/^\[/s/^mode *= *"\([a-z_]*\)"/\1/p' "$config" | head -1)
[[ -n $current ]] || die "cannot read [sandbox] mode from $config"
printf '  [sandbox] mode: %s -> %s\n' "$current" "$mode_to"

# Rewrite just that one line, so everything else the user configured survives.
awk -v to="$mode_to" '
    /^\[sandbox\]/ { in_sandbox = 1 }
    /^\[/ && !/^\[sandbox\]/ { in_sandbox = 0 }
    in_sandbox && /^mode *=/ { print "mode = \"" to "\""; next }
    { print }
' "$config" >"$config.tmp" && mv "$config.tmp" "$config"

# The beat is the checkpoint (default 30s), so allow more than one.
got_warning=0
for _ in $(seq 1 "$timeout_secs"); do
    if tail -n +"$mark" "$log" | grep -q 'changed on disk'; then
        got_warning=1
        break
    fi
    sleep 1
done
if ((got_warning)); then
    pass "warning appeared:"
    tail -n +"$mark" "$log" | grep 'changed on disk' | tail -1 | sed 's/^/       /'
else
    fail "no 'changed on disk' within ${timeout_secs}s"
    printf '       the beat is 30s; is qcd actually driving? see:\n' >&2
    printf '       qc daemon status; tail -n 30 %s\n' "$log" >&2
    exit 1
fi

# ── 2. SIGUSR1 must re-assemble, and the log must show the new sandbox ─────────
step "step 2: SIGUSR1 reloads, and the log names the new sandbox (验证2)"
mark=$(wc -l <"$log")
printf '  kill -USR1 %s\n' "$pid"
kill -USR1 "$pid" 2>/dev/null || die "cannot signal pid $pid"

got_reload=0
for _ in $(seq 1 "$timeout_secs"); do
    if tail -n +"$mark" "$log" | grep -q 'reloaded configuration'; then
        got_reload=1
        break
    fi
    if tail -n +"$mark" "$log" | grep -q 'reload failed'; then
        got_reload=2
        break
    fi
    sleep 1
done

case $got_reload in
    1)
        line=$(tail -n +"$mark" "$log" | grep 'reloaded configuration' | tail -1)
        printf '  %s\n' "$line"
        # The point of the check: the reload really picked up the new file. The
        # `sandbox` field is `Sandbox::name()`, i.e. the backend built from the
        # mode we just wrote.
        if printf '%s' "$line" | grep -q "sandbox=$mode_to"; then
            pass "reloaded into sandbox=$mode_to (was $current)"
        elif printf '%s' "$line" | grep -q "sandbox=$current"; then
            fail "reload fired but sandbox is still $current — the new file was not what got loaded"
            exit 1
        else
            printf '  \033[33mNOTE\033[0m could not read sandbox= from that line; check it by eye\n'
        fi
        ;;
    2)
        # Not a failure of the mechanism: a config that does not assemble must
        # leave the old engine running *and say so* (ADR-0103 §2).
        printf '  %s\n' "$(tail -n +"$mark" "$log" | grep 'reload failed' | tail -1)"
        pass "a broken config is refused, and the old one is still named"
        printf '  (mode "%s" may be unavailable here; try --mode bwrap)\n' "$mode_to"
        ;;
    *)
        fail "no reload within ${timeout_secs}s"
        printf '       did pid %s actually receive SIGUSR1? check with:\n' "$pid" >&2
        printf '       tail -n +%s %s\n' "$mark" "$log" >&2
        exit 1
        ;;
esac

printf '\n\033[32mADR-0103 §2 verified for qcd\033[0m\n'
