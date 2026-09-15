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
#
# 시험용 탈출구는 클라이언트(`nl_update::url::require_https`)와 **같은 두 조건**일 때만 열린다:
# `NL_ALLOW_HTTP=1` 이고 호스트가 루프백일 것. 루프백 서버로 업데이트 경로를 실제로 돌려 보는
# 연습(docs/RELEASE.md ⑨)에 필요하다 — 여기서만 막으면 클라이언트가 받아 주는 매니페스트를
# 정작 이 도구로는 만들 수 없다. 바깥 주소에는 어떤 경우에도 열리지 않는다.
nl_base_url_ok() {
  local base="$1" rest host
  case "$base" in
    https://*) return 0 ;;
    http://*)  rest="${base#http://}" ;;
    *) echo "기본 URL 에 스킴이 없습니다 (https:// 로 시작해야 합니다): $base" >&2; return 1 ;;
  esac
  if [[ "${NL_ALLOW_HTTP:-}" != "1" ]]; then
    echo "기본 URL 은 https 여야 합니다: $base" >&2; return 1
  fi
  # 사용자 정보·경로·질의를 걷어내고 호스트만 남긴다. IPv6 리터럴은 대괄호 안이 호스트다.
  host="${rest%%[/?#]*}"
  host="${host##*@}"
  case "$host" in
    \[*\]*) host="${host#\[}"; host="${host%%\]*}" ;;
    *)        host="${host%%:*}" ;;
  esac
  case "$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')" in
    localhost|127.*|::1) return 0 ;;
    *) echo "NL_ALLOW_HTTP 는 루프백 주소에만 먹습니다 (받은 주소: $base)" >&2; return 1 ;;
  esac
}
nl_base_url_ok "$BASE" || exit 1
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
#
# 공개키는 **필수다.** 없으면 배포 앱이 업데이트 기능 자체를 켜지 않는다
# (Updater 가 Disabled 상태로 남고 런타임은 업데이트 UI 를 감춘다).
# 배포 서버가 뚫려도 바꿔치기된 매니페스트로 자산을 내려받지 않게 하는 것이 이 키의 목적이다.
#
# MINISIGN_KEY 는 **키 파일 경로**나 **키 파일 내용** 둘 다 받는다 (CI 시크릿은 내용으로 온다).
# minisign CLI 는 필요 없다 — nl-update 의 예제 도구가 같은 형식을 만든다.
#   cargo run -p nl-update --example nl-keygen -- keygen     # 키 쌍 만들기
if [[ -n "${MINISIGN_KEY:-}" ]]; then
  ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  if [[ -f "$MINISIGN_KEY" ]]; then
    cargo run -q --manifest-path "$ROOT/Cargo.toml" -p nl-update --example nl-keygen -- \
      sign latest.json --key "$MINISIGN_KEY"
  else
    # 내용이 그대로 들어온 경우. 도구가 MINISIGN_KEY 환경 변수를 직접 읽는다.
    cargo run -q --manifest-path "$ROOT/Cargo.toml" -p nl-update --example nl-keygen -- sign latest.json
  fi
fi
