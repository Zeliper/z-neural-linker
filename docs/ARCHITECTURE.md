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

의존 방향은 한쪽이다. `nl-core → nl-engine → nl-io → nl-gui` 순으로 쌓이고, `nl-bundle → nl-update` 는 있지만
그 반대는 없다 — `nl-update` 는 배포 앱에도 들어가므로 번들 포맷을 알 필요가 없고, 그래야 순환이 생기지 않는다.
`nl-app`·`nl-runtime`·`nl-cli` 는 잎이라 서로를 모른다.

## 크레이트

| 크레이트 | 역할 | 의존 |
|---|---|---|
| `nl-core` | 프로젝트 문서 모델, 레이어 그래프, 형상 추론, 페이로드/데이터셋/파이프라인/GUI 레이아웃 스펙, op·undo·diff, 번들 포맷 | serde 만 |
| `nl-engine` | burn 기반 **런타임 정의 그래프** 인터프리터, 장치 선택(CPU ndarray / GPU wgpu), 학습 루프(스레드 + 이벤트 채널), 체크포인트(safetensors), 추론 세션 | nl-core, burn |
| [`nl-io`](../crates/nl-io/README.md) | 화면 캡처(Linux: libwayshot·xdg-desktop-portal·x11rb, 그 밖: xcap), 입력 시뮬레이션(enigo), HTTP 호출·**자체 인바운드 HTTP 서버(`httpd`)**, WebSocket(tungstenite+rustls), 화면 녹화기(`Recorder`), 자원 조회(sysinfo), **파이프라인 실행기 `Runner`** | nl-core, nl-engine |
| `nl-gui` | 빌더 미리보기와 런타임이 **같은 픽셀을 그리도록** 공유하는 egui 요소: `GuiLayout` 렌더러(`render_layout`), 위젯 상태(`GuiState`)·이벤트(`GuiEvent`), 편집/실행 두 모드(`RenderMode`), 한글 폰트·테마 | nl-core, nl-engine, egui |
| `nl-bundle` | `.nlapp` zip 읽기/쓰기, 런타임 바이너리 첨부, 배포 아카이브(tar.gz/zip), Windows 설치 프로그램(Inno Setup) 생성, 아이콘 변환(PNG→ICO), 업데이트 매니페스트 생성, 외부 도구 설치·크로스 빌드 계획 | nl-core, nl-update, zip, image |
| `nl-update` | 자동 업데이트 코어: 매니페스트 확인(https 강제·서명 필수·신선도·동일 오리진), 스트리밍 다운로드 + sha256, 적용 직전 재검증 후 교체 — **GUI 무의존**. 서명 키 도구는 예제 대상 `nl-keygen` | serde, semver, ureq, minisign-verify |
| `nl-app` | 빌더 GUI | 위 전부 + eframe |
| `nl-runtime` | 배포판 실행기(단일 바이너리 + 번들) | nl-core, nl-engine, nl-io, nl-gui, nl-bundle, nl-update, eframe |
| [`nl-cli`](../crates/nl-cli/README.md) | `nl` 명령줄 도구 — inspect·devices·train·infer·run·record·build·tls-cert·sample. 스크립트·CI 용이라 종료 코드가 계약이다. **실행 파일 이름은 `nl`** (크레이트 이름과 다르다) | 위 전부 (GUI 제외) |
| `tools/uitest` | 헤드리스 sway GUI 테스트 하네스 + 시나리오 러너·골든 이미지 비교 (trust-pms 이식) | — |

## nl-core

### 문서 모델 (`model.rs`)
- `Project` = `models`, `datasets`, `payloads`, `pipelines`, `gui`, `runs`, `settings`. 모든 컬렉션은 `BTreeMap<Id, _>`
  (결정적 순회 → 안정적 레이아웃/테스트/diff).
- `ModelDef { graph: Graph, train: TrainConfig }`, `Graph { nodes: BTreeMap<NodeId, Node>, edges: BTreeMap<EdgeId, Edge> }`.
  `Node { kind: LayerKind, pos, name }`, `Edge { from: Port, to: Port }` (`Port { node, slot }`). 다입력 레이어(Add/Concat)는
  `slot` 로 구분한다.
- `LayerKind` 는 데이터 열거형이다(`Linear { out_features, bias }`, `Conv2d {..}`, `Activation(Act)`, `Dropout`, `BatchNorm`,
  `LayerNorm`, `Flatten`, `Reshape`, `MaxPool2d`, `AvgPool2d`, `Add`, `Concat`, `Embedding`, `Input`, `Output`, …).
  M3 에서 순환·어텐션이 더해졌다: `Lstm`/`Gru { hidden, bidirectional, return_sequence }` 는 `[L, D]` 를 받아
  `return_sequence` 면 `[L, H]`, 아니면 마지막 상태 `[H]` 를 내고(양방향이면 `H` 가 두 배), `MultiHeadAttention { heads, dropout }`
  는 셀프 어텐션으로 `[L, D] → [L, D]` 다(`D % heads == 0`). 셋 다 burn 의 `nn` 모듈 대신 파라미터 텐서 + 게이트 수식으로
  직접 구현했다 — 그래프가 런타임에 정해져 `Module` 파생을 쓸 수 없기 때문이다.
  각 종류의 입력 슬롯 수·파라미터 유무·표시 색은 `LayerKind::spec()` 이 단일 소유한다. 엔진은 `LayerKind` 를 해석해
  텐서 연산으로 바꾸며, **새 레이어 추가 = core 의 enum 변형 + spec + shape 규칙 + engine 의 연산 하나**다.
- `add_edge` 는 자기참조/중복/슬롯 점유만 거부하고 **순환은 허용** — 검증기(`validate`)가 감지해 오류 목록으로 보고하고,
  엔진은 검증 통과 그래프만 받는다(trust-pms 의 "문서를 깨뜨리지 않는다" 원칙).
- 모든 새 필드는 `#[serde(default)]` — 옛 문서 그대로 로드. `FORMAT_VERSION` 상수와 `ProjectFile::from_json` 마이그레이션.

### 레이어 템플릿 (`templates.rs`)
- 팔레트에서 **한 번에** 넣는 노드 묶음이다. `list()` 가 `TemplateSpec { name, label, category, description, default_params }`
  를 주고, `instantiate(name, at, &params)` 가 `(Vec<Node>, Vec<Edge>)` 를 새 id 로 만들어 돌려준다 — 문서에 넣는 것은 호출자가
  `Op::UpsertNode`/`UpsertEdge` 로 하므로 undo 한 번에 묶인다. 현재 셋: `residual_block { width }`(Linear→ReLU→Linear+잔차),
  `transformer_block { d_model, heads, ff_mult }`(pre-norm 어텐션 + 피드포워드, 잔차 둘), `conv_block { channels }`
  (Conv2d→BatchNorm→ReLU→MaxPool).
- **마지막 노드가 블록 출력이고, 엣지가 붙지 않은 입력 슬롯이 블록 입력**이다(`open_inputs`). 잔차가 있는 두 템플릿은 그 슬롯이
  **둘**(본줄기와 우회로)이고 **둘 다 같은 상류에 이어야** 잔차 덧셈의 형상이 맞는다. `transformer_block` 이 `d_model` 을 따로
  받는 것도 같은 이유다 — 피드포워드의 마지막 `Linear` 가 입력 폭으로 돌아와야 더할 수 있다.

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

### 번들과 빌드 스펙 (`bundle.rs`)
- `.nlapp` = zip: `manifest.json`(이름·버전·대상 모델·진입 파이프라인·GUI), `project.json`, `weights/<model>.safetensors`, `assets/`.
- 배포 바이너리 = 런타임 실행 파일 + 번들 바이트 + 꼬리표 `NLAPP1` + u64 길이. 런타임은 자기 실행 파일 끝을 읽어 번들을 꺼낸다
  → **사용자 PC 에 Rust 툴체인이 필요 없다.** 런타임 바이너리는 빌더 옆 `runtimes/<target>/` 에 두거나 매니페스트로 내려받는다.
  꼬리표의 길이는 신뢰할 수 없는 입력이라 `find_attached` 가 `usize::try_from` 으로 받아 들어가지 않으면 거절한다.
- `BuildSpec` 은 빌드 설정이다: `app_name`·`app_version`·`targets`·`entry_pipeline`·`autostart`·`default_device`·`models`·
  `output_dir`·`icon`, 그리고 배포 정책 넷 — `update_base_url`·`update_url`·`update_public_key`·`auto_update`·`arm_input`.
- `BundleManifest` 는 그중 **배포판이 실행 중에 보는 것**만 담는다: `format`·`app_name`·`app_version`·`built_with`·
  `entry_pipeline`·`models`·`default_device`·`autostart` + `update_url`·`update_public_key`·`auto_update`·`arm_input`.
  뒤의 넷이 배포 앱의 신뢰 경계를 정한다(아래 "보안 모델").
  - `arm_input` — **기본 꺼짐.** 꺼져 있으면 `Sink::MouseKeyboard` 는 로그만 남긴다. 받은 사람이 모르는 사이 커서가 움직이지
    않도록 빌더에서 명시적으로 켜야 한다.
  - `update_public_key` — minisign 공개키. **없으면 배포 앱이 자동 업데이트 기능 자체를 켜지 않는다.**

### 샘플 (`sample.rs`)
빌더의 "샘플 열기" 와 `nl sample` 이 **같은 프로젝트**를 만들도록 한 곳에 모았다. 노드 id 는 고정이라 두 경로의 산출물이
바이트까지 같다. `xor_project()`(MLP 분류), `quadrants_cnn_project()`(CNN 분류), `new_project()`(빈 뼈대),
그리고 추론 API 파이프라인을 만드는 `api_pipeline(base, bind, model, payload)` — HttpServer 소스 → 모델 → HttpReply.
목록은 `SAMPLES`.

### HTTP 서버 노드의 접근 규칙 (`pipeline.rs`)
파이프라인이 여는 인바운드 서버는 문서 모델 단계에서 이미 규칙을 갖는다.
- `new_token()` — URL-safe 32자(192비트) 토큰. `Source::HttpServer` 를 만들 때 기본으로 붙는다.
- `is_loopback_bind(bind)` — 바인드 주소가 루프백인지. **토큰 없이 서버를 열어도 되는지**를 이것이 가른다.
  루프백이 아니면서 토큰이 없는 파이프라인은 검증에서 걸린다.

## nl-engine

### 장치 (`device.rs`)
- `DevicePref::{Auto, Cpu, Gpu(index)}` (core) → `Device::{Cpu(NdArrayDevice), Gpu(WgpuDevice)}`.
  `enumerate()` 가 wgpu 어댑터 목록(이름·백엔드·VRAM)과 CPU 정보를 돌려준다. `probe(pref)` 가 그 장치에서 작은 matmul+backward 를
  실제로 돌려(별도 스레드·catch_unwind·20초 타임아웃) 동작 여부를 캐시하고, **Auto = probe 를 통과하는 첫 이산 GPU → 통합 GPU → CPU**
  (예: 오픈소스 NVK 드라이버의 RTX 2060 은 컴퓨트가 죽어 건너뛰고 Intel iGPU 를 고른다). 명시적 선택은 검사 없이 존중.
- **UI 스레드에서 부를 수 있는 것과 없는 것이 나뉜다.** `resolve(Auto)` 의 첫 호출은 GPU 셰이더 컴파일로 수십 초가 걸리고
  `probe` 는 스레드를 띄운다 — 둘 다 UI 스레드 금지다. 매 프레임 도는 상태바는 `resolve_cached`/`probe_cached` 를 쓴다.
  이들은 **이미 정해진 결과만** 돌려주고 아직 없으면 `None` 이라, 장치 열거도 스레드 생성도 하지 않는다.
- 백엔드 타입: `type Cpu = Autodiff<NdArray<f32>>`, `type Gpu = Autodiff<Wgpu<f32, i32>>`. 제네릭 코드는 `B: AutodiffBackend` 로
  한 번만 쓰고 `dispatch!(device, |B| …)` 매크로가 둘 중 하나로 단형화한다.

### 인터프리터 (`exec.rs`)
- burn 텐서는 랭크가 const generic 이라 `DynTensor<B>` = `enum { R1(Tensor<B,1>), R2, R3, R4, R5 }` 로 감싸고
  `LayerKind` 별 연산을 `apply(kind, inputs: &[DynTensor], params: &mut Params) -> DynTensor` 로 구현한다.
- `Params { tensors: BTreeMap<(NodeId, &'static str), Tensor<B, D>> }` — 초기화는 `ShapeReport` 와 kaiming/xavier.
  학습 시 `require_grad()`, 옵티마이저(SGD/Adam/AdamW)는 `Gradients` 에서 텐서별 grad 를 꺼내 직접 갱신한다
  (burn 의 `Module` 파생에 묶이지 않아 런타임 정의 그래프가 가능).
- 실행 순서는 core 의 위상정렬 결과를 그대로 쓴다. Dropout/BatchNorm 은 `train: bool` 로 분기.
- **순환 레이어의 비용과 한계.** `Lstm`/`Gru` 는 시각마다 게이트 커널을 새로 띄우는 구조라 한 스텝 값이 길이에 비례해서 는다.
  LSTM(은닉 64, 배치 32, 임베딩 32, `return_sequence: false`) 순전파+역전파 **스텝당** 시간 — 6회 측정의 최소값이고
  공용 개발 머신이라 절대값보다 비율을 보는 편이 낫다:

  | 시퀀스 길이 | 32 | 128 | 512 |
  |---|---|---|---|
  | CPU (ndarray, 릴리스 빌드) | 50 ms | 240 ms | 2.5 s |
  | GPU (Intel UHD 630, wgpu) | 155 ms | 610 ms | 3.1 s |

  **길이 수백이 현실적인 상한이다.** 512 면 스텝당 2~3 초라 1000 스텝짜리 에포크 하나가 한 시간 가까이 간다.
  짧은 쪽(32~128)에서는 CPU 가 3~4 배 빠르다 — iGPU 는 시각당 약 5 ms 가 커널 디스패치 고정비로 나가고
  `[32, 64]` 짜리 행렬곱은 그 비용을 메울 만큼 크지 않다. GPU 가 앞서는 구간은 없고, 길이 512 에서야 겨우 비슷해진다.
  CPU 쪽이 512 에서 유독 가파른 것(시각당 1.6 ms → 4.9 ms)은 자동미분 테이프가 길이에 비례해 커지면서 캐시를 벗어나기 때문이다.
  더 늘리려면 시각을 묶어 커널 수를 줄이거나(fused cell) 절단 역전파(TBPTT)가 필요한데, 둘 다 아직 없다.
- **스텝 루프 안에는 장치→호스트 읽기가 없다.** `recurrent`/`self_attention` 은 `narrow`·`linear`·`sigmoid`·`tanh`·`cat`
  같은 텐서 연산만 쓰고 `into_scalar`/`to_host` 를 부르지 않는다 — 시각마다 동기화가 걸리면 GPU 는 쓸 수 없게 느려진다.
  호스트로 내려오는 값은 학습 **스텝당 최대 한 번**이고(손실 이벤트가 나갈 차례일 때와 에포크 마지막 스텝),
  에포크 손실은 장치 텐서로 누적했다가 에포크 끝에 한 번 읽는다. 그래디언트 클리핑도 제곱합을 장치에서 더해 한 번만 읽는다
  (기본값 `grad_clip: 0.0` 이라 평소에는 이 읽기조차 없다).

### 학습 (`train.rs`)
- `Trainer::spawn(model, dataset, config, device) -> (JoinHandle, Receiver<TrainEvent>, Control)`.
  `TrainEvent::{Started{device}, Step{epoch, step, loss}, Epoch{epoch, train_loss, val_loss, val_metric}, Checkpoint(path),
  Finished, Failed(String)}`, `Control::{pause, resume, stop}`.
- 데이터 로더는 `DatasetSpec` → `Batch { inputs: Vec<DynTensor>, targets }`; 합성·CSV·이미지 폴더·녹화(`Recorded`) 를 지원한다.
- 체크포인트 = safetensors(파라미터 이름 `"{node_id}.{name}"`) + `run.json`(설정·지표). `RunRecord` 가 프로젝트에 남는다.
- **상주 배치.** 데이터셋이 작으면 에포크마다 호스트에서 배치를 만들지 않고 통째로 장치에 올려 둔다(`DeviceSet`).
  에포크 셔플은 `select_rows` 한 번, 배치 잘라내기는 `narrow_dim` + **`detach`** 다. detach 를 빠뜨리면 역전파가 데이터셋
  전체 버퍼를 다뤄 두 배 가까이 느려진다(실측 252s → 129s). 상한은 `RESIDENT_LIMIT_BYTES`(256 MiB, 셔플 사본 때문에 실제
  점유는 두 배)이고, CPU 는 샘플이 `RESIDENT_MIN_SAMPLE_BYTES` 보다 작으면 호스트 경로가 빠르다. `NL_NO_RESIDENT=1` 로 끌 수 있다.
- **학습률 스케줄**은 core 의 `LrSchedule::{None, Step{every,gamma}, Cosine{min_lr}, Plateau{patience,factor}}` 이고,
  엔진의 `LrController` 가 스케줄·워밍업·정체 감쇠를 한 곳에서 계산한다. 조기 종료도 여기에 붙는다.
- **다입출력**은 전용 타입 없이 규칙 두 개로 돈다. Input 이 하나면 데이터셋 샘플 형상이 정확히 일치해야 하고, 여럿이면
  샘플의 평탄한 원소 수가 각 Input 원소 수의 **합**과 같아야 한다(`check_dataset_inputs`). 배치는 `Graph::input_nodes()`
  순서대로 잘라 각 형상으로 reshape 하고(`split_inputs`), 손실은 첫 Output 으로만 계산한다.

### 추론 (`infer.rs`)
- `Session::load(model, weights, device)`; `run(&[HostTensor]) -> Vec<HostTensor>`. `codec.rs` 가 페이로드 Transform 을 적용해
  이미지/CSV/JSON ↔ 텐서를 오간다.
### ONNX 내보내기 (`onnx/`)

- `onnx::export(&ModelDef, weights, out, ExportOptions { opset, batch }) -> ExportReport { nodes, initializers, unsupported }`.
  학습한 모델을 **opset 17** ONNX 파일로 쓴다. 읽기는 기본 꺼진 feature 다(아래 "가져오기").
- **opset 17 인 이유**: `LayerNormalization` 이 17 에서 들어왔고 `Softmax` 가 축을 실제 축으로 읽는 것이 13 부터다.
  위로는 검증에 쓰는 tract 의 보장 범위가 18 까지다. 그래서 opset 20 의 `Gelu` 와 opset 23 의 `Attention` 은
  쓰지 않고 각각 `Erf` 전개와 분해로 간다.
- **가중치 변환이 거의 없다.** burn 의 `module::linear` 이 `x @ w` 라 우리 `Linear.weight` 가 `[in, out]` 이고
  이게 ONNX `MatMul` 의 B 와 그대로 맞는다(PyTorch 는 `[out, in]` 이라 전치가 필요하다). `Conv2d` 도 형상이 같고,
  `BatchNorm` 은 ONNX 가 추론에서 요구하는 running 통계를 우리가 이미 저장하고 있다. `Gemm` 은 2D 전용이라
  `[B, L, D]` 도 되는 `MatMul`+`Add` 로 통일한다.
- `Dropout` 은 노드를 만들지 않고 상류 이름을 물려준다(추론에서 항등). `Concat` 의 축은 우리가 샘플 기준이라
  **+1** 보정한다. `Embedding` 은 우리 인덱스가 f32 라 `Cast(INT64)` → `Gather(axis=0)` 다.
  배치는 기본이 `dim_param = "B"` 기호라 어떤 배치 크기로도 돈다(`Batch::Fixed(n)` 로 박을 수도 있다).
- **순환 레이어(`Lstm`·`Gru`)는 세 가지가 동시에 다르다.** 하나라도 빠뜨리면 오류 없이 값만 틀린다.
  1. **게이트 순서** — 우리는 PyTorch 를 따라 `i,f,g,o`(LSTM)·`r,z,n`(GRU), ONNX 는 `i,o,f,c`·`z,r,h` 다.
     `hidden` 크기 블록 단위로 각각 `[0,3,1,2]`·`[1,0,2]` 로 재배열한다. LSTM 의 `g` 와 ONNX 의 `c` 는 같은 게이트다.
  2. **가중치 방향** — 우리 `weight_ih` 는 `[D, gates·H]`(burn 의 `x @ w`), ONNX `W` 는 `[방향, gates·H, D]`(`Xt·Wᵀ`)다.
     전치한 뒤 방향 축을 붙인다. 편향은 우리가 `bias_ih`·`bias_hh` 를 따로 두고 ONNX `B` 도 `[Wb, Rb]` 를 이어 붙인
     `[방향, 2·gates·H]` 라 구조가 같다.
  3. **배치 축 위치** — 우리는 `[B, L, D]`(batch-first), ONNX 기본 `layout=0` 은 `[L, B, D]` 다.
     **`layout=0` + 앞뒤 `Transpose`** 를 쓴다(아래).
  `return_sequence` 면 `Y` `[L, 방향, B, H]` 를, 아니면 `Y_h` `[방향, B, H]` 를 받아 `Transpose`+`Reshape` 로
  우리 형상에 맞춘다. 안 쓰는 출력은 빈 이름으로 둔다(ONNX 가 정한 선택적 출력 표기). 양방향은
  `direction="bidirectional"` 에 정방향·역방향 가중치를 방향 축으로 쌓는다.
- **GRU 는 `linear_before_reset=1` 이 필수다.** ONNX 기본값 0 은 리셋을 `h(t-1)` 에 먼저 곱하는 쪽인데, 우리 구현은
  `n = tanh(gi_n + r ⊙ gh_n)` 이고 `gh_n` 이 `bias_hh` 를 이미 포함하므로 1 쪽이다. **빠뜨려도 파일은 정상이고
  읽는 쪽도 오류를 내지 않는다 — 값만 달라진다.** 왕복 테스트에서 실제로 이걸 0 으로 바꿔 보면 출력이 0.021 어긋난다.
- **어텐션은 분해한다.** 표준 `Attention` 은 opset 23 이고 `com.microsoft` 쪽은 ONNX Runtime 전용이라 둘 다 못 쓴다.
  `exec.rs` 의 `self_attention` 과 같은 순서로 q·k·v 투영(`MatMul`+`Add`) → `Reshape`/`Transpose` 헤드 분리 →
  `MatMul`·`Div(√head_dim)`·`Softmax`·`MatMul` → `Transpose`/`Reshape` 결합 → out 투영을 늘어놓는다.
  `Reshape` 의 목표 형상에는 배치·길이 자리에 **0**(그 자리 입력 차원 그대로)을 써서 기호 차원을 지킨다.
  학습용 `dropout` 은 추론에서 항등이라 내보내지 않는다.
- **tract 의 실제 동작을 확인했다.** `layout=1`(batch-first)도 읽고 `layout=0` 과 **같은 값**을 낸다 —
  그래도 `layout` 속성은 opset 14 부터라 읽는 쪽을 가리므로 `Transpose` 두 개를 쓰는 쪽이 이식성이 낫다.
  `linear_before_reset` 도 tract 가 제대로 해석한다(0 으로 바꾸면 결과가 달라지는 것으로 확인).
  출력 이름을 빈 문자열로 둔 선택적 출력(`Y` 를 버리고 `Y_h` 만 받기)도 그대로 받아들인다.
- **검증은 왕복이다.** `tests/onnx.rs` 가 학습 → 내보내기 → `tract-onnx` 로 읽기 → 우리 `Session` 과 1e-4 이내 비교를
  한다. `tract-onnx` 는 **dev-dependency 라 배포 바이너리에 들어가지 않는다**. 우리 코드끼리 비교하면 규약을 잘못
  이해한 경우를 못 잡으니 바깥 구현이어야 의미가 있다. 순환 레이어는 단방향·양방향 × `return_sequence` 네 경우를
  모두 돌고, 어텐션과 `templates` 의 트랜스포머 블록 전체도 함께 본다. 배치는 2 로 잡는다 — 1 이면 배치 축이
  뒤섞이는 실수를 놓친다.
### ONNX 가져오기 (`onnx_import.rs`) — 선택 feature

```sh
cargo build -p nl-engine --features onnx-import
cargo test  -p nl-engine --features onnx-import      # 왕복 테스트 둘이 이때만 돈다
```

- **기본이 꺼져 있다.** 실행은 `tract-onnx` 가 하는데 배포 바이너리가 **34 MiB** 늘어난다
  (현재 `nl-runtime` 74 MiB 대비 +46%). 기능 플래그로도 못 줄인다 — `tract-onnx` 0.23.7 에는
  `optional` 의존성이 하나도 없다. 끈 상태에서는 의존성 트리에 tract 가 **아예 나타나지 않는다**.
  **CI 에 feature 켠 잡을 두지 않는다**(빌드가 14분 늘어난다).
- `OnnxSession::load(path)` → `run(&[HostTensor]) -> Vec<HostTensor>`. 입출력 순서는 **파일에 적힌 순서**다.
  우리 `Session` 이 `Graph::input_nodes()` 순서를 쓰는 자리와 같으므로, 우리가 내보낸 파일은 그대로 맞는다.
- **추론 전용이다.** 학습도, `ModelDef` 로 되돌리는 변환도, 캔버스 표시도 없다.
  `PNodeKind::OnnxModel` 변형은 **아직 `nl-core` 에 넣지 않았다** — `PNodeKind` 를 전수 match 하는 앱 코드가
  함께 고쳐져야 해서다. 지금은 이 모듈을 직접 부르는 것만 된다.
- **실행 계획은 첫 `run` 때 만든다.** 동적 배치로 내보낸 모델은 배치가 기호로 남아 그대로는 최적화할 수 없다.
  실측 결과 **tract 는 기호 배치가 남은 LSTM 을 최적화하려다 패닉한다**(`UndeterminedSymbol("B")`).
  그래서 입력 형상을 아는 순간에 계획을 만들고 `catch_unwind` 로 감싼다 — 남의 파일 하나 때문에 앱이 죽으면
  안 된다. 같은 형상이 다시 오면 만들어 둔 계획을 그대로 쓴다.
- 파일 크기는 가중치와 같은 상한(`MAX_WEIGHTS_BYTES`)을 건다. protobuf 는 중첩 깊이·크기 공격이 가능한 포맷이다.
  입력 형상은 우리가 먼저 검사해서 tract 내부 오류 대신 읽을 수 있는 메시지를 낸다(기호 차원은 무엇이든 받는다).

- protobuf 메시지 정의(`onnx/pb.rs`)는 **생성 결과를 커밋**해 둔다 — 빌드에 `protoc` 도 코드 생성도 필요 없다
  (tract 가 쓰는 방식과 같다). 재생성 절차는 `scripts/onnxgen/README.md` 에 있고 순수 Rust(`protox` + `prost-build`)다.

- 필드 하나가 아니라 **페이로드 전체**를 옮길 때는 `codec::{encode_inputs, decode_outputs}` 를 쓴다.
  `payload.inputs` 순서 = `Graph::input_nodes()` 순서, `payload.outputs` 순서 = `Graph::output_nodes()` 순서가 **계약**이다
  (이름으로 맞추지 않는다 — 개수가 다르면 오류). 출력 필드가 하나면 값 자체를, 여럿이면 필드 이름을 키로 한 JSON 객체를 돌려준다.
  입력도 대칭이라 필드가 하나면 값을 그대로 받고(키 하나짜리 객체로 감싸 와도 벗겨 준다), 여럿이면 JSON 객체의 키가 곧 필드 이름이다.
  파이프라인의 `ModelOutput.field` 와 HTTP 추론 응답이 같은 모양을 보게 하려는 것이다.
- `Transform::Tokenize { vocab, max_len }`(core 의 `payload.rs`)는 문자 단위 토크나이저다 — `Text` 필드를 Embedding 입력으로
  바꾼다. `vocab` 의 문자 하나가 인덱스 하나이고 **인덱스는 1부터**, 0 은 패딩 겸 미지 문자다(그래서 Embedding 의 `vocab` 은
  글자 수 + 1 이상이어야 한다). 결과는 길이 `max_len` 정수 텐서이고 **인코드 전용**이다.

## nl-io

### 화면 캡처 (`screen.rs`)
`Backend::{Wayland, Portal, X11, XCap}` 넷이 있고 Linux 는 **wlroots → 포털 → X11** 순으로 시도한다.
시스템 개발 라이브러리 없이 빌드되도록 순수 Rust 경로만 쓴다.

| 백엔드 | 쓰는 곳 | 구현 | 비고 |
| --- | --- | --- | --- |
| `Wayland` | sway·Hyprland 등 wlroots | `libwayshot`(기본 feature 끔) | `zwlr_screencopy_v1` |
| `Portal` | GNOME·KDE Wayland | zbus + `org.freedesktop.portal.Screenshot` | **초당 1~3장**, 첫 호출에 권한 대화상자, `PORTAL_TIMEOUT` 30초 |
| `X11` | 순수 X11 세션 | x11rb root 창 `GetImage` | |
| `XCap` | Windows·macOS | xcap | xcap 의 Linux 백엔드는 pipewire/EGL 개발 패키지를 요구해 쓰지 않는다 |

`Capturer` 는 연결을 유지하는 쪽이고 일회성은 자유 함수 `capture(region)` 이다. 고 fps 가 필요한 ScreenCast(pipewire)
경로는 아직 없다 — 붙인다면 기본 꺼진 feature 여야 한다.

### 녹화 (`record.rs`)
`Recorder::start(dir, region, fps)` 가 화면을 캡처해 **학습용 데이터셋 폴더**로 바로 저장한다
(`nl_core::DataSource::Recorded` 가 그대로 읽는 형식).

```
<dir>/frames/000001.png …   캡처 프레임
<dir>/labels.jsonl          {"frame":"000001.png","label":3,"t_ms":1234}
<dir>/meta.json             영역·fps·시작 시각·백엔드
```

캡처 스레드와 저장 스레드가 나뉘고 그 사이 큐는 `WRITE_QUEUE`(32)다 — 차면 프레임을 버린다. 파일 번호는 **저장된 순서**로
매겨 드롭이 있어도 번호가 끊기지 않는다. `RecorderHandle` 로 `set_label`·`frames_written`·`frames_dropped`·`stop` 을 본다.

### 입력 시뮬레이션 (`input.rs`)
`input::{move_to, click, key, type_text}`(enigo). **안전장치**는 두 겹이다. 빌더에서는 사용자가 파이프라인 실행을 켠 동안만
동작하고 `Esc` 킬 스위치가 있다. 배포 앱에는 킬 스위치가 없으므로 `BundleManifest.arm_input` 이 꺼져 있으면 `Sink::MouseKeyboard`
가 **로그만 남기고 아무것도 하지 않는다**(기본 꺼짐).

### 자체 HTTP 서버 (`httpd.rs`)
인바운드 HTTP 는 **표준 라이브러리만 쓴 자체 HTTP/1.1 서버**다. tiny_http 를 쓰다가 바꿨다 — 연결 수·헤더 상한과 소켓
타임아웃을 걸 방법이 없어 slowloris 와 헤더 폭탄에 그대로 노출됐기 때문이다. keep-alive 는 없다(응답마다
`Connection: close`, 연결 하나에 요청 하나). HTTP/1.0 요청도 받되 응답은 언제나 `HTTP/1.1`.

| 상한 | 값 | 넘으면 |
| --- | --- | --- |
| `HEADER_TIMEOUT` | 5초 | 요청줄+헤더 마감 (slowloris 차단) |
| `BODY_TIMEOUT` | 30초 | 본문 읽기 |
| `WRITE_TIMEOUT` | 10초 | 응답 쓰기 |
| `MAX_HEADER_BYTES` | 16 KiB | 431 |
| `MAX_HEADERS` | 64개 | 431 |
| `HTTP_MAX_CONNECTIONS` | 64 | 503 (`runner.rs` 가 건다) |
| `MAX_HTTP_REQUEST_BYTES` | 8 MiB | 413 (`runner.rs` 가 건다) |

### 접근 제어 (`AccessPolicy`)
서버 노드에 들어온 요청은 **세 가지를 순서대로** 통과해야 한다.

1. `Origin` 헤더가 **있으면 403**. 브라우저에서 온 요청을 통째로 막는다(CORS preflight 도 함께 막힌다).
2. `Host` 가 바인드 주소도 루프백 이름도 아니면 **400**. DNS rebinding 을 막는다.
3. `Authorization: Bearer <토큰>` 또는 `X-NL-Token` 이 맞아야 한다. 아니면 **401**. 비교는 상수 시간이다.

토큰이 없는 서버는 **루프백에서만** 열 수 있다(`nl_core::is_loopback_bind`). 토큰은 `NL_HTTP_TOKEN`,
포트별로는 `NL_HTTP_TOKEN_<포트>` 로 덮어쓴다(포트별이 우선).

### 파이프라인 실행기 (`runner.rs`)
`Runner` 는 틱 루프다. `RunnerHandle { events, inputs, stop }` — GUI 위젯 이벤트는 `RunnerInput` 으로 들어가고
`Sink::GuiWidget` 값은 `RunnerEvent::Widget` 으로 나온다. `set_armed` 로 실행 중에 입력 무장을 토글하고,
`ValuePreview` 썸네일과 `Stats`(틱 속도·틱당 시간)를 이벤트로 흘린다.

**준비 순서가 중요하다.** 소스(HTTP 서버·WebSocket·stdin)를 **모델보다 먼저** 연다. 모델을 올리는 동안 들어온 요청은
큐에 쌓지 않고 곧바로 `503` + `MODEL_LOADING` 으로 돌려보낸다. 포트는 즉시 열리되 준비되기 전에는 정직하게 거절하는 것이다.
인증·Origin·Host 검사가 이 준비 검사보다 **앞**이라, 준비가 안 됐다는 사실조차 아무에게나 알리지 않는다.

**파일 경로는 프로젝트 폴더 안으로 묶인다.** `resolve_inside(base_dir, path)` 가 절대 경로·`..`·드라이브 접두사·심볼릭
링크(중간이든 끝이든)를 거절한다. `Sink::File` 같은 파일 노드뿐 아니라 **모델 가중치 경로**도 `Session::load` 전에 이것을
거친다 — 신뢰할 수 없는 `.nlapp` 이 남의 파일을 읽거나 덮어쓰지 못하게.

**큐는 전부 상한이 있다.** 무제한 큐는 상대가 보내는 속도만큼 메모리를 먹는다.

| 상한 | 값 | 넘으면 |
| --- | --- | --- |
| `HTTP_QUEUE_LIMIT` | 16 | 503 |
| `STREAM_QUEUE_LIMIT` | 256 | WebSocket·stdin 은 오래된 것부터 버린다 |
| `IMAGE_BACKLOG_LIMIT` | 4 | 이미지 프레임 드롭 |
| `HTTP_REPLY_TIMEOUT` | 10초 | 504 |

### 그 밖
- `http::call(method, url, headers, body) -> Response`(ureq, 타임아웃 필수 — trust-pms 교훈).
- WebSocket 소스·싱크는 URL 별 연결 풀·팬아웃·지수 백오프 재접속이고 `wss`(rustls)를 쓴다.
  비루프백 평문 `ws://` 는 경고를 낸다.
- `resources::snapshot()` — CPU/메모리(sysinfo) + GPU 목록(`nl_engine::enumerate()` 가 진실). 앱 상태바·자원 패널.

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
- 계획과 실행은 나뉘어 있다: `nl_bundle::tools::{install_inno_setup_plan, cross_build_plan}` 이 `ToolPlan` 을 만들고
  (`name`·`url`·`sha256`·`verified`·`size_hint`·`disk_hint`·`dest`·`steps`·`commands`), `run_tool_plan`/`run_cross_build`
  가 실행한다. 덕분에 "무엇을 받아 어디에 놓고 얼마나 걸리는지" 를 먼저 보여 주고 승인을 받을 수 있다.
- **검증할 수 없는 것은 실행하지 않는다.** `ToolPlan::verified` 는 알려진 sha256 이 있는지를 요약한 값이고,
  `false` 면 `run_tool_plan` 이 내려받기까지만 하고 **실행 단계를 거부한다**. 받은 파일은 남겨 사용자가 발행처에서 해시를
  확인한 뒤 직접 실행할 수 있게 한다. 승인 모달은 `verification_note()` 를 그대로 보여 주면 된다.
- 내려받기는 https 만 받고(리다이렉트로도 http 로 내려가지 않는다) 도구 폴더는 `~/.cache/neural-linker/tools`(0700)다.
  이 폴더는 `lld-link` 대체 스크립트가 놓이는 자리이자 크로스 빌드 내내 `PATH` 맨 앞에 오는 자리라, 공용 `/tmp` 로
  떨어지지 않게 `ProjectDirs → $XDG_RUNTIME_DIR → $HOME/.cache` 순으로만 물러선다.
- 외부 도구는 이름이 아니라 **풀어낸 절대 경로**로 띄우고 그 경로를 로그에 남긴다(`resolve_program`).

#### Windows 런타임 크로스 빌드 (Linux 에서, 실측됨)
관리자 권한 없이 Linux 에서 `nl-runtime.exe` 를 만들 수 있다. 빌더의 "Windows 런타임 없음 → 동의 후 준비" 는
내려받기가 아니라 **이 빌드**다.

```
cargo install cargo-xwin --locked
rustup target add x86_64-pc-windows-msvc
cargo xwin build --release -p nl-runtime --target x86_64-pc-windows-msvc
```

- `cargo-xwin` 이 Microsoft 의 Windows SDK·CRT 를 `~/.cache/cargo-xwin` 에 내려받는다(실측 1.2 GB).
  Visual Studio 도 관리자 권한도 필요 없다.
- 필요한 것: `clang`, `llvm-lib`, `lld-link`. 앞의 둘은 clang 패키지에 들어 있지만 **`lld-link` 는 별도 패키지**다
  (Fedora `lld`). 관리자 권한이 없으면 `tools::ensure_lld_link` 가 rustup 이 들고 있는 `rust-lld` 를
  `-flavor link` 로 부르는 얇은 스크립트를 만들어 대신 쓴다 — 같은 LLVM 에서 나온 같은 링커다.
  주의: rustc 가 링커 이름을 보고 스스로 `-flavor link` 를 붙여 보낼 때가 있어 스크립트는 중복을 걸러야 한다.
- burn/cubecl(wgpu dx12·vulkan), eframe/accesskit_windows, xcap, enigo, ring 모두 그대로 컴파일된다.
  `nl-io/build.rs` 는 `CARGO_CFG_TARGET_OS != linux` 면 아무것도 하지 않아 Windows 빌드에 끼어들지 않는다.
- 실측: 산출물 53.7 MB(PE32+ x86-64, GUI 서브시스템), 처음 빌드 15분 남짓, 디스크 약 3 GB
  (SDK 캐시 1.2 GB + 대상 target 1.9 GB).
- 만든 실행 파일을 `runtimes/x86_64-pc-windows-msvc/nl-runtime.exe` 에 두면 `nl_bundle::find_runtime` 이 찾는다.

### 빌드 (`build.rs`)
- `BuildSpec`(→ nl-core 절) → `nl_bundle::Bundle::to_zip` → 대상별 런타임에 `nl_bundle::attach` →
  Linux: `<slug>-<ver>-linux-x86_64.tar.gz`(+ install.sh, .desktop, 아이콘), Windows: `<slug>-<ver>-windows-x86_64.zip`
  (+ Inno Setup 이 있으면 `setup.exe`). 배포용 `latest.json`(`nl_bundle::write_manifest`)도 함께 만든다.
- 버전은 **semver 만** 받고(`check_version`), 산출물 경로는 마지막에 `out_dir` 안인지 정규화해 확인한다 —
  `Path::join` 은 구분자를 하위 경로로 받아들이므로 검사하지 않으면 산출물이 폴더 밖에 떨어진다.
- 파일 이름은 `slugify` 를 거쳐 `[a-z0-9-]` 만 남는다. 사람이 읽는 이름은 템플릿마다 문맥에 맞게 이스케이프한다(아래 "보안 모델").

## nl-runtime
- 단독 실행 시 인자로 `.nlapp` 를 받거나, 자기 실행 파일 꼬리표에서 번들을 읽는다. 인자로 받은 파일은 통째로 메모리에
  올리므로 먼저 크기를 본다(`MAX_BUNDLE_FILE_BYTES`, 2 GB).
- 창 제목/크기 = `GuiLayout.window`, 위젯 = `nl_gui::render_layout`, 파이프라인 = `Runner`. 장치는 번들 기본값 + 설정 창에서 변경.
- `--headless` 로 GUI 없이 파이프라인만(서버형 배포). `--run-for <초>` 로 자동 종료, `--no-update` 로 업데이트 확인 끔.
- **작업 폴더.** 번들 가중치는 `tempfile` 이 만든 무작위 이름 0700 폴더에 풀고 종료할 때 지운다. 예측 가능한 경로를 쓰면
  공용 `/tmp` 에서 다른 로컬 사용자가 먼저 만들어 두는 것만으로 가중치가 남의 폴더에 풀린다.
- **업데이트 UI.** 새 버전이 있으면 상단 바에 `⬆ 새 버전 x.y.z` 배지가 뜨고 누르면 릴리스 노트·진행률·"지금 적용" 창이
  열린다. **번들에 서명 공개키가 없으면 이 UI 자체가 만들어지지 않는다** — 확인도 배지도 없다.
  `auto_update` 가 켜져 있어도 자동으로 하는 일은 "파이프라인이 멈춰 있을 때 미리 내려받기" 까지고, 적용은 언제나 사용자 확인을 거친다.
- **입력 무장 표시.** `arm_input` 이 켜진 번들은 상단 바에 `⚠ 입력 무장` 배지가 붙고, 파이프라인을 처음 시작할 때 로그에
  한 번 안내한다. 꺼져 있으면 배지가 없고 입력은 로그로만 남는다.
- **Windows 콘솔.** 릴리스 빌드는 GUI 서브시스템이라 검은 창이 같이 뜨지 않는다. 대신 `--version`·`--help`·`--headless`
  처럼 터미널에서 부른 것이 분명한 경우에만 부모 콘솔에 붙어(`AttachConsole`) 출력을 보여 준다.

## 보안 모델

`.nlapp` 번들 · 프로젝트 파일 · safetensors · CSV/이미지 · 업데이트 매니페스트 · 인바운드 HTTP 요청을 전부
**신뢰할 수 없는 외부 입력**으로 본다. 2026-09-14 보안 리뷰의 항목별 처리 현황은
[`docs/reviews/security-2026-09-14.md`](reviews/security-2026-09-14.md) 맨 위 표에 있다.

### 자동 업데이트 — fail-closed

신뢰의 사슬은 **번들에 박힌 공개키 → 매니페스트 서명 → 매니페스트의 sha256 → 자산** 하나다. 끊기면 진행하지 않는다.

| 규칙 | 어기면 |
| --- | --- |
| 공개키가 있어야 한다 | `Updater` 가 `State::Disabled` 로 남고 확인조차 하지 않는다. 런타임은 UI 를 숨긴다 |
| 주소는 https | `Disabled`. 에이전트에 `https_only` 를 걸어 리다이렉트로도 http 로 내려가지 않는다 |
| 매니페스트에 유효한 서명 | 서명을 통과하기 전에는 **내용을 읽지도 않는다** |
| `published_at` 이 30일 안 | 거절. 정품 서명이 붙은 옛 매니페스트를 다시 들려주는 재생 공격을 막는다 |
| 자산은 매니페스트와 같은 오리진 | 거절. 예외는 매니페스트 안에 서명된 `allowed_asset_hosts` 뿐 |
| 자산마다 sha256 | 받은 뒤 검증하고, **적용 직전에 다시 계산해 맞춘다** |

내려받기는 사용자 캐시(0700)에 하고 `.part` 는 `O_EXCL`·`O_NOFOLLOW`·0600 으로 연다. 교체는 같은 폴더의 무작위 이름
임시 파일에 부어 `rename` 한다. 자산 종류(`Installer`/`Binary`)와 해시는 **서명된 매니페스트에서만** 온다 — 파일 이름의
확장자로 짐작하지 않는다. 짐작하면 공격자가 정한 URL 의 `.exe` 를 설치 프로그램으로 실행하게 된다.

**한계.** Windows 설치본의 Authenticode 서명은 검증하지 않는다. 실행 전에 확인하는 것은 서명된 매니페스트가 말한
sha256 뿐이다. 사슬은 닫혀 있지만 배포 서버와 서명 키를 동시에 쥔 공격자는 막지 못한다.

### 번들 읽기 — zip bomb

`Bundle::from_zip` 은 세 가지 상한을 함께 본다: 엔트리 개수 `MAX_ZIP_ENTRIES`(10,000), 엔트리 하나
`MAX_ENTRY_BYTES`(512 MB), 번들 전체 누적 `MAX_BUNDLE_BYTES`(2 GB). 중앙 디렉터리의 `uncompressed_size` 는
**공격자가 적는 숫자**라 그대로 선할당하지 않고 8 MiB 로 자르며, 실제로 읽은 길이가 그 숫자와 다르면 거절한다.
정규화하면 같은 파일이 되는 항목이 두 번 나오면(`weights/a` 와 `weights/./a`) 거절한다 — 무엇이 풀릴지 알 수 없어진다.

### 빌드 템플릿 — 문맥별 이스케이프

앱 이름과 버전은 프로젝트 파일에서 오는 신뢰할 수 없는 값인데 네 가지 출력 포맷에 들어간다. 포맷마다 문법이 다르므로
이스케이프도 다르다(`Syntax`).

| 출력 | 처리 |
| --- | --- |
| `install.sh` | POSIX 단일 인용. 값은 `APP_NAME='…'` 변수 대입으로만 들어가고 본문은 `"$APP_NAME"` 으로 참조한다 |
| `.desktop` | 명세대로 개행·탭·역슬래시를 두 글자 표기로. 개행으로 새 키나 새 그룹을 만들 수 없다 |
| `.iss` | 큰따옴표를 **지운다**. Inno Setup 은 `[Icons] Name:` 에서 이중화(`""`)조차 거절한다(6.7.3 실측) |
| README | 제어문자만 걸러 낸다 |

`.iss` 는 **표시용과 경로용 토큰이 나뉘어 있다.** `DefaultDirName`·`DefaultGroupName`·`[Icons]` 의 바로가기 이름은
전부 경로라 `windows_dir_name` 을 거친다. 나누지 않으면 이름에 `/` 나 `:` 가 있을 때 컴파일은 통과하고 **설치할 때**
`The folder name is not valid.` 로 터진다(실측). `AppId` 는 앱 이름 + 발행자의 UUID v5 라, 이름만 베낀 앱이 남의
설치를 업그레이드로 덮어쓰지 못한다.

### 외부 도구

Inno Setup 설치본은 **버전이 박힌 주소와 고정 sha256** 으로만 받는다(6.7.3). 예전에 쓰던 `download.php/is.exe` 는
설치본이 아니라 안내 페이지로 302 하는 주소였고 버전이 계속 바뀌어 해시를 박을 수도 없었다.
해시가 없는 계획은 내려받기만 하고 실행하지 않는다(위 "도구 설치").

## 테스트

| 갈래 | 무엇 | 어떻게 |
| --- | --- | --- |
| 단위·통합 | core(형상 추론·op/undo/diff 왕복·직렬화), engine(작은 MLP 가 XOR/선형 회귀 수렴), io(서버·경로 제한·큐 상한), bundle/update(신뢰 모델) | `cargo test --workspace` |
| GPU | wgpu 경로 | `NL_TEST_GPU=1` 일 때만 |
| 인프로세스 렌더 | `egui_kittest` 스냅샷 — nl-gui 위젯과 nl-runtime 상단 바가 픽셀까지 같은지 | `cargo test`, 갱신은 `UPDATE_SNAPSHOTS=1` |
| 종단 | `nl sample → train → build → 배포판 HTTP /infer` 한 줄로 | `NL_E2E=1 cargo test -p nl-cli --test e2e` |
| 실제 GUI | 헤드리스 sway 안에서 진짜 클릭·드래그·캡처, PPM 골든 비교 | `tools/uitest/uitest.sh`, 시나리오는 `scenarios/*.uit` |

**건너뛰기를 실패로 바꾸는 스위치가 둘 있다.** cargo 는 통과한 테스트의 출력을 삼키므로, 환경이 없어 조용히 건너뛴
테스트는 통과와 구분되지 않는다.

- `NL_SNAPSHOT_REQUIRED=1` — 렌더 백엔드(wgpu 어댑터)가 없어 건너뛰는 것을 실패로. CI 가 켠다.
  반면 **글꼴이 없어 건너뛰는 것은 실패로 바꾸지 않는다** — 배포판마다 Noto CJK 판본이 달라 강제할 수 없고,
  다른 글꼴로 찍으면 영문 모를 불일치가 난다.
- `NL_E2E=1` — 무겁고(학습·링크·20 MB 아카이브) 런타임 바이너리를 먼저 빌드해야 해서 기본으로는 돌지 않는다.

**검증은 다른 것과 나란히 도는 것을 전제한다.** 종단 시험은 샘플의 고정 포트(XOR 8799, CNN 8800)를
그대로 열지 않고 프로젝트 복사본의 주소를 빈 포트로 바꿔 띄우며, 산출물 폴더에는 pid 를 붙이고,
정리할 때 자기가 띄운 프로세스만 죽인다. 규칙과 그렇게 된 이유는 `scripts/README.md`.

## CI · 릴리스

워크플로는 `.github/workflows/` 와 `.forgejo/workflows/` 에 **같은 내용으로 두 벌** 있다.
Forgejo Actions 가 GitHub 문법을 그대로 읽으므로 파일이 같고, 고칠 때 둘을 함께 고쳐야 한다.
Forgejo 인스턴스는 `app.ini` 에 `[actions] DEFAULT_ACTIONS_URL = github` 이 있어야 액션을 받아 온다.

릴리스 단계의 실제 내용은 **`packaging/lib.sh`** 에 있다. 워크플로와 로컬 스크립트가 같은 함수를 부르므로 결과가
어긋나지 않는다 — `nl_release_version`·`nl_pack_linux`·`nl_pack_windows`·`nl_make_app_manifest`·
`nl_make_runtimes_manifest`·`nl_sign`·`nl_verify`·`nl_artifact_table`·`nl_bin_names`.
(실행 파일 이름은 크레이트 이름과 다를 수 있어 `cargo metadata` 에 물어본다 — `nl-cli` 는 `nl` 을 만든다.)

### `ci.yml` — push·PR
1. 시스템 의존: `libxkbcommon0`(enigo 런타임), `mesa-vulkan-drivers`(lavapipe — 스냅샷 테스트가 쓰는
   소프트웨어 wgpu 어댑터), `fonts-noto-cjk`.
2. `cargo fmt --all -- --check` — **아직 강제하지 않는다**(`continue-on-error`). 워크스페이스 전체 포맷을
   적용하지 않은 상태라 지금 켜면 온통 빨간불이 된다. 한 번 정리되면 이 플래그를 뺀다.
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo test --workspace` (`NL_SNAPSHOT_REQUIRED=1`)
5. `cargo build --release -p nl-runtime` — 배포 런타임의 릴리스 빌드가 깨지면 배포가 막힌다.
6. 별도 잡 `windows`(`windows-latest`) — `cargo test -p nl-core -p nl-io -p nl-cli -p nl-bundle -p nl-update -p nl-engine`.
   개발 기계에서는 `cargo xwin` 으로 컴파일만 볼 수 있어, 소켓 타임아웃·경로처럼 **실제로 돌려 봐야 아는 것**을 여기서 잡는다.
   GUI 크레이트는 뺐다(러너에 GPU·CJK 글꼴이 없어 스냅샷이 의미 없다). Linux 잡과 독립이라 한쪽이 깨져도 다른 쪽 결과는 그대로 보인다.

`NL_SNAPSHOT_REQUIRED=1` 은 **렌더 백엔드가 없어 건너뛰는 것**을 실패로 바꾼다. cargo 가 통과한 테스트의
출력을 삼켜서, 이 장치가 없으면 스냅샷이 조용히 통과해 버린다. 반면 **글꼴이 없어 건너뛰는 것**은 실패로
바꾸지 않는다 — 배포판마다 Noto CJK 판본이 달라 강제할 수 없고, 다른 글꼴로 찍으면 영문 모를 불일치가 난다.

### `release.yml` — 태그 `v*`
한 러너에서 Linux 네이티브와 Windows 크로스(`cargo-xwin`)를 함께 만든다. 러너는 root 라
`clang lld llvm` 을 패키지로 깐다(개발 기계에서는 rustup 의 `rust-lld` 로 대신한다 — `packaging/README.md`).

산출물:

| 파일 | 쓰임 |
| --- | --- |
| `neural-linker-<ver>-linux-x86_64.tar.gz` | 빌더 + 런타임 + `install.sh` + `.desktop`. 풀고 `./install.sh` |
| `neural-linker-<ver>-windows-x86_64.zip` | 빌더 + 런타임 (설치 프로그램 없이 풀어 씀) |
| `nl-app`, `nl-app.exe` | 자동 업데이트가 그대로 내려받는 알맹이 |
| `nl-runtime`, `nl-runtime.exe` | 빌더가 배포판을 만들 때 붙이는 런타임 |
| `latest.json` | 빌더 자체 업데이트 (`packaging/make-manifest.sh`) |
| `runtimes/latest.json` | 빌더가 대상별 런타임을 받아 오는 매니페스트 (`packaging/make-runtimes-manifest.py`) |

두 매니페스트 모두 `nl_update::Manifest` 형식이다. 차이는 자산 종류다 — 런타임 매니페스트는 언제나
`binary` 다(빌더가 받아서 `runtimes/<triple>/` 에 놓기만 하고 실행하지 않는다).
`MINISIGN_KEY` 시크릿이 있으면 두 매니페스트에 `.minisig` 를 붙이고, `UPDATE_PUBLIC_KEY` 변수가 있으면 올리기 전에
**같은 코드로 검증까지 한다**. 서명·검증은 `minisign` CLI 가 아니라 `nl-update` 의 예제 도구
`nl-keygen`(`keygen`/`sign`/`verify`)이 한다 — 러너에 설치할 것이 없고, 검증이 배포 앱이 쓰는
`nl_update::verify_manifest` 를 그대로 부르므로 여기서 통과하면 사용자 앱에서도 통과한다.
비밀키는 비밀번호가 없어야 하고(CI 는 대화형 입력을 받을 수 없다) 그래서 키 파일 자체가 곧 비밀이다 —
`.gitignore` 가 `packaging/keys/*.key` 를 막는다.

### 로컬 드라이런

태그를 밀기 전에 CI 가 무엇을 내놓을지 그대로 볼 수 있다.

```sh
packaging/release-local.sh --crates nl-runtime,nl-cli --out /tmp/dist-local
```

`--key` 를 주지 않으면 임시 키로 서명하고 검증까지 해 본다(형식 확인용). `--targets` 로 플랫폼을,
`--skip-build` 로 이미 빌드된 산출물 재포장을, `--installer` 로 (Inno Setup 이 있을 때) setup.exe 까지 만든다.
`--installer` 는 러너에 Inno Setup 이 없어 **CI 에는 없는 단계**다. 전체 절차는 [`docs/RELEASE.md`](RELEASE.md).

`latest.json` 에 **Windows 자산은 아직 넣지 않는다.** 자기 자신을 바꿔치울 수 없는 Windows 는
`kind: installer` 가 맞는데, Inno Setup 이 러너에 없어 설치 프로그램을 만들지 못한다. 그때까지 Windows
사용자는 zip 을 받아 덮어쓴다.

## 플랫폼 주의사항 (trust-pms 에서 계승)
- glow(OpenGL) 백엔드 명시, Wayland `vsync: false`, 한글 폰트 시스템 폴백(`font_definitions`).
- 모든 파일 쓰기는 원자적(임시 파일 + rename). 자동 저장·복구는 M1.
- GPU 계산(wgpu)과 GUI 렌더(glow)는 서로 다른 컨텍스트다 — 학습은 항상 별도 스레드, UI 스레드는 이벤트만 받는다.
