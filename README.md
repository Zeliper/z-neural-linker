# Neural Linker

범용 **신경망 모델 빌더 & 실행기**. 레이어 그래프를 캔버스에서 설계하고, CPU/GPU 에서 학습하고, 화면 캡처·마우스/키보드·
API·외부 앱 파이프라인에 연결한 뒤, 프로그램 안에서 GUI 를 디자인해 Linux / Windows 배포판으로 빌드·공유한다.

파이썬도 명령줄도 필요 없다. 만들어진 앱은 **혼자 도는 단일 실행 파일**이라 사용자 PC 에 Rust 도 런타임도 필요 없다.

![모델 캔버스](docs/img/01-model-canvas.png)

**[사용자 가이드](docs/GUIDE.md)** — 설치부터 배포까지 화면과 함께.

## 무엇을 할 수 있나

- **모델** — 우클릭 팔레트에서 레이어 17종을 놓고 포트를 끌어 잇는다. 형상은 실시간으로 추론되어 노드에
  `[B, 16]` 으로 붙고, 순환·형상 오류는 빨간 테두리와 문제 목록으로 알려 준다.
- **데이터** — 합성 4종(XOR·두 나선·선형 회귀·사분면 이미지), CSV, 이미지 폴더, **화면 녹화**.
  녹화 중 숫자키 0~9 로 라벨을 바꾼다.
- **학습** — SGD/Adam/AdamW, CrossEntropy/MSE/BCE/MAE, 손실 곡선과 실행 기록. 체크포인트는 safetensors.
  장치는 자동 선택하되 **실제로 한 번 돌려 보고** 드라이버가 깨진 GPU 는 건너뛴다.
- **파이프라인** — 소스 9종(타이머·화면 캡처·HTTP 폴링/서버·WebSocket·stdin·파일·GUI 이벤트·수동),
  로직 5종, 싱크 8종. `HTTP 서버` + `HTTP 응답` 을 짝지으면 파이프라인이 곧 추론 API 가 된다.
  마우스/키보드 싱크는 **빌더에서도 배포 앱에서도 기본 비무장**이라 켜기 전에는 로그만 남긴다.
  빌더는 **Esc 0.5초 길게 누르면 즉시 정지**한다.
- **GUI 디자이너** — 위젯 9종을 배치하고 파이프라인 노드에 묶는다. 미리보기는 런타임과 같은 렌더러로 돈다.
- **빌드** — 런타임에 번들을 붙여 Linux tar.gz / Windows zip·설치 프로그램을 만든다. 없는 도구는
  "무엇을 어디서 받아 어디에 놓는지" 를 먼저 보여 주고 승인을 받는다. 배포 앱이 마우스·키보드를
  조작하려면 빌드할 때 **입력 무장을 명시적으로 켜야** 한다 — 켜면 앱 상단에 `⚠ 입력 무장` 배지가 붙는다.
- **자동 업데이트** — 빌더 자신과 만들어진 앱 모두. 매니페스트는 minisign 으로 서명할 수 있다.

GPU 는 wgpu(Vulkan/DX12/Metal) 로 쓰므로 CUDA 설치가 필요 없다. NVIDIA/AMD/Intel 모두 드라이버만 있으면 된다.

## 빌드 & 실행

```bash
cargo run --release -p nl-app                     # 빌더
cargo run --release -p nl-runtime -- app.nlapp    # 배포 번들 실행기
cargo run --release -p nl-cli -- inspect p.nlproj # 명령줄 도구 (`nl`)
```

### 명령줄 `nl`

GUI 없이 프로젝트를 다룬다. 종료 코드는 `0` 성공, `1` 문제를 찾음, `2` 인자/환경 문제.

```bash
nl inspect p.nlproj                         # 내용과 검증 결과
nl devices --probe                          # 장치 목록 (실제로 돌려 보기)
nl train p.nlproj --model "XOR MLP" --epochs 20
nl infer p.nlproj --model XOR --input '[0,1]'
nl run p.nlproj --for 10                    # 파이프라인을 헤드리스로
nl record out/ --fps 4 --for 30             # 화면을 찍어 학습용 폴더로
nl build p.nlproj --target all --out dist   # 배포판 만들기
nl build p.nlproj --arm-input               # 배포 앱의 마우스·키보드 조작 허용 (기본 금지)
nl sample new.nlproj                        # XOR 샘플 프로젝트
```

### 테스트

```bash
cargo test --workspace                      # 단위 · 헤드리스 렌더 · 통합
NL_SNAPSHOT_REQUIRED=1 cargo test --workspace   # CI: 렌더 백엔드가 없으면 실패시킨다
UPDATE_SNAPSHOTS=1 cargo test -p nl-gui     # 골든 이미지 갱신
```

`egui_kittest` 스냅샷은 **컴포지터 없이** wgpu 로 오프스크린 렌더해 `crates/*/tests/snapshots/` 의 PNG 와
견준다. GPU 가 없어도 소프트웨어 래스터라이저(lavapipe)만 있으면 돈다.

### 격리 GUI 테스트 (`tools/uitest`)

실제 데스크톱 세션의 키보드·마우스·화면을 **전혀 건드리지 않고** 헤드리스 sway 안에서 앱을 띄워
클릭·키 입력·캡처를 한다.

```bash
U=tools/uitest/uitest.sh
$U start                                    # 헤드리스 sway + 가상 입력 장치
UITEST_FRESH=1 $U app demo.nlproj           # 앱 실행
$U click 161 45 ; $U key -M ctrl -k s -m ctrl ; $U shot /tmp/a.png
$U run tools/uitest/scenarios/smoke.uit     # 시나리오 + 골든 이미지 비교
$U stop
```

## 워크스페이스

| 크레이트 | 역할 |
|---|---|
| `nl-core` | 프로젝트·그래프·페이로드·파이프라인·GUI 스펙, op/undo, 형상 추론, 번들 포맷 (GUI/ML 무의존) |
| `nl-engine` | burn 인터프리터, CPU/GPU 장치 선택·검증, 학습, 체크포인트, 추론 |
| `nl-io` | 화면 캡처, 입력 시뮬레이션, HTTP/WS/stdio, 녹화, 자원 조회, 파이프라인 실행기 |
| `nl-gui` | 빌더 미리보기와 런타임이 공유하는 egui 요소: `GuiLayout` 렌더러·폰트·테마 |
| `nl-bundle` | `.nlapp` 번들, 런타임 첨부, 배포 아카이브, Windows 설치 프로그램, 아이콘 변환, 도구 설치 계획 |
| `nl-update` | 자동 업데이트 코어 — 매니페스트·minisign 서명·다운로드·적용 (GUI 무의존) |
| `nl-app` | 빌더 GUI (eframe/egui, glow) |
| `nl-runtime` | 배포판 실행기 (단일 바이너리 + 첨부 번들) |
| `nl-cli` | `nl` 명령줄 도구 |
| `tools/uitest` | 헤드리스 sway 테스트 하네스 + 시나리오 러너 |

## 문서

- **[사용자 가이드](docs/GUIDE.md)** — 설치·첫 실행·각 뷰·배포·명령줄·문제 해결
- [아키텍처](docs/ARCHITECTURE.md) — 크레이트 구조, 설계 결정, CI·릴리스
- [로드맵](docs/ROADMAP.md)
- [배포와 설치](packaging/README.md) — 패키징, Windows 크로스 빌드, 업데이트 매니페스트·서명

UI 구조·문서 상태(op 기반 undo)·GUI 테스트 하네스·패키징 방식은 [trust-pms](../trust-pms) 를 잇는다.
