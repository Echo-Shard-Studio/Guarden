"""Build the app's block art from the two 16x16 textures in this folder (stdlib only).

    python3 app/art/render.py

- app/src/textures/{top,side}.png: textures upscaled 8x (nearest), crisp under CSS 3D transforms
- app/src/block.png and app/src-tauri/icons/icon.{png,ico}: the block in isometric view from above
"""
import struct
import zlib
from pathlib import Path

HERE = Path(__file__).parent
APP = HERE.parent


def load(path):
    """16x16 RGBA PNG (8-bit, non-interlaced) -> rows of (r, g, b, a)."""
    d = path.read_bytes()
    i, idat, w, h = 8, b"", 0, 0
    while i < len(d):
        n = struct.unpack(">I", d[i:i + 4])[0]
        t, c = d[i + 4:i + 8], d[i + 8:i + 8 + n]
        if t == b"IHDR":
            w, h, depth, color = struct.unpack(">IIBB", c[:10])
            assert (depth, color) == (8, 6), "expects 8-bit RGBA"
        elif t == b"IDAT":
            idat += c
        i += 12 + n
    raw, stride, out, prev, p = zlib.decompress(idat), w * 4, [], bytearray(w * 4), 0
    for _ in range(h):
        f, line = raw[p], bytearray(raw[p + 1:p + 1 + stride])
        p += 1 + stride
        for x in range(stride):
            a = line[x - 4] if x >= 4 else 0
            b, c = prev[x], prev[x - 4] if x >= 4 else 0
            if f == 1: line[x] = (line[x] + a) & 255
            elif f == 2: line[x] = (line[x] + b) & 255
            elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        out.append([tuple(line[x:x + 4]) for x in range(0, stride, 4)])
        prev = line
    return out


def png(rows):
    h, w = len(rows), len(rows[0])
    raw = b"".join(b"\0" + b"".join(bytes(px) for px in r) for r in rows)
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def upscale(tex, k):
    return [[tex[y // k][x // k] for x in range(16 * k)] for y in range(16 * k)]


def iso(top, side, size):
    """The block seen from above in 2:1-ish isometric view: top, left and right faces, no bottom.
    Each output pixel is mapped back onto a face and its texel (nearest), Minecraft-style shading."""
    s = size / 2  # edge length in px, the image is 2s tall
    ex, ez, ey = (0.866 * s, 0.5 * s), (-0.866 * s, 0.5 * s), (0.0, s)
    ox, oy = size / 2, 0.0  # back corner of the top face
    faces = [  # (origin, axis u, axis v, texture, brightness)
        ((ox, oy), ex, ez, top, 1.0),
        ((ox + ez[0], oy + ez[1]), ex, ey, side, 0.8),   # left face
        ((ox + ex[0], oy + ex[1]), ez, ey, side, 0.62),  # right face
    ]
    out = [[(0, 0, 0, 0)] * size for _ in range(size)]
    for y in range(size):
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            for (fx, fy), (ux, uy), (vx, vy), tex, k in faces:
                det = ux * vy - uy * vx
                a = ((px - fx) * vy - (py - fy) * vx) / det
                b = (ux * (py - fy) - uy * (px - fx)) / det
                if 0 <= a < 1 and 0 <= b < 1:
                    r, g, bl, al = tex[int(b * 16)][int(a * 16)]
                    out[y][x] = (min(255, int(r * k)), min(255, int(g * k)), min(255, int(bl * k)), al)
                    break
    return out


top, side = load(HERE / "block_top.png"), load(HERE / "block_side.png")
(APP / "src" / "textures").mkdir(exist_ok=True)
(APP / "src" / "textures" / "top.png").write_bytes(png(upscale(top, 8)))
(APP / "src" / "textures" / "side.png").write_bytes(png(upscale(side, 8)))
(APP / "src" / "block.png").write_bytes(png(iso(top, side, 512)))

icons = APP / "src-tauri" / "icons"
(icons / "icon.png").write_bytes(png(iso(top, side, 512)))
sizes = [16, 32, 48, 256]
imgs = [png(iso(top, side, n)) for n in sizes]
entries, offset = b"", 6 + 16 * len(sizes)
for n, im in zip(sizes, imgs):
    entries += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(im), offset)
    offset += len(im)
(icons / "icon.ico").write_bytes(struct.pack("<HHH", 0, 1, len(sizes)) + entries + b"".join(imgs))
print("ok")
