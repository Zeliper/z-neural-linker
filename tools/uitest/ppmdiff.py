#!/usr/bin/env python3
"""두 P6 PPM 을 견줘 다른 픽셀 비율을 재고, 다르면 차이 이미지를 남긴다.

표준 라이브러리만 쓴다 — 캡처를 PPM(`grim -t ppm`)으로 받는 이유가 이것이다.
PNG 는 zlib 압축과 필터를 직접 풀어야 하지만 P6 는 헤더 뒤가 그냥 RGB 바이트다.

  ppmdiff.py <기준.ppm> <실제.ppm> [차이.ppm]

환경 변수
  UITEST_TOLERANCE     허용 오차 비율(%). 기본 0.5
  UITEST_PIXEL_DELTA   채널 차이가 이 값 이하면 같은 픽셀로 본다. 기본 8
                       (글꼴 안티에일리어싱·커서 깜빡임 같은 잔 떨림을 흡수)

표준 출력에 `<다른 픽셀 비율>% <다른 픽셀 수>/<전체>` 한 줄.
허용 오차 안이면 0, 넘으면 1, 읽지 못하면 2.
"""

import os
import sys

TOLERANCE = float(os.environ.get("UITEST_TOLERANCE", "0.5"))
DELTA = int(os.environ.get("UITEST_PIXEL_DELTA", "8"))


class PpmError(Exception):
    pass


def read_ppm(path: str):
    """P6 PPM → (너비, 높이, 픽셀 바이트). 헤더의 주석과 공백을 규격대로 넘긴다."""
    try:
        with open(path, "rb") as f:
            data = f.read()
    except OSError as e:
        raise PpmError(f"읽지 못했습니다 ({path}): {e}") from e

    if not data.startswith(b"P6"):
        raise PpmError(f"P6 PPM 이 아닙니다: {path}")

    pos = 2
    fields = []
    while len(fields) < 3:
        while pos < len(data) and data[pos : pos + 1].isspace():
            pos += 1
        if data[pos : pos + 1] == b"#":  # 주석은 줄 끝까지
            while pos < len(data) and data[pos : pos + 1] not in (b"\n", b"\r"):
                pos += 1
            continue
        start = pos
        while pos < len(data) and not data[pos : pos + 1].isspace():
            pos += 1
        if start == pos:
            raise PpmError(f"헤더가 잘렸습니다: {path}")
        fields.append(int(data[start:pos]))
    pos += 1  # 헤더와 데이터 사이의 공백 한 글자

    width, height, maxval = fields
    if maxval != 255:
        raise PpmError(f"maxval 이 255 가 아닙니다 ({maxval}): {path}")
    expected = width * height * 3
    pixels = data[pos : pos + expected]
    if len(pixels) != expected:
        raise PpmError(f"픽셀 데이터가 모자랍니다 ({len(pixels)}/{expected}): {path}")
    return width, height, pixels


def compare(a: bytes, b: bytes, width: int, height: int, want_diff: bool):
    """다른 픽셀 수와 (필요하면) 차이 이미지를 돌려준다.

    줄 단위로 먼저 견준다 — 대부분의 줄은 그대로라 바이트 비교(C 속도)로 끝나고,
    다른 줄만 픽셀 단위로 내려간다.
    """
    stride = width * 3
    differing = 0
    out = bytearray(len(a)) if want_diff else None

    for y in range(height):
        lo = y * stride
        hi = lo + stride
        row_a = a[lo:hi]
        row_b = b[lo:hi]
        if row_a == row_b:
            if want_diff:
                # 같은 곳은 원본을 어둡게 깔아 다른 곳이 눈에 띄게 한다.
                out[lo:hi] = bytes(v >> 2 for v in row_a)
            continue
        for x in range(0, stride, 3):
            dr = row_a[x] - row_b[x]
            dg = row_a[x + 1] - row_b[x + 1]
            db = row_a[x + 2] - row_b[x + 2]
            if abs(dr) > DELTA or abs(dg) > DELTA or abs(db) > DELTA:
                differing += 1
                if want_diff:
                    out[lo + x] = 255  # 다른 픽셀은 빨강
                    out[lo + x + 1] = 0
                    out[lo + x + 2] = 0
            elif want_diff:
                out[lo + x] = row_a[x] >> 2
                out[lo + x + 1] = row_a[x + 1] >> 2
                out[lo + x + 2] = row_a[x + 2] >> 2
    return differing, out


def write_ppm(path: str, width: int, height: int, pixels: bytes) -> None:
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(path, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (width, height))
        f.write(pixels)


def main(argv: list) -> int:
    if len(argv) < 3:
        print("사용법: ppmdiff.py <기준.ppm> <실제.ppm> [차이.ppm]", file=sys.stderr)
        return 2
    golden_path, actual_path = argv[1], argv[2]
    diff_path = argv[3] if len(argv) > 3 else None

    try:
        gw, gh, gpx = read_ppm(golden_path)
        aw, ah, apx = read_ppm(actual_path)
    except PpmError as e:
        print(e, file=sys.stderr)
        return 2

    if (gw, gh) != (aw, ah):
        print(f"크기가 다릅니다: 기준 {gw}x{gh}, 실제 {aw}x{ah}", file=sys.stderr)
        return 1

    total = gw * gh
    if total == 0:
        print("빈 이미지입니다", file=sys.stderr)
        return 2

    # 완전히 같으면 픽셀 순회 없이 끝낸다.
    if gpx == apx:
        print(f"0.000% 0/{total}")
        return 0

    differing, diff = compare(gpx, apx, gw, gh, want_diff=diff_path is not None)
    ratio = differing * 100.0 / total
    print(f"{ratio:.3f}% {differing}/{total}")

    if ratio <= TOLERANCE:
        return 0
    if diff_path and diff is not None:
        write_ppm(diff_path, gw, gh, bytes(diff))
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
