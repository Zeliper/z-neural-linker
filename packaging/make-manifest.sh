#!/usr/bin/env bash
# 업데이트 매니페스트(latest.json) 생성. 앱은 이 파일을 읽어 현재 버전과 비교한다.
#   ./make-manifest.sh <버전> <자산 다운로드 기본 URL> [자산 파일...]
# 예: ./make-manifest.sh 0.2.0 https://updates.trustanc.dev/pms/0.2.0 \
#       ../target/release/pms-app windows/Output/trust-pms-setup-0.2.0.exe
# 자산 파일명으로 플랫폼을 정한다: pms-app → linux-x86_64 (binary), *.exe → windows-x86_64 (installer).
set -euo pipefail
VERSION="$1"; BASE="$2"; shift 2
NOTES="${NOTES:-}"
# 발행 시각은 서명 대상 안에 들어가 재생 공격을 막는다. 배포 앱은 30일보다 오래된 매니페스트를 거절한다.
PUBLISHED_AT="${PUBLISHED_AT:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

# 자산 주소는 https 여야 한다 — 배포 앱이 평문 http 매니페스트·자산을 받지 않는다.
case "$BASE" in
  https://*) ;;
  *) echo "기본 URL 은 https 여야 합니다: $BASE" >&2; exit 1 ;;
esac
entries=()
for f in "$@"; do
  name="$(basename "$f")"
  sum="$(sha256sum "$f" | cut -d' ' -f1)"
  size="$(stat -c %s "$f")"
  case "$name" in
    *.exe) key="windows-x86_64"; kind="installer" ;;
    *)     key="linux-x86_64";   kind="binary" ;;
  esac
  entries+=("\"$key\": {\"url\": \"$BASE/$name\", \"sha256\": \"$sum\", \"kind\": \"$kind\", \"size\": $size}")
done
joined="$(IFS=,; echo "${entries[*]}")"
printf '{\n  "version": "%s",\n  "notes": "%s",\n  "published_at": "%s",\n  "assets": {%s}\n}\n' \
  "$VERSION" "$NOTES" "$PUBLISHED_AT" "$joined" > latest.json
echo "latest.json 생성:"; cat latest.json

# 선택: 매니페스트 서명. nl-update 의 verify_manifest 가 latest.json 옆의 latest.json.minisig 를 검증한다.
#   minisign -Sm latest.json                        # 비밀키 암호를 대화형으로 묻는다
#   minisign -Sm latest.json -s ~/.minisign/nl.key  # 키 파일 지정
# 공개키(minisign.pub 의 둘째 줄)는 **필수다.** 없으면 배포 앱이 업데이트 기능 자체를 켜지 않는다
# (Updater 가 Disabled 상태로 남고 런타임은 업데이트 UI 를 감춘다).
# 배포 서버가 뚫려도 바꿔치기된 매니페스트로 자산을 내려받지 않게 하는 것이 이 키의 목적이다.
# MINISIGN_KEY 를 지정하면 여기서 바로 서명한다.
if [[ -n "${MINISIGN_KEY:-}" ]]; then
  command -v minisign >/dev/null || { echo "minisign 이 없습니다 — MINISIGN_KEY 를 지정했지만 서명할 수 없습니다"; exit 1; }
  minisign -Sm latest.json -s "$MINISIGN_KEY"
  echo "서명 생성: latest.json.minisig"
fi
