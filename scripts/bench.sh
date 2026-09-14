#!/usr/bin/env bash
# 성능 기준값을 재고 직전 결과와 비교한다.
#
#   scripts/bench.sh [--rounds N] [--gpu] [--filter <패턴>] [--out <폴더>] [--baseline <파일>]
#
#     --rounds N     몇 번 돌려 중앙값을 낼지 (기본 3). 프로세스를 새로 띄우므로 캐시도 식는다
#     --gpu          NL_TEST_GPU=1 NL_TEST_DEVICE=gpu 로 GPU 경로까지 잰다 (오래 걸린다)
#     --filter <패턴> 이 패턴에 맞는 벤치만 (cargo test 의 이름 필터)
#     --out <폴더>   결과 위치. 기본 dist-local/bench
#     --baseline <파일> 비교 대상. 기본은 `--out` 안의 가장 최근 JSON
#
# 결과는 `<out>/<시각>-<pid>.json` 으로 남고 직전 것과 비교한 표를 찍는다.
# **종료 코드는 정보용 0 이다** — 성능은 기계 상태에 흔들려서, 빌드를 세우는 기준으로 쓰면
# 아무도 믿지 않게 된다. 사람이 표를 보고 판단한다.
#
# 이 기계는 다른 에이전트·워크트리와 함께 쓴다. 부하가 높으면 수치가 두세 배까지 흔들리므로
# JSON 에 `load` 를 함께 적는다. 비교표에서 부하가 크게 다르면 그 줄은 의심하라.
# 그 밖의 동시 실행 규칙은 `scripts/README.md`.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

ROUNDS=3
WANT_GPU=0
FILTER="bench_"
OUT="dist-local/bench"
BASELINE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --rounds)   ROUNDS="${2:?--rounds 에 숫자가 필요하다}"; shift 2 ;;
    --gpu)      WANT_GPU=1; shift ;;
    --filter)   FILTER="${2:?--filter 에 패턴이 필요하다}"; shift 2 ;;
    --out)      OUT="${2:?--out 에 폴더가 필요하다}"; shift 2 ;;
    --baseline) BASELINE="${2:?--baseline 에 파일이 필요하다}"; shift 2 ;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "모르는 인자: $1 (--help 로 사용법)" >&2; exit 2 ;;
  esac
done

if ! [[ "$ROUNDS" =~ ^[1-9][0-9]*$ ]]; then
  echo "--rounds 는 1 이상의 정수여야 한다 (지금 '$ROUNDS')" >&2
  exit 2
fi

mkdir -p "$OUT"
STAMP="$(date +%Y%m%d-%H%M%S)-$$"
RAW="$(mktemp -d "${TMPDIR:-/tmp}/nl-bench-$$-XXXX")"
trap 'rm -rf "$RAW"' EXIT

# 비교 대상은 **이번 것을 쓰기 전에** 정한다. 안 그러면 자기 자신과 비교한다.
if [[ -z "$BASELINE" ]]; then
  BASELINE="$(ls -1 "$OUT"/*.json 2>/dev/null | sort | tail -1)"
fi

env_args=()
label="cpu"
if [[ $WANT_GPU -eq 1 ]]; then
  env_args=(NL_TEST_GPU=1 NL_TEST_DEVICE=gpu)
  label="gpu"
fi

echo "== 릴리스 빌드"
# 벤치는 **반드시 릴리스로** 돈다. 디버그에서는 순환 레이어 루프가 19배 느려 숫자가 무의미하다.
if ! cargo build --release -p nl-engine --tests --offline >"$RAW/build.log" 2>&1; then
  echo "빌드 실패:"; tail -25 "$RAW/build.log"; exit 1
fi

echo "== $ROUNDS 회 측정 (장치 $label)"
for ((i = 1; i <= ROUNDS; i++)); do
  printf '  %d/%d  부하 %s ... ' "$i" "$ROUNDS" "$(cut -d' ' -f1 /proc/loadavg 2>/dev/null || echo '?')"
  env NL_BENCH=1 "${env_args[@]}" \
    cargo test --release -p nl-engine --offline "$FILTER" -- --nocapture \
    >"$RAW/round$i.log" 2>&1
  n=$(grep -c '^BENCHJSON ' "$RAW/round$i.log" || true)
  grep -h '^BENCHJSON ' "$RAW/round$i.log" >>"$RAW/all.txt" || true
  echo "$n 항목"
done

if [[ ! -s "$RAW/all.txt" ]]; then
  echo "측정값이 하나도 없다. 마지막 실행 로그:" >&2
  tail -25 "$RAW/round$ROUNDS.log" >&2
  exit 1
fi

# ── 중앙값 계산 · JSON 쓰기 · 직전과 비교 ──
OUT_FILE="$OUT/$STAMP.json"
python3 - "$RAW/all.txt" "$OUT_FILE" "$BASELINE" "$label" "$ROUNDS" <<'PY'
import json, os, platform, subprocess, sys, datetime

raw, out_file, baseline, device, rounds = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])

def sh(*cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=10).stdout.strip()
    except Exception:
        return ""

samples = {}
for line in open(raw, encoding="utf-8"):
    if not line.startswith("BENCHJSON "):
        continue
    try:
        m = json.loads(line[len("BENCHJSON "):])
    except Exception:
        continue
    samples.setdefault(m["name"], {"unit": m["unit"], "values": []})["values"].append(float(m["value"]))

def median(v):
    v = sorted(v); n = len(v)
    return v[n // 2] if n % 2 else (v[n // 2 - 1] + v[n // 2]) / 2

metrics = {
    k: {
        "unit": v["unit"],
        "median": round(median(v["values"]), 4),
        "min": round(min(v["values"]), 4),
        "max": round(max(v["values"]), 4),
        "n": len(v["values"]),
    }
    for k, v in sorted(samples.items())
}

try:
    load = os.getloadavg()[0]
except OSError:
    load = None

doc = {
    "when": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
    "git": sh("git", "rev-parse", "--short", "HEAD"),
    "git_dirty": bool(sh("git", "status", "--porcelain")),
    "rustc": sh("rustc", "--version"),
    "device": device,
    "rounds": rounds,
    "machine": {
        "os": f"{platform.system()} {platform.release()}",
        "arch": platform.machine(),
        "cpus": os.cpu_count(),
        "load1": load,
    },
    "metrics": metrics,
}
with open(out_file, "w", encoding="utf-8") as f:
    json.dump(doc, f, ensure_ascii=False, indent=2)
    f.write("\n")
print(f"\n== 결과 {out_file}")

# ── 비교 ──
THRESHOLD = 0.20
if not baseline or not os.path.exists(baseline) or os.path.abspath(baseline) == os.path.abspath(out_file):
    print("비교할 직전 결과가 없다 (이번 것이 기준이 된다)")
else:
    old = json.load(open(baseline, encoding="utf-8"))
    print(f"직전 {os.path.basename(baseline)}  {old.get('git','?')}  부하 {old.get('machine',{}).get('load1')}"
          f"  →  이번 {doc['git']}  부하 {doc['machine']['load1']}")
    if old.get("device") != device:
        print(f"  주의: 직전은 장치 '{old.get('device')}' 였다 — 그대로 비교하면 안 된다")

    rows, same = [], 0
    for name, cur in metrics.items():
        prev = old.get("metrics", {}).get(name)
        if not prev:
            rows.append((name, None, cur["median"], cur["unit"], "새 항목"))
            continue
        p, c = prev["median"], cur["median"]
        delta = (c - p) / p if p else 0.0
        if abs(delta) > THRESHOLD:
            rows.append((name, p, c, cur["unit"], f"{delta * 100:+.0f}%"))
        else:
            same += 1
    gone = [n for n in old.get("metrics", {}) if n not in metrics]

    if rows:
        w = max(len(r[0]) for r in rows)
        print(f"\n  {'항목'.ljust(w)}  {'직전':>10}  {'이번':>10}  변화")
        for name, p, c, unit, note in rows:
            ps = f"{p:.2f}" if p is not None else "-"
            print(f"  {name.ljust(w)}  {ps:>10}  {c:>10.2f}  {note}  ({unit})")
    print(f"\n  ±{int(THRESHOLD * 100)}% 안: {same} 항목" + (f", 사라진 항목: {', '.join(gone)}" if gone else ""))
    if rows:
        print("  부하가 크게 다르면 이 표는 믿을 것이 못 된다. 조용할 때 다시 재라.")
PY

echo
echo "종료 코드는 정보용 0 이다 (성능은 기계 상태에 흔들린다)."
exit 0
