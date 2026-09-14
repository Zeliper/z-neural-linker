# nl-engine

**런타임에 정해지는 그래프**를 해석해 돌리는 실행 엔진. 장치 선택 · 학습 · 추론 · 체크포인트 ·
데이터 적재 · 페이로드 변환 · ONNX 내보내기를 맡는다. 백엔드는 burn 0.21 이다.

이 크레이트가 특이한 점은 **모델이 컴파일 타임에 없다는 것**이다. 사용자가 캔버스에서 만든
`nl_core::ModelDef` 를 그때그때 읽어 텐서 연산으로 바꾼다. 그래서 burn 의 `Module` 파생을 쓰지
못하고, 파라미터 텐서와 옵티마이저를 직접 들고 있다.

전체 설계는 [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md), 사용자 관점 설명은
[`docs/GUIDE.md`](../../docs/GUIDE.md) 에 있다.

## 모듈 지도

| 파일 | 하는 일 |
| --- | --- |
| `exec.rs` | 그래프 인터프리터. `LayerKind` 20종을 텐서 연산으로. 가장 큰 파일이다 |
| `train.rs` | 학습 스레드. 이벤트 채널 · 옵티마이저 · 학습률 스케줄 · 체크포인트 |
| `infer.rs` | `Session` — 모델 하나를 올려 두고 반복 추론 |
| `device.rs` | 장치 열거 · 검사(`probe`) · `Auto` 선택 · 백엔드 단형화 매크로 |
| `data.rs` | 데이터 소스 4종 적재와 훑기(`scan`)·미리보기(`preview`) |
| `codec.rs` | 페이로드 변환. 이미지/CSV/JSON ↔ 텐서 |
| `weights.rs` | safetensors 저장·적재 (mmap) |
| `tensor.rs` | `HostTensor` — 크레이트 경계를 넘는 f32 텐서 |
| `limits.rs` | 자원 상한과 안전한 디코더 |
| `paths.rs` | 오류·로그에 넣을 짧은 경로 |
| `onnx/` | ONNX 내보내기. `pb.rs` 는 생성 결과를 커밋해 둔 protobuf 정의다 |
| `onnx_import.rs` | ONNX 가져오기. **`--features onnx-import` 로만 들어온다** |
| `lib.rs` | 공개 API. 여기 re-export 된 것이 계약의 전부다 |

## 공개 API

`lib.rs` 의 re-export 가 계약이다. 모듈 자체도 `pub` 이지만 다른 크레이트는 아래 이름만 쓴다.

| 이름 | 쓰임 |
| --- | --- |
| `Session` | 추론. `Session::load(model, weights, device)` → `run(&[HostTensor])` |
| `start`, `TrainRequest`, `TrainHandle`, `TrainEvent` | 학습. `start(req)` 가 스레드를 띄우고 핸들을 준다 |
| `HostTensor` | 크레이트 경계의 f32 텐서 (`shape` + `data`) |
| `enumerate`, `resolve`, `resolve_cached`, `probe`, `probe_cached`, `describe` | 장치 |
| `DeviceInfo`, `DeviceKind`, `Resolved`, `CpuB`, `GpuB` | 장치 타입 |
| `encode`, `decode` | 필드 하나 ↔ 텐서 |
| `encode_inputs`, `decode_outputs` | **페이로드 전체** ↔ 텐서 묶음 (다입출력) |
| `Value` | 엔진 경계의 바깥 값 (숫자·문자열·이미지·JSON·텐서) |
| `scan`, `preview`, `load_source`, `load_all`, `count_csv_rows`, `Sample` | 데이터셋 |
| `export_onnx`, `ExportOptions`, `ExportReport`, `Batch` | ONNX 내보내기 |
| `param_name`, `Model`, `DynTensor` | 인터프리터 내부 (도구·시험용) |
| `checkpoint_summary`, `WEIGHTS_FORMAT` | 체크포인트 조회 |
| `MAX_IMAGE_DIM`, `MAX_IMAGE_PIXELS`, `MAX_TENSOR_ELEMS` | 자주 쓰는 상한 |

## 장치 선택

`DevicePref::{Auto, Cpu, Gpu(index)}` (core) → `Device::{Cpu, Gpu}`.

- **`probe(pref)`** 가 그 장치에서 작은 matmul + 역전파를 **실제로** 돌려 본다. 별도 스레드에
  `catch_unwind` 와 20초 타임아웃을 걸고, 결과는 프로세스 수명 동안 캐시한다. wgpu 드라이버는
  실패를 오류가 아니라 **패닉**이나 무응답으로 알리기 때문이다.
- **`Auto` = probe 를 통과하는 첫 이산 GPU → 통합 GPU → CPU.** 소프트웨어 래스터라이저는 뺀다.
  명시적으로 고른 장치는 검사 없이 존중한다.
- **UI 스레드에서 부르면 안 되는 것**: `resolve(Auto)` 의 첫 호출(GPU 셰이더 컴파일로 수십 초),
  `probe`(스레드를 띄우고 최대 20초 멈춘다). 매 프레임 도는 상태바는 **`resolve_cached` /
  `probe_cached`** 를 쓴다 — 이미 정해진 결과만 돌려주고 없으면 `None` 이라 아무것도 하지 않는다.
- `describe(pref)` 는 툴팁에 넣을 한 줄 설명을 준다.

## 학습 루프 계약

`start(TrainRequest) -> TrainHandle`. 핸들의 `events` 는 `Receiver<TrainEvent>` 이고
`pause` / `resume` / `stop` / `is_paused` / `is_done` 으로 제어한다.

**이벤트 순서**는 이렇다.

```
Started → (Step* Epoch [Checkpoint])* → Finished | Failed
```

- `Started { device, batches_per_epoch, params }` 가 **반드시 처음**에 한 번.
- `Finished { run }` 또는 `Failed { run, error }` 가 **반드시 마지막**에 한 번. 둘 다 `RunRecord` 를
  담으므로, 중간 이벤트를 하나도 안 읽어도 최종 결과는 온전하다.
- `Log(String)` 은 아무 때나 끼어든다 (진단 메시지).
- 채널이 `Finished`/`Failed` 없이 끊겼다면 학습 스레드가 비정상 종료한 것이다.

**채널 드롭 정책.** 채널은 용량 1024 의 유계 채널이다. 넘칠 때 **`Step` 만 버린다** — 최신 값이
다음에 다시 오기 때문이다. `Epoch`·`Checkpoint`·`Finished`·`Failed` 는 절대 버리지 않고, 소비자가
읽을 때까지 최대 30초 기다린다(그 뒤에는 포기하고 로그를 남긴다 — 학습 스레드가 영영 멈춰 있는
편이 더 나쁘다). `Step` 은 보내기 전에 시간으로도 솎아 낸다(기본 50ms, `NL_STEP_EVENT_MS` 로 조정,
0 이면 매 스텝). 버리거나 솎아 낸 수는 마지막에 로그 한 줄로 알린다.

**체크포인트 파일 규약.** `run_dir` 아래에 이렇게 남는다.

| 파일 | 언제 |
| --- | --- |
| `final.safetensors` | 학습이 끝날 때 (중단되어도 남긴다) |
| `best.safetensors` | 검증 손실이 가장 낮았던 에포크가 마지막이 아닐 때만 |
| `run.json` | 설정과 에포크별 지표. 학습 중에도 갱신하고 끝에 최종 상태로 다시 쓴다 |

`RunRecord.checkpoint` 와 `.best_checkpoint` 는 **`base_dir` 기준 상대 경로**다. `best` 가 없으면
`checkpoint` 를 쓴다. 쓰기는 임시 파일 + 이름 바꾸기라 도중에 죽어도 반쯤 덮인 파일이 남지 않는다.

파라미터 이름은 `"{node_id}.{part}"` 다 (`param_name`). 순환 레이어의 역방향은 `_reverse` 가 붙는다.

## 데이터 소스

| 종류 | 형태 |
| --- | --- |
| `Csv { path, input_cols, target_cols, header }` | 열 이름(헤더) 또는 0 기반 번호 문자열 |
| `ImageFolder { path }` | `path/<클래스>/*.png\|jpg`. 클래스 = 하위 폴더 이름(정렬 순) |
| `Recorded { path }` | 녹화 폴더 (`frames/*.png` + `labels.jsonl` + `meta.json`) |
| `Synthetic { kind, samples }` | `Xor` · `Spirals` · `LinearRegression` · `Quadrants` |

**다입력 규칙은 둘뿐이다.**

- Input 노드가 **하나**면 데이터셋 샘플의 형상이 정확히 일치해야 한다.
- Input 노드가 **여럿**이면 샘플의 평탄한 원소 수가 각 Input 원소 수의 **합**과 같아야 한다.
  배치는 `Graph::input_nodes()` 순서대로 잘라 각 형상으로 reshape 한다. CSV 는 `input_cols` 순서가
  그 순서에 대응한다.

`Graph::input_nodes()`/`output_nodes()` 는 **이름순**이다(이름이 같으면 id 순). 이름을 비워 두면
무작위 id 순이 되어 입출력이 뒤바뀐다 — 다입출력 모델은 노드에 이름을 붙여야 한다.

손실은 **첫 Output** 으로만 계산한다.

`scan` 은 CSV 전체를 읽지 않는다. 헤더와 앞 1,000 행만 보고 파일 크기로 행 수를 어림하며,
그때 `DatasetInfo.samples_estimated` 가 참이 된다. 정확한 값이 필요하면 `count_csv_rows` 를 쓴다.
**둘 다 UI 스레드에서 부르지 말 것.**

## 페이로드 변환 (`codec`)

필드 하나는 `encode`/`decode`, 페이로드 전체는 `encode_inputs`/`decode_outputs` 다.
**순서가 계약이다** — `payload.inputs` 순서 = `Graph::input_nodes()` 순서,
`payload.outputs` 순서 = `Graph::output_nodes()` 순서. 이름으로 맞추지 않으며 개수가 다르면 오류다.
출력 필드가 하나면 값 자체를, 여럿이면 필드 이름을 키로 한 JSON 객체를 돌려준다.

| Transform | 방향 | 하는 일 |
| --- | --- | --- |
| `Resize { width, height }` | 인코드 | 이미지 크기 조정 |
| `Grayscale` | 인코드 | 1 채널로 |
| `Crop { x, y, width, height }` | 인코드 | 잘라내기 |
| `Normalize { mean, std }` | 양방향 | 채널별 `(x - mean) / std`. 길이 1 이면 전체에 |
| `Scale { min, max }` | 양방향 | 선형 `[min, max]` → `[0, 1]` |
| `OneHot { classes }` | 인코드 | 인덱스 → 원핫 |
| `Tokenize { vocab, max_len }` | **인코드 전용** | 문자 단위 토크나이저 |
| `JsonPointer { pointer }` | 인코드 | JSON 포인터로 값 추출 |
| `Argmax` | 디코드 | 최대 인덱스 |
| `Softmax` | 디코드 | 확률로 |
| `Threshold { value }` | 디코드 | 임계값으로 0/1 |
| `MapLabel` | 디코드 | 인덱스 → 라벨 이름 (`ClassLabel` 필드와 짝) |

`Tokenize` 는 **인덱스가 1 부터**이고 0 이 패딩 겸 미지 문자다. 그래서 Embedding 의 `vocab` 은
`vocab.chars().count() + 1` 이상이어야 한다.

**`FieldKind::Image` 가 받는 입력 형식**은 넷이다. 어느 쪽이든 `[channels, h, w]` f32 가 된다.

- `Value::Image { width, height, rgba }` — 이미 디코드된 RGBA 버퍼
- `Value::Numbers` — 평탄한 숫자 배열 (선언한 형상의 원소 수와 맞아야 한다)
- `Value::Json` 안의 숫자 배열 — 중첩(`[[[r,g,b],…],…]`)과 평탄 둘 다. 중첩 순서로 HWC/CHW 를 가른다
- `Value::Text` 의 base64 — `data:image/png;base64,…` 같은 data URL 이나 맨 base64. PNG·JPEG

`FieldKind::Json` 은 파이프라인 사이를 지나가는 값이라 모델에 직접 넣을 수 없다.

## 자원 상한 (`limits.rs`)

바깥에서 온 파일과 프로젝트 파일의 값이 메모리를 통째로 먹지 않게 막는 선이다.

| 상수 | 값 | 무엇 |
| --- | --- | --- |
| `MAX_IMAGE_DIM` | 8192 | 이미지 한 변 |
| `MAX_IMAGE_PIXELS` | 4096 × 4096 | 이미지 픽셀 수 |
| `MAX_IMAGE_ALLOC` | 256 MiB | 이미지 디코드 할당 |
| `MAX_TENSOR_ELEMS` | 256 Mi | 텐서 원소 수 |
| `MAX_CLASSES` | 1,000,000 | 클래스 수 |
| `MAX_TOKENS` | 1,000,000 | 토크나이저 어휘·길이 |
| `MAX_WEIGHTS_BYTES` | 4 GiB | 체크포인트·ONNX 파일 크기 |
| `MAX_WEIGHTS_TENSORS` | 100,000 | 체크포인트 텐서 수 |
| `MAX_PARAM_NAME_LEN` | 1 KiB | 파라미터 이름 |
| `MAX_CSV_BYTES` | 512 MiB | CSV 파일 |
| `MAX_CSV_ROWS` | 5,000,000 | CSV 행 |
| `MAX_CSV_COLS` | 4096 | CSV 열 |
| `MAX_DATASET_ELEMS` | 1 Gi | 데이터셋 전체 원소 수 |
| `MAX_LABELS_BYTES` | 64 MiB | `labels.jsonl` |

## ONNX

**내보내기**는 기본으로 들어 있다. opset **17** 을 겨냥한다.

| 레이어 | ONNX |
| --- | --- |
| `Input` / `Output` | 그래프 입출력 (`Output` 은 `Identity`) |
| `Linear` | `MatMul` (+ `Add`) — 우리 가중치가 `[in, out]` 이라 전치가 필요 없다 |
| `Conv2d` | `Conv` — 형상이 그대로 맞는다 |
| `MaxPool2d` / `AvgPool2d` | `MaxPool` / `AveragePool` |
| `GlobalAvgPool` | `GlobalAveragePool` + `Reshape` |
| `Flatten` / `Reshape` | `Flatten(axis=1)` / `Reshape` |
| `Activation` 8종 | `Relu`·`LeakyRelu`·`Sigmoid`·`Tanh`·`Softmax`·`LogSoftmax`, `Gelu` 는 `Erf` 전개, `Silu` 는 `Mul`+`Sigmoid` |
| `Dropout` | **노드 없음** (추론에서 항등) |
| `BatchNorm` | `BatchNormalization` — running 통계를 그대로 넘긴다 |
| `LayerNorm` | `LayerNormalization` (opset 17) |
| `Add` / `Mul` / `Concat` | 같은 이름. `Concat` 축은 샘플 기준이라 **+1** 보정 |
| `Embedding` | `Cast(INT64)` + `Gather(axis=0)` |
| `Lstm` / `Gru` | `LSTM` / `GRU` — 게이트 재배열 + 전치 + `layout=0` 앞뒤 `Transpose` |
| `MultiHeadAttention` | 분해 (표준 `Attention` 은 opset 23 이라 못 쓴다) |

**20종 전부 나간다.** 배치는 기본이 기호(`dim_param = "B"`)라 어떤 배치 크기로도 돌고,
`Batch::Fixed(n)` 로 박을 수도 있다.

**GRU 는 `linear_before_reset=1` 이 필수다.** ONNX 기본값 0 과 우리 수식이 다른데, 빠뜨려도
파일은 정상이고 읽는 쪽도 오류를 내지 않는다 — **값만 달라진다**.

명령줄은 `nl export-onnx <프로젝트> --model <이름> [--out] [--batch N]` 이다.

**가져오기**는 선택 feature 다.

```sh
cargo build -p nl-engine --features onnx-import
```

`onnx_import::OnnxSession::{load, run}` 이고 **추론 전용**이다. 켜면 배포 바이너리가 **34 MiB**
늘어난다(`nl-runtime` 74 MiB 대비 +46%) — `tract-onnx` 에는 줄일 수 있는 optional 의존성이 없다.
끈 상태에서는 의존성 트리에 tract 가 아예 나타나지 않고, **CI 에도 켠 잡을 두지 않는다**.
실측과 판단 근거는 [`docs/research/onnx-2026-09-14.md`](../../docs/research/onnx-2026-09-14.md).

## 시험 게이트

| 환경 변수 | 하는 일 |
| --- | --- |
| `NL_TEST_GPU=1` | 이름에 `gpu` 가 든 시험을 켠다 |
| `NL_TEST_DEVICE=gpu` | 학습·추론 시험의 장치를 `Auto` 로 바꾼다 (상주 배치 경로를 탄다) |
| `NL_BENCH=1` | 벤치 시험(`bench_*`)을 켠다 |
| `NL_STEP_EVENT_MS` | `Step` 이벤트 최소 간격 (기본 50, 0 = 매 스텝) |
| `NL_NO_RESIDENT=1` | 상주 배치를 끈다 |
| `NL_WEIGHTS_FORCE=1` | 체크포인트의 모델 id 불일치를 무시한다 |

GPU 경로를 제대로 보려면 **둘 다** 줘야 한다. `NL_TEST_GPU=1` 만 주면 학습 시험은 여전히 CPU 다.

```sh
cargo test -p nl-engine
NL_TEST_GPU=1 NL_TEST_DEVICE=gpu cargo test -p nl-engine
NL_BENCH=1 cargo test --release -p nl-engine bench_ -- --nocapture
cargo test -p nl-engine --features onnx-import        # ONNX 가져오기 왕복
```

벤치는 **릴리스로 돌려야 한다.** 디버그에서는 순환 레이어 루프가 19배 느리다.

## 알려진 한계

- **순환 레이어는 길이 수백이 상한이다.** 시각마다 커널을 새로 띄우는 구조라 길이 512 면 스텝당
  2~3초다. 1000 스텝 에포크 하나가 한 시간 가까이 간다. 실측표는 `docs/ARCHITECTURE.md` 에 있다.
  짧은 쪽(32~128)에서는 통합 GPU 보다 CPU 가 3~4배 빠르다 — 시각당 약 5ms 가 커널 디스패치
  고정비인데 작은 행렬곱이 그 비용을 못 메운다. fused cell 도 절단 역전파(TBPTT)도 아직 없다.
- **개발 PC 의 RTX 2060 은 오픈소스 NVK 드라이버라 wgpu 컴퓨트가 죽는다**
  (`ComputePipeline ... is invalid`). SPIR-V 와 WGSL 둘 다 같은 결과라 드라이버 문제다.
  `probe` 가 이걸 걸러 `Auto` 가 Intel UHD 630 을 고른다. 다른 기기에서는 정상 동작한다.
- **f32 만 다룬다.** 체크포인트는 mmap 으로 매핑하지만 텐서 본문은 f32 `Vec` 으로 한 번 복사한다
  (M15). zero-copy 는 dtype 일반화가 먼저다.
- **CSV 훑기의 추정 모드에서는 클래스 목록이 표본 기준**이라 뒤쪽에만 나오는 클래스가 빠질 수 있다
  (M17). 그래서 그때는 `empty_classes` 를 비워 둔다.
- **학습은 프로세스당 하나를 전제로 쓰였다.** 여러 학습을 동시에 띄우면 장치 메모리를 두고 다툰다.
- **ONNX 가져오기의 실행 계획은 첫 `run` 때 만든다.** 동적 배치가 기호로 남아 있으면 그대로는
  최적화할 수 없고, 실제로 tract 는 기호 배치가 남은 LSTM 을 최적화하려다 패닉한다.
  그래서 계획 생성을 `catch_unwind` 로 감쌌다.
