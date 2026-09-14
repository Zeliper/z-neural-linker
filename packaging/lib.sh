#!/usr/bin/env bash
# 릴리스 단계의 공통 함수. CI(`.github/workflows/release.yml`)와 로컬 스크립트
# (`packaging/release-local.sh`)가 같은 코드를 쓰도록 여기에 모은다.
#
#   source packaging/lib.sh
#
# 이 파일은 `set -euo pipefail` 을 스스로 켜지 않는다 — 부르는 쪽이 정한다.

# 저장소 뿌리. 이 파일이 `packaging/` 안에 있다는 것만 가정한다.
NL_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export NL_ROOT

nl_log()  { printf '\033[1;34m▸\033[0m %s\n' "$*" >&2; }
nl_warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
nl_die()  { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# 워크스페이스 버전. `[workspace.package] version` 첫 줄을 읽는다.
nl_workspace_version() {
  sed -n 's/^version = "\(.*\)"$/\1/p' "$NL_ROOT/Cargo.toml" | head -1
}

# 태그가 있으면 태그 버전, 없으면 워크스페이스 버전. CI 의 "버전 확인" 단계와 같은 규칙이다.
#   nl_release_version "$GITHUB_REF"
nl_release_version() {
  local ref="${1:-}"
  if [[ "$ref" == refs/tags/v* ]]; then
    printf '%s\n' "${ref#refs/tags/v}"
  else
    nl_workspace_version
  fi
}

# 아직 없는 파일도 절대 경로로 바꾼다. `cd` 하는 함수에 상대 경로를 넘겨도 엉뚱한 곳에 쓰지 않게.
nl_abs() {
  local p="$1"
  case "$p" in
    /*) printf '%s\n' "$p" ;;
    *)  printf '%s/%s\n' "$(pwd)" "$p" ;;
  esac
}

# 크레이트가 내놓는 실행 파일 이름들. **크레이트 이름과 다를 수 있다** — `nl-cli` 는 `nl` 을 만든다.
# 이름을 짐작하지 말고 cargo 에게 묻는다.
#   nl_bin_names nl-cli   → nl
nl_bin_names() {
  local crate="$1"
  cargo metadata --no-deps --format-version 1 --manifest-path "$NL_ROOT/Cargo.toml" 2>/dev/null |
    python3 -c '
import json, sys
want = sys.argv[1]
meta = json.load(sys.stdin)
for pkg in meta["packages"]:
    if pkg["name"] != want:
        continue
    for t in pkg["targets"]:
        if "bin" in t["kind"]:
            print(t["name"])
' "$crate"
}

# 파일 크기(바이트)와 sha256. 산출물 표와 매니페스트가 같은 값을 쓰게 한다.
nl_size()   { stat -c %s "$1"; }
nl_sha256() { sha256sum "$1" | cut -d' ' -f1; }

# ── 포트 격리 ─────────────────────────────────────────────────────────
#
# 샘플 프로젝트의 추론 API 는 **고정 포트**(XOR 8799, CNN 8800)를 쓴다. 사람이 손으로 부를
# 주소를 문서에 적어 두려면 고정이라야 하기 때문이다. 그런데 검증 스크립트가 그대로 띄우면
# 같은 기계에서 다른 세션·다른 에이전트가 돌리는 것과 포트를 다툰다. 실제로 그렇게 깨졌다 —
# 부하 시험이 8799 를 잡고 있는데 검증이 같은 포트를 열려다 실패했고, 포트를 잡은 쪽을
# 남의 찌꺼기로 오해해 죽이는 일까지 났다.
#
# 그래서 **검증은 샘플을 그대로 띄우지 않는다.** 프로젝트를 복사해 포트만 빈 포트로 바꿔 띄운다.

# 지금 비어 있는 TCP 포트 하나를 고른다. 운영체제에 0번을 달라고 해 받은 번호를 돌려준다.
#
# 받은 즉시 놓아 주므로 쓰기 전에 남이 채 갈 틈이 이론상 있다. 높은 임의 포트라 실제로는
# 부딪히지 않으며, 고정 포트를 쓰는 것보다 훨씬 안전하다.
nl_free_port() {
  python3 -c '
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
'
}

# 프로젝트 파일의 HTTP 서버 주소를 다른 포트로 바꾼다. **파일을 제자리에서 고친다** —
# 원본이 아니라 복사본을 넘겨라.
#
#   cp 샘플.nlproj "$TMP/시험.nlproj"
#   PORT=$(nl_free_port)
#   nl_rebind_project "$TMP/시험.nlproj" "$PORT"
#   ./내앱 --headless ... # 이제 $PORT 에서 듣는다
#
# 바꿀 주소를 못 찾으면 실패한다. 조용히 넘어가면 고정 포트로 돌아 버리기 때문이다.
nl_rebind_project() {
  local proj="$1" port="$2"
  [[ -f "$proj" ]] || nl_die "프로젝트 파일이 없다: $proj"
  python3 - "$proj" "$port" <<'PYEOF'
import json, re, sys

path, port = sys.argv[1], int(sys.argv[2])
text = open(path, encoding="utf-8").read()

# 샘플이 쓰는 두 고정 주소. 어느 쪽이 들었든 같은 빈 포트로 모은다 — 한 프로젝트가
# 두 주소를 함께 쓰는 일은 없다.
found = [m for m in re.findall(r"127\.0\.0\.1:(8799|8800)", text)]
if not found:
    sys.exit(f"{path} 에 바꿀 고정 주소(127.0.0.1:8799|8800)가 없다")

new = re.sub(r"127\.0\.0\.1:(?:8799|8800)", f"127.0.0.1:{port}", text)
json.loads(new)  # 바꾼 결과가 여전히 JSON 인지 본다
open(path, "w", encoding="utf-8").write(new)
print(f"127.0.0.1:{port}")
PYEOF
}

# ── 아카이브 ──────────────────────────────────────────────────────────

# Linux tar.gz. install.sh 가 기대하는 배치 그대로 담는다.
#   nl_pack_linux <출력 tar.gz> <스테이징 부모> <바이너리...>
nl_pack_linux() {
  local out parent
  out="$(nl_abs "$1")"; parent="$(nl_abs "$2")"; shift 2
  # 아카이브 안의 폴더 이름은 `neural-linker` 로 고정이라 바꿀 수 없다. 대신 **한 겹 위를 pid 로
  # 가른다** — 두 실행이 같은 부모를 쓰면 서로의 스테이징을 지워 버린다.
  local work="$parent/.pack-$$-linux"
  local stage="$work/neural-linker"
  rm -rf "$work"; mkdir -p "$stage"
  cp "$@" "$stage/"
  cp "$NL_ROOT/packaging/linux/install.sh" \
     "$NL_ROOT/packaging/linux/neural-linker.desktop" \
     "$NL_ROOT/packaging/linux/neural-linker-mime.xml" \
     "$NL_ROOT/packaging/linux/neural-linker.svg" "$stage/"
  # 아이콘 원본(SVG)이 들어가야 install.sh 가 아이콘 테마에 넣는다.
  tar -czf "$out" -C "$work" neural-linker
  rm -rf "$work"
}

# Windows zip. 설치 프로그램 없이 풀어 쓰는 묶음이다.
#   nl_pack_windows <출력 zip> <스테이징 부모> <exe...>
nl_pack_windows() {
  local out parent
  out="$(nl_abs "$1")"; parent="$(nl_abs "$2")"; shift 2
  local stage="$parent/.pack-$$-win"
  rm -rf "$stage"; mkdir -p "$stage"
  cp "$@" "$stage/"
  ( cd "$stage" && zip -q -r "$out" . )
  rm -rf "$stage"
}

# ── 매니페스트 ────────────────────────────────────────────────────────

# 빌더 자체 업데이트 매니페스트. `make-manifest.sh` 는 자기가 있는 폴더에 latest.json 을 쓴다.
#   nl_make_app_manifest <버전> <자산 기본 URL> <출력 파일> <자산...>
nl_make_app_manifest() {
  local version="$1" base="$2" out
  out="$(nl_abs "$3")"; shift 3
  local args=()
  for f in "$@"; do args+=("$(realpath "$f")"); done
  ( cd "$NL_ROOT/packaging" && NOTES="${NOTES:-$version 릴리스}" ./make-manifest.sh "$version" "$base" "${args[@]}" >/dev/null )
  mkdir -p "$(dirname "$out")"
  mv "$NL_ROOT/packaging/latest.json" "$out"
  # 서명이 함께 만들어졌다면 같이 옮긴다.
  [[ -f "$NL_ROOT/packaging/latest.json.minisig" ]] && mv "$NL_ROOT/packaging/latest.json.minisig" "$out.minisig"
  return 0
}

# 빌더가 대상별 런타임을 받아 오는 매니페스트.
#   nl_make_runtimes_manifest <버전> <자산 기본 URL> <출력 파일> <대상키=파일...>
nl_make_runtimes_manifest() {
  local version="$1" base="$2" out
  out="$(nl_abs "$3")"; shift 3
  python3 "$NL_ROOT/packaging/make-runtimes-manifest.py" "$version" "$base" "$out" "$@" >/dev/null
}

# ── 서명·검증 ─────────────────────────────────────────────────────────

# nl-keygen 예제 도구. minisign CLI 는 쓰지 않는다.
nl_keygen() {
  cargo run -q --manifest-path "$NL_ROOT/Cargo.toml" -p nl-update --example nl-keygen -- "$@"
}

# 매니페스트 서명. 키는 경로이거나 MINISIGN_KEY 환경 변수의 내용이다.
#   nl_sign <파일> [키 경로]
nl_sign() {
  local f="$1" key="${2:-}"
  if [[ -n "$key" && -f "$key" ]]; then
    nl_keygen sign "$f" --key "$key"
  else
    nl_keygen sign "$f"
  fi
}

# 서명 검증. 배포 앱이 쓰는 nl_update::verify_manifest 를 그대로 부른다.
#   nl_verify <파일> <공개키 base64 또는 .pub 경로>
nl_verify() {
  nl_keygen verify "$1" --pubkey "$2"
}

# ── 산출물 표 ─────────────────────────────────────────────────────────

# 폴더 아래 모든 파일을 경로·크기·sha256 으로 찍는다.
#   nl_artifact_table <폴더>
nl_artifact_table() {
  local dir
  dir="$(nl_abs "$1")"
  printf '%-52s %12s  %s\n' "파일" "크기(바이트)" "sha256"
  printf '%-52s %12s  %s\n' "----" "------------" "------"
  while IFS= read -r f; do
    printf '%-52s %12s  %s\n' "${f#"$dir"/}" "$(nl_size "$f")" "$(nl_sha256 "$f")"
  done < <(find "$dir" -type f | sort)
}
