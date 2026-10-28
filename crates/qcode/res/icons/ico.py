#!/usr/bin/env python3
"""Assemble PNG files into a Windows .ico.

Usage: ico.py out.ico 16:in16.png 24:in24.png ...

Each output entry stores the PNG payload verbatim. Windows Vista and later
read PNG-compressed icon entries at every size, which keeps the file small;
the shell picks the best match for the current DPI.
"""

import struct
import sys


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    out = sys.argv[1]
    entries = []
    for arg in sys.argv[2:]:
        size_s, path = arg.split(":", 1)
        size = int(size_s)
        with open(path, "rb") as fh:
            data = fh.read()
        if not data.startswith(b"\x89PNG\r\n\x1a\n"):
            print(f"{path}: not a PNG", file=sys.stderr)
            return 1
        entries.append((size, data))

    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    directory = b""
    blobs = b""
    for size, data in entries:
        dim = 0 if size >= 256 else size
        directory += struct.pack(
            "<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset
        )
        blobs += data
        offset += len(data)

    with open(out, "wb") as fh:
        fh.write(header + directory + blobs)
    print(f"{out}: {len(entries)} sizes ({', '.join(str(e[0]) for e in entries)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
