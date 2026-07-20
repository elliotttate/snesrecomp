#!/usr/bin/env python3
"""Minimal stdlib BMP(24/32-bit, BI_RGB)->PNG converter. No deps.
Usage: bmp2png.py in.bmp out.png"""
import struct, sys, zlib

def bmp2png(src, dst):
    d = open(src, "rb").read()
    if d[:2] != b"BM":
        raise ValueError("not a BMP")
    pixoff = struct.unpack_from("<I", d, 10)[0]
    hdr = struct.unpack_from("<I", d, 14)[0]
    w, h = struct.unpack_from("<ii", d, 18)
    planes, bpp = struct.unpack_from("<HH", d, 26)
    topdown = h < 0
    h = abs(h)
    if bpp not in (24, 32):
        raise ValueError(f"unsupported bpp {bpp}")
    row_bytes = ((bpp * w + 31) // 32) * 4
    rows = []
    for y in range(h):
        sy = y if topdown else (h - 1 - y)
        off = pixoff + sy * row_bytes
        out = bytearray(1 + w * 3)          # filter byte 0 + RGB
        o = 1
        for x in range(w):
            p = off + x * (bpp // 8)
            b, g, r = d[p], d[p + 1], d[p + 2]
            out[o], out[o + 1], out[o + 2] = r, g, b
            o += 3
        rows.append(bytes(out))
    raw = b"".join(rows)

    def chunk(typ, data):
        c = typ + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9))
    png += chunk(b"IEND", b"")
    open(dst, "wb").write(png)
    return w, h

if __name__ == "__main__":
    w, h = bmp2png(sys.argv[1], sys.argv[2])
    print(f"{sys.argv[2]} {w}x{h}")
