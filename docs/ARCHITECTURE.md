# Neural Linker 아키텍처

범용 신경망 모델 **설계 → 학습 → 연결 → GUI 제작 → 빌드·배포**를 한 프로그램 안에서 끝내는 데스크톱 도구.
UI 구현 방식·문서 상태(op 기반 undo)·GUI 테스트·패키징은 `../trust-pms` 를 그대로 잇는다.

## 전체 그림

```
┌───────────────────────────────────────────────────────────────────────┐
│  nl-app (빌더 GUI, eframe/egui + glow)                                  │
│  프로젝트 관리 · 모델 그래프 캔버스 · 데이터/페이로드 · 학습 대시보드     │
│  파이프라인 편집 · GUI 디자이너 · 빌드/배포 · 도구 설치(동의 팝업)         │
│                                                                       │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                 │
│  │   nl-core    │  │  nl-engine   │  │    nl-io     │                 │
│  │ 모델·그래프  │  │ burn 인터프리터│  │ 화면 캡처    │                 │
│  │ 페이로드     │  │ CPU/GPU 장치 │  │ 마우스/키보드│                 │
│  │ 파이프라인   │  │ 학습 루프    │  │ HTTP/WS/stdio│                 │
│  │ GUI 레이아웃 │  │ 체크포인트   │  │ 자원 조회    │                 │
│  │ op/undo/검증 │  │ 추론 세션    │  │              │                 │
│  └──────────────┘  └──────────────┘  └──────────────┘                 │
└───────────────────────────────────────────────────────────────────────┘
          │ 빌드 = 런타임 바이너리 + .nlapp 번들(꼬리표 첨부)
          ▼
┌───────────────────────────────────────────────────────────────────────┐
│  nl-runtime (배포판 실행기, eframe/egui)                                │
│  번들 로드 → GUI 레이아웃 렌더 → 파이프라인 실행 → 모델 추론(nl-engine)  │
│  nl-core · nl-engine · nl-io 를 그대로 사용 (빌더와 같은 코드 경로)      │
└───────────────────────────────────────────────────────────────────────┘
```

`nl-core` 는 GUI/네트워크/ML 프레임워크 의존성이 없는 순수 로직 크레이트다. 빌더와 런타임이 같은 모델·같은 op 적용 코드를
공유하므로 "빌더에서 보이는 것 = 배포판에서 도는 것"이 보장된다.

## 크레이트

| 크레이트 | 역할 | 의존 |
|---|---|---|
| `nl-core` | 프로젝트 문서 모델, 레이어 그래프, 형상 추론, 페이로드/데이터셋/파이프라인/GUI 레이아웃 스펙, op·undo·diff, 번들 포맷 | serde 만 |
| `nl-engine` | burn 기반 **런타임 정의 그래프** 인터프리터, 장치 선택(CPU ndarray / GPU wgpu), 학습 루프(스레드 + 이벤트 채널), 체크포인트(safetensors), 추론 세션 | nl-core, burn |
| `nl-io` | 화면 캡처(Linux: x11rb·wlr-screencopy, Windows: xcap), 입력 시뮬레이션(enigo), HTTP 호출(ureq), 자원 조회(sysinfo), **파이프라인 실행기 `Runner`** | nl-core, nl-engine |
| `nl-gui` | 빌더 미리보기와 런타임이 공유하는 egui 요소: `GuiLayout` 렌더러, 한글 폰트, 테마 | nl-core, nl-engine, egui |
| `nl-bundle` | `.nlapp` zip 읽기/쓰기, 런타임 바이너리 첨부, 배포 아카이브(tar.gz/zip) | nl-core, zip |
| `nl-app` | 빌더 GUI | 위 전부 + eframe |
| `nl-runtime` | 배포판 실행기(단일 바이너리 + 번들) | nl-core, nl-engine, nl-io, nl-gui, nl-bundle, eframe |
| `tools/uitest` | 헤드리스 sway GUI 테스트 하네스 (trust-pms 이식) | — |

## nl-core

### 문서 모델 (`model.rs`)
- `Project` = `models`, `datasets`, `payloads`, `pipelines`, `gui`, `runs`, `settings`. 모든 컬렉션은 `BTreeMap<Id, _>`
  (결정적 순회 → 안정적 레이아웃/테스트/diff).
- `ModelDef { graph: Graph, train: TrainConfig }`, `Graph { nodes: BTreeMap<NodeId, Node>, edges: BTreeMap<EdgeId, Edge> }`.
  `Node { kind: LayerKind, pos, name }`, `Edge { from: Port, to: Port }` (`Port { node, slot }`). 다입력 레이어(Add/Concat)는
  `slot` 로 구분한다.
- `LayerKind` 는 데이터 열거형이다(`Linear { out_features, bias }`, `Conv2d {..}`, `Activation(Act)`, `Dropout`, `BatchNorm`,
  `LayerNorm`, `Flatten`, `Reshape`, `MaxPool2d`, `AvgPool2d`, `Add`, `Concat`, `Embedding`, `Input`, `Output`, …).
  각 종류의 입력 슬롯 수·파라미터 유무·표시 색은 `LayerKind::spec()` 이 단일 소유한다. 엔진은 `LayerKind` 를 해석해
  텐서 연산으로 바꾸며, **새 레이어 추가 = core 의 enum 변형 + spec + shape 규칙 + engine 의 연산 하나**다.
- `add_edge` 는 자기참조/중복/슬롯 점유만 거부하고 **순환은 허용** — 검증기(`validate`)가 감지해 오류 목록으로 보고하고,
  엔진은 검증 통과 그래프만 받는다(trust-pms 의 "문서를 깨뜨리지 않는다" 원칙).
- 모든 새 필드는 `#[serde(default)]` — 옛 문서 그대로 로드. `FORMAT_VERSION` 상수와 `ProjectFile::from_json` 마이그레이션.

### 형상 추론 (`shape.rs`)
- `infer(graph, batch: Option<usize>) -> ShapeReport { shapes: BTreeMap<NodeId, Shape>, errors: Vec<GraphError> }`.
  위상정렬(Kahn) → 순환 노드는 `GraphError::Cycle` 로 격리, 슬롯 누락은 `MissingInput`, 규칙 위반은 `ShapeMismatch{expected, got}`.
- `Shape = Vec<Dim>`, `Dim::{Batch, Fixed(usize)}`. 배치 차원은 기호로 두어 배치 크기와 무관하게 추론한다.
- 캔버스는 포트 옆에 형상을 표시하고 오류 노드를 붉게 칠한다. 엔진은 `ShapeReport` 로 파라미터를 초기화한다.

### 편집 op · undo (`ops.rs`)
- trust-pms 와 같은 구조: `Op::{UpsertNode, DeleteNode, UpsertEdge, DeleteEdge, SetModelMeta, UpsertDataset, UpsertPayload,
  UpsertPipelineNode, …, SetGui}`. `apply_op` 은 항상 성공(멱등·수렴 지향), `inverse_ops(project, &ops)` 가 역연산을 만들고,
  `diff_ops(from, to)` 가 두 스냅샷의 차이를 op 목록으로 돌려준다.
- 앱의 `DocState` 는 `HistoryEntry { forward, inverse }` 스택(최대 100) + 인스펙터 타이핑 burst(1초 무입력까지 한 항목).
  협업 서버는 현 단계에서 없지만 op 경로를 유지해 두어 나중에 그대로 붙일 수 있다.

### 페이로드 (`payload.rs`)
"페이로드"는 **바깥 세계의 데이터 ↔ 모델 텐서** 사이의 계약이다.
- `PayloadSpec { inputs: Vec<Field>, outputs: Vec<Field> }`, `Field { name, kind: FieldKind, encode: Vec<Transform>, decode: Vec<Transform> }`.
- `FieldKind::{Tensor{shape, dtype}, Image{w, h, channels}, Scalar, Vector(n), Text, Json{schema}, ClassLabel{labels}}`.
- `Transform::{Resize{w,h}, Grayscale, Normalize{mean,std}, Scale{min,max}, OneHot(n), Argmax, Threshold(t), Softmax,
  MapLabel(labels), Crop{x,y,w,h}, Tokenize{vocab}}` — 순수 데이터 정의이며 실제 실행은 `nl-engine::codec` 이 맡는다.
- 데이터셋(`DatasetSpec`)과 파이프라인의 소스/싱크는 모두 페이로드 필드에 바인딩된다. 즉 "화면 캡처 → Image 필드 →
  Resize/Normalize → 모델 입력" 과 "모델 출력 → Argmax → MapLabel → 마우스 클릭 액션" 이 같은 언어로 적힌다.

### 데이터셋 (`dataset.rs`)
- `DatasetSpec { source: DataSource, payload: PayloadId, split: Split, shuffle, seed }`.
- `DataSource::{Csv{path, input_cols, target_cols, header}, ImageFolder{path, class_from_dir}, Npy{inputs, targets},
  Synthetic{kind, n}, Recorded{dir}}`. `Recorded` 는 빌더의 "녹화" 기능(화면 캡처 + 입력 이벤트 라벨)이 만든 폴더.

### 파이프라인 (`pipeline.rs`)
- `Pipeline { nodes: BTreeMap<PNodeId, PNode>, links: BTreeMap<LinkId, Link>, tick_hz }`.
- `PNodeKind::Source(Source)` / `Model { model, payload }` / `Sink(Sink)` / `Logic(Logic)`.
  - `Source::{ScreenCapture{region, fps}, HttpPoll{url, interval_ms, headers}, WebSocket{url}, StdinJson, File{path},
    Timer{ms}, GuiEvent{widget}, Manual}`
  - `Sink::{MouseKeyboard{actions}, HttpCall{method, url, body_template, headers}, WebSocketSend{url}, StdoutJson,
    GuiWidget{widget}, File{path}, Log}`
  - `Logic::{Threshold, Debounce{ms}, Select{index}, Map{table}, Script(예약)}`
- 실행기는 `nl-io::runner::Runner` (틱 루프 + 스레드; IO 어댑터와 `nl-engine::Session` 을 둘 다 보는 크레이트라 여기 둔다).
  빌더의 "시험 실행"과 런타임이 같은 Runner 를 쓴다.

### GUI 레이아웃 (`gui.rs`)
- `GuiLayout { window: WindowSpec, widgets: BTreeMap<WidgetId, Widget> }`,
  `Widget { kind: WidgetKind, rect, binding: Option<Binding>, style }`.
- `WidgetKind::{Label, Button, Toggle, Slider{min,max}, TextInput, Image, Plot, Table, Group}`.
- `Binding::{PipelineInput(PNodeId), PipelineOutput(PNodeId), ModelOutputField{model, field}, Action(ActionId)}`.
- 렌더러 `nl_gui::render_layout(ui, layout, state, RenderMode::{Run, Design})` 하나를 빌더 디자이너와 런타임이 공유한다.

### 번들 (`bundle.rs`)
- `.nlapp` = zip: `manifest.json`(이름·버전·대상 모델·진입 파이프라인·GUI), `project.json`, `weights/<model>.safetensors`, `assets/`.
- 배포 바이너리 = 런타임 실행 파일 + 번들 바이트 + 꼬리표 `NLAPP1` + u64 길이. 런타임은 자기 실행 파일 끝을 읽어 번들을 꺼낸다
  → **사용자 PC 에 Rust 툴체인이 필요 없다.** 런타임 바이너리는 빌더 옆 `runtimes/<target>/` 에 두거나 매니페스트로 내려받는다.

## nl-engine

### 장치 (`device.rs`)
- `DevicePref::{Auto, Cpu, Gpu(index)}` (core) → `Device::{Cpu(NdArrayDevice), Gpu(WgpuDevice)}`.
  `enumerate()` 가 wgpu 어댑터 목록(이름·백엔드·VRAM)과 CPU 정보를 돌려준다. Auto = 첫 이산 GPU → 통합 GPU → CPU.
- 백엔드 타입: `type Cpu = Autodiff<NdArray<f32>>`, `type Gpu = Autodiff<Wgpu<f32, i32>>`. 제네릭 코드는 `B: AutodiffBackend` 로
  한 번만 쓰고 `dispatch!(device, |B| …)` 매크로가 둘 중 하나로 단형화한다.

### 인터프리터 (`exec.rs`)
- burn 텐서는 랭크가 const generic 이라 `DynTensor<B>` = `enum { R1(Tensor<B,1>), R2, R3, R4, R5 }` 로 감싸고
  `LayerKind` 별 연산을 `apply(kind, inputs: &[DynTensor], params: &mut Params) -> DynTensor` 로 구현한다.
- `Params { tensors: BTreeMap<(NodeId, &'static str), Tensor<B, D>> }` — 초기화는 `ShapeReport` 와 kaiming/xavier.
  학습 시 `require_grad()`, 옵티마이저(SGD/Adam/AdamW)는 `Gradients` 에서 텐서별 grad 를 꺼내 직접 갱신한다
  (burn 의 `Module` 파생에 묶이지 않아 런타임 정의 그래프가 가능).
- 실행 순서는 core 의 위상정렬 결과를 그대로 쓴다. Dropout/BatchNorm 은 `train: bool` 로 분기.

### 학습 (`train.rs`)
- `Trainer::spawn(model, dataset, config, device) -> (JoinHandle, Receiver<TrainEvent>, Control)`.
  `TrainEvent::{Started{device}, Step{epoch, step, loss}, Epoch{epoch, train_loss, val_loss, val_metric}, Checkpoint(path),
  Finished, Failed(String)}`, `Control::{pause, resume, stop}`.
- 데이터 로더는 `DatasetSpec` → `Batch { inputs: Vec<DynTensor>, targets }`; CSV/이미지 폴더/합성 3종을 먼저 지원.
- 체크포인트 = safetensors(파라미터 이름 `"{node_id}.{name}"`) + `run.json`(설정·지표). `RunRecord` 가 프로젝트에 남는다.

### 추론 (`infer.rs`)
- `Session::load(model, weights, device)`; `run(&[DynTensor]) -> Vec<DynTensor>`. `codec.rs` 가 페이로드 Transform 을 적용해
  이미지/CSV/JSON ↔ 텐서를 오간다.

## nl-io
- `screen::capture(region) -> Frame`. Linux 는 시스템 개발 라이브러리 없이 빌드되도록 순수 Rust 경로만 쓴다: X11(x11rb `GetImage`) →
  wlroots(wlr-screencopy, `libwayshot` 기본 feature 끔). GNOME/KDE Wayland 는 M1 에서 xdg-desktop-portal(ashpd). Windows/macOS 는 xcap.
  (xcap 의 Linux 백엔드는 libpipewire/EGL 개발 패키지를 요구해 제외했다.)
- `input::{move_to, click, key, type_text}` (enigo). Wayland 에서는 `libei`/`xdo` 폴백. **안전장치**: 시뮬레이션은 사용자가
  파이프라인 실행을 켠 동안만, 그리고 `Esc` 를 길게 누르면 즉시 중단(킬 스위치).
- `http::{call(method, url, headers, body) -> Response}` (ureq, 타임아웃 필수 — trust-pms 교훈).
- `stream::{ws_connect, stdin_lines, stdout_json}`.
- `resources::snapshot()` — CPU/메모리(sysinfo) + GPU 목록(`nl_engine::enumerate()` 가 진실). 앱 상태바·자원 패널.
- `runner::Runner` — 파이프라인 틱 루프. `RunnerHandle { events, inputs, stop }`. GUI 위젯 이벤트는 `RunnerInput` 으로 들어가고
  `Sink::GuiWidget` 값은 `RunnerEvent::Widget` 으로 나온다.

## nl-app

### 화면 구성 (trust-pms 의 도킹 구조 계승)
- 상단 툴바(프로젝트·저장·되돌리기·장치 선택·실행/학습) → 뷰 바(`Ctrl+1`…) → 좌 아웃라인(프로젝트 트리: 모델/데이터/페이로드/
  파이프라인/GUI/실행 기록) · 중앙 뷰 · 우 인스펙터 · 하단 도크(문제/로그/자원) · 상태바(장치·VRAM·표시 노드 수).
- 뷰: **모델**(레이어 그래프 캔버스) · **데이터**(데이터셋·페이로드 편집, 미리보기) · **학습**(설정 + 손실/지표 플롯 + 실행 기록) ·
  **파이프라인**(소스/모델/싱크 노드 캔버스) · **GUI**(디자이너: 위젯 팔레트 + 캔버스 + 바인딩) · **빌드**(대상·산출물·도구 상태).
- 뷰는 문서를 직접 바꾸지 않고 `ViewAction` 을 돌려주며 앱이 `DocState` 로 적용한다.

### 그래프 캔버스 (`canvas.rs`)
- trust-pms `canvas.rs` 의 구조를 잇는다: `Camera { pan, zoom }`, 월드 좌표, 커서 기준 줌, 누른 지점의 `Zone`(포트 > 바디)이
  드래그 종류를 정한다. 바디 = 이동(놓으면 `CanvasAction::MoveNodes`), 출력 포트 → 입력 포트 드롭 = 연결(순환·형상 오류는
  빨간 고스트 + 차단), 빈 곳 우클릭 = 레이어 팔레트, `Alt`+드래그 러버밴드, 다중 선택·복제·`Del`·`Ctrl+Z`.
- 노드는 종류별 색(`LayerKind::spec().color`), 포트 옆 형상 라벨, 오류 노드 붉은 테두리 + 툴팁.
- 파이프라인 뷰와 GUI 디자이너는 같은 캔버스 기반(제네릭 `GraphCanvas<N>` 은 M1 에서 추출)을 쓴다.

### 도구 설치 (`tools.rs`)
- 빌드/배포에 필요한 외부 요소: 대상별 런타임 바이너리, Windows 설치 프로그램 제작기(Inno Setup, Windows 호스트 또는 wine),
  선택적 GPU 런타임. `ToolManager::check()` 가 상태를 보고하고, 없는 것은 **동의 모달**("무엇을 어디서 받아 어디에 놓는지"
  명시) → 승인 시 백그라운드 다운로드(sha256 검증) → 진행률 → 재검사. 거부하면 그 도구가 필요 없는 산출물(zip/tar)로 대체.

### 빌드 (`build.rs`)
- `BuildSpec { app_name, version, targets, entry_pipeline, gui, models, output_dir }` → `nl_bundle::Bundle::to_zip` → 대상별 런타임에 `nl_bundle::attach` →
  Linux: `<name>-<ver>-linux-x86_64.tar.gz`(+ install.sh, .desktop, 아이콘), Windows: `<name>-<ver>-windows-x86_64.zip`
  (+ Inno Setup 이 있으면 `setup.exe`). 산출물 sha256 매니페스트(`latest.json`, trust-pms 형식) 함께 생성.

## nl-runtime
- `nl-runtime` 단독 실행 시 인자로 `.nlapp` 를 받거나, 자기 실행 파일 꼬리표에서 번들을 읽는다.
- 창 제목/크기 = `GuiLayout.window`, 위젯 = `gui_render`, 파이프라인 = `Runner`. 장치는 번들 기본값 + 설정 창에서 변경.
- `--headless` 로 GUI 없이 파이프라인만(서버형 배포).

## 테스트
- `cargo test --workspace`: core 단위 테스트(형상 추론·op/undo/diff 왕복·직렬화), engine(작은 MLP 가 XOR/선형 회귀를 CPU 에서
  수렴, GPU 는 `NL_TEST_GPU=1` 일 때), app 의 `egui_kittest` 헤드리스 렌더 테스트(모든 뷰가 패닉 없이 그려짐).
- `tools/uitest/uitest.sh`: 헤드리스 sway 안에서 실제 클릭·드래그·캡처(trust-pms 와 동일, app_id `neural-linker`).

## 플랫폼 주의사항 (trust-pms 에서 계승)
- glow(OpenGL) 백엔드 명시, Wayland `vsync: false`, 한글 폰트 시스템 폴백(`font_definitions`).
- 모든 파일 쓰기는 원자적(임시 파일 + rename). 자동 저장·복구는 M1.
- GPU 계산(wgpu)과 GUI 렌더(glow)는 서로 다른 컨텍스트다 — 학습은 항상 별도 스레드, UI 스레드는 이벤트만 받는다.
