#!/usr/bin/env bash
# Neural Linker 사용자 설치 (Linux). 바이너리·데스크톱 항목·MIME(.nlproj) 연결을 ~/.local 아래에 넣는다.
# 빌더(nl-app) 옆에 배포 런타임(nl-runtime)과 CLI(nl)가 같이 들어 있으면 함께 설치한다.
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
  rm -f "$BIN_DIR/nl-app" "$BIN_DIR/nl-runtime" "$BIN_DIR/nl" "$APP_DIR/neural-linker.desktop" \
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
# 배포 런타임이 같이 들어 있으면 빌더 옆에 둔다 — nl_bundle::find_runtime 규칙 ② 가 거기서 찾는다.
SRC_DIR="$(dirname "$BIN_SRC")"
if [[ -x "$SRC_DIR/nl-runtime" ]]; then
  install -m 755 "$SRC_DIR/nl-runtime" "$BIN_DIR/nl-runtime"
fi
# CLI. 실행 파일 이름은 크레이트 이름(nl-cli)이 아니라 `nl` 이다.
if [[ -x "$SRC_DIR/nl" ]]; then
  install -m 755 "$SRC_DIR/nl" "$BIN_DIR/nl"
fi
install -m 644 "$HERE/neural-linker-mime.xml" "$MIME_DIR/packages/neural-linker-mime.xml"
# 아이콘. 배포 아카이브에는 들어 있지만 빌드 트리에서 바로 돌릴 때는 없을 수 있어 조건부로 둔다.
if [[ -f "$HERE/neural-linker.svg" ]]; then
  install -m 644 "$HERE/neural-linker.svg" "$ICON_DIR/neural-linker.svg"
fi
update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
xdg-mime default neural-linker.desktop application/x-neural-linker >/dev/null 2>&1 || true
echo "설치했습니다: $BIN_DIR/nl-app ($("$BIN_DIR/nl-app" --version))"
# `set -e` 아래에서는 `[[ … ]] && echo` 가 마지막 줄이면 조건이 거짓일 때 종료 코드가 1 이 된다.
if [[ -x "$BIN_DIR/nl-runtime" ]]; then echo "             $BIN_DIR/nl-runtime (배포 런타임)"; fi
if [[ -x "$BIN_DIR/nl" ]]; then echo "             $BIN_DIR/nl (명령줄 도구)"; fi
echo ".nlproj 파일을 더블클릭하면 Neural Linker 로 열립니다. \$PATH 에 $BIN_DIR 이 있어야 합니다."
