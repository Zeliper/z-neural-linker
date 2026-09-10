#!/usr/bin/env bash
# 업데이트 매니페스트(latest.json) 생성. 앱은 이 파일을 읽어 현재 버전과 비교한다.
#   ./make-manifest.sh <버전> <자산 다운로드 기본 URL> [자산 파일...]
# 예: ./make-manifest.sh 0.2.0 https://updates.trustanc.dev/pms/0.2.0 \
#       ../target/release/pms-app windows/Output/trust-pms-setup-0.2.0.exe
# 자산 파일명으로 플랫폼을 정한다: pms-app → linux-x86_64 (binary), *.exe → windows-x86_64 (installer).
set -euo pipefail
VERSION="$1"; BASE="$2"; shift 2
NOTES="${NOTES:-}"
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
printf '{\n  "version": "%s",\n  "notes": "%s",\n  "assets": {%s}\n}\n' "$VERSION" "$NOTES" "$joined" > latest.json
echo "latest.json 생성:"; cat latest.json
