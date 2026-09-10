#!/usr/bin/env bash
# {{APP_NAME}} 사용자 설치 (Linux). 실행 파일과 데스크톱 항목을 ~/.local 아래에 넣는다.
#   ./install.sh              설치
#   ./install.sh --uninstall  제거
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_SRC="$HERE/{{APP_SLUG}}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN_DIR/{{APP_SLUG}}" "$APP_DIR/{{APP_SLUG}}.desktop"
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  echo "제거했습니다."
  exit 0
fi

[[ -f "$BIN_SRC" ]] || { echo "실행 파일이 없습니다: $BIN_SRC"; exit 1; }
mkdir -p "$BIN_DIR" "$APP_DIR"
install -m 755 "$BIN_SRC" "$BIN_DIR/{{APP_SLUG}}"
install -m 644 "$HERE/{{APP_SLUG}}.desktop" "$APP_DIR/{{APP_SLUG}}.desktop"
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
echo "설치했습니다: $BIN_DIR/{{APP_SLUG}} ({{APP_NAME}} {{APP_VERSION}})"
echo "\$PATH 에 $BIN_DIR 이 있어야 터미널에서 바로 실행됩니다."
