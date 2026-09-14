#!/usr/bin/env bash
# Neural Linker 사용자 설치 (Linux). 바이너리·데스크톱 항목·MIME(.nlproj) 연결을 ~/.local 아래에 넣는다.
# 빌더(nl-app) 옆에 배포 런타임(nl-runtime)과 CLI(nl)가 같이 들어 있으면 함께 설치한다.
#   ./install.sh [바이너리 경로]          기본: ../../target/release/nl-app
#   ./install.sh --service <앱 경로>     위에 더해 배포 앱을 헤드리스 사용자 서비스로 등록한다
#   ./install.sh --uninstall             서비스까지 함께 제거한다
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MODE=install
SERVICE_APP=""
case "${1:-}" in
  --uninstall) MODE=uninstall; shift ;;
  --service)   MODE=service; SERVICE_APP="${2:-}"; shift 2 || true ;;
esac

# 인자로 주면 그것을, 없으면 스크립트 옆(배포 아카이브를 푼 자리) → 빌드 트리 순으로 찾는다.
if [[ -n "${1:-}" ]]; then
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
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_NAME="neural-linker-app.service"
ENV_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/neural-linker"

if [[ "$MODE" == uninstall ]]; then
  # 서비스가 돌고 있으면 먼저 세운다. systemd 가 없는 환경에서도 설치는 지워져야 하므로 실패를 무시한다.
  if command -v systemctl >/dev/null; then
    systemctl --user disable --now "$UNIT_NAME" >/dev/null 2>&1 || true
  fi
  rm -f "$UNIT_DIR/$UNIT_NAME"
  if command -v systemctl >/dev/null; then
    systemctl --user daemon-reload >/dev/null 2>&1 || true
  fi
  # app.env 는 사용자가 쓴 비밀이라 지우지 않는다 — 지웠다가는 토큰을 잃는다.
  rm -f "$BIN_DIR/nl-app" "$BIN_DIR/nl-runtime" "$BIN_DIR/nl" "$APP_DIR/neural-linker.desktop" \
        "$MIME_DIR/packages/neural-linker-mime.xml" "$ICON_DIR/neural-linker.svg"
  update-mime-database "$MIME_DIR" >/dev/null 2>&1 || true
  update-desktop-database "$APP_DIR" >/dev/null 2>&1 || true
  echo "제거했습니다."
  if [[ -f "$ENV_DIR/app.env" ]]; then
    echo "환경 파일은 남겨 두었습니다(비밀이 들어 있습니다): $ENV_DIR/app.env"
  fi
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

# ── 배포 앱을 헤드리스 서비스로 (--service) ──────────────────────────
if [[ "$MODE" == service ]]; then
  [[ -n "$SERVICE_APP" ]] || { echo "--service 뒤에 배포 앱 경로가 필요합니다"; exit 1; }
  # `-x` 는 디렉터리에도 참이라 `-f` 를 함께 본다.
  [[ -f "$SERVICE_APP" && -x "$SERVICE_APP" ]] || { echo "배포 앱이 없거나 실행할 수 없습니다: $SERVICE_APP"; exit 1; }
  command -v systemctl >/dev/null || { echo "systemctl 이 없어 서비스를 등록할 수 없습니다"; exit 1; }

  APP_ABS="$(cd "$(dirname "$SERVICE_APP")" && pwd)/$(basename "$SERVICE_APP")"
  APP_NAME="$(basename "$SERVICE_APP")"
  # 번들을 풀어 둘 고정 작업 폴더. 앱마다 나눠 두 앱이 서로의 폴더를 덮지 않게 한다.
  WORK_DIR="$HOME/.local/share/neural-linker/$APP_NAME"
  mkdir -p "$UNIT_DIR" "$ENV_DIR"
  # 유닛의 ReadWritePaths 가 가리키는 곳. 없어도 뜨긴 하지만(`-` 접두사) 앱이 쓸 자리는 있어야 한다.
  mkdir -p "$WORK_DIR" "$HOME/.cache/neural-linker"
  chmod 700 "$WORK_DIR"
  # 사용자가 인증서 같은 것을 두는 자리. 번들을 갱신해도 앱이 건드리지 않는다.
  mkdir -p "$WORK_DIR/local"

  # 유닛의 자리표시자를 실제 경로로 바꾼다. %h 는 systemd 가 풀어 주므로 그대로 둔다.
  sed "s|^ExecStart=.*|ExecStart=$APP_ABS --headless --device cpu --work-dir $WORK_DIR|" \
      "$HERE/neural-linker-app.service" > "$UNIT_DIR/$UNIT_NAME"
  chmod 644 "$UNIT_DIR/$UNIT_NAME"

  # 환경 파일은 비밀이 들어가는 곳이다. 없을 때만 0600 으로 만들어 두고 덮어쓰지 않는다.
  if [[ ! -f "$ENV_DIR/app.env" ]]; then
    umask 077
    printf '# 배포 앱 환경 변수. 이 파일은 0600 이어야 한다.\n# NL_HTTP_TOKEN=여기에_토큰\n' > "$ENV_DIR/app.env"
    umask 022
  fi
  chmod 600 "$ENV_DIR/app.env"

  systemctl --user daemon-reload
  systemctl --user enable --now "$UNIT_NAME"
  echo
  echo "서비스를 등록했습니다: $UNIT_DIR/$UNIT_NAME"
  echo "  상태  systemctl --user status ${UNIT_NAME%.service}"
  echo "  로그  journalctl --user -u ${UNIT_NAME%.service} -f"
  echo "  토큰  $ENV_DIR/app.env 에 NL_HTTP_TOKEN=... 을 적고 systemctl --user restart ${UNIT_NAME%.service}"
  echo "  작업  $WORK_DIR (번들이 풀리는 곳)"
  echo "  파일  $WORK_DIR/local (인증서 등 — 번들을 갱신해도 남습니다. 예: cert_pem \"local/server.crt\")"
  echo "로그아웃 뒤에도 돌게 하려면: loginctl enable-linger $USER"
fi
