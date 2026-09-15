# 인수인계 — 새 세션에서 이어서 하기 (2026-09-15)

저장소: https://github.com/Zeliper/z-neural-linker (public). 메인 브랜치 하나, 에이전트 워크트리·브랜치는 모두 병합 후 삭제됨.

## 지금 상태 한눈에
- 워크스페이스 크레이트 9개, Rust 약 6.5만 줄. `cargo test --workspace` 809개 통과(무시 1: `update_key.rs` 문서 예제), 종단 6개, GUI 시나리오 3개, clippy 0, `cargo fmt --all --check` 클린(CI 강제).
- 마일스톤: M0 완료 · M1 거의 완료(pipewire 고 fps 캡처만 미착수) · M2 코드 완료(배포 운영 항목만 남음) · M3 진행(ONNX 내보내기 완료, 가져오기는 선택 feature 뼈대) · M4 절반(서버형 배포 완료, 협업 서버 미착수). 상세: `docs/ROADMAP.md`.
- 보안 리뷰 58건 중 44 수정·3 부분·2 미처리(상위 의존성). 현황표: `docs/reviews/security-2026-09-14.md`.

## 먼저 읽을 문서 (순서대로)
1. `README.md` → `docs/ARCHITECTURE.md`(크레이트 구조·보안 모델·테스트 전략)
2. `docs/GUIDE.md`(사용자 가이드) · `crates/*/README.md`(크레이트별 API·한계)
3. `docs/ROADMAP.md` · `CHANGELOG.md` · `docs/RELEASE.md` · `docs/release-dryrun-2026-09-14.md`(①~⑩ 완주 기록·찾은 문제 9건)
4. 리뷰: `docs/reviews/{security,engine,deps}-2026-09-14.md` · 조사: `docs/research/onnx-2026-09-14.md`

## 검증 방법
```bash
scripts/verify-all.sh            # fmt·clippy·test·release build·e2e (약 30~60분, 전 단계 계속 실행)
scripts/verify-all.sh --gui      # + 헤드리스 sway 시나리오(smoke/startup/pipeline)
NL_E2E=1 cargo test -p nl-cli --test e2e     # 종단 6개(학습→빌드→배포판 HTTP)
tools/uitest/uitest.sh run tools/uitest/scenarios/pipeline.uit   # 단일 GUI 시나리오
scripts/bench.sh                 # 성능 기준값(JSON, 직전 비교)
```
규칙: `scripts/README.md` — 고정 포트 금지, pid 접미사, 자기가 띄운 pid만 종료(`pgrep -f` 금지).

## 이 개발 PC의 환경 사실
- RTX 2060은 NVK 드라이버라 wgpu 컴퓨트 실패 → 엔진 `probe`가 걸러 Intel UHD 630 사용. NVIDIA 드라이버 설치 시 RTX 사용 가능.
- 세션은 KDE Plasma 6 Wayland → 화면 캡처는 xdg-desktop-portal 경로(약 2.8fps). 고 fps는 `pipewire-devel` 필요.
- `libxkbcommon.so` 개발 링크가 없어 `crates/nl-io/build.rs`가 OUT_DIR에 심볼릭 링크를 만들어 링크.
- Windows 크로스 빌드: `cargo-xwin` 설치됨. 링커는 `~/.local/bin/lld-link` — rust-lld 를 `-flavor link` 로 부르는 셸 심이다(`clang-cl`·`llvm-lib` 는 `/usr/bin`). `nl_bundle::tools::cross_build_plan` 참고.
- Inno Setup 6.7.3 은 wine 접두사(`~/.wine/drive_c/Program Files (x86)/Inno Setup 6/ISCC.exe`)에 있어야 `--installer` 가 돈다. 사라졌으면 `wine ~/.cache/neural-linker/tools/innosetup-6.7.3.exe /VERYSILENT` 로 다시 깐다.

## 바로 이어서 할 일 (우선순위)
1. ~~릴리스 드라이런 기록 완성~~ **완료(2026-09-15).** ①~⑩ 을 끝까지 돌았고 문제 9건을 고쳤다. 로컬로 할 수 없어 비워 둔 칸(⑤ 태그, ⑥ CI 산출물, ⑧ 실제 업로드, ⑨ 적용 뒤 `--version`, ⑩ Windows 실기 설치)은 문서 끝 "남은 것" 에 있다.
2. **Windows 실기 검증**: 이 저장소는 Linux 에서만 검증됐다. Windows PC 에서 할 일은 [`docs/WINDOWS-CHECK.md`](WINDOWS-CHECK.md) 에 항목·명령·보고 형식까지 적어 두었다.
3. **첫 릴리스 전 사람이 할 일**: minisign 키 발급(`cargo run -p nl-update --example nl-keygen -- keygen`), 공개키를 `crates/nl-app/src/update_key.rs`에 삽입, 실제 배포 서버 URL(현재 `updates.trustanc.dev/neural-linker`는 503), Forgejo/GitHub 러너·시크릿, Windows 실기 검증(`packaging/windows/install-service.ps1`, `nl-io` 소켓 동작).
4. **미착수**: pipewire ScreenCast(optional feature), ONNX 가져오기 파이프라인 연결(`PNodeKind::OnnxModel`), 협업 서버(M4), Authenticode 서명, `Graph`/`Edge`/`EpochMetrics`의 미지 필드 보존.
5. **알려진 한계**: 순환 레이어는 시퀀스 길이에 선형(길이 512에서 스텝 2~3초), iGPU는 짧은 시퀀스에서 CPU보다 느림, `ModelOutput.field`는 다출력에서 미사용, Windows 개인키 권한 비트 없음.

## 작업 방식 메모
- 이 저장소는 여러 에이전트가 크레이트별 워크트리에서 병렬로 만들었다. 공개 API는 각 `lib.rs` re-export가 계약이고, `nl-core` 변경은 추가 전용(`#[serde(default)]`)이 원칙.
- 새 `LayerKind`/`Source`/`Sink` 변형을 추가하면 nl-app 인스펙터·팔레트·ONNX `supported()`가 컴파일 오류로 알려 준다(의도된 전수 매치).
- 자세한 세션 결정 기록은 `~/.claude/projects/-home-zeliper-Workspace-z-neural-linker/memory/`.
