#!/usr/bin/env bash
# 격리된 GUI 테스트 하네스 — 실제 세션의 키보드·마우스·화면을 건드리지 않는다.
#
# sway 를 헤드리스(가상 출력) 컴포지터로 띄우고 그 안에서 앱을 실행한다. 입력은 sway IPC(커서 절대 좌표·
# 버튼 누름/뗌)와 wtype(가상 키보드)으로 그 컴포지터에만 넣고, 캡처는 grim 이 그 가상 출력만 찍는다.
# 한글 입력 검증용으로 fcitx5 를 전용 D-Bus 세션 안에서 같은 컴포지터에 붙일 수 있다.
#
#   uitest.sh start [WxH]            헤드리스 sway 시작(기본 1600x1000)
#   uitest.sh app [파일.nlproj]         앱 실행(릴리스 빌드, 없으면 빌드). UITEST_FRESH=1 이면 앱 데이터(복구 스냅샷·설정)를
#                                    비우고 시작 — 이전 강제 종료의 복구 모달이 떠서 클릭을 막는 일이 없다.
#   uitest.sh ime                    fcitx5(한글) 를 이 컴포지터에 붙인다
#   uitest.sh shot out.png [x,y WxH]  캡처(선택 영역)
#   uitest.sh move X Y | click X Y [right|middle] | dblclick X Y | drag X1 Y1 X2 Y2 [steps]
#   uitest.sh hold alt | release alt   수식키를 누른 채 두기(Alt+드래그 복제 등) / 떼기
#   uitest.sh key <wtype 인자…>        예: key -M ctrl -k s -m ctrl  /  key -k Tab  /  key -M alt -k Left -m alt
#   uitest.sh type "문자열"            글자 그대로 입력(가상 키보드, 입력기 거치지 않음)
#   uitest.sh wait-app                앱 창이 뜰 때까지 대기(최대 15초)
#   uitest.sh log [줄수]               앱 로그 꼬리
#   uitest.sh status | stop
#
# 상태는 $UITEST_DIR(기본 /tmp/uitest-$USER) 에 둔다. 좌표는 가상 출력 기준 픽셀(좌상단 0,0).
# 여러 하네스를 동시에 쓰려면(에이전트 병렬 검증) UITEST_DIR 을 다르게 준다 — sway 인스턴스마다 소켓이 다르다.
# UITEST_SWAY_DEBUG=1 이면 sway 를 -d 로 띄워 $UITEST_DIR/sway.log 에 상세 로그를 남긴다.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIR="${UITEST_DIR:-/tmp/uitest-$USER}"
STATE="$DIR/state.env"
RT="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
mkdir -p "$DIR"

load() {
    [[ -f "$STATE" ]] || { echo "하네스가 꺼져 있습니다. 먼저 'uitest.sh start'" >&2; exit 1; }
    # shellcheck disable=SC1090
    source "$STATE"
    export WAYLAND_DISPLAY="$NESTED_DISPLAY" SWAYSOCK="$NESTED_SOCK"
    unset DISPLAY
}

# 우리 설정 파일로 뜬 sway 의 pid. `pgrep -f` 는 호출자의 명령줄까지 잡을 수 있어 /proc 을 직접 본다.
own_sway_pid() {
    local p
    for p in $(pgrep -x sway || true); do
        if tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null | grep -q -- "$DIR/sway.cfg"; then echo "$p"; return; fi
    done
    echo ""
}

sway_cfg() {
    local w="$1" h="$2"
    {
        echo "# 테스트 전용 — 키 바인딩·바 없음, 창은 전부 출력 크기로."
        echo "output * resolution ${w}x${h} position 0 0 bg #202020 solid_color"
        echo "xwayland disable"
        echo "default_border none"
        echo "focus_follows_mouse no"
        echo "seat * hide_cursor 0"
        echo "input * xkb_layout us"
    } > "$DIR/sway.cfg"
}

cmd_start() {
    if [[ -f "$STATE" ]]; then
        # shellcheck disable=SC1090
        source "$STATE"
        if kill -0 "${SWAY_PID:-0}" 2>/dev/null; then
            echo "이미 실행 중: $NESTED_DISPLAY (sway $SWAY_PID)"; return
        fi
        rm -f "$STATE"
    fi
    local size="${1:-1600x1000}"
    sway_cfg "${size%x*}" "${size#*x}"
    # 실제 세션과 분리: 헤드리스 백엔드, 실제 입력 장치 없음. 렌더러는 GPU(gles2), 안 되면 WLR_RENDERER=pixman.
    (
        unset DISPLAY WAYLAND_DISPLAY SWAYSOCK I3SOCK
        export WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 XDG_SESSION_TYPE=wayland
        export WLR_RENDERER="${WLR_RENDERER:-gles2}"
        setsid sway ${UITEST_SWAY_DEBUG:+-d} -c "$DIR/sway.cfg" > "$DIR/sway.log" 2>&1 < /dev/null &
    )
    # setsid 가 fork 하면 $! 는 sway 가 아니다 — 실제 sway 프로세스를 찾는다.
    local pid="" sock="" disp=""
    for _ in $(seq 1 50); do
        pid=$(own_sway_pid); [[ -n "$pid" ]] && break; sleep 0.2
    done
    [[ -n "$pid" ]] || { echo "sway 가 뜨지 않았습니다:"; tail -20 "$DIR/sway.log"; exit 1; }
    echo "$pid" > "$DIR/sway.pid"
    # IPC 소켓은 sway 가 만든다: sway-ipc.<uid>.<pid>.sock
    for _ in $(seq 1 50); do
        if [[ -S "$RT/sway-ipc.$(id -u).$pid.sock" ]]; then sock="$RT/sway-ipc.$(id -u).$pid.sock"; break; fi
        sleep 0.2
    done
    [[ -n "$sock" ]] || { echo "sway IPC 소켓이 없습니다:"; tail -20 "$DIR/sway.log"; exit 1; }
    # WAYLAND_DISPLAY 이름은 sway 가 자식에게 넘기는 환경에서 읽는다(로그 형식에 기대지 않는다).
    rm -f "$DIR/env.txt"
    swaymsg -s "$sock" exec "sh -c 'env > $DIR/env.txt'" > /dev/null
    for _ in $(seq 1 50); do
        disp=$( (grep '^WAYLAND_DISPLAY=' "$DIR/env.txt" 2>/dev/null || true) | cut -d= -f2)
        [[ -n "$disp" ]] && break; sleep 0.2
    done
    [[ -n "$disp" ]] || { echo "WAYLAND_DISPLAY 를 알 수 없습니다"; exit 1; }
    printf 'SWAY_PID=%s\nNESTED_DISPLAY=%s\nNESTED_SOCK=%s\n' "$pid" "$disp" "$sock" > "$STATE"
    load
    start_vseat
    swaymsg -t get_outputs | python3 -c 'import json,sys; [print("출력", o["name"], o["current_mode"]["width"], "x", o["current_mode"]["height"]) for o in json.load(sys.stdin)]'
    echo "시작됨: WAYLAND_DISPLAY=$disp SWAYSOCK=$sock"
}

# 시트에 가상 포인터·키보드를 상시 붙인다(없으면 앱이 wl_pointer 를 못 받아 클릭이 닿지 않는다).
start_vseat() {
    local bin="$ROOT/target/release/vseat"
    [[ -x "$bin" ]] || (cd "$ROOT" && cargo build --release -p vseat)
    setsid "$bin" > "$DIR/vseat.log" 2>&1 < /dev/null &
    echo $! > "$DIR/vseat.pid"
    sleep 0.5
    grep -q '붙임' "$DIR/vseat.log" || { echo "vseat 실패:"; cat "$DIR/vseat.log"; exit 1; }
}

cmd_app() {
    load
    local bin="$ROOT/target/release/nl-app"
    [[ -x "$bin" ]] || (cd "$ROOT" && cargo build --release -p nl-app)
    if [[ -f "$DIR/app.pid" ]] && kill -0 "$(cat "$DIR/app.pid")" 2>/dev/null; then
        kill "$(cat "$DIR/app.pid")" || true; sleep 0.5
    fi
    (
        export WINIT_UNIX_BACKEND=wayland XDG_SESSION_TYPE=wayland
        # 앱 설정(last_file 등)과 복구 폴더가 실제 사용자의 것과 섞이지 않게 별도 홈.
        export XDG_DATA_HOME="$DIR/xdg-data" XDG_CONFIG_HOME="$DIR/xdg-config"
        [[ "${UITEST_FRESH:-0}" == "1" ]] && rm -rf "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"
        mkdir -p "$XDG_DATA_HOME" "$XDG_CONFIG_HOME"
        export RUST_LOG="${RUST_LOG:-warn}"
        setsid "$bin" "$@" > "$DIR/app.log" 2>&1 < /dev/null &
        echo $! > "$DIR/app.pid"
    )
    cmd_wait_app
}

cmd_wait_app() {
    load
    for _ in $(seq 1 75); do
        if swaymsg -t get_tree | grep -q '"app_id": "neural-linker"'; then
            swaymsg '[app_id="neural-linker"] focus' > /dev/null
            sleep 0.5
            echo "앱 창 준비됨 (pid $(cat "$DIR/app.pid"))"; return
        fi
        if ! kill -0 "$(cat "$DIR/app.pid")" 2>/dev/null; then
            echo "앱이 종료됐습니다:"; tail -20 "$DIR/app.log"; exit 1
        fi
        sleep 0.2
    done
    echo "앱 창이 뜨지 않았습니다:"; tail -20 "$DIR/app.log"; exit 1
}

cmd_ime() {
    load
    if [[ -f "$DIR/ime.pid" ]] && kill -0 "$(cat "$DIR/ime.pid")" 2>/dev/null; then
        echo "fcitx5 이미 실행 중"; return
    fi
    # 실제 세션의 fcitx5 와 D-Bus 이름이 충돌하지 않게 전용 세션 버스 안에서 띄운다.
    (
        unset DISPLAY XMODIFIERS
        setsid dbus-run-session -- fcitx5 --disable=xim,ibus,kimpanel > "$DIR/ime.log" 2>&1 < /dev/null &
        echo $! > "$DIR/ime.pid"
    )
    sleep 1.5
    echo "fcitx5 붙임 (pid $(cat "$DIR/ime.pid")) — 앱 텍스트 필드에서 Ctrl+Space 로 한글 전환"
}

cmd_shot() {
    load
    local out="${1:?출력 파일}"
    if [[ -n "${2:-}" ]]; then grim -g "$2" "$out"; else grim "$out"; fi
    echo "$out"
}

cursor_set() { swaymsg "seat - cursor set $1 $2" > /dev/null; }

cmd_move() { load; cursor_set "$1" "$2"; }

cmd_click() {
    load
    local btn="BTN_LEFT"
    [[ "${3:-}" == "right" ]] && btn="BTN_RIGHT"
    [[ "${3:-}" == "middle" ]] && btn="BTN_MIDDLE"
    cursor_set "$1" "$2"; sleep 0.05
    swaymsg "seat - cursor press $btn" > /dev/null; sleep 0.05
    swaymsg "seat - cursor release $btn" > /dev/null
}

cmd_dblclick() { load; cmd_click "$1" "$2"; sleep 0.08; cmd_click "$1" "$2"; }

cmd_drag() {
    load
    local x1="$1" y1="$2" x2="$3" y2="$4" steps="${5:-12}"
    # 버튼을 누른 동안 `swaymsg cursor set` 의 모션은 클라이언트에 전달되지 않는다(릴리스가 누른 자리에서 일어난 것처럼
    # 보인다). 모션은 wlr-virtual-pointer(wlrctl, 상대 이동)로 넣고, 버튼만 sway IPC 로 누르고 뗀다.
    cursor_set "$x1" "$y1"; sleep 0.05
    swaymsg "seat - cursor press BTN_LEFT" > /dev/null; sleep 0.05
    local i px="$x1" py="$y1" nx ny
    for i in $(seq 1 "$steps"); do
        nx=$(( x1 + (x2 - x1) * i / steps )); ny=$(( y1 + (y2 - y1) * i / steps ))
        wlrctl pointer move $(( nx - px )) $(( ny - py )); px=$nx; py=$ny; sleep 0.03
    done
    sleep 0.05
    swaymsg "seat - cursor release BTN_LEFT" > /dev/null
}

# 수식키를 누른 채로 두기: hold alt|ctrl|shift  → 이어지는 click/drag/type 동안 유지, release 로 뗀다.
cmd_hold() {
    load
    local mod="${1:?alt|ctrl|shift|logo}"
    cmd_release "$mod" 2>/dev/null || true
    setsid wtype -M "$mod" -s 60000 -m "$mod" > /dev/null 2>&1 < /dev/null &
    echo $! > "$DIR/hold-$mod.pid"
    sleep 0.15
}

cmd_release() {
    local mod="${1:?alt|ctrl|shift|logo}"
    if [[ -f "$DIR/hold-$mod.pid" ]]; then
        kill "$(cat "$DIR/hold-$mod.pid")" 2>/dev/null || true
        rm -f "$DIR/hold-$mod.pid"
        sleep 0.1
    fi
}

cmd_key() { load; wtype "$@"; }
cmd_type() { load; wtype -- "$1"; }
cmd_log() { tail -n "${1:-30}" "$DIR/app.log"; }

cmd_status() {
    if [[ ! -f "$STATE" ]]; then echo "꺼짐"; return; fi
    load
    echo "sway $SWAY_PID ($(kill -0 "$SWAY_PID" 2>/dev/null && echo 살아있음 || echo 죽음)) display=$NESTED_DISPLAY"
    [[ -f "$DIR/app.pid" ]] && echo "app $(cat "$DIR/app.pid") ($(kill -0 "$(cat "$DIR/app.pid")" 2>/dev/null && echo 살아있음 || echo 죽음))"
    [[ -f "$DIR/vseat.pid" ]] && echo "vseat $(cat "$DIR/vseat.pid") ($(kill -0 "$(cat "$DIR/vseat.pid")" 2>/dev/null && echo 살아있음 || echo 죽음))"
    [[ -f "$DIR/ime.pid" ]] && echo "fcitx5 $(cat "$DIR/ime.pid") ($(kill -0 "$(cat "$DIR/ime.pid")" 2>/dev/null && echo 살아있음 || echo 죽음))"
    swaymsg -t get_tree 2>/dev/null | (grep -o '"app_id": "[^"]*"' || true) | sort -u
}

cmd_stop() {
    local f p
    for f in hold-alt hold-ctrl hold-shift hold-logo app ime vseat; do
        if [[ -f "$DIR/$f.pid" ]]; then
            p=$(cat "$DIR/$f.pid")
            # dbus-run-session 은 자식(fcitx5)까지 세션 그룹으로 정리한다.
            kill -- -"$p" 2>/dev/null || kill "$p" 2>/dev/null || true
            rm -f "$DIR/$f.pid"
        fi
    done
    p=$(own_sway_pid); [[ -n "$p" ]] && kill "$p" 2>/dev/null || true
    rm -f "$DIR/sway.pid" "$STATE"
    echo "정지"
}

case "${1:-}" in
    start) shift; cmd_start "$@";;
    app) shift; cmd_app "$@";;
    wait-app) cmd_wait_app;;
    ime) cmd_ime;;
    shot) shift; cmd_shot "$@";;
    move) shift; cmd_move "$@";;
    click) shift; cmd_click "$@";;
    dblclick) shift; cmd_dblclick "$@";;
    drag) shift; cmd_drag "$@";;
    hold) shift; cmd_hold "$@";;
    release) shift; cmd_release "$@";;
    key) shift; cmd_key "$@";;
    type) shift; cmd_type "$@";;
    log) shift; cmd_log "$@";;
    status) cmd_status;;
    stop) cmd_stop;;
    *) sed -n '2,20p' "$0"; exit 1;;
esac
