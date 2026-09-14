#!/usr/bin/env bash
# 릴리스 전에 한 번에 돌리는 검증 러너.
#
#   scripts/verify-all.sh [--gui] [--fail-fast] [--out <폴더>]
#
#     --gui         헤드리스 sway 하네스로 uitest 시나리오까지 돈다 (sway·wtype·grim 필요)
#     --fail-fast   첫 실패에서 멈춘다. 기본은 끝까지 돌고 마지막에 실패 목록을 낸다
#     --out <폴더>  로그 위치. 기본 dist-local/verify/<시각>-<pid>
#
# 기본은 **끝까지 돈다.** 릴리스 직전에 알고 싶은 것은 "무엇이 처음 깨졌나" 가 아니라
# "무엇무엇이 깨져 있나" 이기 때문이다. 한 번 돌려 목록을 받고 한꺼번에 고치는 편이 빠르다.
#
# 종료 코드는 실패한 단계 수(최대 125)다. 0 이면 전부 통과다.
#
# 이 스크립트는 **다른 것과 나란히 도는 것을 전제한다.** 산출물 폴더에 pid 를 붙이고, 종단 시험은
# 샘플의 고정 포트(8799·8800) 대신 빈 포트를 쓰며, 정리할 때 남의 프로세스를 건드리지 않는다.
# 규칙과 이유는 `scripts/README.md`.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

WANT_GUI=0
FAIL_FAST=0
OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --gui)        WANT_GUI=1; shift ;;
    --fail-fast)  FAIL_FAST=1; shift ;;
    --out)        OUT="$2"; shift 2 ;;
    -h|--help)    sed -n "2,13p" "${BASH_SOURCE[0]}"; exit 0 ;;
    *)            echo "모르는 옵션: $1" >&2; exit 2 ;;
  esac
done
# 시각에 pid 를 더한다. 같은 초에 두 번 돌아도 로그가 섞이지 않아야 한다 — `scripts/README.md`.
[[ -n "$OUT" ]] || OUT="$ROOT/dist-local/verify/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$OUT"

# 하네스는 다른 에이전트·세션과 겹치지 않게 전용 폴더를 쓴다.
UITEST_DIR="${UITEST_DIR:-/tmp/uitest-verify-$USER-$$}"
export UITEST_DIR

log()  { printf '\033[1;34m▸\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*"; }

NAMES=(); TIMES=(); RESULTS=(); LOGS=()
FAILED=0
STEP=0

# 한 단계를 돌리고 결과를 적는다. 실패해도 돌아온다 — 멈출지는 호출자가 정한다.
#
#   run_step <파일이름조각> <보여 줄 이름> <명령...>
#
# 파일 이름 조각은 **ASCII 로 직접 준다.** 한글 이름에서 뽑으면 전부 걸러져 `01-.log` 가 된다.
run_step() {
  local slug="$1" name="$2"; shift 2
  STEP=$((STEP + 1))
  local file start end rc
  file="$OUT/$(printf '%02d' "$STEP")-$slug.log"
  log "[$STEP] $name"
  start=$(date +%s)
  "$@" > "$file" 2>&1
  rc=$?
  end=$(date +%s)

  NAMES+=("$name"); TIMES+=("$((end - start))"); LOGS+=("$file")
  if [[ $rc -eq 0 ]]; then
    RESULTS+=("통과")
  else
    RESULTS+=("실패($rc)")
    FAILED=$((FAILED + 1))
    warn "$name 실패 — $file"
    tail -15 "$file" | sed 's/^/    /'
    [[ $FAIL_FAST -eq 1 ]] && { summary; exit $((FAILED > 125 ? 125 : FAILED)); }
  fi
  return 0
}

# ── GUI 하네스 ────────────────────────────────────────────────────────
harness_started=0

# 하네스와 앱을 반드시 정리한다. 남은 sway 는 다음 실행을 방해하고,
# 남은 nl-app 은 화면을 잡은 채 CPU 를 먹는다.
cleanup_gui() {
  [[ $harness_started -eq 1 ]] || return 0
  tools/uitest/uitest.sh stop >/dev/null 2>&1
  # 이름이 정확히 일치하는 것만 고른다. `pkill -f` 는 제 명령줄에도 걸려 호출한 셸을 죽인다.
  local pid
  for pid in $(pgrep -x nl-app 2>/dev/null); do
    # pgrep 과 여기 사이에 프로세스가 끝나면 /proc 항목이 사라진다 — 리다이렉트 실패는 셸이
    # 직접 찍으므로 `tr` 의 stderr 만 막아서는 새어 나온다. 블록째로 막고 존재도 먼저 본다.
    { [[ -r "/proc/$pid/environ" ]] &&
      tr '\0' '\n' < "/proc/$pid/environ" | grep -qx "UITEST_DIR=$UITEST_DIR" &&
      kill "$pid"; } 2>/dev/null
  done
  harness_started=0
}
trap cleanup_gui EXIT INT TERM

gui_start() {
  tools/uitest/uitest.sh start && harness_started=1
}

gui_scenario() {
  tools/uitest/uitest.sh run "tools/uitest/scenarios/$1"
}

# ── 요약 ──────────────────────────────────────────────────────────────
summary() {
  echo
  printf '\033[1m%-46s %7s  %s\033[0m\n' "단계" "소요" "결과"
  printf '%-46s %7s  %s\n' "----" "----" "----"
  local i
  for i in "${!NAMES[@]}"; do
    printf '%-46s %6ss  %s\n' "${NAMES[$i]}" "${TIMES[$i]}" "${RESULTS[$i]}"
  done
  echo
  if [[ $FAILED -eq 0 ]]; then
    printf '\033[1;32m전부 통과\033[0m (로그 %s)\n' "$OUT"
  else
    printf '\033[1;31m실패 %d건\033[0m\n' "$FAILED"
    for i in "${!NAMES[@]}"; do
      [[ "${RESULTS[$i]}" == 통과 ]] || printf '  %s — %s\n' "${NAMES[$i]}" "${LOGS[$i]}"
    done
  fi
}

# ── 단계 ──────────────────────────────────────────────────────────────
log "저장소 $ROOT"
log "로그   $OUT"
[[ $WANT_GUI -eq 1 ]] && log "하네스 $UITEST_DIR"

run_step fmt      "포맷 검사"   cargo fmt --all -- --check
run_step clippy   "클리피"      cargo clippy --workspace --all-targets -- -D warnings
run_step test     "테스트"      env NL_SNAPSHOT_REQUIRED=1 cargo test --workspace
run_step build    "릴리스 빌드" cargo build --release -p nl-runtime -p nl-cli
# 종단 시험은 샘플 프로젝트를 복사해 **빈 포트로 옮겨** 띄운다(`rebind_http_server`). 8799 를
# 다른 세션이 쥐고 있어도 통과해야 하며, 통과하지 않으면 그것이 버그다.
run_step e2e      "종단 시험"   env NL_E2E=1 cargo test -p nl-cli --test e2e

if [[ $WANT_GUI -eq 1 ]]; then
  run_step harness  "하네스 시작" gui_start
  if [[ $harness_started -eq 1 ]]; then
    run_step smoke    "시나리오 smoke"   gui_scenario smoke.uit
    run_step startup  "시나리오 startup" gui_scenario startup.uit
  else
    warn "하네스가 뜨지 않아 시나리오를 건너뜁니다"
  fi
  cleanup_gui
fi

summary
exit $((FAILED > 125 ? 125 : FAILED))
