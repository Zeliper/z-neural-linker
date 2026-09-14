#!/usr/bin/env python3
"""`neural-linker.svg` 와 같은 도형을 PNG 로 그린다. 표준 라이브러리만 쓴다.

SVG 래스터라이저(resvg·rsvg-convert)가 없는 기계에서도 아이콘을 다시 만들 수 있게 두는 것이다.
도형이 원·선분·둥근 사각형뿐이라 거리 함수로 정확히 그릴 수 있고, 거리로 앤티에일리어싱까지 한다.

    packaging/make-icon.py [크기] [출력]        기본: 256 packaging/linux/neural-linker-256.png

SVG 를 고치면 아래 도형 목록도 같이 고쳐야 한다 — 두 파일이 같은 그림을 그린다.
"""

import math
import struct
import sys
import zlib

BG = (0x17, 0x1C, 0x26)
EDGE = (0x3B, 0x6F, 0xC4)
NODE = (0x42, 0x85, 0xF4)
OUT = (0x8A, 0xB4, 0xF8)

RADIUS = 56.0  # 배경 둥근 사각형
EDGE_W = 13.0
NODE_R = 25.0

EDGES = [((70, 128), (128, 76)), ((70, 128), (128, 180)), ((128, 76), (186, 128)), ((128, 180), (186, 128))]
NODES = [((70, 128), NODE), ((128, 76), NODE), ((128, 180), NODE), ((186, 128), OUT)]


def rounded_rect_distance(x, y, w, h, r):
    """둥근 사각형 바깥이 양수인 부호 거리."""
    dx = abs(x - w / 2) - (w / 2 - r)
    dy = abs(y - h / 2) - (h / 2 - r)
    outside = math.hypot(max(dx, 0.0), max(dy, 0.0))
    return outside + min(max(dx, dy), 0.0) - r


def segment_distance(x, y, a, b):
    """선분까지의 거리. 끝을 둥글게 처리하므로 round cap 과 같다."""
    ax, ay = a
    bx, by = b
    vx, vy = bx - ax, by - ay
    wx, wy = x - ax, y - ay
    denom = vx * vx + vy * vy
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / denom))
    return math.hypot(wx - t * vx, wy - t * vy)


def coverage(d):
    """거리 → 덮인 비율. 경계 한 픽셀에 걸쳐 부드럽게 넘어간다."""
    return max(0.0, min(1.0, 0.5 - d))


def blend(dst, src, a):
    return tuple(round(dst[i] * (1 - a) + src[i] * a) for i in range(3))


def render(size):
    scale = size / 256.0
    px = bytearray()
    for py in range(size):
        px.append(0)  # PNG 필터 바이트 (없음)
        y = (py + 0.5) / scale
        for pxi in range(size):
            x = (pxi + 0.5) / scale
            bg_a = coverage(rounded_rect_distance(x, y, 256, 256, RADIUS) * scale)
            if bg_a <= 0.0:
                px.extend((0, 0, 0, 0))
                continue
            color = BG
            for a, b in EDGES:
                a_edge = coverage((segment_distance(x, y, a, b) - EDGE_W / 2) * scale)
                if a_edge > 0:
                    color = blend(color, EDGE, a_edge)
            for (cx, cy), node_color in NODES:
                a_node = coverage((math.hypot(x - cx, y - cy) - NODE_R) * scale)
                if a_node > 0:
                    color = blend(color, node_color, a_node)
            px.extend((*color, round(bg_a * 255)))
    return bytes(px)


def write_png(path, size, raw):
    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8비트 RGBA
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main(argv):
    size = int(argv[1]) if len(argv) > 1 else 256
    out = argv[2] if len(argv) > 2 else "packaging/linux/neural-linker-256.png"
    write_png(out, size, render(size))
    print(f"{out} ({size}x{size})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
