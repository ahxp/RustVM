#!/usr/bin/env python3
"""Generates a crisp macOS RustVM.icns icon using only Python stdlib + macOS iconutil."""

import math
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib


def write_png(path: str, width: int, height: int, pixels: bytes) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)
        raw.extend(pixels[y * stride : (y + 1) * stride])

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
        + chunk(b"IEND", b"")
    )
    with open(path, "wb") as f:
        f.write(png)


def render_icon(size: int) -> bytes:
    buf = bytearray(size * size * 4)
    cx = size * 0.5
    cy = size * 0.5
    pad = size * 0.08
    half = size * 0.5 - pad
    corner = size * 0.20

    # Inner virtual display frame
    mon_l = size * 0.21
    mon_r = size * 0.79
    mon_t = size * 0.23
    mon_b = size * 0.67
    mon_rad = size * 0.055
    bezel = size * 0.032

    for y in range(size):
        py = y + 0.5
        for x in range(size):
            px = x + 0.5
            # Rounded rectangle SDF for macOS icon squircle
            dx = max(abs(px - cx) - (half - corner), 0.0)
            dy = max(abs(py - cy) - (half - corner), 0.0)
            dist = math.hypot(dx, dy) - corner
            if dist > 1.0:
                continue

            alpha = max(0.0, min(1.0, 0.5 - dist))

            # Background vertical gradient: deep obsidian to rich slate-indigo
            t = (py - pad) / max(1.0, size - 2.0 * pad)
            r = 24 + t * 18
            g = 28 + t * 24
            b = 46 + t * 34

            # Subtle orange-blue radial glow behind monitor
            gd = math.hypot((px - cx) / size, (py - cy * 0.92) / size)
            glow = max(0.0, 1.0 - gd * 2.2)
            r += glow * 45
            g += glow * 22
            b += glow * 16

            # Monitor outer frame (Rust orange -> amber gradient)
            mdx = max(mon_l + mon_rad - px, 0.0, px - (mon_r - mon_rad))
            mdy = max(mon_t + mon_rad - py, 0.0, py - (mon_b - mon_rad))
            mdist = math.hypot(mdx, mdy) - mon_rad

            if mdist <= 0.8:
                # Inside monitor bezel
                in_l = mon_l + bezel
                in_r = mon_r - bezel
                in_t = mon_t + bezel
                in_b = mon_b - bezel
                in_rad = max(2.0, mon_rad * 0.55)
                idx = max(in_l + in_rad - px, 0.0, px - (in_r - in_rad))
                idy = max(in_t + in_rad - py, 0.0, py - (in_b - in_rad))
                idist = math.hypot(idx, idy) - in_rad

                if idist <= 0.0:
                    # Dark terminal screen inside monitor
                    r, g, b = 15.0, 21.0, 33.0
                    # Terminal prompt chevron `>_` in bright emerald/amber
                    nx = (px - in_l) / max(1.0, in_r - in_l)
                    ny = (py - in_t) / max(1.0, in_b - in_t)

                    # Chevron `>`
                    in_chev_top = abs((ny - 0.34) - (nx - 0.16) * 0.95) < 0.045 and 0.16 <= nx <= 0.34
                    in_chev_bot = abs((ny - 0.68) + (nx - 0.16) * 0.95) < 0.045 and 0.16 <= nx <= 0.34
                    # Underscore `_`
                    in_cursor = 0.40 <= nx <= 0.62 and 0.62 <= ny <= 0.70
                    # Accent Linux status dots in top-left of screen
                    d1 = math.hypot(nx - 0.12, ny - 0.14) < 0.036
                    d2 = math.hypot(nx - 0.22, ny - 0.14) < 0.036
                    d3 = math.hypot(nx - 0.32, ny - 0.14) < 0.036

                    if in_chev_top or in_chev_bot:
                        r, g, b = 247.0, 114.0, 56.0  # Rust orange
                    elif in_cursor:
                        r, g, b = 82.0, 224.0, 165.0  # Mint green
                    elif d1:
                        r, g, b = 255.0, 95.0, 87.0
                    elif d2:
                        r, g, b = 254.0, 188.0, 46.0
                    elif d3:
                        r, g, b = 40.0, 200.0, 64.0
                else:
                    # Monitor bezel: vibrant Rust orange gradient
                    mt = (px - mon_l) / max(1.0, mon_r - mon_l)
                    r = 228.0 + mt * 24.0
                    g = 88.0 + mt * 52.0
                    b = 44.0 + mt * 20.0

            # Monitor stand neck & base
            neck_l, neck_r = size * 0.45, size * 0.55
            neck_t, neck_b = mon_b - 1.0, size * 0.74
            base_l, base_r = size * 0.34, size * 0.66
            base_t, base_b = size * 0.73, size * 0.775
            if (neck_l <= px <= neck_r and neck_t <= py <= neck_b) or (
                base_l <= px <= base_r and base_t <= py <= base_b
            ):
                r, g, b = 235.0, 110.0, 58.0

            # Top highlight border
            if -1.5 <= dist <= 0.5:
                r = min(255.0, r + 32.0)
                g = min(255.0, g + 34.0)
                b = min(255.0, b + 44.0)

            off = (y * size + x) * 4
            buf[off] = int(max(0, min(255, r)))
            buf[off + 1] = int(max(0, min(255, g)))
            buf[off + 2] = int(max(0, min(255, b)))
            buf[off + 3] = int(max(0, min(255, alpha * 255)))
    return bytes(buf)


def main() -> int:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <output.icns>")
        return 1
    out_icns = os.path.abspath(sys.argv[1])
    os.makedirs(os.path.dirname(out_icns), exist_ok=True)

    tmp = tempfile.mkdtemp(prefix="rustvm_icon_")
    try:
        iconset = os.path.join(tmp, "RustVM.iconset")
        os.makedirs(iconset, exist_ok=True)
        sizes = [
            ("icon_16x16.png", 16),
            ("icon_16x16@2x.png", 32),
            ("icon_32x32.png", 32),
            ("icon_32x32@2x.png", 64),
            ("icon_128x128.png", 128),
            ("icon_128x128@2x.png", 256),
            ("icon_256x256.png", 256),
            ("icon_256x256@2x.png", 512),
        ]
        rendered = {}
        for filename, sz in sizes:
            if sz not in rendered:
                rendered[sz] = render_icon(sz)
            write_png(os.path.join(iconset, filename), sz, sz, rendered[sz])

        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", out_icns],
            check=True,
        )
        print(f"Generated {out_icns}")
        return 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
