# 로드맵

2026-09-10 시작. 설계는 [`docs/ARCHITECTURE.md`](ARCHITECTURE.md), 변경 이력은 [`CHANGELOG.md`](../CHANGELOG.md).
각 마일스톤은 "빌더에서 만들고 배포판에서 도는" 수직 조각을 하나씩 늘린다.
항목 끝의 해시는 그 일이 들어온 커밋이다.

## M0 — 뼈대와 첫 수직 조각 · **완료 (2026-09-14)**

닷새 만에 "설계 → 학습 → 연결 → GUI → 빌드 → 배포 → 자동 업데이트" 한 바퀴가 돌았다.
`nl sample → train → build → 배포판 HTTP 추론`이 한 줄로 이어지고, 그 경로를 종단 시험이 지킨다.

- [x] 워크스페이스·문서·`nl-core` 데이터 모델 계약 — `9a1dd8e`
- [x] `nl-core`: 그래프 op/undo/diff, 형상 추론, 검증, 직렬화 왕복 — `9a1dd8e`
- [x] `nl-engine`: burn 0.21 인터프리터, CPU(ndarray)/GPU(wgpu) + `probe` 기반 Auto, 학습 루프,
      safetensors 체크포인트, 추론 세션, 데이터 로더, codec — `824e155`
- [x] `nl-io`: 자원 조회, 화면 캡처, 입력 시뮬레이션, HTTP, 파이프라인 `Runner` — `1e9bc78`
- [x] `nl-gui` 렌더러 · `nl-bundle` 번들/첨부/아카이브 · `nl-runtime` 실행기 — `401009d`
- [x] `nl-app` 1차: 프로젝트 관리, 모델 캔버스, 인스펙터, 데이터/학습/자원 뷰 — `48e5a7d`
- [x] `nl-app` 2차: 파이프라인 뷰(시험 실행·킬 스위치), GUI 디자이너, 빌드 뷰 — `00d7fb6`
- [x] `nl-app` 3·4차: HttpServer/HttpReply 노드, 녹화 UI, Inno 동의 설치, 빌더 자체 업데이트 — `a150a54`
- [x] 빌더 시작 지연 12.2초 → 1.6초, `[nl-app] ready/focused` 마커 — `4a08ba5`
- [x] `nl-cli`: 헤드리스 inspect/devices/train/infer/run/record/build/sample — `e600a15`
- [x] `nl-update` + 배포 런타임 자동 업데이트(배지/적용) + minisign 검증 — `af588da`, `f1af512`
- [x] Windows 크로스 빌드(cargo-xwin, rust-lld 심), AttachConsole — `8b15a2b`, `16c5149`
- [x] Inno Setup 실컴파일·실설치 확인 (wine 11.0 + Inno 6.7.3) — `436bec4`
- [x] uitest 시나리오 러너(run/wait-log/expect-shot, PPM 골든) — `2123bf3`
- [x] `egui_kittest` 인프로세스 스냅샷 골든 — `3f2f7de`
- [x] GitHub·Forgejo Actions 두 벌, 릴리스 워크플로 — `0ebb3ab`
- [x] 릴리스 공통 함수(`packaging/lib.sh`)와 로컬 드라이런 — `89609a9`
- [x] 앱 아이콘 일습(SVG → PNG → ICO, 실행 파일·설치 프로그램·데스크톱) — `49ccb46`
- [x] 전체 검증 러너 `scripts/verify-all.sh` — `ae81f8a`
- [x] 전 크레이트 포맷 정리, CI fmt 검사 강제 — `6149b01`

### 산출 (2026-09-14 실측)

`cargo test --workspace` **668개 통과**. 크레이트별 내역은 통합 시험 바이너리를 그 크레이트에 합친 수다.

| 크레이트 | 테스트 | 역할 |
| --- | ---: | --- |
| `nl-app` | 130 | 빌더 GUI |
| `nl-io` | 126 | 캡처·입력·HTTP 서버·WebSocket·녹화·`Runner` |
| `nl-engine` | 102 | burn 인터프리터·학습·추론·codec |
| `nl-update` | 85 | 매니페스트·서명·다운로드·적용 |
| `nl-bundle` | 74 | `.nlapp`·첨부·아카이브·설치 프로그램·도구 |
| `nl-core` | 47 | 문서 모델·형상·op/undo·번들 포맷 |
| `nl-runtime` | 46 | 배포 실행기 |
| `nl-gui` | 32 | 공용 렌더러 |
| `nl-cli` | 26 | `nl` 명령줄 |

이 밖에 헤드리스 sway 시나리오 둘(`smoke`·`startup`)과 종단 시험(`NL_E2E=1`)이 따로 돈다.
전부 한 번에 돌리려면 `scripts/verify-all.sh --gui`.

### 환경에서 확인된 사실

- 이 개발 PC 의 RTX 2060 은 오픈소스 NVK(nouveau) 드라이버라 wgpu 컴퓨트가 죽는다
  (`Parent device is lost`). `Auto` 는 `probe` 로 걸러 Intel UHD 630 을 고른다. 공식 드라이버를 깔면 RTX 로 잡힌다.
- 이 PC 의 실제 세션은 KDE Plasma 6 Wayland 다. wlr-screencopy·X11 `GetImage` 둘 다 쓸 수 없어
  xdg-desktop-portal Screenshot 경로를 붙였다 — **실측 2.8 fps**. 고 fps 는 pipewire ScreenCast 가 필요하다.
- Windows 크로스 빌드는 관리자 권한 없이 된다. `lld-link` 가 없으면 rustup 의 `rust-lld` 를 부르는
  얇은 스크립트로 대신한다. SDK 캐시 1.2 GB, 첫 빌드 9분 남짓.
- Inno Setup 의 실제 배포처는 GitHub 릴리스다. `jrsoftware.org/download.php/is.exe` 는 설치본이 아니라
  안내 페이지로 302 한다 — 예전에 "네트워크 차단" 으로 적어 둔 것은 잘못된 주소 탓이었다.
- `pkill -f`/`pgrep -f` 에 명령줄 조각을 넣으면 호출한 셸 자신이 걸려 죽는다. `pgrep -x` 와 `/proc` 를 쓴다.

## 보안

2026-09-14 에 네트워크·번들·업데이트 경로를 읽기 전용으로 리뷰했다(높음 11 · 중간 22 · 낮음 25).
같은 날 엔진 정확성 리뷰도 따로 했다.

- [`docs/reviews/security-2026-09-14.md`](reviews/security-2026-09-14.md) — 항목별 상태·담당 크레이트·근거·남은 한계
- [`docs/reviews/engine-2026-09-14.md`](reviews/engine-2026-09-14.md) — 엔진 리뷰 현황표
- [`docs/RELEASE.md`](RELEASE.md) — 릴리스 체크리스트와 롤백 절차

보안 리뷰 58건 중 **수정 44 · 부분 3 · 진행 중 9 · 미처리 2**. 높음 11건은 9건 수정 + 2건 진행 중이다.
엔진 리뷰 24건은 **수정 23 · 부분 1**. 남은 진행 중 9건은 nl-app 담당이다.

신뢰 모델이 한 줄로 바뀌었다. **서명 공개키가 없으면 자동 업데이트 기능 자체가 켜지지 않는다** — `ed57559`.
예전처럼 "키가 없으면 검증을 건너뛰고 경고만" 하지 않는다.

릴리스 전에 끝내야 하는 일:

- [x] **키 생성·서명 도구** — `nl-keygen`(`keygen`/`sign`/`verify`). `minisign` CLI 도 CI 설치 단계도 필요 없다 — `3e01d27`
- [x] **Inno Setup 실컴파일 확인** — `436bec4`
- [ ] **서명 키 발급** — 위 도구로 키 쌍을 만들고 `MINISIGN_KEY` 시크릿·`UPDATE_PUBLIC_KEY` 변수를 등록한다
- [ ] **공개키를 코드에 박기** — 빌더는 `crates/nl-app/src/update_key.rs` 의 `PUBLIC_KEY`(지금 `None` → 업데이트 꺼짐),
      배포 앱은 빌더 UI 가 번들 매니페스트에 채워 넣는다
- [ ] **실제 배포 서버** — https 로만 서빙하고 `latest.json` 옆에 `.minisig` 를 같이 올린다.
      자산은 매니페스트와 같은 오리진에 둔다
- [ ] **Authenticode 서명** — Windows 설치본에 붙인다. 지금 신뢰의 뿌리는 서명된 매니페스트의 sha256 하나뿐이다

## M1 — 데이터·페이로드·파이프라인 · **거의 완료**

- [x] 화면 캡처 (KDE/GNOME Wayland): `org.freedesktop.portal.Screenshot`(zbus, 순수 Rust) — `1d0917e`
- [x] WebSocket 소스/싱크 — URL 별 연결 풀·팬아웃·지수 백오프·`wss` — `ae762f1`
- [x] 인바운드 HTTP 서버 소스 / 응답 싱크 (배포 앱을 API 로) — `373e4bd`
- [x] 녹화기 `Recorder` → `Recorded` 데이터셋 — `c92e277`
- [x] 데이터셋 가져오기·미리보기: 합성·CSV·이미지 폴더·녹화 — `824e155`, `48e5a7d`
- [x] 페이로드 편집기(필드·Transform 체인)와 `codec` 실행 — `00d7fb6`
- [x] 파이프라인 캔버스와 시험 실행 + 킬 스위치 — `00d7fb6`
- [x] 엔진: 다입출력 학습, LR 스케줄(Step/Cosine/Plateau)·워밍업·조기 종료, 상주 배치 — `c48bc78`
- [x] 배포 앱 입력 무장 opt-in, `Runner` 서버 선기동(모델 로딩 중 503) — `f66309f`
- [x] 자동 저장·복구 — `84e9085`
- [ ] `ScreenCast` + pipewire (고 fps). **미착수** — 빌드 머신에 `pipewire-devel` 이 필요해
      기본 꺼진 선택 기능이어야 한다. 지금 포털 경로로 2.8 fps 는 나온다

## M2 — GUI 디자이너·Windows 배포·도구 설치 · **완료 (배포 운영만 남음)**

- [x] GUI 디자이너(위젯 팔레트·배치·바인딩) + 런타임 공용 렌더러 — `00d7fb6`
- [x] `Binding::ModelOutput` 배선(빌더 편집기 + 런타임 반영) — `99aaef5`, `695689b`
- [x] Windows 빌드: zip + Inno Setup 설치 프로그램, 런타임 크로스 빌드 — `b2a6de7`, `8b15a2b`
- [x] 자동 업데이트 + 매니페스트 서명(minisign) — `af588da`, `ed57559`
- [x] 도구 설치 관리자: 상태 점검 → 동의 모달 → 설치 → 재점검 — `b2a6de7`, `a150a54`
- 남은 것은 코드가 아니라 운영이다 — 위 "보안" 절의 키 발급·배포 서버·Authenticode 셋.

## M3 — 모델 관리·고급 레이어·상호운용 · **진행 중**

- [x] HTTP 서버 TLS — `Source::HttpServer` 에 인증서를 주면 https 로 연다(rustls + ring, 시스템 OpenSSL 불필요).
      토큰 규칙은 TLS 와 무관하게 그대로다 — `1e87de1`
- [x] `nl tls-cert` 로 자체 서명 인증서 만들기, `nl run|build --tls-cert/--tls-key` 주입,
      배포판 인증서 탐색(작업 폴더 → 실행 파일 폴더) — `3bc25a8`
- [x] 레이어: `Lstm`·`Gru`·`MultiHeadAttention` — `[L, D]` 를 받아 `return_sequence` 에 따라 `[L, H]` 나
      마지막 상태 `[H]` 를 내고, 어텐션은 `[L, D] → [L, D]` 다. burn 의 `nn` 모듈 대신 파라미터 텐서 +
      게이트 수식으로 직접 구현했다 — `f085770`. 빌더 인스펙터 편집기는 `91fecd2`
- [ ] `Binding::ModelOutput` 의 `field` 로 다출력 갈라 보내기 — **엔진 담당 작업 중**.
      지금은 같은 모델을 가리키는 위젯이 모두 같은 값을 받는다. 런타임 배선은 이미 있다 — `695689b`
- [ ] Transformer 블록·Residual 템플릿. **미착수** (`Embedding` 과 `Transform::Tokenize` 는 들어왔다 — `89cee6e`)
- [ ] 모델 레지스트리: 실행 기록 비교(지표 표), 버전 태그, 가중치 내보내기/가져오기. **미착수**
- [~] ONNX **내보내기** — opset 17 로 쓴다. `nl_engine::onnx::export`, `nl export-onnx <프로젝트> --model …`.
      `Lstm`·`Gru`·`MultiHeadAttention` 만 아직 빠져 있고(게이트 순서·레이아웃 변환) 나머지 17종은 된다.
      검증은 `tract-onnx`(dev-dependency)로 왕복 비교 — 학습 → 내보내기 → 다시 읽기 → 1e-4 이내 일치
- [ ] ONNX 내보내기의 순환·어텐션 레이어. **미착수**
- [ ] ONNX **가져오기**(tract 로 추론 전용). **미착수 · 선택 기능으로 둔다** —
      tract 를 넣으면 배포 바이너리가 **+34 MiB(+46%)** 라 기본 꺼진 카고 feature(`onnx-import`)로만 넣는다.
      실측과 매핑 근거는 `docs/research/onnx-2026-09-14.md`
- [ ] 학습 상황 프리셋: 분류·회귀·화면 상태 분류·행동 복제 템플릿. **미착수**

## M4 — 협업·서버형 배포 · **절반**

서버형 배포는 됐고 협업은 아직이다.

- [x] 배포 앱을 상시 서버로 운영하기 — systemd 사용자 유닛 템플릿과 `install.sh --service` (`e7a5a12`),
      고정 작업 폴더 `--work-dir`/`NL_WORK_DIR` (`d45abdf`). 인증서는 `<작업 폴더>/local/` 에 둔다.
      실측: 서비스 기동 → `curl` 추론 200 → 재시작 → `SIGTERM` 정상 종료, https 포함
- [x] Windows 런타임도 같은 경로로 돈다 — wine 에서 헤드리스 기동 후 HTTP 추론 200 확인
- [ ] `--headless` 서빙을 다중 모델·다중 파이프라인으로 넓히기. **미착수**
      (단일 파이프라인 서빙은 M1 에서 됐다 — `373e4bd`)
- [ ] 프로젝트 동기화 서버 (trust-pms `pms-server` 계열, op 경로 재사용). **미착수**
