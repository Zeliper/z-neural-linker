# 로드맵

2026-09-10 시작. 설계는 `docs/ARCHITECTURE.md`. 각 마일스톤은 "빌더에서 만들고 배포판에서 도는" 수직 조각을 하나씩 늘린다.

## M0 — 뼈대와 첫 수직 조각 (2026-09-10~11, 진행 중)
- [x] 워크스페이스·문서·`nl-core` 데이터 모델 계약
- [x] `nl-core`: 그래프 op/undo/diff, 형상 추론, 검증, 직렬화 왕복 테스트 (22)
- [x] `nl-engine`: burn 0.21 인터프리터(LayerKind 17종), CPU(ndarray)/GPU(wgpu) + `probe` 기반 Auto, 학습 루프(SGD/Adam/AdamW ×
      MSE/CrossEntropy/BCE/MAE), safetensors 체크포인트, 추론 세션, 데이터 로더(합성/CSV/이미지 폴더/녹화), codec (34)
- [x] `nl-io`: 자원 조회, 화면 캡처(X11·wlroots), 입력 시뮬레이션, HTTP, 파이프라인 Runner (44)
- [x] `nl-gui` 렌더러 · `nl-bundle` 번들/첨부/아카이브 · `nl-runtime` 실행기(`--headless --run-for`) (37)
- [x] `nl-app` 1차: 프로젝트 관리, 모델 캔버스, 인스펙터, 데이터/학습/자원 뷰, 샘플 (65)
- [x] `nl-app` 2차: 파이프라인 뷰(시험 실행·입력 무장·킬 스위치), GUI 디자이너(바인딩·미리보기), 빌드 뷰(도구 상태·동의 모달·tar.gz/zip·latest.json)
- [x] `nl-app` 3·4차: HttpServer/HttpReply 노드, 녹화 UI, Inno Setup 동의 설치, 빌더 자체 업데이트, 시작 지연 수정(12.2s→1.6s)·`[nl-app] ready/focused` 마커
- [x] `nl-cli`: 헤드리스 inspect/devices/train/infer/run/record/build/sample — 실측 sample→train(98%)→build→배포판 HTTP /infer 응답 일치
- [x] Windows: cargo-xwin 크로스 빌드(nl-runtime.exe 53.7MB, nl-app.exe 61MB), AttachConsole, .iss 생성(Inno 실컴파일은 네트워크 차단으로 미확인)
- [x] `nl-update` + 배포 런타임 자동 업데이트(배지/적용) + minisign 검증
- [x] uitest 시나리오 러너(run/wait-log/expect-shot, PPM 골든) · egui_kittest 스냅샷 골든(nl-gui/nl-runtime)
- [x] `tools/uitest` 이식(app_id `neural-linker`), egui_kittest 헤드리스 렌더 테스트
- [x] 패키징(`packaging/linux`, `packaging/windows`) 이식

### 환경에서 확인된 사실 (2026-09-10)
- 이 개발 PC 의 RTX 2060 은 오픈소스 NVK 드라이버라 wgpu 컴퓨트가 죽는다(`Parent device is lost`). Auto 는 probe 로 걸러 Intel UHD 630 을
  고른다. NVIDIA 공식 드라이버를 깔면 RTX 로 잡힌다.
- 이 PC 의 실제 세션은 KDE Plasma 6 Wayland → wlr-screencopy·X11 GetImage 둘 다 불가. **xdg-desktop-portal 경로가 M1 최우선**(아래).

## 보안

2026-09-14 에 네트워크·번들·업데이트 경로를 읽기 전용으로 리뷰했다(높음 11 · 중간 22 · 낮음 25).
같은 날 엔진 정확성 리뷰도 따로 했다(버그 7 · 의심 9).

- [`docs/reviews/security-2026-09-14.md`](reviews/security-2026-09-14.md) — 맨 위 "처리 현황" 표가 항목별 상태·담당 크레이트·근거·남은 한계
- [`docs/reviews/engine-2026-09-14.md`](reviews/engine-2026-09-14.md) — 같은 자리에 현황표(아직 전부 열려 있다)

높음 11건 중 9건, 전체 58건 중 37건을 고쳤다. 남은 것은 nl-engine(M15~M19·L21)과
nl-app(H5·H9·M21·M22·L1~L3·L22) 담당이고, 낮음 6건은 영향이 낮아 미룬 것이다.

신뢰 모델이 한 줄로 바뀌었다. **서명 공개키가 없으면 자동 업데이트 기능 자체가 켜지지 않는다.**
예전처럼 "키가 없으면 검증을 건너뛰고 경고만" 하지 않는다.

릴리스 전에 끝내야 하는 일:

- [ ] **서명 키 발급** — minisign 키 쌍을 만들고 비밀키를 보관할 곳을 정한다.
      절차는 `packaging/README.md` 의 "서명" 절에 있다
- [ ] **공개키를 코드에 박기** — 빌더는 `crates/nl-app/src/update_key.rs` 의 `PUBLIC_KEY`(지금 `None` → 업데이트 꺼짐),
      배포 앱은 빌더 UI 가 번들 매니페스트의 `update_public_key` 에 채워 넣는다
- [ ] **실제 배포 서버** — https 로만 서빙하고 `latest.json` 옆에 `latest.json.minisig` 를 같이 올린다.
      자산은 매니페스트와 같은 오리진에 둔다(아니면 `allowed_asset_hosts` 에 적는다)
- [ ] **Inno Setup 실컴파일 확인** — 설치본 해시는 고정했으나 설치·컴파일 경로는 아직 확인하지 못했다
- [ ] **Authenticode 서명** — Windows 설치본에 붙인다. 지금 신뢰의 뿌리는 서명된 매니페스트의 sha256 하나뿐이다(M10)

## M1 — 데이터·페이로드·파이프라인
- [x] **화면 캡처 (KDE/GNOME Wayland)**: `org.freedesktop.portal.Screenshot`(zbus, 순수 Rust) — 이 PC 실측 2.8fps
- [ ] `ScreenCast` + pipewire(고 fps; 빌드 머신에 `pipewire-devel` 필요 → optional feature `pipewire`)
- [x] WebSocket 소스/싱크, 인바운드 HTTP 서버/응답 노드(배포 앱을 API 로), 녹화기(`Recorder` → `Recorded` 데이터셋)
- [x] 엔진: 다입출력 학습, LR 스케줄(Step/Cosine/Plateau)·워밍업·조기 종료, 상주 배치(detach 버그 수정)
- [x] 배포 앱 입력 무장 opt-in(`BundleManifest.arm_input`), Runner 서버 선기동(모델 로딩 중 503), 샘플 통일(nl-core 로 이동)
- 데이터셋: CSV, 이미지 폴더, 녹화(화면 + 입력 라벨) 가져오기와 미리보기
- 페이로드 편집기(필드·Transform 체인), 인코더/디코더 실행(`codec`)
- 파이프라인 캔버스: 화면 캡처 → 모델 → 마우스/키보드, HTTP 폴링 → 모델 → HTTP 호출, stdio JSON 연결
- 파이프라인 시험 실행(빌더 내부) + 킬 스위치
- 자동 저장·복구(trust-pms `recovery.rs` 이식)

## M2 — GUI 디자이너·Windows 배포·도구 설치
- GUI 디자이너(위젯 팔레트·드래그 배치·바인딩) + 런타임 공용 렌더러
- Windows 빌드: 런타임 바이너리 매니페스트 내려받기(동의 팝업), zip + Inno Setup(가능할 때)
- 자동 업데이트(trust-pms `update.rs` 이식), 매니페스트 서명(minisign)
- 도구 설치 관리자: 상태 점검 → 동의 → 설치 → 재점검

## M3 — 모델 관리·고급 레이어·상호운용
- 모델 레지스트리: 실행 기록 비교(지표 표), 버전 태그, 가중치 내보내기/가져오기
- 레이어: Embedding, LSTM/GRU, MultiHeadAttention, Transformer 블록, Residual 템플릿
- ONNX 가져오기(tract 로 추론 전용) / 내보내기(검토)
- 학습 상황 프리셋: 분류·회귀·화면 상태 분류·행동 복제(입력 라벨) 템플릿

## M4 — 협업·서버형 배포
- `--headless` 런타임 + HTTP/WS 서빙, 프로젝트 동기화 서버(trust-pms `pms-server` 계열, op 경로 재사용)
