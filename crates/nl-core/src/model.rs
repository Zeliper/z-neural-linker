//! 프로젝트 문서 모델. 모든 컬렉션은 `BTreeMap<Id, _>` (결정적 순회 → 안정적 레이아웃/테스트/diff).
//! 새 필드는 반드시 `#[serde(default)]` — 옛 문서가 그대로 열려야 한다.

use crate::dataset::DatasetSpec;
use crate::gui::GuiLayout;
use crate::ids::*;
use crate::payload::PayloadSpec;
use crate::pipeline::Pipeline;
use crate::train::{DevicePref, RunRecord, TrainConfig};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 파일 포맷 버전. 구조가 바뀌면 올리고 `ProjectFile::from_json` 에 마이그레이션을 더한다.
pub const FORMAT_VERSION: u32 = 1;

/// 프로젝트 파일 확장자 (내용은 JSON).
pub const PROJECT_EXT: &str = "nlproj";

// ───────────────────────────── 프로젝트 ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub models: BTreeMap<ModelId, ModelDef>,
    #[serde(default)]
    pub datasets: BTreeMap<DatasetId, DatasetSpec>,
    #[serde(default)]
    pub payloads: BTreeMap<PayloadId, PayloadSpec>,
    #[serde(default)]
    pub pipelines: BTreeMap<PipelineId, Pipeline>,
    #[serde(default)]
    pub gui: GuiLayout,
    #[serde(default)]
    pub runs: BTreeMap<RunId, RunRecord>,
    #[serde(default)]
    pub settings: ProjectSettings,
    /// 이 버전이 모르는 필드. 새 버전이 만든 문서를 열고 저장해도 그대로 돌려준다 (보안 리뷰 L3).
    ///
    /// `flatten` 이라 JSON 에서는 이 구조체의 필드와 같은 자리에 평평하게 놓인다. 비어 있으면
    /// 직렬화에도 나타나지 않으므로 기존 파일의 모양은 바뀌지 않는다.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectSettings {
    /// 학습·추론 기본 장치.
    #[serde(default)]
    pub default_device: DevicePref,
    /// 실행 기록·체크포인트 폴더. 없으면 프로젝트 파일 옆 `<이름>.runs/`.
    #[serde(default)]
    pub runs_dir: Option<String>,
    /// 배포 빌드 설정. 빌드 뷰에서 한 번 정하면 문서에 남는다.
    #[serde(default)]
    pub build: Option<crate::bundle::BuildSpec>,
}

impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            default_device: DevicePref::Auto,
            runs_dir: None,
            build: None,
        }
    }
}

impl Project {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: ProjectId::new(),
            name: name.into(),
            description: String::new(),
            created: Utc::now(),
            models: BTreeMap::new(),
            datasets: BTreeMap::new(),
            payloads: BTreeMap::new(),
            pipelines: BTreeMap::new(),
            gui: GuiLayout::default(),
            runs: BTreeMap::new(),
            settings: ProjectSettings::default(),
            extra: BTreeMap::new(),
        }
    }

    /// 빈 모델 하나를 만들어 넣고 id 를 돌려준다.
    pub fn add_model(&mut self, name: impl Into<String>) -> ModelId {
        let m = ModelDef::new(name);
        let id = m.id;
        self.models.insert(id, m);
        id
    }
}

// ───────────────────────────── 모델 ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelDef {
    pub id: ModelId,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub graph: Graph,
    #[serde(default)]
    pub train: TrainConfig,
    /// 입력/출력 페이로드 바인딩 (없으면 Input/Output 노드 형상만으로 동작).
    #[serde(default)]
    pub payload: Option<PayloadId>,
    /// 마지막으로 학습된 가중치 파일 (프로젝트 폴더 기준 상대 경로).
    #[serde(default)]
    pub weights: Option<String>,
    /// 이 버전이 모르는 필드. 새 버전이 만든 문서를 열고 저장해도 그대로 돌려준다 (보안 리뷰 L3).
    ///
    /// `flatten` 이라 JSON 에서는 이 구조체의 필드와 같은 자리에 평평하게 놓인다. 비어 있으면
    /// 직렬화에도 나타나지 않으므로 기존 파일의 모양은 바뀌지 않는다.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ModelDef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: ModelId::new(),
            name: name.into(),
            description: String::new(),
            graph: Graph::default(),
            train: TrainConfig::default(),
            payload: None,
            weights: None,
            extra: BTreeMap::new(),
        }
    }
}

/// 레이어 그래프. 노드는 레이어, 엣지는 텐서 흐름. 다입력 레이어는 입력 `slot` 으로 구분한다.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default)]
    pub nodes: BTreeMap<NodeId, Node>,
    #[serde(default)]
    pub edges: BTreeMap<EdgeId, Edge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    #[serde(default)]
    pub name: String,
    pub kind: LayerKind,
    /// 캔버스 월드 좌표 (줌 1.0 기준 px).
    #[serde(default)]
    pub pos: [f32; 2],
    /// 이 버전이 모르는 필드. 새 버전이 만든 문서를 열고 저장해도 그대로 돌려준다 (보안 리뷰 L3).
    ///
    /// `flatten` 이라 JSON 에서는 이 구조체의 필드와 같은 자리에 평평하게 놓인다. 비어 있으면
    /// 직렬화에도 나타나지 않으므로 기존 파일의 모양은 바뀌지 않는다.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Node {
    pub fn new(kind: LayerKind, pos: [f32; 2]) -> Self {
        Self {
            id: NodeId::new(),
            name: String::new(),
            kind,
            pos,
            extra: BTreeMap::new(),
        }
    }

    /// 표시 이름: 이름이 비었으면 레이어 라벨.
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.kind.spec().label.to_string()
        } else {
            self.name.clone()
        }
    }
}

/// 노드의 입력 슬롯 하나 (출력 슬롯은 현 단계에서 하나뿐이라 `slot` 은 항상 입력 쪽 번호다).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Port {
    pub node: NodeId,
    #[serde(default)]
    pub slot: usize,
}

impl Port {
    pub fn new(node: NodeId, slot: usize) -> Self {
        Self { node, slot }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    /// 출력 노드 (slot 은 0 고정).
    pub from: NodeId,
    /// 입력 노드 + 슬롯.
    pub to: Port,
}

impl Edge {
    pub fn new(from: NodeId, to: Port) -> Self {
        Self {
            id: EdgeId::new(),
            from,
            to,
        }
    }
}

impl Graph {
    pub fn add_node(&mut self, node: Node) -> NodeId {
        let id = node.id;
        self.nodes.insert(id, node);
        id
    }

    /// 엣지 추가. 자기참조·같은 (from, to) 중복·이미 점유된 입력 슬롯·슬롯 범위 밖은 거부하고 `None`.
    /// **순환은 허용** — 문서를 깨뜨리지 않고 `shape::infer`/`validate` 가 감지해 보고한다. UI 는 `would_create_cycle` 로 사전 차단.
    pub fn add_edge(&mut self, from: NodeId, to: Port) -> Option<EdgeId> {
        if from == to.node || !self.nodes.contains_key(&from) {
            return None;
        }
        let target = self.nodes.get(&to.node)?;
        if to.slot >= target.kind.spec().inputs {
            return None;
        }
        if self
            .edges
            .values()
            .any(|e| e.to == to || (e.from == from && e.to.node == to.node))
        {
            return None;
        }
        let e = Edge::new(from, to);
        let id = e.id;
        self.edges.insert(id, e);
        Some(id)
    }

    /// 노드와 연결된 엣지를 함께 지운다. 지운 엣지 목록을 돌려준다(undo 용).
    pub fn remove_node(&mut self, id: NodeId) -> (Option<Node>, Vec<Edge>) {
        let node = self.nodes.remove(&id);
        let gone: Vec<EdgeId> = self
            .edges
            .values()
            .filter(|e| e.from == id || e.to.node == id)
            .map(|e| e.id)
            .collect();
        let edges = gone.iter().filter_map(|eid| self.edges.remove(eid)).collect();
        (node, edges)
    }

    /// `from → to` 를 잇는 엣지가 순환을 만드는가 (to 에서 from 으로 이미 도달 가능한가).
    pub fn would_create_cycle(&self, from: NodeId, to: NodeId) -> bool {
        if from == to {
            return true;
        }
        let mut stack = vec![to];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(n) = stack.pop() {
            if n == from {
                return true;
            }
            if !seen.insert(n) {
                continue;
            }
            for e in self.edges.values().filter(|e| e.from == n) {
                stack.push(e.to.node);
            }
        }
        false
    }

    /// 노드의 입력 슬롯별 출처 (slot → from). 비어 있는 슬롯은 없다.
    pub fn inputs_of(&self, id: NodeId) -> BTreeMap<usize, NodeId> {
        self.edges
            .values()
            .filter(|e| e.to.node == id)
            .map(|e| (e.to.slot, e.from))
            .collect()
    }

    /// 노드의 출력을 받는 노드들.
    pub fn outputs_of(&self, id: NodeId) -> Vec<NodeId> {
        self.edges
            .values()
            .filter(|e| e.from == id)
            .map(|e| e.to.node)
            .collect()
    }

    pub fn nodes_of_kind(&self, pred: impl Fn(&LayerKind) -> bool) -> Vec<NodeId> {
        self.nodes.values().filter(|n| pred(&n.kind)).map(|n| n.id).collect()
    }

    /// 입력 노드들(이름 → 정렬, 이름이 같으면 id 순). 엔진의 입력 텐서 순서.
    pub fn input_nodes(&self) -> Vec<NodeId> {
        let mut v: Vec<&Node> = self
            .nodes
            .values()
            .filter(|n| matches!(n.kind, LayerKind::Input { .. }))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        v.into_iter().map(|n| n.id).collect()
    }

    /// 출력 노드들(같은 규칙).
    pub fn output_nodes(&self) -> Vec<NodeId> {
        let mut v: Vec<&Node> = self
            .nodes
            .values()
            .filter(|n| matches!(n.kind, LayerKind::Output))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        v.into_iter().map(|n| n.id).collect()
    }
}

// ───────────────────────────── 레이어 ─────────────────────────────

/// 활성화 함수.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Act {
    Relu,
    LeakyRelu {
        slope: f32,
    },
    Gelu,
    Silu,
    Sigmoid,
    Tanh,
    /// 마지막 차원 softmax.
    Softmax,
    /// 마지막 차원 log-softmax (CrossEntropy 와 함께 쓰지 말 것 — 손실이 내부에서 처리).
    LogSoftmax,
}

impl Act {
    pub const ALL: [Act; 8] = [
        Act::Relu,
        Act::LeakyRelu { slope: 0.01 },
        Act::Gelu,
        Act::Silu,
        Act::Sigmoid,
        Act::Tanh,
        Act::Softmax,
        Act::LogSoftmax,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Act::Relu => "ReLU",
            Act::LeakyRelu { .. } => "LeakyReLU",
            Act::Gelu => "GELU",
            Act::Silu => "SiLU",
            Act::Sigmoid => "Sigmoid",
            Act::Tanh => "Tanh",
            Act::Softmax => "Softmax",
            Act::LogSoftmax => "LogSoftmax",
        }
    }
}

/// 레이어 종류 = 데이터 열거형. 엔진이 해석해 텐서 연산으로 바꾼다.
/// **새 레이어 추가** = 여기 변형 + `spec()` + `shape.rs` 규칙 + `nl-engine::exec` 연산 하나.
/// 형상은 배치 차원을 뺀 "샘플 형상"으로 적는다 (예: 이미지 `[3, 28, 28]`, 벡터 `[16]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LayerKind {
    /// 모델 입력. `shape` = 샘플 형상.
    Input {
        shape: Vec<usize>,
    },
    /// 모델 출력 (항등). 여러 개 가능.
    Output,
    /// 완전연결. 마지막 차원 `in → out_features`.
    Linear {
        out_features: usize,
        #[serde(default = "yes")]
        bias: bool,
    },
    /// 2D 합성곱. 입력 `[C, H, W]`.
    Conv2d {
        out_channels: usize,
        #[serde(default = "k3")]
        kernel: [usize; 2],
        #[serde(default = "s1")]
        stride: [usize; 2],
        #[serde(default = "p0")]
        padding: [usize; 2],
        #[serde(default = "yes")]
        bias: bool,
    },
    MaxPool2d {
        #[serde(default = "k2")]
        kernel: [usize; 2],
        #[serde(default = "s2")]
        stride: [usize; 2],
    },
    AvgPool2d {
        #[serde(default = "k2")]
        kernel: [usize; 2],
        #[serde(default = "s2")]
        stride: [usize; 2],
    },
    /// `[C, H, W] → [C]`.
    GlobalAvgPool,
    /// `[a, b, c] → [a*b*c]`.
    Flatten,
    /// 샘플 형상을 `shape` 로 (원소 수 동일).
    Reshape {
        shape: Vec<usize>,
    },
    Activation {
        act: Act,
    },
    Dropout {
        #[serde(default = "half")]
        p: f32,
    },
    /// 채널(첫 샘플 차원) 기준 배치 정규화. `[C, …]`.
    BatchNorm {
        #[serde(default = "eps5")]
        eps: f32,
        #[serde(default = "mom01")]
        momentum: f32,
    },
    /// 마지막 차원 기준 레이어 정규화.
    LayerNorm {
        #[serde(default = "eps5")]
        eps: f32,
    },
    /// 원소별 합 (입력 2, 같은 형상).
    Add,
    /// 원소별 곱 (입력 2, 같은 형상).
    Mul,
    /// 샘플 차원 `dim`(0 = 첫 샘플 차원) 기준 이어붙이기 (입력 2).
    Concat {
        #[serde(default)]
        dim: usize,
    },
    /// 정수 인덱스 `[L] → [L, dim]`.
    Embedding {
        vocab: usize,
        dim: usize,
    },
    /// LSTM. 입력 `[L, D]` → `return_sequence` 면 `[L, H]`, 아니면 마지막 상태 `[H]`.
    /// 양방향이면 `H` 가 `hidden * 2` 다 (두 방향을 마지막 차원에서 이어 붙인다).
    Lstm {
        hidden: usize,
        #[serde(default)]
        bidirectional: bool,
        #[serde(default = "yes")]
        return_sequence: bool,
    },
    /// GRU. 형상 규칙은 [`LayerKind::Lstm`] 과 같다.
    Gru {
        hidden: usize,
        #[serde(default)]
        bidirectional: bool,
        #[serde(default = "yes")]
        return_sequence: bool,
    },
    /// 셀프 어텐션. 입력 `[L, D]` → 출력 `[L, D]`. `D` 는 `heads` 로 나누어떨어져야 한다.
    MultiHeadAttention {
        heads: usize,
        #[serde(default)]
        dropout: f32,
    },
}

fn yes() -> bool {
    true
}
fn k3() -> [usize; 2] {
    [3, 3]
}
fn k2() -> [usize; 2] {
    [2, 2]
}
fn s1() -> [usize; 2] {
    [1, 1]
}
fn s2() -> [usize; 2] {
    [2, 2]
}
fn p0() -> [usize; 2] {
    [0, 0]
}
fn half() -> f32 {
    0.5
}
fn eps5() -> f32 {
    1e-5
}
fn mom01() -> f32 {
    0.1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayerCategory {
    Io,
    Dense,
    Conv,
    Pool,
    Shape,
    Activation,
    Regularize,
    Normalize,
    Merge,
    Embed,
    /// 순환 (LSTM·GRU).
    Sequence,
    /// 어텐션.
    Attention,
}

impl LayerCategory {
    pub fn label(&self) -> &'static str {
        match self {
            LayerCategory::Io => "입출력",
            LayerCategory::Dense => "완전연결",
            LayerCategory::Conv => "합성곱",
            LayerCategory::Pool => "풀링",
            LayerCategory::Shape => "형상",
            LayerCategory::Activation => "활성화",
            LayerCategory::Regularize => "정규화(드롭아웃)",
            LayerCategory::Normalize => "정규화(통계)",
            LayerCategory::Merge => "병합",
            LayerCategory::Embed => "임베딩",
            LayerCategory::Sequence => "순환",
            LayerCategory::Attention => "어텐션",
        }
    }
}

/// 레이어 종류의 정적 성질. 캔버스 표시·슬롯 수·파라미터 유무의 단일 소유자.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerSpec {
    pub label: &'static str,
    pub category: LayerCategory,
    /// 입력 슬롯 수 (Input 은 0).
    pub inputs: usize,
    /// 출력 포트 존재 여부 (Output 은 없음).
    pub has_output: bool,
    /// 학습 파라미터를 가지는가.
    pub has_params: bool,
    /// 노드 색 (RGB).
    pub color: [u8; 3],
}

impl LayerKind {
    /// 팔레트에 보이는 기본 인스턴스 목록 (카테고리 순).
    pub fn palette() -> Vec<LayerKind> {
        vec![
            LayerKind::Input { shape: vec![16] },
            LayerKind::Output,
            LayerKind::Linear {
                out_features: 64,
                bias: true,
            },
            LayerKind::Conv2d {
                out_channels: 16,
                kernel: [3, 3],
                stride: [1, 1],
                padding: [1, 1],
                bias: true,
            },
            LayerKind::MaxPool2d {
                kernel: [2, 2],
                stride: [2, 2],
            },
            LayerKind::AvgPool2d {
                kernel: [2, 2],
                stride: [2, 2],
            },
            LayerKind::GlobalAvgPool,
            LayerKind::Flatten,
            LayerKind::Reshape { shape: vec![1, 4, 4] },
            LayerKind::Activation { act: Act::Relu },
            LayerKind::Dropout { p: 0.5 },
            LayerKind::BatchNorm {
                eps: 1e-5,
                momentum: 0.1,
            },
            LayerKind::LayerNorm { eps: 1e-5 },
            LayerKind::Add,
            LayerKind::Mul,
            LayerKind::Concat { dim: 0 },
            LayerKind::Embedding { vocab: 1000, dim: 32 },
            LayerKind::Lstm {
                hidden: 64,
                bidirectional: false,
                return_sequence: true,
            },
            LayerKind::Gru {
                hidden: 64,
                bidirectional: false,
                return_sequence: true,
            },
            LayerKind::MultiHeadAttention { heads: 4, dropout: 0.0 },
        ]
    }

    pub fn spec(&self) -> LayerSpec {
        use LayerCategory as C;
        let s = |label, category, inputs, has_output, has_params, color| LayerSpec {
            label,
            category,
            inputs,
            has_output,
            has_params,
            color,
        };
        match self {
            LayerKind::Input { .. } => s("Input", C::Io, 0, true, false, [76, 175, 80]),
            LayerKind::Output => s("Output", C::Io, 1, false, false, [76, 175, 80]),
            LayerKind::Linear { .. } => s("Linear", C::Dense, 1, true, true, [66, 133, 244]),
            LayerKind::Conv2d { .. } => s("Conv2d", C::Conv, 1, true, true, [3, 169, 244]),
            LayerKind::MaxPool2d { .. } => s("MaxPool2d", C::Pool, 1, true, false, [0, 188, 212]),
            LayerKind::AvgPool2d { .. } => s("AvgPool2d", C::Pool, 1, true, false, [0, 188, 212]),
            LayerKind::GlobalAvgPool => s("GlobalAvgPool", C::Pool, 1, true, false, [0, 188, 212]),
            LayerKind::Flatten => s("Flatten", C::Shape, 1, true, false, [158, 158, 158]),
            LayerKind::Reshape { .. } => s("Reshape", C::Shape, 1, true, false, [158, 158, 158]),
            LayerKind::Activation { .. } => s("Activation", C::Activation, 1, true, false, [255, 152, 0]),
            LayerKind::Dropout { .. } => s("Dropout", C::Regularize, 1, true, false, [121, 85, 72]),
            LayerKind::BatchNorm { .. } => s("BatchNorm", C::Normalize, 1, true, true, [156, 39, 176]),
            LayerKind::LayerNorm { .. } => s("LayerNorm", C::Normalize, 1, true, true, [156, 39, 176]),
            LayerKind::Add => s("Add", C::Merge, 2, true, false, [255, 193, 7]),
            LayerKind::Mul => s("Mul", C::Merge, 2, true, false, [255, 193, 7]),
            LayerKind::Concat { .. } => s("Concat", C::Merge, 2, true, false, [255, 193, 7]),
            LayerKind::Embedding { .. } => s("Embedding", C::Embed, 1, true, true, [233, 30, 99]),
            LayerKind::Lstm { .. } => s("LSTM", C::Sequence, 1, true, true, [63, 81, 181]),
            LayerKind::Gru { .. } => s("GRU", C::Sequence, 1, true, true, [92, 107, 192]),
            LayerKind::MultiHeadAttention { .. } => s("MultiHeadAttention", C::Attention, 1, true, true, [0, 150, 136]),
        }
    }

    /// 인스펙터/노드 부제에 쓰는 짧은 파라미터 요약.
    pub fn summary(&self) -> String {
        fn dims(v: &[usize]) -> String {
            v.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("×")
        }
        match self {
            LayerKind::Input { shape } => format!("[{}]", dims(shape)),
            LayerKind::Output => String::new(),
            LayerKind::Linear { out_features, bias } => {
                format!("→ {}{}", out_features, if *bias { "" } else { " (no bias)" })
            }
            LayerKind::Conv2d {
                out_channels,
                kernel,
                stride,
                padding,
                ..
            } => {
                format!(
                    "{}ch k{} s{} p{}",
                    out_channels,
                    dims(kernel),
                    dims(stride),
                    dims(padding)
                )
            }
            LayerKind::MaxPool2d { kernel, stride } | LayerKind::AvgPool2d { kernel, stride } => {
                format!("k{} s{}", dims(kernel), dims(stride))
            }
            LayerKind::GlobalAvgPool | LayerKind::Flatten | LayerKind::Add | LayerKind::Mul => String::new(),
            LayerKind::Reshape { shape } => format!("[{}]", dims(shape)),
            LayerKind::Activation { act } => match act {
                Act::LeakyRelu { slope } => format!("{} {slope}", act.label()),
                _ => act.label().to_string(),
            },
            LayerKind::Dropout { p } => format!("p={p}"),
            LayerKind::BatchNorm { .. } => String::new(),
            LayerKind::LayerNorm { .. } => String::new(),
            LayerKind::Concat { dim } => format!("dim {dim}"),
            LayerKind::Embedding { vocab, dim } => format!("{vocab} → {dim}"),
            LayerKind::Lstm {
                hidden,
                bidirectional,
                return_sequence,
            }
            | LayerKind::Gru {
                hidden,
                bidirectional,
                return_sequence,
            } => {
                let dir = if *bidirectional { " 양방향" } else { "" };
                let seq = if *return_sequence { "" } else { " 마지막만" };
                format!("h{hidden}{dir}{seq}")
            }
            LayerKind::MultiHeadAttention { heads, dropout } => {
                if *dropout > 0.0 {
                    format!("{heads} 헤드 p={dropout}")
                } else {
                    format!("{heads} 헤드")
                }
            }
        }
    }
}

// ───────────────────────────── 파일 ─────────────────────────────

/// 디스크에 저장되는 최상위 구조.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectFile {
    pub format_version: u32,
    pub project: Project,
    /// 이 버전이 모르는 필드. 새 버전이 만든 문서를 열고 저장해도 그대로 돌려준다 (보안 리뷰 L3).
    ///
    /// `flatten` 이라 JSON 에서는 이 구조체의 필드와 같은 자리에 평평하게 놓인다. 비어 있으면
    /// 직렬화에도 나타나지 않으므로 기존 파일의 모양은 바뀌지 않는다.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl ProjectFile {
    pub fn new(project: Project) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            project,
            extra: BTreeMap::new(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("ProjectFile 직렬화")
    }

    /// 옛 버전 문서의 마이그레이션은 여기서 한다.
    ///
    /// 버전이 더 높은 문서도 읽힌다. 이 버전이 모르는 필드는 각 구조체의 `extra` 에 담겨 저장할 때
    /// 그대로 되돌아간다 — 새 버전이 만든 문서를 열었다 저장해도 그쪽 설정이 사라지지 않는다.
    /// 다만 **이 앱이 그 값을 해석하지는 않으므로** 호출자는 `newer_than_app` 으로 알려 주는 것이 좋다.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let f: ProjectFile = serde_json::from_str(s)?;
        Ok(f)
    }

    pub fn newer_than_app(&self) -> bool {
        self.format_version > FORMAT_VERSION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 이 버전이 모르는 필드는 열고 저장해도 살아남는다 (보안 리뷰 L3).
    ///
    /// 새 버전이 만든 문서를 옛 빌더로 한 번 열었다 저장하면 그쪽 설정이 통째로 날아가던 문제다.
    #[test]
    fn unknown_fields_survive_a_load_and_save() {
        let mut base: serde_json::Value = serde_json::from_str(&ProjectFile::new(Project::new("p")).to_json()).unwrap();
        // 미래 버전이 최상위·프로젝트·모델·노드에 각각 무언가를 더한 문서를 흉내 낸다.
        base["미래_최상위"] = serde_json::json!({"a": 1});
        base["project"]["미래_프로젝트"] = serde_json::json!("값");
        let mut m = ModelDef::new("m");
        let node = Node::new(LayerKind::Output, [0.0, 0.0]);
        let (mid, nid) = (m.id, node.id);
        m.graph.nodes.insert(nid, node);
        base["project"]["models"] = serde_json::json!({ mid.to_string(): serde_json::to_value(&m).unwrap() });
        base["project"]["models"][mid.to_string()]["미래_모델"] = serde_json::json!(true);
        base["project"]["models"][mid.to_string()]["graph"]["nodes"][nid.to_string()]["미래_노드"] =
            serde_json::json!([1, 2]);

        let text = serde_json::to_string(&base).unwrap();
        let loaded = ProjectFile::from_json(&text).expect("읽기");
        // 아는 필드는 평소대로 읽힌다.
        assert_eq!(loaded.project.name, "p");
        assert!(loaded.project.models.contains_key(&mid));

        let back: serde_json::Value = serde_json::from_str(&loaded.to_json()).unwrap();
        assert_eq!(back["미래_최상위"], serde_json::json!({"a": 1}));
        assert_eq!(back["project"]["미래_프로젝트"], "값");
        assert_eq!(back["project"]["models"][mid.to_string()]["미래_모델"], true);
        assert_eq!(
            back["project"]["models"][mid.to_string()]["graph"]["nodes"][nid.to_string()]["미래_노드"],
            serde_json::json!([1, 2])
        );
    }

    /// 모르는 필드가 없으면 파일 모양이 예전과 같다 — `extra` 가 빈 객체로 새어 나오면 안 된다.
    #[test]
    fn a_plain_document_gains_no_extra_keys() {
        let text = ProjectFile::new(Project::new("p")).to_json();
        assert!(!text.contains("extra"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let top: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(top, vec!["format_version", "project"], "{top:?}");
    }

    fn lin(g: &mut Graph, out: usize) -> NodeId {
        g.add_node(Node::new(
            LayerKind::Linear {
                out_features: out,
                bias: true,
            },
            [0.0, 0.0],
        ))
    }

    #[test]
    fn add_edge_rejects_self_duplicate_and_taken_slot() {
        let mut g = Graph::default();
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = lin(&mut g, 8);
        let c = lin(&mut g, 8);
        assert!(g.add_edge(a, Port::new(a, 0)).is_none(), "자기참조");
        assert!(g.add_edge(a, Port::new(b, 0)).is_some());
        assert!(g.add_edge(a, Port::new(b, 0)).is_none(), "중복");
        assert!(g.add_edge(c, Port::new(b, 0)).is_none(), "점유된 슬롯");
        assert!(g.add_edge(a, Port::new(b, 1)).is_none(), "슬롯 범위 밖");
    }

    #[test]
    fn cycles_are_allowed_but_detectable() {
        let mut g = Graph::default();
        let a = lin(&mut g, 8);
        let b = lin(&mut g, 8);
        g.add_edge(a, Port::new(b, 0)).unwrap();
        assert!(g.would_create_cycle(b, a));
        assert!(!g.would_create_cycle(a, b));
        assert!(
            g.add_edge(b, Port::new(a, 0)).is_some(),
            "문서는 순환을 거부하지 않는다"
        );
    }

    #[test]
    fn remove_node_takes_its_edges() {
        let mut g = Graph::default();
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = lin(&mut g, 8);
        let c = g.add_node(Node::new(LayerKind::Output, [0.0, 0.0]));
        g.add_edge(a, Port::new(b, 0)).unwrap();
        g.add_edge(b, Port::new(c, 0)).unwrap();
        let (node, edges) = g.remove_node(b);
        assert!(node.is_some());
        assert_eq!(edges.len(), 2);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn palette_covers_every_variant_once() {
        let p = LayerKind::palette();
        let tags: Vec<String> = p
            .iter()
            .map(|k| serde_json::to_value(k).unwrap()["type"].as_str().unwrap().to_string())
            .collect();
        let mut dedup = tags.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(tags.len(), dedup.len(), "팔레트에 같은 종류가 두 번");
        assert_eq!(tags.len(), 20, "새 LayerKind 변형을 팔레트에 추가할 것");
    }

    #[test]
    fn project_file_round_trips() {
        let mut p = Project::new("t");
        let m = p.add_model("mlp");
        let g = &mut p.models.get_mut(&m).unwrap().graph;
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [10.0, 20.0]));
        let b = lin(g, 8);
        g.add_edge(a, Port::new(b, 0)).unwrap();
        let json = ProjectFile::new(p.clone()).to_json();
        let back = ProjectFile::from_json(&json).unwrap();
        assert_eq!(back.project, p);
        assert!(!back.newer_than_app());
    }

    #[test]
    fn old_documents_without_new_fields_still_load() {
        let json = r#"{"format_version":1,"project":{"id":"00000000-0000-0000-0000-000000000001","name":"x","created":"2026-01-01T00:00:00Z"}}"#;
        let f = ProjectFile::from_json(json).unwrap();
        assert_eq!(f.project.name, "x");
        assert!(f.project.models.is_empty());
    }
}
