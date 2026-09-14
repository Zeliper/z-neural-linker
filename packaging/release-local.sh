#!/usr/bin/env bash
# 릴리스 워크플로(`.github/workflows/release.yml`)의 단계를 로컬에서 그대로 재현한다.
# 태그를 밀기 전에 "CI 가 무엇을 내놓을지" 를 먼저 보는 용도다.
#
#   packaging/release-local.sh [옵션]
#
#     --version <버전>      기본: 워크스페이스 Cargo.toml 의 version
#     --out <폴더>          기본: dist-local
#     --crates a,b,c        기본: nl-app,nl-runtime,nl-cli
#     --targets linux,windows   기본: linux,windows
#     --key <경로|env>      서명 키. `env` 면 MINISIGN_KEY 환경 변수의 내용을 쓴다.
#                           생략하면 임시 키를 만들어 서명·검증까지 해 본다(드라이런).
#     --base-url <URL>      매니페스트에 적을 자산 기본 주소. 기본: https://updates.trustanc.dev/neural-linker
#     --installer           Inno Setup 이 있으면 Windows setup.exe 도 만든다 (CI 에는 없는 단계)
#     --skip-build          이미 빌드된 산출물을 그대로 쓴다
#
# CI 와 다른 점은 셋뿐이다.
#   · 버전을 태그가 아니라 인자/워크스페이스에서 읽는다
#   · `--key` 를 주지 않으면 **임시 키**로 서명한다 — 배포용이 아니라 형식 확인용이다
#   · `--installer` 는 CI 러너에 Inno Setup 이 없어 CI 에는 없는 단계다
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

VERSION=""
OUT="$NL_ROOT/dist-local"
CRATES="nl-app,nl-runtime,nl-cli"
TARGETS="linux,windows"
KEY=""
BASE_URL="https://updates.trustanc.dev/neural-linker"
WANT_INSTALLER=0
SKIP_BUILD=0
WINDOWS_TARGET="x86_64-pc-windows-msvc"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)   VERSION="$2"; shift 2 ;;
    --out)       OUT="$2"; shift 2 ;;
    --crates)    CRATES="$2"; shift 2 ;;
    --targets)   TARGETS="$2"; shift 2 ;;
    --key)       KEY="$2"; shift 2 ;;
    --base-url)  BASE_URL="$2"; shift 2 ;;
    --installer) WANT_INSTALLER=1; shift ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h|--help)   sed -n '2,28p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *)           nl_die "모르는 옵션: $1" ;;
  esac
done

[[ -n "$VERSION" ]] || VERSION="$(nl_workspace_version)"
OUT="$(nl_abs "$OUT")"
IFS=',' read -r -a CRATE_LIST <<< "$CRATES"
IFS=',' read -r -a TARGET_LIST <<< "$TARGETS"
has_target() { [[ " ${TARGET_LIST[*]} " == *" $1 "* ]]; }
has_crate()  { [[ " ${CRATE_LIST[*]} " == *" $1 "* ]]; }

LIN="$NL_ROOT/target/release"
WIN="$NL_ROOT/target/$WINDOWS_TARGET/release"
APP_BASE="$BASE_URL/$VERSION"

nl_log "버전 $VERSION · 크레이트 ${CRATE_LIST[*]} · 대상 ${TARGET_LIST[*]}"
nl_log "출력 $OUT"
rm -rf "$OUT"; mkdir -p "$OUT"

# ── 빌드 ──────────────────────────────────────────────────────────────
pkg_args=()
for c in "${CRATE_LIST[@]}"; do pkg_args+=(-p "$c"); done

if [[ $SKIP_BUILD -eq 0 ]]; then
  if has_target linux; then
    nl_log "Linux 빌드: cargo build --release ${pkg_args[*]}"
    ( cd "$NL_ROOT" && cargo build --release "${pkg_args[@]}" )
  fi
  if has_target windows; then
    command -v cargo-xwin >/dev/null || nl_die "cargo-xwin 이 없습니다 — cargo install cargo-xwin --locked"
    nl_log "Windows 크로스 빌드: cargo xwin build --release ${pkg_args[*]} --target $WINDOWS_TARGET"
    ( cd "$NL_ROOT" && cargo xwin build --release "${pkg_args[@]}" --target "$WINDOWS_TARGET" )
  fi
else
  nl_warn "--skip-build: 이미 있는 산출물을 씁니다"
fi

# ── 아카이브 ──────────────────────────────────────────────────────────
# 빌드한 크레이트의 실행 파일만 모은다. 이름 규칙은 크레이트 이름 그대로다.
# 실행 파일 이름은 크레이트 이름과 다를 수 있다 (`nl-cli` → `nl`). cargo 에게 묻는다.
lin_bins=(); win_bins=(); bin_names=()
for c in "${CRATE_LIST[@]}"; do
  while IFS= read -r b; do
    [[ -n "$b" ]] || continue
    bin_names+=("$b")
    has_target linux   && [[ -f "$LIN/$b"     ]] && lin_bins+=("$LIN/$b")
    has_target windows && [[ -f "$WIN/$b.exe" ]] && win_bins+=("$WIN/$b.exe")
  done < <(nl_bin_names "$c")
done
[[ ${#bin_names[@]} -gt 0 ]] || nl_die "실행 파일을 내놓는 크레이트가 없습니다: ${CRATE_LIST[*]}"
nl_log "실행 파일: ${bin_names[*]}"

if has_target linux; then
  [[ ${#lin_bins[@]} -gt 0 ]] || nl_die "Linux 실행 파일이 없습니다 (${LIN})"
  nl_log "Linux tar.gz (${#lin_bins[@]}개)"
  nl_pack_linux "$OUT/neural-linker-$VERSION-linux-x86_64.tar.gz" "$OUT" "${lin_bins[@]}"
  for b in "${lin_bins[@]}"; do cp "$b" "$OUT/"; done
fi

if has_target windows; then
  [[ ${#win_bins[@]} -gt 0 ]] || nl_die "Windows 실행 파일이 없습니다 (${WIN})"
  nl_log "Windows zip (${#win_bins[@]}개)"
  nl_pack_windows "$OUT/neural-linker-$VERSION-windows-x86_64.zip" "$OUT" "${win_bins[@]}"
  for b in "${win_bins[@]}"; do cp "$b" "$OUT/"; done
fi

# ── Windows 설치 프로그램 (CI 에는 없는 단계) ─────────────────────────
# CI 러너에는 Inno Setup 이 없어 zip 만 만든다. 로컬에 있으면 여기서 한 번 더 확인해 볼 수 있다.
if [[ $WANT_INSTALLER -eq 1 ]]; then
  if has_target windows && has_crate nl-app && [[ -f "$WIN/nl-app.exe" ]]; then
    nl_log "Windows 설치 프로그램 (Inno Setup)"
    if cargo run -q --manifest-path "$NL_ROOT/Cargo.toml" -p nl-bundle --example nl-installer -- \
         "$WIN/nl-app.exe" "Neural Linker" "$VERSION" "Trust A&C" "$OUT"; then
      :
    else
      nl_warn "설치 프로그램을 만들지 못했습니다 — zip 으로 갑니다"
    fi
  else
    nl_warn "--installer 는 windows 대상 + nl-app 이 있어야 합니다 — 건너뜁니다"
  fi
fi

# ── 매니페스트 ────────────────────────────────────────────────────────
# 빌더 자체 업데이트: 자동 업데이트가 그대로 내려받는 알맹이가 자산이다.
# CI 와 같은 이유로 Linux 자산만 넣는다 (Windows 는 kind=installer 여야 맞는데 러너에 Inno 가 없다).
if has_crate nl-app && has_target linux && [[ -f "$OUT/nl-app" ]]; then
  nl_log "latest.json"
  nl_make_app_manifest "$VERSION" "$APP_BASE" "$OUT/latest.json" "$OUT/nl-app"
else
  nl_warn "nl-app Linux 산출물이 없어 latest.json 을 건너뜁니다"
fi

# 빌더가 받아 오는 대상별 런타임.
runtime_pairs=()
has_target linux   && [[ -f "$OUT/nl-runtime"     ]] && runtime_pairs+=("linux-x86_64=$OUT/nl-runtime")
has_target windows && [[ -f "$OUT/nl-runtime.exe" ]] && runtime_pairs+=("windows-x86_64=$OUT/nl-runtime.exe")
if [[ ${#runtime_pairs[@]} -gt 0 ]]; then
  nl_log "runtimes/latest.json (${#runtime_pairs[@]}개 대상)"
  nl_make_runtimes_manifest "$VERSION" "$BASE_URL/$VERSION/runtimes" \
    "$OUT/runtimes/latest.json" "${runtime_pairs[@]}"
else
  nl_warn "런타임 산출물이 없어 runtimes/latest.json 을 건너뜁니다"
fi

# ── 서명·검증 ─────────────────────────────────────────────────────────
TMP_KEYS=""
PUBKEY=""
case "$KEY" in
  "")
    TMP_KEYS="$(mktemp -d)"
    nl_warn "--key 가 없어 임시 키로 서명합니다 (형식 확인용, 배포용 아님)"
    nl_keygen keygen --out "$TMP_KEYS" --name dryrun >/dev/null
    KEY="$TMP_KEYS/dryrun.key"
    PUBKEY="$TMP_KEYS/dryrun.pub"
    ;;
  env)
    [[ -n "${MINISIGN_KEY:-}" ]] || nl_die "--key env 인데 MINISIGN_KEY 가 비어 있습니다"
    KEY=""   # nl_sign 이 환경 변수를 직접 읽는다
    ;;
  *)
    [[ -f "$KEY" ]] || nl_die "키 파일이 없습니다: $KEY"
    [[ -f "${KEY%.key}.pub" ]] && PUBKEY="${KEY%.key}.pub"
    ;;
esac

for m in "$OUT/latest.json" "$OUT/runtimes/latest.json"; do
  [[ -f "$m" ]] || continue
  # make-manifest.sh 가 MINISIGN_KEY 로 이미 서명했을 수 있다. 없을 때만 여기서 붙인다.
  [[ -f "$m.minisig" ]] || nl_sign "$m" "$KEY"
  if [[ -n "$PUBKEY" ]]; then
    nl_verify "$m" "$PUBKEY"
  else
    nl_warn "공개키를 몰라 $m 검증을 건너뜁니다 (--key <경로> 를 주면 옆의 .pub 을 씁니다)"
  fi
done

[[ -n "$TMP_KEYS" ]] && rm -rf "$TMP_KEYS"

# ── 결과 ──────────────────────────────────────────────────────────────
echo
nl_log "산출물"
nl_artifact_table "$OUT"
echo
nl_log "다음: docs/RELEASE.md 의 체크리스트"
