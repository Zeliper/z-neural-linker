#!/usr/bin/env python3
"""빌더가 대상별 `nl-runtime` 을 받아 오는 매니페스트(`runtimes/latest.json`)를 만든다.

형식은 `nl_update::Manifest` 와 같다 — `nl_bundle::manifest::write_manifest` 가 만드는 것과 같은 모양이다.
배포 앱의 `latest.json`(`packaging/make-manifest.sh`)과 달리 **자산 종류가 언제나 `binary`** 다.
빌더는 이 파일을 받아 `runtimes/<triple>/` 에 놓기만 하고 실행해 설치하지 않는다.

    make-runtimes-manifest.py <버전> <자산 기본 URL> <출력 파일> <대상키=파일> …

예)
    make-runtimes-manifest.py 0.2.0 https://updates.example/neural-linker/0.2.0/runtimes \\
        dist/runtimes/latest.json \\
        linux-x86_64=dist/nl-runtime windows-x86_64=dist/nl-runtime.exe
"""

import datetime
import hashlib
import json
import os
import sys
import urllib.parse


def base_url_ok(base: str) -> bool:
    """자산 기본 URL 이 받는 쪽(`nl_update::url::require_https`)의 규칙에 맞는가.

    https 는 언제나 통과. 평문 http 는 **두 조건을 모두** 만족할 때만 — `NL_ALLOW_HTTP=1` 이고
    호스트가 루프백일 때만 — 통과한다. 루프백 서버로 업데이트 경로를 실제로 돌려 보는
    연습(docs/RELEASE.md ⑨)에 필요하다. 바깥 주소에는 어떤 경우에도 열리지 않는다.
    """
    parsed = urllib.parse.urlsplit(base)
    if parsed.scheme == "https":
        return True
    if parsed.scheme != "http":
        print(f"자산 기본 URL 은 https 여야 합니다: {base}", file=sys.stderr)
        return False
    if os.environ.get("NL_ALLOW_HTTP") != "1":
        print(f"자산 기본 URL 은 https 여야 합니다: {base}", file=sys.stderr)
        return False
    host = (parsed.hostname or "").lower()
    if host == "localhost" or host == "::1" or host.startswith("127."):
        return True
    print(f"NL_ALLOW_HTTP 는 루프백 주소에만 먹습니다 (받은 주소: {base})", file=sys.stderr)
    return False


def main(argv: list) -> int:
    if len(argv) < 5:
        print(__doc__, file=sys.stderr)
        return 2
    version, base, out_path = argv[1], argv[2].rstrip("/"), argv[3]
    # 자산 주소는 https 여야 한다 — 받는 쪽(nl-update)이 평문 http 를 거절한다.
    if not base_url_ok(base):
        return 2

    assets = {}
    for pair in argv[4:]:
        if "=" not in pair:
            print(f"대상키=파일 꼴이어야 합니다: {pair}", file=sys.stderr)
            return 2
        key, path = pair.split("=", 1)
        try:
            with open(path, "rb") as f:
                data = f.read()
        except OSError as e:
            print(f"자산을 읽지 못했습니다 ({path}): {e}", file=sys.stderr)
            return 1
        if key in assets:
            print(f"대상 키가 겹칩니다: {key}", file=sys.stderr)
            return 1
        assets[key] = {
            "url": f"{base}/{os.path.basename(path)}",
            "sha256": hashlib.sha256(data).hexdigest(),
            "kind": "binary",
            "size": len(data),
        }

    # published_at 은 서명 대상 안에 들어가 재생 공격을 막는다 — 받는 쪽이 30일 넘은 매니페스트를 거절한다.
    published_at = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    manifest = {
        "version": version,
        "notes": f"{version} 런타임",
        "published_at": published_at,
        "assets": assets,
    }
    parent = os.path.dirname(out_path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print(json.dumps(manifest, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
