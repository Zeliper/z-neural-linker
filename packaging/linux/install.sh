#!/usr/bin/env bash
# Neural Linker 사용자 설치 (Linux). 바이너리·데스크톱 항목·MIME(.nlproj) 연결을 ~/.local 아래에 넣는다.
#   ./install.sh [바이너리 경로]      기본: ../../target/release/nl-app
#   ./install.sh --uninstall
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_SRC="${1:-$HERE/../../target/release/nl-app}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
MIME_DIR="$HOME/.local/share/mime"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN_DIR/nl-app" "$APP_DIR/neural-linker.desktop" "$MIME_DIR/packages/neural-linker-mime.xml" "$ICON_DIR/neural-linker.svg"
  update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  echo "제거했습니다."
  exit 0
fi

[[ -x "$BIN_SRC" ]] || { echo "바이너리가 없습니다: $BIN_SRC (cargo build --release 먼저)"; exit 1; }
mkdir -p "$BIN_DIR" "$APP_DIR" "$MIME_DIR/packages" "$ICON_DIR"
install -m 755 "$BIN_SRC" "$BIN_DIR/nl-app"
install -m 644 "$HERE/neural-linker.desktop" "$APP_DIR/neural-linker.desktop"
install -m 644 "$HERE/neural-linker-mime.xml" "$MIME_DIR/packages/neural-linker-mime.xml"
install -m 644 "$HERE/neural-linker.svg" "$ICON_DIR/neural-linker.svg"
update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
xdg-mime default neural-linker.desktop application/x-neural-linker >/dev/null 2>&1 || true
echo "설치했습니다: $BIN_DIR/nl-app ($("$BIN_DIR/nl-app" --version))"
echo ".nlproj 파일을 더블클릭하면 Neural Linker 로 열립니다. \$PATH 에 $BIN_DIR 이 있어야 합니다."
