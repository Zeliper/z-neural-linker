#!/usr/bin/env bash
# {{APP_SLUG}} 사용자 설치 (Linux). 실행 파일·데스크톱 항목·아이콘을 ~/.local 아래에 넣는다.
#   ./install.sh              설치
#   ./install.sh --uninstall  제거
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 앱 이름과 버전은 단일 인용 문자열로만 들어온다 — 원문에 따옴표·$·개행이 있어도 명령이 되지 않는다.
APP_NAME={{APP_NAME}}
APP_VERSION={{APP_VERSION}}
BIN_SRC="$HERE/{{APP_SLUG}}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/256x256/apps"

refresh() {
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
}

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN_DIR/{{APP_SLUG}}" "$APP_DIR/{{APP_SLUG}}.desktop" "$ICON_DIR/{{APP_SLUG}}.png"
  refresh
  echo "제거했습니다."
  exit 0
fi

[[ -f "$BIN_SRC" ]] || { echo "실행 파일이 없습니다: $BIN_SRC"; exit 1; }
mkdir -p "$BIN_DIR" "$APP_DIR"
install -m 755 "$BIN_SRC" "$BIN_DIR/{{APP_SLUG}}"
install -m 644 "$HERE/{{APP_SLUG}}.desktop" "$APP_DIR/{{APP_SLUG}}.desktop"
if [[ -f "$HERE/{{APP_SLUG}}.png" ]]; then
  mkdir -p "$ICON_DIR"
  install -m 644 "$HERE/{{APP_SLUG}}.png" "$ICON_DIR/{{APP_SLUG}}.png"
fi
refresh
echo "설치했습니다: $BIN_DIR/{{APP_SLUG}} ($APP_NAME $APP_VERSION)"
echo "\$PATH 에 $BIN_DIR 이 있어야 터미널에서 바로 실행됩니다."
