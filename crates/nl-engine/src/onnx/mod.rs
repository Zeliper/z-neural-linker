//! ONNX 내보내기.
//!
//! 우리 `ModelDef` + safetensors 가중치를 **opset 17** ONNX 파일로 쓴다. 읽기(가져오기)는 아직 없다.
//!
//! ## 왜 opset 17 인가
//! `LayerNormalization` 이 17 에서 들어왔고, `Softmax`/`LogSoftmax` 가 축을 실제 축으로 해석하는 것이
//! 13 부터다. 위로는 검증에 쓰는 `tract-onnx` 의 보장 범위가 18 까지라, 17 이 둘을 모두 만족하는
//! 가장 넓은 지점이다. 그래서 opset 20 의 `Gelu` 와 opset 23 의 `Attention` 은 쓰지 않고 전개한다.
//!
//! ## 배치 차원
//! 우리 `Shape` 는 배치를 `Dim::Batch` 기호로 둔다. 기본값 [`Batch::Dynamic`] 은 이것을 ONNX 의
//! `dim_param = "B"` 로 내보내므로 배치 크기와 무관한 모델이 된다. [`Batch::Fixed`] 는 고정 정수로
//! 박는다 — 배치를 못 바꾸는 대신 런타임이 형상을 더 강하게 최적화할 수 있다.
//!
//! ## 아직 안 되는 것
//! `Lstm`·`Gru`·`MultiHeadAttention` 은 [`ExportReport::unsupported`] 에 담겨 나온다. 게이트 순서
//! 재배열과 레이아웃 변환이 필요해 따로 다룬다. 그 세 종이 그래프에 있으면 내보내기 자체가 실패한다 —
//! 조용히 빠뜨린 모델을 내주면 값이 틀린 채로 돌아가기 때문이다.
//!
//! ## 검증
//! `tract-onnx` 를 **dev-dependency 로만** 두고 통합 테스트에서 왕복 비교한다
//! (`tests/onnx.rs`). 배포 바이너리에는 들어가지 않는다.

pub mod pb;

use crate::tensor::HostTensor;
use crate::weights;
use anyhow::{bail, Context, Result};
use nl_core::model::{Act, LayerKind, ModelDef};
use nl_core::shape::{self, Dim, Shape, ShapeReport};
use nl_core::NodeId;
use prost::Message;
use std::collections::BTreeMap;
use std::path::Path;

/// `TensorProto::data_type` 의 f32.
const F32: i32 = pb::tensor_proto::DataType::Float as i32;
/// `TensorProto::data_type` 의 i64.
const I64: i32 = pb::tensor_proto::DataType::Int64 as i32;

/// 우리가 겨냥하는 기본 opset.
pub const DEFAULT_OPSET: i64 = 17;

/// ONNX IR 버전. opset 17~18 세대가 쓰는 값이다 (ONNX 1.12~1.13).
const IR_VERSION: i64 = 8;

/// 그래프의 배치 차원을 어떻게 쓸 것인가.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Batch {
    /// `dim_param = "B"` 기호로 둔다. 배치 크기를 바꿔 가며 쓸 수 있다.
    #[default]
    Dynamic,
    /// 고정 정수로 박는다.
    Fixed(usize),
}

/// 내보내기 설정.
#[derive(Clone, Copy, Debug)]
pub struct ExportOptions {
    /// 목표 opset. [`DEFAULT_OPSET`] 말고 다른 값을 넣으면 경고만 하고 그대로 쓴다 —
    /// 연산자 선택은 17 기준이라 낮추면 읽는 쪽에서 깨질 수 있다.
    pub opset: i64,
    pub batch: Batch,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            opset: DEFAULT_OPSET,
            batch: Batch::Dynamic,
        }
    }
}

/// 내보낸 결과 요약.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// 쓴 ONNX 노드 수. 우리 레이어 하나가 여러 노드가 되기도 한다(`Linear` → `MatMul`+`Add`).
    pub nodes: usize,
    /// 이니셜라이저(가중치·상수) 수.
    pub initializers: usize,
    /// 아직 내보낼 수 없는 레이어들. 비어 있지 않으면 [`export`] 가 실패한다.
    pub unsupported: Vec<String>,
}

/// 모델과 가중치를 ONNX 파일로 쓴다.
///
/// `weights` 는 safetensors 체크포인트다. 그래프 형상 추론이 통과해야 하고, 쓰는 모든 파라미터가
/// 체크포인트에 있어야 한다.
///
/// 지원하지 않는 레이어가 하나라도 있으면 **파일을 쓰지 않고 오류**를 낸다. 오류 메시지에 어떤
/// 레이어인지 담기고, 같은 목록이 [`ExportReport::unsupported`] 형태로도 들어간다.
pub fn export(model: &ModelDef, weights: &Path, out: &Path, opts: ExportOptions) -> Result<ExportReport> {
    let rep = shape::infer(&model.graph);
    if !rep.is_ok() {
        let mut msgs: Vec<String> = rep
            .errors
            .iter()
            .map(|(id, e)| format!("{}: {e}", label(model, *id)))
            .collect();
        msgs.sort();
        bail!(
            "형상 추론이 통과하지 못한 그래프는 내보낼 수 없습니다 — {}",
            msgs.join(", ")
        );
    }

    let unsupported = unsupported_layers(model, &rep);
    if !unsupported.is_empty() {
        bail!(
            "아직 ONNX 로 내보낼 수 없는 레이어가 있습니다: {}. \
             순환·어텐션 레이어는 게이트 순서와 레이아웃 변환이 필요해 준비 중입니다",
            unsupported.join(", ")
        );
    }

    // 없는 파일은 여기서 먼저 잡는다 — mmap 실패가 연쇄된 메시지보다 "파일이 없습니다" 가 읽기 좋다.
    // 경로 축약은 `weights::load_for` 도 하므로(`paths::short`) 이건 순전히 메시지 품질 때문이다.
    if !weights.is_file() {
        bail!(
            "가중치 파일이 없습니다: {} — 먼저 학습하거나 다른 체크포인트를 지정하세요",
            crate::paths::short(weights)
        );
    }
    let params = weights::load_for(weights, Some(model.id))
        .with_context(|| format!("가중치를 읽을 수 없습니다: {}", crate::paths::short(weights)))?;

    let mut b = Builder::new(model, &rep, &params, opts);
    b.build()?;
    let proto = b.finish()?;

    let bytes = proto.encode_to_vec();
    write_atomic(out, &bytes).with_context(|| format!("ONNX 파일을 쓸 수 없습니다: {}", crate::paths::short(out)))?;

    Ok(ExportReport {
        nodes: proto.graph.as_ref().map_or(0, |g| g.node.len()),
        initializers: proto.graph.as_ref().map_or(0, |g| g.initializer.len()),
        unsupported: Vec::new(),
    })
}

/// 내보내기 전에 지원 여부만 본다. 앱이 메뉴를 회색으로 만들 때 쓴다.
///
/// 가중치 파일을 읽지 않으므로 학습 전에도 부를 수 있다.
pub fn check(model: &ModelDef) -> ExportReport {
    let rep = shape::infer(&model.graph);
    ExportReport {
        nodes: 0,
        initializers: 0,
        unsupported: unsupported_layers(model, &rep),
    }
}

/// 아직 내보낼 수 없는 레이어. **지금은 없다** — 20종 전부 매핑되어 있다.
///
/// 새 `LayerKind` 가 core 에 들어오면 [`supported`] 의 match 가 컴파일 오류를 내서,
/// 조용히 빠뜨린 모델이 나가는 일이 없게 한다.
fn unsupported_layers(model: &ModelDef, _rep: &ShapeReport) -> Vec<String> {
    let mut v: Vec<String> = model
        .graph
        .nodes
        .values()
        .filter(|n| !supported(&n.kind))
        .map(|n| format!("{} ({})", n.display_name(), n.kind.spec().label))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// 이 레이어를 ONNX 로 옮길 수 있는가.
///
/// **모든 변형을 빠짐없이 적는다.** `_ => false` 로 두면 새 레이어가 core 에 들어왔을 때
/// 컴파일러가 알려 주지 않고, 사용자는 "왜 이 레이어만 빠지지" 를 런타임에 알게 된다.
fn supported(kind: &LayerKind) -> bool {
    match kind {
        LayerKind::Input { .. }
        | LayerKind::Output
        | LayerKind::Linear { .. }
        | LayerKind::Conv2d { .. }
        | LayerKind::MaxPool2d { .. }
        | LayerKind::AvgPool2d { .. }
        | LayerKind::GlobalAvgPool
        | LayerKind::Flatten
        | LayerKind::Reshape { .. }
        | LayerKind::Activation { .. }
        | LayerKind::Dropout { .. }
        | LayerKind::BatchNorm { .. }
        | LayerKind::LayerNorm { .. }
        | LayerKind::Add
        | LayerKind::Mul
        | LayerKind::Concat { .. }
        | LayerKind::Embedding { .. }
        | LayerKind::Lstm { .. }
        | LayerKind::Gru { .. }
        | LayerKind::MultiHeadAttention { .. } => true,
    }
}

fn label(model: &ModelDef, id: NodeId) -> String {
    model
        .graph
        .nodes
        .get(&id)
        .map(|n| n.display_name())
        .unwrap_or_else(|| id.short())
}

fn write_atomic(out: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = out.with_extension("onnx.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, out)?;
    Ok(())
}

// ───────────────────────────── 순환 셀 ─────────────────────────────

/// 순환 셀 종류. 게이트 순서 변환표를 들고 있다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    Lstm,
    Gru,
}

/// 순환 가중치를 ONNX 로 옮길 때 늘 함께 다니는 셋.
struct RnnLayout<'o> {
    /// 방향 목록. 정방향(`false`) 먼저 — ONNX 방향 축도 0 이 정방향이다.
    dirs: &'o [bool],
    hidden: usize,
    /// ONNX 게이트 블록 `i` 번째가 우리 블록 몇 번째인지.
    order: &'o [usize],
}

impl RnnLayout<'_> {
    /// `gates · hidden` — W/R 의 행 수이자 편향 절반의 길이.
    fn rows(&self) -> usize {
        self.order.len() * self.hidden
    }
}

impl Cell {
    fn op(self) -> &'static str {
        match self {
            Cell::Lstm => "LSTM",
            Cell::Gru => "GRU",
        }
    }

    /// ONNX 의 게이트 블록 `i` 번째가 우리 블록 몇 번째인지.
    ///
    /// LSTM: 우리(PyTorch) `i,f,g,o` → ONNX `i,o,f,c`. `g` 와 `c` 는 같은 게이트다.
    /// GRU: 우리 `r,z,n` → ONNX `z,r,h`. 앞 둘을 맞바꾼다.
    fn gate_order(self) -> &'static [usize] {
        match self {
            Cell::Lstm => &[0, 3, 1, 2],
            Cell::Gru => &[1, 0, 2],
        }
    }
}

// ───────────────────────────── 그래프 조립 ─────────────────────────────

struct Builder<'a> {
    model: &'a ModelDef,
    rep: &'a ShapeReport,
    params: &'a BTreeMap<String, HostTensor>,
    opts: ExportOptions,
    nodes: Vec<pb::NodeProto>,
    initializers: Vec<pb::TensorProto>,
    /// 노드 id → 그 노드의 출력 값 이름. `Dropout` 은 상류 이름을 그대로 물려준다.
    values: BTreeMap<NodeId, String>,
    /// 이름 충돌을 막는 일련번호.
    seq: usize,
}

impl<'a> Builder<'a> {
    fn new(
        model: &'a ModelDef,
        rep: &'a ShapeReport,
        params: &'a BTreeMap<String, HostTensor>,
        opts: ExportOptions,
    ) -> Self {
        Self {
            model,
            rep,
            params,
            opts,
            nodes: Vec::new(),
            initializers: Vec::new(),
            values: BTreeMap::new(),
            seq: 0,
        }
    }

    fn build(&mut self) -> Result<()> {
        for &id in &self.rep.order {
            let kind = self.model.graph.nodes[&id].kind.clone();
            self.emit(id, &kind)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<pb::ModelProto> {
        let g = self.model.graph.clone();
        let mut input = Vec::new();
        for id in g.input_nodes() {
            let name = self.values.get(&id).context("입력 노드의 값 이름이 없습니다")?;
            input.push(self.value_info(name, self.shape_of(id)?, F32));
        }
        let mut output = Vec::new();
        for id in g.output_nodes() {
            let name = self.values.get(&id).context("출력 노드의 값 이름이 없습니다")?;
            output.push(self.value_info(name, self.shape_of(id)?, F32));
        }
        if input.is_empty() {
            bail!("Input 노드가 없는 모델은 내보낼 수 없습니다");
        }
        if output.is_empty() {
            bail!("Output 노드가 없는 모델은 내보낼 수 없습니다");
        }

        let graph = pb::GraphProto {
            node: self.nodes,
            name: if self.model.name.is_empty() {
                "model".into()
            } else {
                self.model.name.clone()
            },
            initializer: self.initializers,
            input,
            output,
            ..Default::default()
        };
        Ok(pb::ModelProto {
            ir_version: IR_VERSION,
            producer_name: "neural-linker".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            opset_import: vec![pb::OperatorSetIdProto {
                domain: String::new(), // 기본 도메인 ai.onnx
                version: self.opts.opset,
            }],
            graph: Some(graph),
            ..Default::default()
        })
    }

    // ── 이름 ──

    fn fresh(&mut self, hint: &str) -> String {
        self.seq += 1;
        format!("{hint}_{}", self.seq)
    }

    /// 노드의 출력 값 이름. 사람이 읽을 수 있게 표시 이름을 섞는다.
    fn out_name(&mut self, id: NodeId, kind: &LayerKind) -> String {
        let n = &self.model.graph.nodes[&id];
        let base = if n.name.is_empty() {
            kind.spec().label.to_string()
        } else {
            n.name.clone()
        };
        let base: String = base.chars().map(|c| if c.is_whitespace() { '_' } else { c }).collect();
        self.fresh(&base)
    }

    /// 입력 슬롯 `slot` 의 상류 값 이름.
    fn input_of(&self, id: NodeId, slot: usize) -> Result<String> {
        let from = self
            .model
            .graph
            .inputs_of(id)
            .get(&slot)
            .copied()
            .with_context(|| format!("{} 의 입력 {}번이 비어 있습니다", label(self.model, id), slot + 1))?;
        self.values
            .get(&from)
            .cloned()
            .context("상류 노드가 아직 처리되지 않았습니다 (위상 순서 오류)")
    }

    /// 입력 슬롯 `slot` 상류의 **샘플 형상**(배치 제외).
    fn in_sample(&self, id: NodeId, slot: usize) -> Result<Vec<usize>> {
        let from = self
            .model
            .graph
            .inputs_of(id)
            .get(&slot)
            .copied()
            .with_context(|| format!("{} 의 입력 {}번이 비어 있습니다", label(self.model, id), slot + 1))?;
        Ok(self.shape_of(from)?.sample())
    }

    fn shape_of(&self, id: NodeId) -> Result<&Shape> {
        self.rep
            .shape(id)
            .with_context(|| format!("{} 의 형상을 알 수 없습니다", label(self.model, id)))
    }

    // ── 이니셜라이저 ──

    /// 학습 파라미터를 이니셜라이저로 넣고 그 이름을 돌려준다.
    fn param(&mut self, id: NodeId, part: &str) -> Result<String> {
        let key = crate::exec::param_name(id, part);
        let t = self.params.get(&key).with_context(|| {
            format!(
                "가중치에 '{}' 가 없습니다 ({} 의 {part}) — 체크포인트가 이 그래프의 것인지 확인하세요",
                key,
                label(self.model, id)
            )
        })?;
        let name = self.fresh(part);
        self.initializers.push(float_tensor(&name, &t.shape, &t.data));
        Ok(name)
    }

    fn const_i64(&mut self, hint: &str, values: &[i64]) -> String {
        let name = self.fresh(hint);
        self.initializers.push(pb::TensorProto {
            dims: vec![values.len() as i64],
            data_type: I64,
            name: name.clone(),
            raw_data: values.iter().flat_map(|v| v.to_le_bytes()).collect(),
            ..Default::default()
        });
        name
    }

    fn const_f32(&mut self, hint: &str, value: f32) -> String {
        let name = self.fresh(hint);
        self.initializers.push(float_tensor(&name, &[], &[value]));
        name
    }

    // ── 노드 ──

    fn push(&mut self, op: &str, inputs: Vec<String>, out: String, attrs: Vec<pb::AttributeProto>) {
        let name = format!("{op}_{}", self.nodes.len());
        self.nodes.push(pb::NodeProto {
            input: inputs,
            output: vec![out],
            name,
            op_type: op.into(),
            attribute: attrs,
            ..Default::default()
        });
    }

    /// 출력이 여럿인 연산자(`LSTM`·`GRU`)용. 안 쓰는 출력은 빈 이름으로 둔다.
    fn push_multi(&mut self, op: &str, inputs: Vec<String>, outputs: Vec<String>, attrs: Vec<pb::AttributeProto>) {
        let name = format!("{op}_{}", self.nodes.len());
        self.nodes.push(pb::NodeProto {
            input: inputs,
            output: outputs,
            name,
            op_type: op.into(),
            attribute: attrs,
            ..Default::default()
        });
    }

    /// 연산자 하나를 붙이고 새 값 이름을 돌려준다 (중간 값용).
    fn op1(&mut self, op: &str, inputs: Vec<String>, hint: &str, attrs: Vec<pb::AttributeProto>) -> String {
        let out = self.fresh(hint);
        self.push(op, inputs, out.clone(), attrs);
        out
    }

    fn value_info(&self, name: &str, shape: &Shape, elem: i32) -> pb::ValueInfoProto {
        let dim = shape
            .0
            .iter()
            .map(|d| pb::tensor_shape_proto::Dimension {
                value: Some(match (d, self.opts.batch) {
                    (Dim::Fixed(n), _) => pb::tensor_shape_proto::dimension::Value::DimValue(*n as i64),
                    (Dim::Batch, Batch::Fixed(n)) => pb::tensor_shape_proto::dimension::Value::DimValue(n as i64),
                    (Dim::Batch, Batch::Dynamic) => pb::tensor_shape_proto::dimension::Value::DimParam("B".into()),
                }),
                ..Default::default()
            })
            .collect();
        pb::ValueInfoProto {
            name: name.into(),
            r#type: Some(pb::TypeProto {
                value: Some(pb::type_proto::Value::TensorType(pb::type_proto::Tensor {
                    elem_type: elem,
                    shape: Some(pb::TensorShapeProto { dim }),
                })),
                denotation: String::new(),
            }),
            ..Default::default()
        }
    }

    // ── 레이어별 변환 ──

    fn emit(&mut self, id: NodeId, kind: &LayerKind) -> Result<()> {
        let out = match kind {
            // 그래프 입력. 노드가 아니라 이름만 잡아 둔다.
            LayerKind::Input { .. } => self.out_name(id, kind),

            // 항등이지만 그래프 출력 이름을 따로 갖게 Identity 를 둔다.
            // (Output 둘이 같은 상류를 볼 때 이름이 겹치지 않는다.)
            LayerKind::Output => {
                let x = self.input_of(id, 0)?;
                let out = self.out_name(id, kind);
                self.push("Identity", vec![x], out.clone(), vec![]);
                out
            }

            // 추론에서 항등이라 노드를 만들지 않고 상류 이름을 물려준다.
            LayerKind::Dropout { .. } => self.input_of(id, 0)?,

            LayerKind::Linear { bias, .. } => {
                let x = self.input_of(id, 0)?;
                let w = self.param(id, crate::exec::P_WEIGHT)?;
                // 우리 weight 는 `[in, out]` 이라 ONNX MatMul 의 B 와 그대로 맞는다 (전치 불필요).
                // Gemm 은 2D 만 받으므로 `[B, L, D]` 도 되는 MatMul 로 통일한다.
                let y = self.op1("MatMul", vec![x, w], "matmul", vec![]);
                if *bias {
                    let b = self.param(id, crate::exec::P_BIAS)?;
                    let out = self.out_name(id, kind);
                    self.push("Add", vec![y, b], out.clone(), vec![]);
                    out
                } else {
                    y
                }
            }

            LayerKind::Conv2d {
                kernel,
                stride,
                padding,
                bias,
                ..
            } => {
                let x = self.input_of(id, 0)?;
                // 우리 weight 는 `[out, C, kh, kw]` 로 ONNX Conv 의 W 와 같다 (변환 불필요).
                let w = self.param(id, crate::exec::P_WEIGHT)?;
                let mut ins = vec![x, w];
                if *bias {
                    ins.push(self.param(id, crate::exec::P_BIAS)?);
                }
                let out = self.out_name(id, kind);
                self.push(
                    "Conv",
                    ins,
                    out.clone(),
                    vec![
                        ints("kernel_shape", &[kernel[0] as i64, kernel[1] as i64]),
                        ints("strides", &[stride[0] as i64, stride[1] as i64]),
                        // ONNX pads 는 [시작…, 끝…] 순이다.
                        ints(
                            "pads",
                            &[
                                padding[0] as i64,
                                padding[1] as i64,
                                padding[0] as i64,
                                padding[1] as i64,
                            ],
                        ),
                        ints("dilations", &[1, 1]),
                        int("group", 1),
                    ],
                );
                out
            }

            LayerKind::MaxPool2d { kernel, stride } | LayerKind::AvgPool2d { kernel, stride } => {
                let x = self.input_of(id, 0)?;
                let op = if matches!(kind, LayerKind::MaxPool2d { .. }) {
                    "MaxPool"
                } else {
                    "AveragePool"
                };
                let out = self.out_name(id, kind);
                // 우리 conv_out 은 내림이라 ceil_mode=0 (기본)이 맞다. 패딩은 쓰지 않는다.
                self.push(
                    op,
                    vec![x],
                    out.clone(),
                    vec![
                        ints("kernel_shape", &[kernel[0] as i64, kernel[1] as i64]),
                        ints("strides", &[stride[0] as i64, stride[1] as i64]),
                        ints("pads", &[0, 0, 0, 0]),
                    ],
                );
                out
            }

            LayerKind::GlobalAvgPool => {
                let x = self.input_of(id, 0)?;
                // ONNX 는 `[N, C, 1, 1]` 을 내지만 우리는 `[N, C]` 다 — Reshape 로 뒤 두 축을 떨군다.
                let pooled = self.op1("GlobalAveragePool", vec![x], "gap", vec![]);
                let c = self.shape_of(id)?.sample()[0] as i64;
                let shape = self.const_i64("gap_shape", &[0, c]);
                let out = self.out_name(id, kind);
                self.push("Reshape", vec![pooled, shape], out.clone(), vec![]);
                out
            }

            LayerKind::Flatten => {
                let x = self.input_of(id, 0)?;
                let out = self.out_name(id, kind);
                // axis=1 → `[B, a, b, c]` 를 `[B, a*b*c]` 로. 의미가 정확히 같다.
                self.push("Flatten", vec![x], out.clone(), vec![int("axis", 1)]);
                out
            }

            LayerKind::Reshape { shape } => {
                let x = self.input_of(id, 0)?;
                // 첫 0 은 "그 자리 입력 차원 그대로" — 배치를 건드리지 않는다 (allowzero=0 기본).
                let mut dims = vec![0i64];
                dims.extend(shape.iter().map(|&d| d as i64));
                let target = self.const_i64("reshape_shape", &dims);
                let out = self.out_name(id, kind);
                self.push("Reshape", vec![x, target], out.clone(), vec![]);
                out
            }

            LayerKind::Activation { act } => {
                let x = self.input_of(id, 0)?;
                self.activation(id, kind, x, *act)?
            }

            LayerKind::BatchNorm { eps, .. } => {
                let x = self.input_of(id, 0)?;
                // 추론용 BatchNormalization 은 scale·B·mean·var 를 전부 요구한다.
                // running 통계를 우리가 이미 저장하고 있어 그대로 넘긴다.
                let gamma = self.param(id, crate::exec::P_GAMMA)?;
                let beta = self.param(id, crate::exec::P_BETA)?;
                let mean = self.param(id, crate::exec::P_RUNNING_MEAN)?;
                let var = self.param(id, crate::exec::P_RUNNING_VAR)?;
                let out = self.out_name(id, kind);
                self.push(
                    "BatchNormalization",
                    vec![x, gamma, beta, mean, var],
                    out.clone(),
                    vec![float("epsilon", *eps), int("training_mode", 0)],
                );
                out
            }

            LayerKind::LayerNorm { eps } => {
                let x = self.input_of(id, 0)?;
                let gamma = self.param(id, crate::exec::P_GAMMA)?;
                let beta = self.param(id, crate::exec::P_BETA)?;
                let out = self.out_name(id, kind);
                // opset 17 에서 들어온 연산자. 우리도 마지막 차원 기준이라 axis=-1 이다.
                self.push(
                    "LayerNormalization",
                    vec![x, gamma, beta],
                    out.clone(),
                    vec![int("axis", -1), float("epsilon", *eps)],
                );
                out
            }

            LayerKind::Add | LayerKind::Mul => {
                let a = self.input_of(id, 0)?;
                let b = self.input_of(id, 1)?;
                let op = if matches!(kind, LayerKind::Add) { "Add" } else { "Mul" };
                let out = self.out_name(id, kind);
                self.push(op, vec![a, b], out.clone(), vec![]);
                out
            }

            LayerKind::Concat { dim } => {
                let a = self.input_of(id, 0)?;
                let b = self.input_of(id, 1)?;
                let out = self.out_name(id, kind);
                // 우리 dim 은 샘플 축(0 = 첫 샘플 차원)이라 배치만큼 +1 한다.
                self.push("Concat", vec![a, b], out.clone(), vec![int("axis", *dim as i64 + 1)]);
                out
            }

            LayerKind::Embedding { .. } => {
                let x = self.input_of(id, 0)?;
                let table = self.param(id, crate::exec::P_WEIGHT)?;
                // 우리 인덱스는 f32 텐서다 — Gather 가 정수 인덱스를 요구하므로 Cast 를 앞에 둔다.
                let idx = self.op1("Cast", vec![x], "idx", vec![int("to", I64 as i64)]);
                let out = self.out_name(id, kind);
                self.push("Gather", vec![table, idx], out.clone(), vec![int("axis", 0)]);
                out
            }

            LayerKind::Lstm {
                hidden,
                bidirectional,
                return_sequence,
            } => self.recurrent(id, kind, Cell::Lstm, *hidden, *bidirectional, *return_sequence)?,

            LayerKind::Gru {
                hidden,
                bidirectional,
                return_sequence,
            } => self.recurrent(id, kind, Cell::Gru, *hidden, *bidirectional, *return_sequence)?,

            LayerKind::MultiHeadAttention { heads, .. } => self.attention(id, kind, *heads)?,
        };
        self.values.insert(id, out);
        Ok(())
    }

    // ── 순환 레이어 ──

    /// `Lstm`/`Gru` → ONNX `LSTM`/`GRU`.
    ///
    /// 우리 구현과 ONNX 사이에 **세 가지가 동시에 다르다** — 하나라도 빠뜨리면 오류 없이 값만 틀린다.
    ///
    /// 1. **게이트 순서.** 우리는 PyTorch 를 따라 `i,f,g,o`(LSTM)·`r,z,n`(GRU) 이고
    ///    ONNX 는 `i,o,f,c`·`z,r,h` 다. `hidden` 크기 블록 단위로 재배열한다([`Cell::gate_order`]).
    /// 2. **가중치 방향.** 우리 `weight_ih` 는 `[D, gates·H]`(burn 의 `x @ w`)이고
    ///    ONNX `W` 는 `[방향, gates·H, D]`(`Xt · Wᵀ`)다. 전치한 뒤 방향 축을 붙인다.
    /// 3. **배치 축 위치.** 우리는 `[B, L, D]`(batch-first), ONNX 기본은 `layout=0` 즉 `[L, B, D]` 다.
    ///    `layout=1` 을 선언하는 길도 있지만 **앞뒤에 `Transpose` 를 두는 쪽**을 쓴다 —
    ///    `layout` 은 opset 14 부터라 읽는 쪽을 가리고, `Transpose` 는 어디서나 돈다.
    ///
    /// 편향은 우리가 `bias_ih`·`bias_hh` 를 따로 두는데 ONNX `B` 도 `[Wb, Rb]` 를 이어 붙인
    /// `[방향, 2·gates·H]` 라 구조가 같다 — 각각 재배열해 잇는다.
    fn recurrent(
        &mut self,
        id: NodeId,
        kind: &LayerKind,
        cell: Cell,
        hidden: usize,
        bidirectional: bool,
        return_sequence: bool,
    ) -> Result<String> {
        let x = self.input_of(id, 0)?;
        let sample = self.in_sample(id, 0)?;
        if sample.len() != 2 {
            bail!(
                "{} 의 입력은 [L, D] 여야 합니다 (지금 {sample:?})",
                label(self.model, id)
            );
        }
        let d_in = sample[1];
        // 정방향 먼저 — ONNX 의 방향 축도 0 이 정방향이다.
        let dirs: Vec<bool> = if bidirectional { vec![false, true] } else { vec![false] };
        let n_dir = dirs.len();
        let lay = RnnLayout {
            dirs: &dirs,
            hidden,
            order: cell.gate_order(),
        };

        let w = self.rnn_matrix(id, crate::exec::P_WEIGHT_IH, &lay, d_in, "W")?;
        let r = self.rnn_matrix(id, crate::exec::P_WEIGHT_HH, &lay, hidden, "R")?;
        let b = self.rnn_bias(id, &lay)?;

        // [B, L, D] → [L, B, D]
        let seq_first = self.op1("Transpose", vec![x], "rnn_in", vec![ints("perm", &[1, 0, 2])]);

        let mut attrs = vec![
            int("hidden_size", hidden as i64),
            string("direction", if bidirectional { "bidirectional" } else { "forward" }),
        ];
        if cell == Cell::Gru {
            // **필수.** ONNX 기본값 0 은 리셋을 h(t-1) 에 먼저 곱하는 쪽인데, 우리 구현은
            // `n = tanh(gi_n + r ⊙ gh_n)` 이고 `gh_n` 이 bias_hh 를 이미 포함하므로 1 쪽이다.
            // 빠뜨리면 읽는 쪽이 오류 없이 다른 값을 낸다.
            attrs.push(int("linear_before_reset", 1));
        }

        let ins = vec![seq_first, w, r, b];
        let (raw, perm, merged): (String, &[i64], Vec<i64>) = if return_sequence {
            // Y: [L, 방향, B, H] → [B, L, 방향, H] → [B, L, 방향·H]
            let y = self.fresh("rnn_seq");
            self.push_multi(cell.op(), ins, vec![y.clone()], attrs);
            (y, &[2, 0, 1, 3], vec![0, 0, (n_dir * hidden) as i64])
        } else {
            // Y_h: [방향, B, H] → [B, 방향, H] → [B, 방향·H].
            // Y(첫 출력)는 쓰지 않으므로 빈 이름을 준다 — ONNX 가 정한 선택적 출력 표기다.
            let yh = self.fresh("rnn_last");
            self.push_multi(cell.op(), ins, vec![String::new(), yh.clone()], attrs);
            (yh, &[1, 0, 2], vec![0, (n_dir * hidden) as i64])
        };

        let moved = self.op1("Transpose", vec![raw], "rnn_bt", vec![ints("perm", perm)]);
        let shape = self.const_i64("rnn_shape", &merged);
        let out = self.out_name(id, kind);
        self.push("Reshape", vec![moved, shape], out.clone(), vec![]);
        Ok(out)
    }

    /// `weight_ih`/`weight_hh` 를 ONNX `W`/`R` 로. 결과는 `[방향, gates·H, in]`.
    fn rnn_matrix(&mut self, id: NodeId, base: &str, lay: &RnnLayout<'_>, n_in: usize, hint: &str) -> Result<String> {
        let rows = lay.rows();
        let mut data = Vec::with_capacity(lay.dirs.len() * rows * n_in);
        for &dir in lay.dirs {
            let key = crate::exec::rnn_name(id, base, dir);
            let t = self.rnn_param(&key, id, &[n_in, rows])?;
            data.extend(transpose_and_reorder(&t.data, n_in, rows, lay.hidden, lay.order));
        }
        let name = self.fresh(hint);
        self.initializers
            .push(float_tensor(&name, &[lay.dirs.len(), rows, n_in], &data));
        Ok(name)
    }

    /// `bias_ih`·`bias_hh` 를 ONNX `B` 로. 결과는 `[방향, 2·gates·H]` = `[Wb, Rb]`.
    fn rnn_bias(&mut self, id: NodeId, lay: &RnnLayout<'_>) -> Result<String> {
        let width = lay.rows();
        let mut data = Vec::with_capacity(lay.dirs.len() * width * 2);
        for &dir in lay.dirs {
            for base in [crate::exec::P_BIAS_IH, crate::exec::P_BIAS_HH] {
                let key = crate::exec::rnn_name(id, base, dir);
                let t = self.rnn_param(&key, id, &[width])?;
                data.extend(reorder_blocks(&t.data, lay.hidden, lay.order));
            }
        }
        let name = self.fresh("B");
        self.initializers
            .push(float_tensor(&name, &[lay.dirs.len(), width * 2], &data));
        Ok(name)
    }

    /// 순환 파라미터 하나를 형상까지 확인해 꺼낸다.
    fn rnn_param(&self, key: &str, id: NodeId, want: &[usize]) -> Result<&'a HostTensor> {
        let t = self.params.get(key).with_context(|| {
            format!(
                "가중치에 '{key}' 가 없습니다 ({}) — 체크포인트가 이 그래프의 것인지 확인하세요",
                label(self.model, id)
            )
        })?;
        if t.shape != want {
            bail!("'{key}' 의 형상이 {:?} 인데 {want:?} 를 기대했습니다", t.shape);
        }
        Ok(t)
    }

    // ── 어텐션 ──

    /// `MultiHeadAttention` → 표준 연산자로 분해.
    ///
    /// ONNX 의 `Attention` 은 opset 23 이라 17 에서는 못 쓴다. `com.microsoft` 쪽 확장은
    /// ONNX Runtime 전용이라 이식성이 없다. 그래서 `exec.rs` 의 `self_attention` 과 **같은 순서로**
    /// MatMul·Reshape·Transpose·Softmax 를 늘어놓는다.
    ///
    /// `dropout` 은 추론에서 항등이라 내보내지 않는다.
    fn attention(&mut self, id: NodeId, kind: &LayerKind, heads: usize) -> Result<String> {
        let x = self.input_of(id, 0)?;
        let sample = self.in_sample(id, 0)?;
        if sample.len() != 2 {
            bail!(
                "{} 의 입력은 [L, D] 여야 합니다 (지금 {sample:?})",
                label(self.model, id)
            );
        }
        let d = sample[1];
        if heads == 0 || d % heads != 0 {
            bail!("특징 차원 {d} 가 헤드 수 {heads} 로 나누어떨어지지 않습니다");
        }
        let head_dim = d / heads;

        // 배치·길이는 기호일 수 있으므로 0 (= 그 자리 입력 차원 그대로) 을 쓴다.
        let split = self.const_i64("attn_split", &[0, 0, heads as i64, head_dim as i64]);
        let q = self.attn_head(id, &x, "q", &split)?;
        let k = self.attn_head(id, &x, "k", &split)?;
        let v = self.attn_head(id, &x, "v", &split)?;

        // [B, heads, L, hd] × [B, heads, hd, L] → [B, heads, L, L]
        let kt = self.op1("Transpose", vec![k], "attn_kt", vec![ints("perm", &[0, 1, 3, 2])]);
        let scores = self.op1("MatMul", vec![q, kt], "attn_scores", vec![]);
        let scale = self.const_f32("attn_scale", (head_dim as f64).sqrt() as f32);
        let scaled = self.op1("Div", vec![scores, scale], "attn_scaled", vec![]);
        let probs = self.op1("Softmax", vec![scaled], "attn_probs", vec![int("axis", -1)]);

        // [B, heads, L, hd] → [B, L, heads, hd] → [B, L, D]
        let ctx = self.op1("MatMul", vec![probs, v], "attn_ctx", vec![]);
        let back = self.op1("Transpose", vec![ctx], "attn_back", vec![ints("perm", &[0, 2, 1, 3])]);
        let merge = self.const_i64("attn_merge", &[0, 0, d as i64]);
        let merged = self.op1("Reshape", vec![back, merge], "attn_merged", vec![]);

        let ow = self.param(id, "out_weight")?;
        let ob = self.param(id, "out_bias")?;
        let proj = self.op1("MatMul", vec![merged, ow], "attn_out", vec![]);
        let out = self.out_name(id, kind);
        self.push("Add", vec![proj, ob], out.clone(), vec![]);
        Ok(out)
    }

    /// q·k·v 투영 하나를 `[B, heads, L, head_dim]` 까지 만든다.
    fn attn_head(&mut self, id: NodeId, x: &str, part: &str, split: &str) -> Result<String> {
        let w = self.param(id, &format!("{part}_weight"))?;
        let b = self.param(id, &format!("{part}_bias"))?;
        let mm = self.op1("MatMul", vec![x.to_string(), w], &format!("attn_{part}"), vec![]);
        let add = self.op1("Add", vec![mm, b], &format!("attn_{part}_b"), vec![]);
        let re = self.op1(
            "Reshape",
            vec![add, split.to_string()],
            &format!("attn_{part}_r"),
            vec![],
        );
        Ok(self.op1(
            "Transpose",
            vec![re],
            &format!("attn_{part}_h"),
            vec![ints("perm", &[0, 2, 1, 3])],
        ))
    }

    fn activation(&mut self, id: NodeId, kind: &LayerKind, x: String, act: Act) -> Result<String> {
        let out = self.out_name(id, kind);
        match act {
            Act::Relu => self.push("Relu", vec![x], out.clone(), vec![]),
            Act::LeakyRelu { slope } => self.push("LeakyRelu", vec![x], out.clone(), vec![float("alpha", slope)]),
            Act::Sigmoid => self.push("Sigmoid", vec![x], out.clone(), vec![]),
            Act::Tanh => self.push("Tanh", vec![x], out.clone(), vec![]),
            Act::Softmax => self.push("Softmax", vec![x], out.clone(), vec![int("axis", -1)]),
            Act::LogSoftmax => self.push("LogSoftmax", vec![x], out.clone(), vec![int("axis", -1)]),
            // ONNX 의 Gelu 연산자는 opset 20 이라 17 에서는 쓸 수 없다.
            // burn 의 gelu 는 tanh 근사가 아니라 정확형이므로 erf 로 전개한다:
            //   x · ½ · (1 + erf(x / √2))
            Act::Gelu => {
                let inv_sqrt2 = self.const_f32("inv_sqrt2", std::f32::consts::FRAC_1_SQRT_2);
                let half = self.const_f32("half", 0.5);
                let one = self.const_f32("one", 1.0);
                let scaled = self.op1("Mul", vec![x.clone(), inv_sqrt2], "gelu_scaled", vec![]);
                let erf = self.op1("Erf", vec![scaled], "gelu_erf", vec![]);
                let plus = self.op1("Add", vec![erf, one], "gelu_plus", vec![]);
                let cdf = self.op1("Mul", vec![plus, half], "gelu_cdf", vec![]);
                self.push("Mul", vec![x, cdf], out.clone(), vec![]);
            }
            // SiLU 는 표준 연산자가 없다. x · sigmoid(x).
            Act::Silu => {
                let s = self.op1("Sigmoid", vec![x.clone()], "silu_sig", vec![]);
                self.push("Mul", vec![x, s], out.clone(), vec![]);
            }
        }
        Ok(out)
    }
}

// ───────────────────────────── protobuf 잔손질 ─────────────────────────────

fn float_tensor(name: &str, shape: &[usize], data: &[f32]) -> pb::TensorProto {
    pb::TensorProto {
        dims: shape.iter().map(|&d| d as i64).collect(),
        data_type: F32,
        name: name.into(),
        // raw_data 는 리틀엔디안이다. ONNX 규격이 그렇게 정해 두었다.
        raw_data: data.iter().flat_map(|v| v.to_le_bytes()).collect(),
        ..Default::default()
    }
}

/// `[n_in, rows]` 를 `[rows, n_in]` 으로 전치하면서 `hidden` 크기 게이트 블록을 재배열한다.
///
/// `order[i]` = ONNX 의 `i` 번째 블록이 우리 쪽 몇 번째 블록인가.
fn transpose_and_reorder(src: &[f32], n_in: usize, rows: usize, hidden: usize, order: &[usize]) -> Vec<f32> {
    let mut out = vec![0.0f32; rows * n_in];
    for (dst_blk, &src_blk) in order.iter().enumerate() {
        for h in 0..hidden {
            let dst_row = dst_blk * hidden + h;
            let src_col = src_blk * hidden + h;
            for d in 0..n_in {
                // 우리 쪽은 [n_in, rows] 행 우선이라 (d, src_col) 이 d*rows + src_col.
                out[dst_row * n_in + d] = src[d * rows + src_col];
            }
        }
    }
    out
}

/// 편향처럼 1차원인 것의 게이트 블록만 재배열한다.
fn reorder_blocks(src: &[f32], hidden: usize, order: &[usize]) -> Vec<f32> {
    let mut out = Vec::with_capacity(src.len());
    for &src_blk in order {
        out.extend_from_slice(&src[src_blk * hidden..(src_blk + 1) * hidden]);
    }
    out
}

fn string(name: &str, v: &str) -> pb::AttributeProto {
    pb::AttributeProto {
        name: name.into(),
        r#type: pb::attribute_proto::AttributeType::String as i32,
        s: v.as_bytes().to_vec(),
        ..Default::default()
    }
}

fn int(name: &str, v: i64) -> pb::AttributeProto {
    pb::AttributeProto {
        name: name.into(),
        r#type: pb::attribute_proto::AttributeType::Int as i32,
        i: v,
        ..Default::default()
    }
}

fn ints(name: &str, v: &[i64]) -> pb::AttributeProto {
    pb::AttributeProto {
        name: name.into(),
        r#type: pb::attribute_proto::AttributeType::Ints as i32,
        ints: v.to_vec(),
        ..Default::default()
    }
}

fn float(name: &str, v: f32) -> pb::AttributeProto {
    pb::AttributeProto {
        name: name.into(),
        r#type: pb::attribute_proto::AttributeType::Float as i32,
        f: v,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::model::{Node, Port};

    fn chain(kinds: Vec<LayerKind>) -> ModelDef {
        let mut def = ModelDef::new("t");
        let g = &mut def.graph;
        let mut prev: Option<NodeId> = None;
        for k in kinds {
            let id = g.add_node(Node::new(k, [0.0, 0.0]));
            if let Some(p) = prev {
                g.add_edge(p, Port::new(id, 0)).expect("엣지");
            }
            prev = Some(id);
        }
        let out = g.add_node(Node::new(LayerKind::Output, [0.0, 0.0]));
        g.add_edge(prev.expect("빈 그래프"), Port::new(out, 0)).expect("엣지");
        def
    }

    #[test]
    fn every_palette_layer_can_be_exported() {
        // `supported()` 의 match 는 모든 변형을 빠짐없이 적으므로 새 레이어가 core 에 들어오면
        // 컴파일이 깨진다. 이 테스트는 그 목록이 실제로 **전부 true** 인지를 본다 —
        // 누군가 새 레이어를 false 로 두고 잊는 것을 잡는다.
        for kind in LayerKind::palette() {
            assert!(supported(&kind), "{} 이 미지원으로 남아 있습니다", kind.spec().label);
        }

        let ok = chain(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Linear {
                out_features: 2,
                bias: true,
            },
        ]);
        assert!(check(&ok).unsupported.is_empty());

        // 순환·어텐션도 이제 지원 범위 안이다 (3~4 단계에서 들어왔다).
        let mut seq = ModelDef::new("순환");
        let g = &mut seq.graph;
        let i = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let e = g.add_node(Node::new(LayerKind::Embedding { vocab: 8, dim: 4 }, [0.0, 0.0]));
        let l = g.add_node(Node::new(
            LayerKind::Lstm {
                hidden: 3,
                bidirectional: true,
                return_sequence: false,
            },
            [0.0, 0.0],
        ));
        g.add_edge(i, Port::new(e, 0));
        g.add_edge(e, Port::new(l, 0));
        assert!(check(&seq).unsupported.is_empty());
    }

    #[test]
    fn gate_order_tables_are_permutations() {
        // 재배열표가 순열이 아니면 가중치가 조용히 뒤섞인다.
        for cell in [Cell::Lstm, Cell::Gru] {
            let mut seen: Vec<usize> = cell.gate_order().to_vec();
            let n = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(
                seen,
                (0..n).collect::<Vec<_>>(),
                "{cell:?} 의 게이트 순서표가 순열이 아닙니다"
            );
        }
        assert_eq!(Cell::Lstm.gate_order().len(), 4);
        assert_eq!(Cell::Gru.gate_order().len(), 3);
    }

    #[test]
    fn transpose_and_reorder_moves_gate_blocks_and_flips_the_axes() {
        // [n_in=2, rows=4] (게이트 2개 × hidden 2) → [4, 2] 로 전치하면서 블록을 뒤집는다.
        let src = vec![
            // d=0: [g0h0, g0h1, g1h0, g1h1]
            1.0, 2.0, 3.0, 4.0, //
            // d=1
            5.0, 6.0, 7.0, 8.0,
        ];
        let out = transpose_and_reorder(&src, 2, 4, 2, &[1, 0]);
        // ONNX 블록 0 = 우리 블록 1 = 열 2,3 → 행 0,1 이 [3,7], [4,8]
        assert_eq!(out, vec![3.0, 7.0, 4.0, 8.0, 1.0, 5.0, 2.0, 6.0]);

        // 항등 순서면 순수 전치다.
        let plain = transpose_and_reorder(&src, 2, 4, 2, &[0, 1]);
        assert_eq!(plain, vec![1.0, 5.0, 2.0, 6.0, 3.0, 7.0, 4.0, 8.0]);
    }

    #[test]
    fn reorder_blocks_only_moves_whole_gates() {
        let src = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]; // 게이트 3개 × hidden 2
        assert_eq!(reorder_blocks(&src, 2, &[1, 0, 2]), vec![3.0, 4.0, 1.0, 2.0, 5.0, 6.0]);
        assert_eq!(reorder_blocks(&src, 2, &[0, 1, 2]), src);
    }

    #[test]
    fn export_refuses_a_graph_that_does_not_type_check() {
        let dir = std::env::temp_dir().join(format!("nl-onnx-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Conv2d 는 랭크 4 를 요구하는데 [B, 4] 를 준다.
        let def = chain(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Conv2d {
                out_channels: 2,
                kernel: [3, 3],
                stride: [1, 1],
                padding: [1, 1],
                bias: true,
            },
        ]);
        let e = format!(
            "{:#}",
            export(
                &def,
                &dir.join("없음.safetensors"),
                &dir.join("out.onnx"),
                ExportOptions::default()
            )
            .unwrap_err()
        );
        assert!(e.contains("형상 추론"), "{e}");
        assert!(!dir.join("out.onnx").exists(), "실패했는데 파일을 남겼다");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_weights_file_is_reported_by_name_only() {
        // 오류 메시지에 사용자 절대 경로가 섞이면 안 된다 (로그·화면에 그대로 나간다).
        let dir = std::env::temp_dir().join(format!("nl-onnx-noweights-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let def = chain(vec![
            LayerKind::Input { shape: vec![6] },
            LayerKind::Embedding { vocab: 8, dim: 4 },
            LayerKind::Gru {
                hidden: 3,
                bidirectional: false,
                return_sequence: false,
            },
        ]);
        let e = format!(
            "{:#}",
            export(
                &def,
                &dir.join("없음.safetensors"),
                &dir.join("out.onnx"),
                ExportOptions::default()
            )
            .unwrap_err()
        );
        assert!(e.contains("없음.safetensors"), "파일 이름은 알려 줘야 한다: {e}");
        assert!(!e.contains(&dir.display().to_string()), "절대 경로가 새어 나왔다: {e}");
        assert!(!dir.join("out.onnx").exists(), "실패했는데 파일을 남겼다");

        // 파일이 **있는데 깨진** 경우도 같다 — 이쪽은 `weights::load_for` 의 메시지를 탄다.
        let broken = dir.join("깨짐.safetensors");
        std::fs::write(&broken, "safetensors 가 아닌 내용").unwrap();
        let e = format!(
            "{:#}",
            export(&def, &broken, &dir.join("out2.onnx"), ExportOptions::default()).unwrap_err()
        );
        assert!(e.contains("깨짐.safetensors"), "파일 이름은 알려 줘야 한다: {e}");
        assert!(
            !e.contains(&dir.display().to_string()),
            "깨진 파일에서 절대 경로가 새어 나왔다: {e}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn batch_dimension_is_symbolic_or_fixed() {
        let def = chain(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Linear {
                out_features: 2,
                bias: false,
            },
        ]);
        let rep = shape::infer(&def.graph);
        let params = BTreeMap::new();

        let dyn_b = Builder::new(&def, &rep, &params, ExportOptions::default());
        let vi = dyn_b.value_info("x", &Shape::from_sample(&[4]), F32);
        let dims = tensor_dims(&vi);
        assert!(matches!(
            dims[0].value,
            Some(pb::tensor_shape_proto::dimension::Value::DimParam(ref s)) if s == "B"
        ));
        assert!(matches!(
            dims[1].value,
            Some(pb::tensor_shape_proto::dimension::Value::DimValue(4))
        ));

        let fixed = Builder::new(
            &def,
            &rep,
            &params,
            ExportOptions {
                opset: DEFAULT_OPSET,
                batch: Batch::Fixed(8),
            },
        );
        let vi = fixed.value_info("x", &Shape::from_sample(&[4]), F32);
        assert!(matches!(
            tensor_dims(&vi)[0].value,
            Some(pb::tensor_shape_proto::dimension::Value::DimValue(8))
        ));
    }

    fn tensor_dims(vi: &pb::ValueInfoProto) -> &[pb::tensor_shape_proto::Dimension] {
        let Some(pb::type_proto::Value::TensorType(t)) = &vi.r#type.as_ref().unwrap().value else {
            panic!("텐서 타입이 아님");
        };
        &t.shape.as_ref().unwrap().dim
    }

    #[test]
    fn raw_data_is_little_endian_f32() {
        let t = float_tensor("w", &[2], &[1.0f32, -2.0]);
        assert_eq!(t.data_type, F32);
        assert_eq!(t.dims, vec![2]);
        assert_eq!(t.raw_data.len(), 8);
        let back: Vec<f32> = t
            .raw_data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        assert_eq!(back, vec![1.0, -2.0]);
    }
}
