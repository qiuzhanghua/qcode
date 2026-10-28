#!/usr/bin/env python3
"""Compose preview/qcode-icons-preview.png from the SVG masters."""
import re, subprocess, pathlib

ROOT = pathlib.Path(__file__).resolve().parent.parent
SRC = ROOT / "src"

def load(path):
    text = path.read_text()
    m = re.search(r'viewBox="([\d.\- ]+)"', text)
    vb = [float(v) for v in m.group(1).split()]
    inner = text.split(">", 1)[1].rsplit("</svg>", 1)[0]
    return vb, inner

VBL, BODYL = load(SRC / "qcode-mark.svg")
VBS, BODYS = load(SRC / "qcode-mark-small.svg")
VBA, BODYA = load(SRC / "qcode-app.svg")

def tint(inner, color):
    return inner.replace('#000', color)

def place(body, vb, size, x, y, color):
    s = size / vb[2]
    tx, ty = x - vb[0] * s, y - vb[1] * s
    return f'<g transform="translate({tx:.3f} {ty:.3f}) scale({s:.4f})">{tint(body, color)}</g>'

def place_app(size, x, y):
    s = size / 1024.0
    return f'<g transform="translate({x:.3f} {y:.3f}) scale({s:.6f})">{BODYA}</g>'

TOPBAR_L = "#ECEDEF"; TOPBAR_D = "#1C1C1E"
INK = "#111318"; MUTED = "#6B7280"
parts = []
parts.append(f'<rect width="1600" height="1000" fill="#F6F7F9"/>')
parts.append(f'<text x="64" y="86" font-family="Helvetica Neue, Helvetica, Arial" font-size="42" font-weight="700" fill="{INK}">QC carve</text>')
parts.append(f'<text x="64" y="122" font-family="Helvetica Neue, Helvetica, Arial" font-size="19" fill="{MUTED}">one glyph, four containers — menu-bar template · ICO · ICNS · hicolor PNG</text>')

# macOS menu bar: light + dark
parts.append(f'<text x="64" y="176" font-family="Helvetica Neue, Helvetica, Arial" font-size="16" fill="{MUTED}">macOS menu bar</text>')
parts.append(f'<rect x="64" y="190" width="1472" height="72" rx="12" fill="{TOPBAR_L}"/>')
parts.append(f'<text x="92" y="233" font-family="Helvetica Neue, Helvetica, Arial" font-size="17" fill="#9AA0A6">light appearance</text>')
parts.append(place(BODYL, VBL, 32, 1472, 210, "#000"))
parts.append(f'<rect x="64" y="272" width="1472" height="72" rx="12" fill="{TOPBAR_D}"/>')
parts.append(f'<text x="92" y="315" font-family="Helvetica Neue, Helvetica, Arial" font-size="17" fill="#8E8E93">dark appearance (template auto-tints)</text>')
parts.append(place(BODYL, VBL, 32, 1472, 292, "#FFFFFF"))

# tray sizes
parts.append(f'<text x="64" y="404" font-family="Helvetica Neue, Helvetica, Arial" font-size="16" fill="{MUTED}">Windows / Linux tray — blue on transparent</text>')
sizes = [16, 24, 32, 48, 64, 128]
x = 64; base = 540
for size in sizes:
    body, vb = (BODYS, VBS) if size <= 24 else (BODYL, VBL)
    parts.append(place(body, vb, size, x, base - size, "#2F6BFF"))
    parts.append(f'<text x="{x}" y="{base+24}" font-family="Helvetica Neue, Helvetica, Arial" font-size="14" fill="{MUTED}">{size}px</text>')
    x += size + 46

# app icon
parts.append(f'<text x="64" y="668" font-family="Helvetica Neue, Helvetica, Arial" font-size="16" fill="{MUTED}">App icon — macOS .icns / Windows .ico</text>')
parts.append(place_app(256, 64, 692))
parts.append(place_app(128, 348, 692))
parts.append(place_app(64, 500, 692))
parts.append(place_app(32, 588, 692))

# labelled file list, lower right
files = [
    ("macos/qcodeTemplate.pdf|png|@2x", "menu-bar template (black + alpha)"),
    ("macos/qcode.icns", "Dock icon, 16–1024"),
    ("windows/qcode.ico", "exe + taskbar, 16–256"),
    ("windows/qcode-tray.ico", "notification area, 16–256"),
    ("linux/hicolor/*/apps/qcode.png", "16–256 + symbolic"),
]
fy = 700
for name, note in files:
    parts.append(f'<text x="820" y="{fy}" font-family="SF Mono, Menlo, monospace" font-size="16" fill="{INK}">{name}</text>')
    parts.append(f'<text x="820" y="{fy+22}" font-family="Helvetica Neue, Helvetica, Arial" font-size="14" fill="{MUTED}">{note}</text>')
    fy += 62

svg = f'<svg xmlns="http://www.w3.org/2000/svg" width="1600" height="1000" viewBox="0 0 1600 1000">{"".join(parts)}</svg>'
svg_path = ROOT / "build" / "preview.svg"
svg_path.parent.mkdir(exist_ok=True)
svg_path.write_text(svg)
subprocess.run(["rsvg-convert", "-w", "1600", "-h", "1000", str(svg_path),
                "-o", str(ROOT / "preview" / "qcode-icons-preview.png")], check=True)
print("wrote preview/qcode-icons-preview.png")
