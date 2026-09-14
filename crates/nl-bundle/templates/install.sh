#!/usr/bin/env bash
# {{APP_SLUG}} 사용자 설치 (Linux). 실행 파일·데스크톱 항목·아이콘을 ~/.local 아래에 넣는다.
#   ./install.sh              설치
#   ./install.sh --uninstall  제거
#   ./install.sh --help       사용법
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

usage() {
  cat <<'USAGE'
사용법: ./install.sh [옵션]

  (옵션 없음)   설치합니다.
  --uninstall   설치한 것을 지웁니다.
  --help, -h    이 도움말을 보여 줍니다.

이 스크립트는 설치만 합니다. 앱을 실행하려면 설치한 뒤 실행 파일을 직접 부르세요.
USAGE
}

# **모르는 인자는 거부한다.** 예전에는 무시하고 그냥 설치했는데, `./install.sh --headless` 처럼
# 앱에 줄 법한 플래그를 붙이면 조용히 설치가 되어 버렸다(실제로 그렇게 잘못 설치한 적이 있다).
MODE=install
while [[ $# -gt 0 ]]; do
  case "$1" in
    --uninstall) MODE=uninstall; shift ;;
    --help|-h)   usage; exit 0 ;;
    *)
      echo "모르는 인자입니다: $1" >&2
      echo >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$MODE" == uninstall ]]; then
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
