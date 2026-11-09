#!/usr/bin/env python3
"""Record real interactive programs' PTY output as replayable terminal fixtures.

Each fixture is the exact byte stream a program wrote to an 80x24
`xterm-256color` PTY, from startup through quit. `crates/qcode-terminal` replays
them to check VT coverage against something a human actually looks at, instead of
a hand-written sample that only exercises what the author remembered to include.

    ./scripts/record-terminal-fixtures.py crates/qcode-terminal/tests/fixtures

Re-record only when adding a program: the committed fixtures are the test input,
so replacing them changes what is being verified. Needs `vim`, `less` and `nano`
on PATH; the PTY handling is `pty.fork`, so this runs on Unix only.
"""

import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

COLS, ROWS = 80, 24


def record(argv, steps, settle=0.45, limit=8.0, env_extra=None):
    pid, fd = pty.fork()
    if pid == 0:
        env = os.environ.copy()
        env["TERM"] = "xterm-256color"
        env["COLORTERM"] = "truecolor"
        env["LC_ALL"] = "C.UTF-8"
        if env_extra:
            env.update(env_extra)
        os.execvpe(argv[0], argv, env)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    out = bytearray()
    deadline = time.time() + limit
    pending = list(steps)
    send_at = time.time() + settle
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
        if pending and time.time() >= send_at:
            os.write(fd, pending.pop(0))
            send_at = time.time() + settle
        elif not pending and time.time() > send_at + settle:
            break

    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass
    os.waitpid(pid, 0)
    return bytes(out)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not args:
        print("usage: record-terminal-fixtures.py <dir> [name ...]", file=sys.stderr)
        return 2
    out_dir, wanted = args[0], set(args[1:])
    os.makedirs(out_dir, exist_ok=True)

    work = "/tmp/qcode-terminal-fixture"
    os.makedirs(work, exist_ok=True)
    sample = os.path.join(work, "sample.txt")
    with open(sample, "w", encoding="utf-8") as handle:
        handle.write("alpha line\nbravo line\ncharlie line\n")

    # A private tmux socket, so an existing server on this machine is neither
    # used nor disturbed. Removed afterwards; a leftover socket would make the
    # next recording attach to the old server instead of starting fresh.
    tmux_socket = "qcode-terminal-fixture"

    def tmux(*argv, steps, **kwargs):
        return record(["tmux", "-L", tmux_socket, *argv], steps, **kwargs)

    fixtures = {}

    # vim: alternate screen, cursor addressing, SGR, status line, then back to
    # the primary screen on quit. Only the file's own name and text are asserted.
    fixtures["vim"] = lambda: record(
        ["vim", "-u", "NONE", "-N", "-n", sample],
        [b"ihello qcode\x1b", b":q!\r"],
    )

    # less: alternate screen pager over a known file, then quit.
    fixtures["less"] = lambda: record(["/usr/bin/less", sample], [b"q"])

    # nano: a different alternate-screen UI with a bottom help bar.
    fixtures["nano"] = lambda: record(["nano", "-R", sample], [b"\x18n"])  # Ctrl-X, "no"

    # htop: the heaviest user of cursor addressing and SGR in this set — every
    # meter redraw moves the cursor cell by cell, and the header is repainted on
    # each tick. Quits on `q` and restores the primary screen.
    fixtures["htop"] = lambda: record(["htop", "-d", "50"], [b"q"], settle=1.2)

    # tmux: the only fixture that draws with a scroll region, a pane border and a
    # status line, and the only one that queries the terminal's capabilities
    # (DA, colours) — which is exactly what the DSR/DA wiring has to answer.
    # Detaches with `C-b d` so the output ends in a clean restore.
    fixtures["tmux"] = lambda: tmux(
        "new-session",
        "-x",
        "80",
        "-y",
        "24",
        "sh",
        "-c",
        "printf 'alpha line\\nbravo line\\n'; sleep 30",
        steps=[b"\x02d"],
        settle=1.5,
    )

    try:
        for name in (sorted(fixtures) if not wanted else sorted(wanted)):
            if name not in fixtures:
                print(f"  unknown fixture {name!r}", file=sys.stderr)
                return 2
            data = fixtures[name]()
            path = os.path.join(out_dir, f"{name}.bin")
            with open(path, "wb") as handle:
                handle.write(data)
            print(f"  {name}: {len(data)} bytes -> {path}")
    finally:
        # Never leave a server behind: it would keep the socket alive and the
        # next recording would attach to it.
        os.spawnlp(os.P_NOWAIT, "tmux", "tmux", "-L", tmux_socket, "kill-server")
        time.sleep(0.1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
