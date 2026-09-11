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

import hashlib
import json
import os
import sys


def main(argv: list) -> int:
    if len(argv) < 5:
        print(__doc__, file=sys.stderr)
        return 2
    version, base, out_path = argv[1], argv[2].rstrip("/"), argv[3]

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

    manifest = {"version": version, "notes": f"{version} 런타임", "assets": assets}
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
