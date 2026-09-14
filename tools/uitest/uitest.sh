#!/usr/bin/env bash
# 격리된 GUI 테스트 하네스 — 실제 세션의 키보드·마우스·화면을 건드리지 않는다.
#
# sway 를 헤드리스(가상 출력) 컴포지터로 띄우고 그 안에서 앱을 실행한다. 입력은 sway IPC(커서 절대 좌표·
# 버튼 누름/뗌)와 wtype(가상 키보드)으로 그 컴포지터에만 넣고, 캡처는 grim 이 그 가상 출력만 찍는다.
# 한글 입력 검증용으로 fcitx5 를 전용 D-Bus 세션 안에서 같은 컴포지터에 붙일 수 있다.
#
#   uitest.sh start [WxH]            헤드리스 sway 시작(기본 1600x1000)
#   uitest.sh app [파일.nlproj]         앱 실행(릴리스 빌드, 없으면 빌드). 이미 떠 있는 앱은 창이 사라진 것을 확인하고
#                                    나서 새로 띄운다. UITEST_FRESH=1 이면 앱 데이터(복구 스냅샷·설정)를
#                                    비우고 시작 — 이전 강제 종료의 복구 모달이 떠서 클릭을 막는 일이 없다.
#   uitest.sh ime                    fcitx5(한글) 를 이 컴포지터에 붙인다
#   uitest.sh shot out.png [x,y WxH]  캡처(선택 영역). 상대 경로는 $UITEST_SHOT_DIR(기본 $UITEST_DIR/shots) 아래
#   uitest.sh move X Y | click X Y [right|middle] | dblclick X Y | drag X1 Y1 X2 Y2 [steps]
#   uitest.sh hold alt | release alt   수식키를 누른 채 두기(Alt+드래그 복제 등) / 떼기
#   uitest.sh key <wtype 인자…>        예: key -M ctrl -k s -m ctrl  /  key -k Tab  /  key -M alt -k Left -m alt
#   uitest.sh key-until "<정규식>" <wtype 인자…>  결과가 로그에 보일 때까지 키를 다시 보낸다(기본 5회)
#   uitest.sh type "문자열"            글자 그대로 입력(가상 키보드, 입력기 거치지 않음)
#   uitest.sh wait-app                앱 창이 뜰 때까지 대기(최대 15초)
#   uitest.sh wait-log "<정규식>" [초]  앱 로그에 그 줄이 나올 때까지 대기(기본 10초)
#   uitest.sh write-sample <경로> [xor|cnn|new]  샘플 프로젝트를 파일로 (앱의 --write-sample)
#   uitest.sh expect-port-free <포트>  그 포트를 아무도 안 쓰는지 확인(쓰고 있으면 실패)
#   uitest.sh wait-port <포트> [초]    그 포트가 열릴 때까지 대기(서버가 떴다는 신호)
#   uitest.sh wait-port-free <포트> [초]  그 포트가 닫힐 때까지 대기(파이프라인이 멈췄다는 신호)
#   uitest.sh http <메서드> <URL> [본문]  curl 로 한 번 부르고 상태·본문을 적어 둔다
#   uitest.sh expect-http <코드> [본문 정규식]  마지막 http 결과를 확인
#   uitest.sh expect-shot <이름> [x,y WxH]  골든 이미지와 비교(없으면 만들고 알림)
#   uitest.sh run <시나리오.uit> [--keep-app]   시나리오 실행. 실패한 단계에서 멈추고 종료 코드로 알린다.
#                                    시작할 때 남아 있는 앱을 내린다 — `--keep-app` 이면 그대로 둔다.
#   uitest.sh log [줄수]               앱 로그 꼬리
#   uitest.sh status | stop
#
# 시나리오(.uit): 한 줄에 한 명령, `#` 줄 주석, `set 이름 값`/`$이름` 치환, `include 다른.uit`.
# 골든은 $UITEST_GOLDEN_DIR(기본 tools/uitest/golden)에 PPM 으로 둔다 — 차이 계산이 표준 라이브러리로 끝난다.
# 허용 오차는 UITEST_TOLERANCE(기본 0.5%), 잔 떨림 무시 폭은 UITEST_PIXEL_DELTA(기본 8).
#
# 캡처 기본 위치는 $UITEST_SHOT_DIR(기본 $UITEST_DIR/shots) — 절대 경로로 주면 그대로 쓴다.
# 상태는 $UITEST_DIR(기본 /tmp/uitest-$USER) 에 둔다. 좌표는 가상 출력 기준 픽셀(좌상단 0,0).
# 여러 하네스를 동시에 쓰려면(에이전트 병렬 검증) UITEST_DIR 을 다르게 준다 — sway 인스턴스마다 소켓이 다르다.
# UITEST_SWAY_DEBUG=1 이면 sway 를 -d 로 띄워 $UITEST_DIR/sway.log 에 상세 로그를 남긴다.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
DIR="${UITEST_DIR:-/tmp/uitest-$USER}"
GOLDEN_DIR="${UITEST_GOLDEN_DIR:-$HERE/golden}"
DIFF_DIR="${UITEST_DIFF_DIR:-$DIR/diff}"
# 시나리오가 상대 경로로 찍는 캡처가 떨어지는 곳. 저장소를 더럽히지 않으려고 기본값을 하네스 폴더에 둔다.
SHOT_DIR="${UITEST_SHOT_DIR:-$DIR/shots}"
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

# 살아 있는 프로세스인가. **좀비(Z)는 죽은 것으로 친다** — `kill -0` 은 좀비에도 성공해서,
# 부모가 거둬 가기 전까지 앱이 안 죽은 것처럼 보인다. 실제로 여기서 하네스가 멈췄었다:
# 앱을 연속으로 띄우면 앞선 앱이 몇 초씩 좀비로 남아 "내리지 못했습니다" 로 빠졌다.
app_alive() {
    local st
    [[ -n "${1:-}" ]] || return 1
    st=$(awk '/^State:/{print $2; exit}' "/proc/$1/status" 2>/dev/null || true)
    [[ -n "$st" && "$st" != "Z" ]]
}

# sway 트리에 남아 있는 앱 창들의 pid. 창이 살아 있으면 다음 캡처가 그걸 잡는다.
app_window_pids() {
    local tree
    tree=$(swaymsg -t get_tree 2>/dev/null || true)
    [[ -n "$tree" ]] || return 0
    printf '%s' "$tree" | python3 -c '
import json, sys
found = set()
def walk(n):
    if isinstance(n, dict):
        if n.get("app_id") == "neural-linker" and isinstance(n.get("pid"), int):
            found.add(n["pid"])
        for key in ("nodes", "floating_nodes"):
            for child in n.get(key) or []:
                walk(child)
try:
    walk(json.load(sys.stdin))
except Exception:
    pass
print("\n".join(str(x) for x in sorted(found)))
' 2>/dev/null || true
}

# 창이 하나라도 남아 있나. 대기 루프에서 초당 여러 번 부르므로 python 을 태우지 않고
# 트리 문자열만 본다 (pid 가 필요할 때만 app_window_pids).
any_app_window() { swaymsg -t get_tree 2>/dev/null | grep -q '"app_id": "neural-linker"'; }

# 살아 있는 앱 프로세스들. 보는 곳은 둘뿐이다 — pid 파일과 sway 트리.
# `pgrep -f nl-app` 류는 쓰지 않는다: 호출한 셸 자신의 명령줄까지 걸려 엉뚱한 것을 죽인다.
# 트리까지 보는 이유는 pid 파일이 없거나 어긋난 채 창만 남은 경우가 실제로 있어서다.
app_pids() {
    local out="" p
    if [[ -f "$DIR/app.pid" ]]; then
        p=$(cat "$DIR/app.pid" 2>/dev/null || true)
        app_alive "$p" && out="$p"
    fi
    for p in $(app_window_pids); do
        app_alive "$p" && out+=" $p"
    done
    # `|| true` 가 꼭 있어야 한다 — 이 스크립트는 `set -euo pipefail` 이라,
    # 앱이 하나도 없어 grep 이 아무 것도 못 찾으면 파이프가 실패로 잡혀 호출한 쪽이 통째로 죽는다.
    tr ' ' '\n' <<< "$out" | grep -E '^[0-9]+$' | sort -u || true
}

# 앱을 확실히 내린다. **창이 사라질 때까지** 기다리는 것이 핵심이다 —
# 그냥 kill 하고 넘어가면 `wait_app` 이 아직 남아 있는 옛 창을 보고 바로 통과해,
# 시나리오가 새 앱 대신 엉뚱한 창을 캡처한다.
kill_app() {
    local pids p deadline
    pids=$(app_pids)
    if [[ -n "$pids" ]]; then
        for p in $pids; do kill -TERM "$p" 2>/dev/null || true; done
        deadline=$(( SECONDS + 5 ))
        while (( SECONDS < deadline )) && [[ -n "$(app_pids)" ]]; do sleep 0.1; done

        pids=$(app_pids)
        if [[ -n "$pids" ]]; then                  # 안 죽으면 강제로
            for p in $pids; do kill -KILL "$p" 2>/dev/null || true; done
            deadline=$(( SECONDS + 5 ))
            while (( SECONDS < deadline )) && [[ -n "$(app_pids)" ]]; do sleep 0.1; done
        fi
    fi
    rm -f "$DIR/app.pid"

    # 프로세스가 죽어도 컴포지터가 창을 지우는 데 시간이 걸린다. 창이 둘이 되면 sway 가
    # 타일로 쪼개 좌표가 통째로 어긋나므로 사라질 때까지 기다린다.
    # 넉넉히 잡는 이유: 이 값은 기계 부하를 탄다 — 부하 32 에서 0.9~2.9초가 측정됐다.
    deadline=$(( SECONDS + 20 ))
    while (( SECONDS < deadline )) && any_app_window; do sleep 0.1; done

    if [[ -n "$(app_pids)" ]] || any_app_window; then
        echo "앞선 앱을 내리지 못했습니다 (프로세스 [$(tr '\n' ' ' <<< "$(app_pids)")] 창 [$(tr '\n' ' ' <<< "$(app_window_pids)")]) — 'uitest.sh stop' 으로 하네스를 내렸다가 다시 시작하세요" >&2
        exit 1
    fi
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
    # UITEST_APP_BIN 으로 다른 트리에서 빌드한 바이너리를 쓸 수 있다(워크트리 병렬 작업).
    local bin="${UITEST_APP_BIN:-$ROOT/target/release/nl-app}"
    if [[ ! -x "$bin" ]]; then
        [[ -n "${UITEST_APP_BIN:-}" ]] && { echo "UITEST_APP_BIN 이 실행 파일이 아닙니다: $bin" >&2; exit 1; }
        (cd "$ROOT" && cargo build --release -p nl-app)
    fi
    # 이미 떠 있는 앱은 창까지 사라진 것을 확인하고 나서 새로 띄운다.
    kill_app
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

# 앱 창이 뜰 때까지 기다린다. **방금 띄운 그 pid 의 창**을 기다리는 것이 핵심이다 —
# `app_id` 만 보면 앞선 앱의 창이 아직 안 사라졌을 때 그걸 보고 바로 통과해,
# 시나리오가 새 앱이 아니라 옛 창을 캡처한다(앱 담당자가 겪은 증상).
cmd_wait_app() {
    load
    local want deadline
    want=$(cat "$DIR/app.pid" 2>/dev/null || true)
    deadline=$(( SECONDS + 20 ))
    while (( SECONDS < deadline )); do
        if [[ -n "$want" ]]; then
            if app_window_pids | grep -qx "$want"; then
                swaymsg "[pid=$want] focus" > /dev/null
                sleep 0.5
                echo "앱 창 준비됨 (pid $want)"; return
            fi
            # 좀비도 `kill -0` 을 통과하므로 상태를 직접 본다. 안 그러면 앱이 죽었는데도
            # 시간이 다 찰 때까지 기다린 뒤에야 알게 된다.
            if ! app_alive "$want"; then
                echo "앱이 종료됐습니다:"; tail -20 "$DIR/app.log"; exit 1
            fi
        elif swaymsg -t get_tree | grep -q '"app_id": "neural-linker"'; then
            # pid 파일 없이 부른 경우(손으로 `wait-app`)는 예전처럼 창만 본다.
            swaymsg '[app_id="neural-linker"] focus' > /dev/null
            sleep 0.5
            echo "앱 창 준비됨"; return
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

# 확장자로 형식을 정한다. .ppm 은 헤더 뒤가 그냥 RGB 바이트라 골든 비교가 표준 라이브러리로 끝난다.
# 상대 경로로 준 캡처는 $UITEST_SHOT_DIR 아래에 둔다. 현재 폴더 기준으로 쓰면 저장소 루트에서
# 돌렸을 때 캡처가 작업 트리에 떨어진다(runtime 담당자가 실제로 겪었다).
shot_path() {
    local out="$1"
    case "$out" in
        /*) echo "$out" ;;
        *) echo "$SHOT_DIR/$out" ;;
    esac
}

cmd_shot() {
    load
    local out
    out="$(shot_path "${1:?출력 파일}")"
    local fmt=png
    [[ "$out" == *.ppm ]] && fmt=ppm
    [[ "$out" == *.jpeg || "$out" == *.jpg ]] && fmt=jpeg
    mkdir -p "$(dirname "$out")"
    if [[ -n "${2:-}" ]]; then grim -t "$fmt" -g "$2" "$out"; else grim -t "$fmt" "$out"; fi
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

# 앱이 로그에 찍는 마커를 기다린다. 캡처를 OCR 할 수 없으니 "화면에 무엇이 떴다" 는
# 앱이 스스로 로그로 알리게 하고 여기서 그 줄을 기다린다.
cmd_wait_log() {
    local pattern="${1:?정규식}" secs="${2:-10}"
    local deadline=$(( $(date +%s) + ${secs%.*} ))
    while (( $(date +%s) <= deadline )); do
        if [[ -f "$DIR/app.log" ]] && grep -Eq -- "$pattern" "$DIR/app.log"; then
            echo "로그 확인: $pattern"; return 0
        fi
        sleep 0.2
    done
    echo "로그에 나타나지 않았습니다 ($secs초): $pattern" >&2
    tail -20 "$DIR/app.log" 2>/dev/null >&2 || true
    return 1
}

# 샘플 프로젝트를 파일로 써 둔다. 앱의 `--write-sample` 을 부르는 것이라 파일 대화상자를 피한다.
#
# 시나리오가 스스로 입력을 갖추게 하려는 것이다 — 바깥에서 미리 만들어 두게 하면 시나리오 하나를
# 돌리는 데 두 단계가 필요해지고, 러너(`verify-all.sh`)마다 그 준비를 베껴 적게 된다.
cmd_write_sample() {
    local out="${1:?출력 경로}" kind="${2:-xor}"
    local bin="${UITEST_APP_BIN:-$ROOT/target/release/nl-app}"
    if [[ ! -x "$bin" ]]; then
        [[ -n "${UITEST_APP_BIN:-}" ]] && { echo "UITEST_APP_BIN 이 실행 파일이 아닙니다: $bin" >&2; return 1; }
        (cd "$ROOT" && cargo build --release -p nl-app) || return 1
    fi
    "$bin" --write-sample "$out" "$kind" > /dev/null || { echo "샘플을 쓰지 못했습니다: $out" >&2; return 1; }
    echo "샘플 $kind → $out"
}

# ── 포트·HTTP ───────────────────────────────────────────────────────────────
#
# 빌더의 "시험 실행" 은 파이프라인 로그를 **앱 안 로그 패널**에만 쌓고 표준 오류로 내보내지 않는다.
# 그래서 `wait-log "HTTP 서버 … 열림"` 으로는 서버가 떴는지 알 수 없다. 대신 포트를 직접 본다 —
# 어차피 시험이 확인하려는 것이 "정말 듣고 있는가" 이므로 로그보다 곧은 신호다.

# 그 포트에서 누가 듣고 있는가.
port_listening() {
    local port="${1:?포트}"
    ss -ltn 2>/dev/null | grep -Eq "[:.]${port}[[:space:]]"
}

# 시나리오 시작 전에 포트가 비어 있는지 본다. 남의 서버에 대고 시험하면 통과해도 의미가 없다.
cmd_expect_port_free() {
    local port="${1:?포트}"
    if port_listening "$port"; then
        echo "포트 $port 를 이미 누가 쓰고 있습니다 — 시나리오를 시작할 수 없습니다" >&2
        ss -ltnp 2>/dev/null | grep -E "[:.]${port}[[:space:]]" >&2 || true
        return 1
    fi
    echo "포트 $port 비어 있음"
}

# 포트가 열릴 때까지 기다린다 (서버가 떴다는 신호).
cmd_wait_port() {
    local port="${1:?포트}" secs="${2:-15}"
    local deadline=$(( $(date +%s) + ${secs%.*} ))
    while (( $(date +%s) <= deadline )); do
        if port_listening "$port"; then echo "포트 $port 열림"; return 0; fi
        sleep 0.2
    done
    echo "포트 $port 가 ${secs}초 안에 열리지 않았습니다" >&2
    return 1
}

# 포트가 닫힐 때까지 기다린다 (파이프라인이 멈췄다는 신호).
cmd_wait_port_free() {
    local port="${1:?포트}" secs="${2:-15}"
    local deadline=$(( $(date +%s) + ${secs%.*} ))
    while (( $(date +%s) <= deadline )); do
        if ! port_listening "$port"; then echo "포트 $port 닫힘"; return 0; fi
        sleep 0.2
    done
    echo "포트 $port 가 ${secs}초 안에 닫히지 않았습니다" >&2
    return 1
}

# `http <메서드> <URL> [본문]` — curl 로 한 번 부르고 상태 코드와 본문을 적어 둔다.
#
# 결과는 `$DIR/http.status` 와 `$DIR/http.body` 에 남고 `expect-http` 가 그것을 본다.
# 두 단계로 나눈 이유는 시나리오에서 "부르기" 와 "확인" 을 따로 읽히게 하려는 것이다.
cmd_http() {
    load
    local method="${1:?메서드}" url="${2:?URL}" body="${3:-}"
    local args=(-sS --max-time 15 -o "$DIR/http.body" -w '%{http_code}' -X "$method")
    if [[ -n "$body" ]]; then
        args+=(-H 'Content-Type: application/json' -d "$body")
    fi
    local code
    if ! code=$(curl "${args[@]}" "$url" 2>"$DIR/http.err"); then
        echo "(요청 실패)" > "$DIR/http.status"
        echo "요청이 실패했습니다: $method $url" >&2
        cat "$DIR/http.err" >&2 || true
        return 1
    fi
    printf '%s' "$code" > "$DIR/http.status"
    echo "$method $url → $code $(head -c 200 "$DIR/http.body" 2>/dev/null)"
}

# `expect-http <상태코드> [본문 정규식]` — 마지막 `http` 결과를 확인한다.
cmd_expect_http() {
    local want="${1:?상태코드}" pattern="${2:-}"
    local got
    got=$(cat "$DIR/http.status" 2>/dev/null || echo "(없음)")
    if [[ "$got" != "$want" ]]; then
        echo "상태 코드가 다릅니다: 기대 $want, 실제 $got" >&2
        head -c 400 "$DIR/http.body" 2>/dev/null >&2 || true
        echo >&2
        return 1
    fi
    if [[ -n "$pattern" ]] && ! grep -Eq -- "$pattern" "$DIR/http.body"; then
        echo "본문이 정규식과 맞지 않습니다: $pattern" >&2
        head -c 400 "$DIR/http.body" 2>/dev/null >&2 || true
        echo >&2
        return 1
    fi
    echo "응답 확인: $want${pattern:+ / $pattern}"
}

# 로그에 정규식이 나타났는가 (기다리되 아무것도 찍지 않는다). `key-until` 이 쓴다.
log_has() {
    local pattern="$1" secs="$2"
    local deadline=$(( $(date +%s) + ${secs%.*} ))
    while (( $(date +%s) <= deadline )); do
        if [[ -f "$DIR/app.log" ]] && grep -Eq -- "$pattern" "$DIR/app.log"; then return 0; fi
        sleep 0.2
    done
    return 1
}

# 키를 보내고 그 결과가 로그에 나타날 때까지 다시 보낸다.
#
# 컴포지터가 가상 키보드를 등록하기 전에 보낸 키는 **조용히 사라진다.** 앱이 `[nl-app] input ready` 를
# 찍은 뒤에도 하네스를 막 띄운 회차에서는 첫 한두 개가 없어진다. 앱 쪽에서는 더 할 수 있는 일이 없어
# 여기서 다시 보낸다 — 뷰 전환처럼 여러 번 눌러도 결과가 같은 동작에만 쓴다.
#
#   key-until "<정규식>" <wtype 인자…>
cmd_key_until() {
    local pattern="${1:?정규식}"; shift
    local tries="${UITEST_KEY_TRIES:-5}" wait="${UITEST_KEY_WAIT:-2}" i
    for (( i = 1; i <= tries; i++ )); do
        cmd_key "$@"
        if log_has "$pattern" "$wait"; then
            (( i > 1 )) && echo "키 확인: $pattern ($i회째)" || echo "키 확인: $pattern"
            return 0
        fi
    done
    echo "키가 닿지 않았습니다 (${tries}회 보냈습니다): $pattern" >&2
    tail -20 "$DIR/app.log" 2>/dev/null >&2 || true
    return 1
}

# 화면이 멎을 때까지 기다렸다 찍는다. 잇따른 두 캡처가 바이트까지 같으면 멎은 것으로 본다.
# 애니메이션·커서 깜빡임 때문에 나는 헛실패를 막는다. 오래 걸리는 시작 대기는 이것으로 대신할 수 없다 —
# 앱이 준비되기 전에도 화면은 멎어 있을 수 있으니 시나리오에서 넉넉히 `sleep` 하거나 `wait-log` 를 쓴다.
settle_shot() {
    local out="$1" region="$2"
    local tries="${UITEST_SETTLE_TRIES:-8}" interval="${UITEST_SETTLE_INTERVAL:-0.3}"
    local prev="$DIR/.settle-$$.ppm" i
    cmd_shot "$out" "$region" > /dev/null
    for (( i = 1; i < tries; i++ )); do
        cp "$out" "$prev"
        sleep "$interval"
        cmd_shot "$out" "$region" > /dev/null
        if cmp -s "$prev" "$out"; then rm -f "$prev"; return 0; fi
    done
    rm -f "$prev"
    echo "  (화면이 계속 바뀝니다 — 마지막 캡처로 비교합니다)" >&2
}

# 골든 이미지 비교. 골든이 없으면 지금 캡처를 골든으로 삼고 알린다(첫 생성).
cmd_expect_shot() {
    load
    local name="${1:?골든 이름}"; shift
    # 영역은 "200,60 800x600" 한 덩어리지만 시나리오에서 따옴표 없이 두 토큰으로 와도 받아 준다.
    local region="$*"
    local golden="$GOLDEN_DIR/$name.ppm"
    local actual="$DIR/actual-$name.ppm"
    settle_shot "$actual" "$region"

    if [[ ! -f "$golden" ]]; then
        mkdir -p "$GOLDEN_DIR"
        cp "$actual" "$golden"
        echo "골든 생성: $golden"
        return 0
    fi

    local out rc=0
    out=$(python3 "$HERE/ppmdiff.py" "$golden" "$actual" "$DIFF_DIR/$name.ppm") || rc=$?
    if (( rc == 0 )); then
        echo "일치: $name ($out, 허용 ${UITEST_TOLERANCE:-0.5}%)"
        return 0
    fi
    echo "다릅니다: $name ($out, 허용 ${UITEST_TOLERANCE:-0.5}%)" >&2
    echo "  기준 $golden" >&2
    echo "  실제 $actual" >&2
    [[ -f "$DIFF_DIR/$name.ppm" ]] && echo "  차이 $DIFF_DIR/$name.ppm" >&2
    return 1
}

# 시나리오 한 단계. 이름이 uitest.sh 하위 명령과 같아 시나리오와 손 실행이 같은 문법을 쓴다.
run_step() {
    local where="$1"; shift
    local cmd="$1"; shift
    case "$cmd" in
        app)         cmd_app "$@";;
        wait-app)    cmd_wait_app;;
        wait-log)    cmd_wait_log "$@";;
        write-sample)     cmd_write_sample "$@";;
        expect-port-free) cmd_expect_port_free "$@";;
        wait-port)        cmd_wait_port "$@";;
        wait-port-free)   cmd_wait_port_free "$@";;
        http)             cmd_http "$@";;
        expect-http)      cmd_expect_http "$@";;
        key-until)   cmd_key_until "$@";;
        ime)         cmd_ime;;
        shot)        cmd_shot "$@";;
        expect-shot) cmd_expect_shot "$@";;
        move)        cmd_move "$@";;
        click)       cmd_click "$@";;
        dblclick)    cmd_dblclick "$@";;
        drag)        cmd_drag "$@";;
        hold)        cmd_hold "$@";;
        release)     cmd_release "$@";;
        key)         cmd_key "$@";;
        type)        cmd_type "$@";;
        sleep)       sleep "${1:?초}";;
        log)         cmd_log "$@";;
        stop)        cmd_stop;;
        *) echo "$where: 모르는 명령입니다: $cmd" >&2; return 1;;
    esac
}

cmd_run() {
    # `--keep-app` 은 시나리오 인자가 아니라 하네스 옵션이라 먼저 걸러 낸다.
    local keep_app=0
    local -a rest=()
    local a
    for a in "$@"; do
        if [[ "$a" == "--keep-app" ]]; then keep_app=1; else rest+=("$a"); fi
    done
    set -- ${rest[@]+"${rest[@]}"}

    local file="${1:?시나리오 파일}"; shift || true
    [[ -f "$file" ]] || { echo "시나리오가 없습니다: $file" >&2; exit 1; }
    load
    # 앞선 실행이 남긴 앱을 먼저 치운다. 이걸 안 해서 시나리오가 엉뚱한 창을 캡처한 적이 있다.
    # 일부러 띄워 둔 앱에 이어 붙이고 싶으면 `--keep-app`.
    (( keep_app )) || kill_app
    mkdir -p "$DIFF_DIR"

    local steps
    steps=$(python3 "$HERE/parse_uit.py" "$file" "$@") || exit 1

    local n=0 where cmd line rc
    echo "시나리오 시작: $file"
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        n=$((n + 1))
        local -a tok=()
        IFS=$'\x1f' read -ra tok <<< "$line"
        where="${tok[0]}"; cmd="${tok[1]}"
        printf '[%02d] %s %s\n' "$n" "$cmd" "${tok[*]:2}"
        rc=0
        run_step "$where" "${tok[@]:1}" || rc=$?
        if (( rc != 0 )); then
            local dump="$DIR/fail-$n.ppm"
            grim -t ppm "$dump" 2>/dev/null || true
            echo "실패: $where ($cmd) — 전체 캡처 $dump" >&2
            return 1
        fi
    done <<< "$steps"
    echo "시나리오 통과: $n 단계"
}

cmd_key() { load; wtype "$@"; }
cmd_type() { load; wtype -- "$1"; }
cmd_log() { tail -n "${1:-30}" "$DIR/app.log"; }

cmd_status() {
    if [[ ! -f "$STATE" ]]; then echo "꺼짐"; return; fi
    load
    echo "sway $SWAY_PID ($(kill -0 "$SWAY_PID" 2>/dev/null && echo 살아있음 || echo 죽음)) display=$NESTED_DISPLAY"
    # 앱은 `app_alive` 로 본다 — 좀비는 `kill -0` 을 통과해서 죽은 것을 살아있다고 보고한다.
    [[ -f "$DIR/app.pid" ]] && echo "app $(cat "$DIR/app.pid") ($(app_alive "$(cat "$DIR/app.pid")" && echo 살아있음 || echo 죽음))"
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
    wait-log) shift; cmd_wait_log "$@";;
    write-sample) shift; cmd_write_sample "$@";;
    expect-port-free) shift; cmd_expect_port_free "$@";;
    wait-port) shift; cmd_wait_port "$@";;
    wait-port-free) shift; cmd_wait_port_free "$@";;
    http) shift; cmd_http "$@";;
    expect-http) shift; cmd_expect_http "$@";;
    expect-shot) shift; cmd_expect_shot "$@";;
    run) shift; cmd_run "$@";;
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
    *) sed -n '2,28p' "$0"; exit 1;;
esac
