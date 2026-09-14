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

    let params = weights::load_for(weights, Some(model.id))
        .with_context(|| format!("가중치를 읽을 수 없습니다: {}", shown(weights)))?;

    let mut b = Builder::new(model, &rep, &params, opts);
    b.build()?;
    let proto = b.finish()?;

    let bytes = proto.encode_to_vec();
    write_atomic(out, &bytes).with_context(|| format!("ONNX 파일을 쓸 수 없습니다: {}", shown(out)))?;

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

fn unsupported_layers(model: &ModelDef, _rep: &ShapeReport) -> Vec<String> {
    let mut v: Vec<String> = model
        .graph
        .nodes
        .values()
        .filter(|n| {
            matches!(
                n.kind,
                LayerKind::Lstm { .. } | LayerKind::Gru { .. } | LayerKind::MultiHeadAttention { .. }
            )
        })
        .map(|n| format!("{} ({})", n.display_name(), n.kind.spec().label))
        .collect();
    v.sort();
    v
}

fn label(model: &ModelDef, id: NodeId) -> String {
    model
        .graph
        .nodes
        .get(&id)
        .map(|n| n.display_name())
        .unwrap_or_else(|| id.short())
}

/// 경로를 파일 이름만으로 줄인다 — 오류 메시지에 사용자 절대 경로를 흘리지 않는다.
fn shown(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<경로>".into())
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

            LayerKind::Lstm { .. } | LayerKind::Gru { .. } | LayerKind::MultiHeadAttention { .. } => {
                bail!("{} 은 아직 ONNX 로 내보낼 수 없습니다", label(self.model, id))
            }
        };
        self.values.insert(id, out);
        Ok(())
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
    fn check_flags_recurrent_and_attention_layers() {
        let ok = chain(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Linear {
                out_features: 2,
                bias: true,
            },
        ]);
        assert!(check(&ok).unsupported.is_empty());

        let mut bad = ModelDef::new("순환");
        let g = &mut bad.graph;
        let i = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let e = g.add_node(Node::new(LayerKind::Embedding { vocab: 8, dim: 4 }, [0.0, 0.0]));
        let mut lstm = Node::new(
            LayerKind::Lstm {
                hidden: 3,
                bidirectional: false,
                return_sequence: false,
            },
            [0.0, 0.0],
        );
        lstm.name = "기억".into();
        let l = g.add_node(lstm);
        g.add_edge(i, Port::new(e, 0));
        g.add_edge(e, Port::new(l, 0));
        let rep = check(&bad);
        assert_eq!(rep.unsupported.len(), 1);
        assert!(rep.unsupported[0].contains("기억"), "{:?}", rep.unsupported);
        assert!(rep.unsupported[0].contains("LSTM"), "{:?}", rep.unsupported);
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
    fn export_refuses_unsupported_layers_before_touching_weights() {
        let dir = std::env::temp_dir().join(format!("nl-onnx-unsup-{}", std::process::id()));
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
        // 가중치 파일이 없는데도 그쪽 오류가 아니라 미지원 오류가 나야 한다 (순서 확인).
        assert!(e.contains("내보낼 수 없는 레이어"), "{e}");
        assert!(e.contains("GRU"), "{e}");
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
