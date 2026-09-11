#!/usr/bin/env bash
# Neural Linker 사용자 설치 (Linux). 바이너리·데스크톱 항목·MIME(.nlproj) 연결을 ~/.local 아래에 넣는다.
#   ./install.sh [바이너리 경로]      기본: ../../target/release/nl-app
#   ./install.sh --uninstall
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# 인자로 주면 그것을, 없으면 스크립트 옆(배포 아카이브를 푼 자리) → 빌드 트리 순으로 찾는다.
if [[ -n "${1:-}" && "${1:-}" != "--uninstall" ]]; then
  BIN_SRC="$1"
elif [[ -x "$HERE/nl-app" ]]; then
  BIN_SRC="$HERE/nl-app"
else
  BIN_SRC="$HERE/../../target/release/nl-app"
fi
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
MIME_DIR="$HOME/.local/share/mime"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"

if [[ "${1:-}" == "--uninstall" ]]; then
  rm -f "$BIN_DIR/nl-app" "$BIN_DIR/nl-runtime" "$APP_DIR/neural-linker.desktop" \
        "$MIME_DIR/packages/neural-linker-mime.xml" "$ICON_DIR/neural-linker.svg"
  update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  echo "제거했습니다."
  exit 0
fi

[[ -x "$BIN_SRC" ]] || { echo "바이너리가 없습니다: $BIN_SRC (cargo build --release 먼저)"; exit 1; }
mkdir -p "$BIN_DIR" "$APP_DIR" "$MIME_DIR/packages" "$ICON_DIR"
install -m 755 "$BIN_SRC" "$BIN_DIR/nl-app"
install -m 644 "$HERE/neural-linker.desktop" "$APP_DIR/neural-linker.desktop"
# 배포 런타임이 같이 들어 있으면 빌더 옆에 둔다 — nl_bundle::find_runtime 이 거기서 찾는다.
if [[ -x "$(dirname "$BIN_SRC")/nl-runtime" ]]; then
  install -m 755 "$(dirname "$BIN_SRC")/nl-runtime" "$BIN_DIR/nl-runtime"
fi
install -m 644 "$HERE/neural-linker-mime.xml" "$MIME_DIR/packages/neural-linker-mime.xml"
# 아이콘은 아직 저장소에 없다. 있으면 넣고 없으면 넘어간다 — set -e 때문에 여기서 설치가 통째로 멈추면 안 된다.
if [[ -f "$HERE/neural-linker.svg" ]]; then
  install -m 644 "$HERE/neural-linker.svg" "$ICON_DIR/neural-linker.svg"
fi
update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
xdg-mime default neural-linker.desktop application/x-neural-linker >/dev/null 2>&1 || true
echo "설치했습니다: $BIN_DIR/nl-app ($("$BIN_DIR/nl-app" --version))"
echo ".nlproj 파일을 더블클릭하면 Neural Linker 로 열립니다. \$PATH 에 $BIN_DIR 이 있어야 합니다."
