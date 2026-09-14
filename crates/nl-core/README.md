# nl-core

**문서 모델**과 그 위의 순수 로직. 그래프 · 형상 추론 · 편집 op/undo · 검증 · 샘플 · 템플릿.

의존성이 serde · uuid · chrono · thiserror 뿐이다. **GUI 도 네트워크도 ML 프레임워크도 들어오지
않는다.** 빌더(`nl-app`)와 배포 런타임(`nl-runtime`)이 같은 문서를 같은 코드로 다루게 하려는
것이고, 그래서 이 크레이트의 시험은 빠르고 결정적이다.

전체 설계는 [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md), 사용자 관점 설명은
[`docs/GUIDE.md`](../../docs/GUIDE.md) 에 있다.

## 모듈 지도

| 파일 | 하는 일 |
| --- | --- |
| `model.rs` | `Project` · `ModelDef` · `Graph` · `LayerKind` · 파일 포맷 |
| `shape.rs` | 형상 추론. 위상정렬 + 레이어별 규칙 |
| `ops.rs` | 편집 op 24종과 역연산·diff (undo 의 바닥) |
| `validate.rs` | 저장·빌드 전 점검. 오류와 경고 목록 |
| `pipeline.rs` | 파이프라인 문서 모델 (소스·로직·싱크·링크) 과 토큰 규칙 |
| `payload.rs` | 페이로드 스펙 — 바깥 값과 텐서 사이의 변환 계획 |
| `dataset.rs` | 데이터셋 스펙과 훑기 결과(`DatasetInfo`) |
| `train.rs` | 학습 설정 · 실행 기록 · 장치 선호 |
| `gui.rs` | 배포 앱 화면 배치 (창 · 위젯 · 바인딩) |
| `bundle.rs` | `.nlapp` 번들 매니페스트와 빌드 스펙 |
| `templates.rs` | 팔레트에서 한 번에 넣는 노드 묶음 |
| `sample.rs` | 예제 프로젝트 (빌더와 `nl sample` 의 단일 소유자) |
| `ids.rs` | uuid 기반 newtype id |
| `lib.rs` | 공개 API |

## 공개 API

`lib.rs` 의 re-export 가 계약이다. 자주 쓰는 것만 추리면 이렇다.

| 이름 | 쓰임 |
| --- | --- |
| `Project`, `ProjectFile`, `ProjectSettings`, `FORMAT_VERSION` | 문서 전체와 파일 포맷 |
| `ModelDef`, `Graph`, `Node`, `Edge`, `Port` | 레이어 그래프 |
| `LayerKind`, `LayerSpec`, `LayerCategory`, `Act` | 레이어 종류와 정적 성질 |
| `infer`, `Shape`, `Dim`, `ShapeReport`, `GraphError`, `MAX_RANK` | 형상 추론 |
| `Op`, `apply_op`, `apply_ops`, `inverse_ops`, `diff_ops` | 편집과 undo |
| `validate`, `Issue`, `Severity` | 검증 |
| `Pipeline`, `PNode`, `PNodeKind`, `Source`, `Sink`, `Logic`, `Link` | 파이프라인 |
| `host_of_bind`, `is_loopback_bind`, `new_token`, `TOKEN_LEN` | 바인드 주소와 토큰 |
| `PayloadSpec`, `Field`, `FieldKind`, `Transform`, `Dtype` | 페이로드 |
| `DatasetSpec`, `DataSource`, `Split` | 데이터셋 |
| `TrainConfig`, `Optimizer`, `Loss`, `Metric`, `LrSchedule`, `DevicePref` | 학습 설정 |
| `RunRecord`, `RunStatus`, `EpochMetrics` | 학습 기록 |
| `GuiLayout`, `WindowSpec`, `Widget`, `WidgetKind`, `Binding` | 배포 앱 화면 |
| `BuildSpec`, `BuildTarget`, `BundleManifest` | 번들 |
| `TemplateSpec`, `TemplateParams`, `TemplateError` | 레이어 템플릿 |
| `SAMPLES`, `new_project`, `xor_project`, `quadrants_cnn_project` | 샘플 |

## 문서 모델

`Project` 는 `models` · `datasets` · `payloads` · `pipelines` · `gui` · `runs` · `settings` 다.
**모든 컬렉션이 `BTreeMap<Id, _>`** 인 것이 중요하다 — 순회가 결정적이라 레이아웃도 시험도 diff 도
실행마다 흔들리지 않는다.

**새 필드는 반드시 `#[serde(default)]`** 를 붙인다. 옛 문서가 그대로 열려야 한다.

**미지 필드는 보존된다.** `ProjectFile`·`Project`·`ModelDef`·`Graph`·`Node`·`PNode`·`Widget` 에
`#[serde(flatten)] extra: BTreeMap<String, Value>` 가 있어, 더 새로운 버전이 만든 모르는 필드도 열고 저장하면
그대로 남는다(비어 있으면 직렬화에 나타나지 않는다). 다만 이 앱이 그 값을 해석·편집하지는 않으며,
`diff_ops` 도 `extra` 를 비교하지 않는다. `ProjectFile::newer_than_app()` 이 참이면 호출자가
"보존되지만 편집할 수 없다" 는 경고를 띄운다.

**경계가 있다.** `extra` 는 위의 **컨테이너·그래프 구조체에만** 있다. `ProjectSettings` ·
`DatasetSpec` · `PayloadSpec` · `Pipeline` · `TrainConfig` · `RunRecord` · `Field` · `Link` ·
`GuiLayout` · `WindowSpec` 같은 잎 구조체는 아직 없어서, 그 **안에** 더해진 모르는 필드는
저장할 때 사라진다. 새 버전이 설정이나 학습 설정에 항목을 더하면 그쪽이 먼저 걸린다.
엔진 관점 시험(`nl-engine` 의 `unknown_fields_survive_training_and_a_save`)이 이 경계를 고정해
둔다 — 넓히면 그 시험이 깨지므로 함께 고치면 된다.

## 그래프 규칙

- `Edge { from: NodeId, to: Port }`, `Port { node, slot }`. 다입력 레이어(`Add`·`Mul`·`Concat`)는
  `slot` 으로 가른다.
- `add_edge` 는 자기참조 · 중복 · 이미 점유된 슬롯 · 슬롯 범위 밖만 거부한다.
  **순환은 허용한다** — 문서를 깨뜨리지 않고 `infer`/`validate` 가 감지해 보고한다.
  UI 는 `would_create_cycle` 로 미리 막는다.
- **`input_nodes()`/`output_nodes()` 는 이름순**이다(이름이 같으면 id 순). 이 순서가 엔진의 입출력
  텐서 순서이자 페이로드 필드 순서다. 다입출력 모델은 노드에 이름을 붙여야 순서가 안정된다.
- `LayerKind::spec()` 이 입력 슬롯 수 · 출력 유무 · 파라미터 유무 · 표시 색의 **단일 소유자**다.
  **새 레이어 추가 = 여기 enum 변형 + `spec` + `shape` 규칙 + 엔진의 연산 하나.**

## 형상 추론

`infer(&Graph) -> ShapeReport { shapes, errors, order, cycle_nodes }`.

- `Shape = Vec<Dim>`, `Dim::{Batch, Fixed(usize)}`. **배치는 기호로 둔다** — 배치 크기와 무관하게
  추론한다. `Shape::sample()` 이 배치를 뺀 형상이다.
- 위상정렬(Kahn)로 돌고, 순환 노드는 `GraphError::Cycle` 로 격리한다. 슬롯 누락은 `MissingInput`,
  규칙 위반은 `ShapeMismatch`, 값 자체가 잘못되면 `InvalidParam` 이다.
- 오류 노드의 하류는 `UpstreamError` 다 — 한 곳이 틀렸다고 그래프 전체가 빨개지지 않는다.
- **랭크 상한은 `MAX_RANK = 5`** (배치 포함). 엔진의 `DynTensor` 가 R1..R5 라 그 이상은 실행할 수
  없다. 형상을 늘리는 규칙마다 `checked()` 로 걸러 낸다.

주요 규칙만 추리면 이렇다.

| 레이어 | 입력 → 출력 |
| --- | --- |
| `Linear { out_features }` | `[B, …, in]` → 마지막 차원만 교체 (랭크 2 이상) |
| `Conv2d` | `[B, C, H, W]` → `[B, out_channels, H', W']` |
| `MaxPool2d`/`AvgPool2d` | `[B, C, H, W]` → `[B, C, H', W']` (내림) |
| `GlobalAvgPool` | `[B, C, H, W]` → `[B, C]` |
| `Flatten` | `[B, a, b, c]` → `[B, a·b·c]` |
| `Add`/`Mul` | 두 입력 형상이 **정확히 같아야** 한다 |
| `Concat { dim }` | `dim` 축만 다르면 되고 그 축이 더해진다 (`dim` 은 샘플 기준) |
| `Embedding { vocab, dim }` | `[B, L]` → `[B, L, dim]` |
| `Lstm`/`Gru` | `[B, L, D]` → `return_sequence` 면 `[B, L, H']`, 아니면 `[B, H']` (양방향이면 `H' = 2H`) |
| `MultiHeadAttention { heads }` | `[B, L, D]` → 같은 형상. `D % heads == 0` 이어야 한다 |
| `Output`·`Activation`·`Dropout`·`LayerNorm` | 항등 |

## 편집 op 와 undo

`Op` 는 24 변형이고 계약은 셋이다.

- **`apply_op` 은 항상 성공한다.** 실패하지 않고 수렴을 지향한다(멱등). 없는 것을 지우면 아무 일도
  일어나지 않고, 있는 것을 다시 넣으면 덮어쓴다. 그래서 순서가 어긋나도 문서가 깨지지 않는다.
- **`inverse_ops(p, ops)` 는 `ops` 를 적용하기 전 상태에서** 역연산을 만든다. 돌려주는 순서는 이미
  뒤집혀 있어 그대로 적용하면 된다.
- **`diff_ops(from, to)`** 가 두 스냅샷의 차이를 op 목록으로 준다. 드래그처럼 한 동작이 여러
  변경을 만드는 경우(burst 편집)를 한 번의 undo 로 묶는 데 쓴다.

`Upsert*` 와 `Delete*` 가 짝을 이룬다. 모델 · 노드 · 엣지 · 데이터셋 · 페이로드 · 파이프라인 ·
파이프라인 노드 · 링크 · 위젯 · 실행 기록이 각각 있고, 그 밖에 `SetProjectMeta` · `SetSettings` ·
`SetTrainConfig` · `SetGuiWindow` 가 있다.

노드를 지우면 붙어 있던 엣지도 함께 사라진다. 역연산은 그 엣지들까지 되살린다.

## 검증

`validate(&Project) -> Vec<Issue>`. `Issue { severity, at, message }` 이고 `Severity::{Error, Warning}` 다.
`at` 은 `Where` 로 모델 · 노드 · 파이프라인 노드 중 어디인지 가리킨다.

**모델**

| 규칙 | 등급 |
| --- | --- |
| 모델이 비어 있음 | 경고 |
| Input 노드가 없음 | 오류 |
| Output 노드가 없음 | 오류 |
| 형상 추론 오류 (노드별) | 오류 |
| 출력에 연결되지 않은 노드 | 경고 |
| 학습 데이터셋 · 페이로드가 프로젝트에 없음 | 오류 |

**파이프라인**

| 규칙 | 등급 |
| --- | --- |
| HTTP 서버가 마우스·키보드를 구동하는데 **토큰이 없음** | 오류 |
| 같은 조합인데 토큰이 있음 | 경고 |
| 참조하는 모델 · 페이로드가 없음 | 오류 |
| 모델에 학습된 가중치가 없음 | 경고 |
| HTTP 응답 싱크가 가리키는 서버 노드가 이 파이프라인에 없음 | 오류 |
| HTTP 응답 싱크가 가리키는 노드가 HTTP 서버가 아님 | 오류 |
| 바깥에서 닿는 주소인데 **토큰이 없음** | 오류 |
| 바깥에서 닿는 주소인데 TLS 가 없음 | 경고 |
| HTTP 서버에 짝이 되는 응답 싱크가 없음 (모든 요청이 시간 초과) | 경고 |
| 노드의 입력 · 출력이 연결되지 않음 | 경고 |

원격 요청 하나로 남의 컴퓨터를 조작하게 되는 조합만 **오류**로 막는다. 나머지는 경고다 —
검증기가 작업을 가로막으면 사람이 검증기를 끄기 때문이다.

## 샘플

`SAMPLES` 가 목록이고 빌더의 "샘플 열기" 와 `nl sample` 이 **같은 것**을 쓴다.

| 이름 | 내용 |
| --- | --- |
| XOR 분류 (MLP) | `xor_project` — 2입력 MLP, 합성 XOR, 추론 API 파이프라인 |
| 사분면 분류 (CNN) | `quadrants_cnn_project` — 8×8 이미지 CNN |

`new_project()` 는 "새로 만들기" 의 빈 뼈대다 (Input → Output 만).

id 는 `from_u128` 으로 **고정**한다 — 캔버스 배치 · 시험 · 스크린샷이 실행마다 흔들리지 않는다.
(`Project::created` 만은 만든 시각이라 호출마다 다르다.)

## 레이어 템플릿

`templates::list()` 가 팔레트 항목을, `instantiate(name, at, &params)` 가 노드·엣지 묶음을 준다.
문서에 넣는 것은 호출자가 `Op::UpsertNode`/`UpsertEdge` 로 하므로 **undo 한 번에 묶인다**.

| 이름 | 파라미터 | 노드 |
| --- | --- | --- |
| `residual_block` | `width` | Linear → ReLU → Linear → Add |
| `transformer_block` | `d_model`, `heads`, `ff_mult` | LN → 어텐션 → Add → LN → Linear → GELU → Linear → Add |
| `conv_block` | `channels` | Conv2d → BatchNorm → ReLU → MaxPool |

규약이 둘 있다.

- **마지막 노드가 블록 출력**이다.
- **엣지가 붙지 않은 입력 슬롯이 블록 입력**이다(`open_inputs`). 잔차가 있는 두 템플릿은 그런 슬롯이
  **둘**이고(본줄기와 우회로) **둘 다 같은 상류에 이어야** 형상이 맞는다.

`transformer_block` 이 `d_model` 을 따로 받는 이유는 피드포워드의 마지막 `Linear` 가 입력 폭으로
돌아와야 잔차와 더할 수 있기 때문이다.

## 시험

```sh
cargo test -p nl-core
```

의존성이 가벼워 몇 초면 끝난다. ML 백엔드도 GUI 도 없으므로 게이트가 필요 없다.
