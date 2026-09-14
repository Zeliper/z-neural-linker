//! 레이어 템플릿 — 팔레트에서 **한 번에** 넣는 노드 묶음.
//!
//! 잔차 블록이나 트랜스포머 블록은 손으로 놓으면 예닐곱 번의 드래그와 연결이 필요하고,
//! 그 과정에서 잔차 덧셈의 형상을 어긋나게 두기 쉽다. 여기서는 형상이 맞는 묶음을 통째로 만들어 준다.
//!
//! 만들어진 노드·엣지는 **문서에 아직 들어가지 않은 값**이다. 호출자가 [`crate::Op::UpsertNode`] /
//! [`crate::Op::UpsertEdge`] 로 적용하면 undo 한 번에 묶인다. id 는 호출마다 새로 뽑는다.
//!
//! ## 규약
//! - `nodes` 는 캔버스 배치 순서(왼쪽 → 오른쪽)이고 **마지막 원소가 블록의 출력**이다.
//! - **블록 입력은 엣지가 붙지 않은 입력 슬롯 전부**다. [`open_inputs`] 로 뽑는다.
//!   잔차 블록과 트랜스포머 블록은 그런 슬롯이 **둘**이며(본줄기와 우회로) **둘 다 같은 상류에 이어야**
//!   형상이 맞는다. 합성곱 블록은 하나다.
//! - 좌표는 `at` 에서 오른쪽으로 [`COL_GAP`] 간격의 한 줄이다.

use crate::model::{Act, Edge, LayerCategory, LayerKind, Node, Port};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 노드 사이 가로 간격. `sample.rs` 의 체인 배치와 같은 값이라 샘플과 템플릿이 같은 리듬으로 놓인다.
pub const COL_GAP: f32 = 260.0;

/// 폭·채널 같은 크기 파라미터의 상한. 넘으면 형상 추론 전에 걸러 낸다.
const MAX_WIDTH: usize = 1 << 20;

// ───────────────────────────── 스펙 ─────────────────────────────

/// 팔레트에 보이는 템플릿 하나의 정적 성질.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TemplateSpec {
    /// 안정 키. [`instantiate`] 에 넘기는 이름이고 저장·단축키에 쓰라고 바뀌지 않는다.
    pub name: &'static str,
    /// 팔레트 표시 이름.
    pub label: &'static str,
    /// 팔레트에서 묶이는 자리. 레이어 팔레트와 같은 분류를 써서 같은 머리말 아래 놓을 수 있다.
    pub category: LayerCategory,
    /// 한 줄 설명 (툴팁).
    pub description: &'static str,
    /// 팔레트가 처음 보여 줄 파라미터. 인스펙터가 이 값을 고쳐 [`instantiate`] 에 넘긴다.
    pub default_params: TemplateParams,
}

/// 팔레트에 보이는 템플릿 목록 (카테고리 순).
pub fn list() -> Vec<TemplateSpec> {
    vec![
        TemplateSpec {
            name: RESIDUAL_BLOCK,
            label: "잔차 블록",
            category: LayerCategory::Dense,
            description:
                "Linear → ReLU → Linear 를 지나 원래 입력을 더한다. 폭을 그대로 두므로 깊게 쌓아도 형상이 유지된다.",
            default_params: TemplateParams::ResidualBlock { width: 64 },
        },
        TemplateSpec {
            name: TRANSFORMER_BLOCK,
            label: "트랜스포머 블록",
            category: LayerCategory::Attention,
            description: "사전 정규화(pre-norm) 트랜스포머 한 층. 셀프 어텐션과 피드포워드에 각각 잔차를 붙인다.",
            default_params: TemplateParams::TransformerBlock {
                d_model: 64,
                heads: 4,
                ff_mult: 4,
            },
        },
        TemplateSpec {
            name: CONV_BLOCK,
            label: "합성곱 블록",
            category: LayerCategory::Conv,
            description:
                "Conv2d → BatchNorm → ReLU → MaxPool. 3×3 패딩 1 이라 합성곱은 크기를 유지하고 풀링이 절반으로 줄인다.",
            default_params: TemplateParams::ConvBlock { channels: 32 },
        },
    ]
}

/// 이름으로 스펙 하나.
pub fn spec(name: &str) -> Option<TemplateSpec> {
    list().into_iter().find(|t| t.name == name)
}

pub const RESIDUAL_BLOCK: &str = "residual_block";
pub const TRANSFORMER_BLOCK: &str = "transformer_block";
pub const CONV_BLOCK: &str = "conv_block";

// ───────────────────────────── 파라미터 ─────────────────────────────

/// 템플릿마다 다른 설정. 변형 자체가 어느 템플릿인지 가리키므로
/// [`instantiate`] 의 `name` 과 어긋나면 오류다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateParams {
    /// 폭을 유지하는 잔차 블록. `width` 는 블록 **입력의 마지막 차원과 같아야** 잔차 덧셈이 맞는다.
    ResidualBlock { width: usize },
    /// 트랜스포머 한 층.
    ///
    /// `d_model` 이 따로 필요한 이유: 피드포워드가 `d_model → d_model * ff_mult → d_model` 로 돌아왔다가
    /// 잔차와 더해지므로, 마지막 `Linear` 의 출력 폭을 만들 때 입력 폭을 알아야 한다.
    /// 어텐션 쪽은 형상을 보존해서 필요 없지만 `d_model % heads == 0` 검사에도 쓰인다.
    TransformerBlock {
        d_model: usize,
        heads: usize,
        ff_mult: usize,
    },
    /// 합성곱 한 단. 3×3 패딩 1 이라 합성곱은 `H×W` 를 유지하고 풀링이 절반으로 줄인다.
    ConvBlock { channels: usize },
}

impl TemplateParams {
    /// 이 파라미터가 속한 템플릿 이름.
    pub fn name(&self) -> &'static str {
        match self {
            TemplateParams::ResidualBlock { .. } => RESIDUAL_BLOCK,
            TemplateParams::TransformerBlock { .. } => TRANSFORMER_BLOCK,
            TemplateParams::ConvBlock { .. } => CONV_BLOCK,
        }
    }

    /// 형상 추론에 넘기기 전에 값 자체를 검사한다.
    pub fn check(&self) -> Result<(), TemplateError> {
        let positive = |what: &'static str, v: usize| {
            if v == 0 {
                Err(TemplateError::InvalidParam {
                    what,
                    message: "0 일 수 없습니다".into(),
                })
            } else if v > MAX_WIDTH {
                Err(TemplateError::InvalidParam {
                    what,
                    message: format!("{v} 는 상한 {MAX_WIDTH} 를 넘습니다"),
                })
            } else {
                Ok(())
            }
        };
        match self {
            TemplateParams::ResidualBlock { width } => positive("width", *width),
            TemplateParams::TransformerBlock {
                d_model,
                heads,
                ff_mult,
            } => {
                positive("d_model", *d_model)?;
                positive("heads", *heads)?;
                positive("ff_mult", *ff_mult)?;
                if d_model % heads != 0 {
                    return Err(TemplateError::InvalidParam {
                        what: "heads",
                        message: format!("d_model {d_model} 이 heads {heads} 로 나누어떨어지지 않습니다"),
                    });
                }
                // 피드포워드 폭. 곱이 넘치면 형상 추론이 아니라 여기서 잡는다.
                let ff = d_model.checked_mul(*ff_mult).ok_or(TemplateError::InvalidParam {
                    what: "ff_mult",
                    message: "d_model × ff_mult 가 넘칩니다".into(),
                })?;
                positive("d_model × ff_mult", ff)
            }
            TemplateParams::ConvBlock { channels } => positive("channels", *channels),
        }
    }
}

/// 템플릿을 못 만든 이유.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateError {
    /// 그런 이름의 템플릿이 없다.
    Unknown { name: String },
    /// 이름과 파라미터가 다른 템플릿을 가리킨다.
    Mismatch { name: String, params: &'static str },
    /// 파라미터 값 자체가 잘못됐다.
    InvalidParam { what: &'static str, message: String },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Unknown { name } => write!(f, "'{name}' 이라는 템플릿이 없습니다"),
            TemplateError::Mismatch { name, params } => {
                write!(f, "템플릿 '{name}' 에 '{params}' 의 파라미터를 넘겼습니다")
            }
            TemplateError::InvalidParam { what, message } => write!(f, "{what}: {message}"),
        }
    }
}

impl std::error::Error for TemplateError {}

// ───────────────────────────── 생성 ─────────────────────────────

/// 템플릿 하나를 노드·엣지 묶음으로 만든다.
///
/// `at` 는 첫 노드의 캔버스 좌표이고 나머지는 오른쪽으로 [`COL_GAP`] 씩 놓인다.
/// id 는 호출마다 새로 뽑으므로 같은 템플릿을 여러 번 넣어도 겹치지 않는다.
///
/// 돌려주는 `nodes` 의 **마지막 원소가 블록 출력**이고, **엣지가 붙지 않은 입력 슬롯이 블록 입력**이다
/// ([`open_inputs`] 참고 — 잔차가 있는 템플릿은 그런 슬롯이 둘이며 둘 다 같은 상류에 이어야 한다).
///
/// ```
/// use nl_core::templates::{self, TemplateParams};
///
/// let params = TemplateParams::ResidualBlock { width: 32 };
/// let (nodes, edges) = templates::instantiate(templates::RESIDUAL_BLOCK, [200.0, 120.0], &params).unwrap();
/// assert_eq!(nodes.len(), 4);
/// // 블록 출력 = 마지막 노드, 블록 입력 = 열린 슬롯 둘(본줄기 + 우회로).
/// assert_eq!(templates::open_inputs(&nodes, &edges).len(), 2);
/// ```
pub fn instantiate(name: &str, at: [f32; 2], params: &TemplateParams) -> Result<(Vec<Node>, Vec<Edge>), TemplateError> {
    if spec(name).is_none() {
        return Err(TemplateError::Unknown { name: name.to_string() });
    }
    if name != params.name() {
        return Err(TemplateError::Mismatch {
            name: name.to_string(),
            params: params.name(),
        });
    }
    params.check()?;

    let mut b = Build::new(at);
    match *params {
        TemplateParams::ResidualBlock { width } => residual_block(&mut b, width),
        TemplateParams::TransformerBlock {
            d_model,
            heads,
            ff_mult,
        } => transformer_block(&mut b, d_model, heads, ff_mult),
        TemplateParams::ConvBlock { channels } => conv_block(&mut b, channels),
    }
    Ok((b.nodes, b.edges))
}

/// 엣지가 붙지 않은 입력 슬롯 — 곧 **블록 입력**이다.
///
/// 노드 순서, 그 안에서는 슬롯 번호 순으로 돌려준다. 잔차가 있는 템플릿은 둘이 나오는데
/// **둘 다 같은 상류에 이어야** 잔차 덧셈의 형상이 맞는다.
pub fn open_inputs(nodes: &[Node], edges: &[Edge]) -> Vec<Port> {
    nodes
        .iter()
        .flat_map(|n| {
            (0..n.kind.spec().inputs)
                .map(move |slot| Port::new(n.id, slot))
                .filter(|p| !edges.iter().any(|e| e.to == *p))
        })
        .collect()
}

// ───────────────────────────── 템플릿 본문 ─────────────────────────────

/// `Linear → ReLU → Linear` 를 지난 값에 원래 입력을 더한다.
///
/// 두 `Linear` 가 모두 `width` 를 내므로 블록은 폭을 바꾸지 않는다 — 그래야 잔차 덧셈이 성립하고
/// 같은 블록을 여러 번 쌓을 수 있다.
fn residual_block(b: &mut Build, width: usize) {
    b.push(linear(width), "확장");
    b.chain(LayerKind::Activation { act: Act::Relu }, "ReLU");
    let l2 = b.chain(linear(width), "축소");
    let add = b.push(LayerKind::Add, "잔차");
    // 슬롯 0 은 우회로(블록 입력)라 비워 둔다 — 호출자가 본줄기와 같은 상류에 잇는다.
    b.link(l2, add, 1);
}

/// 사전 정규화 트랜스포머 한 층.
///
/// `x → LN → 어텐션 → (+x) → LN → Linear(d·ff) → GELU → Linear(d) → (+)` 이다.
/// 정규화를 부분층 **앞**에 두는 배치(pre-norm)를 쓴다 — 잔차 경로가 정규화를 타지 않아
/// 층을 쌓아도 학습이 잘 시작한다.
fn transformer_block(b: &mut Build, d_model: usize, heads: usize, ff_mult: usize) {
    let _ln1 = b.push(LayerKind::LayerNorm { eps: 1e-5 }, "LN 1");
    let attn = b.chain(LayerKind::MultiHeadAttention { heads, dropout: 0.0 }, "셀프 어텐션");
    let add1 = b.push(LayerKind::Add, "잔차 1");
    // 슬롯 0 은 블록 입력(우회로)이라 비워 둔다.
    b.link(attn, add1, 1);

    let _ln2 = b.chain(LayerKind::LayerNorm { eps: 1e-5 }, "LN 2");
    let _up = b.chain(linear(d_model * ff_mult), "FF 확장");
    let _gelu = b.chain(LayerKind::Activation { act: Act::Gelu }, "GELU");
    let down = b.chain(linear(d_model), "FF 축소");
    let add2 = b.push(LayerKind::Add, "잔차 2");
    // 이쪽 우회로는 블록 안에 있다 — 어텐션 잔차를 지난 값이다.
    b.link(add1, add2, 0);
    b.link(down, add2, 1);
}

/// `Conv2d → BatchNorm → ReLU → MaxPool`.
///
/// 합성곱은 3×3 패딩 1 이라 `H×W` 를 그대로 두고 풀링이 절반으로 줄인다.
/// 합성곱에 편향을 두지 않는 것은 뒤따르는 `BatchNorm` 이 평균을 빼면서 편향을 지우기 때문이다.
fn conv_block(b: &mut Build, channels: usize) {
    b.push(
        LayerKind::Conv2d {
            out_channels: channels,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [1, 1],
            bias: false,
        },
        "합성곱",
    );
    b.chain(
        LayerKind::BatchNorm {
            eps: 1e-5,
            momentum: 0.1,
        },
        "BatchNorm",
    );
    b.chain(LayerKind::Activation { act: Act::Relu }, "ReLU");
    b.chain(
        LayerKind::MaxPool2d {
            kernel: [2, 2],
            stride: [2, 2],
        },
        "MaxPool",
    );
}

fn linear(out_features: usize) -> LayerKind {
    LayerKind::Linear {
        out_features,
        bias: true,
    }
}

// ───────────────────────────── 배치 보조 ─────────────────────────────

/// 한 줄로 놓이는 노드 묶음을 쌓는다. 인덱스로 서로를 가리켜 id 를 들고 다니지 않는다.
struct Build {
    at: [f32; 2],
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Build {
    fn new(at: [f32; 2]) -> Self {
        Self {
            at,
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// 다음 칸에 노드를 놓는다. 입력은 잇지 않는다.
    fn push(&mut self, kind: LayerKind, name: &str) -> usize {
        let i = self.nodes.len();
        let mut n = Node::new(kind, [self.at[0] + i as f32 * COL_GAP, self.at[1]]);
        n.name = name.to_string();
        self.nodes.push(n);
        i
    }

    /// 다음 칸에 놓고 바로 앞 노드를 슬롯 0 에 잇는다.
    fn chain(&mut self, kind: LayerKind, name: &str) -> usize {
        let i = self.push(kind, name);
        if i > 0 {
            self.link(i - 1, i, 0);
        }
        i
    }

    fn link(&mut self, from: usize, to: usize, slot: usize) {
        let edge = Edge::new(self.nodes[from].id, Port::new(self.nodes[to].id, slot));
        self.edges.push(edge);
    }
}

// ───────────────────────────── 테스트 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Graph;
    use crate::shape::{self, Shape};
    use crate::NodeId;

    /// 템플릿을 `input_shape` 짜리 `Input` 뒤에 붙이고 형상 추론을 돌린다.
    /// 열린 입력 슬롯은 **전부** 그 `Input` 에 잇는다 (잔차 우회로 포함).
    fn infer_after_input(params: TemplateParams, input_shape: &[usize]) -> (Shape, NodeId, shape::ShapeReport) {
        let (nodes, edges) = instantiate(params.name(), [0.0, 0.0], &params).unwrap();
        let out_id = nodes.last().unwrap().id;
        let open = open_inputs(&nodes, &edges);

        let mut g = Graph::default();
        let input = g.add_node(Node::new(
            LayerKind::Input {
                shape: input_shape.to_vec(),
            },
            [-260.0, 0.0],
        ));
        for n in nodes {
            g.add_node(n);
        }
        for e in edges {
            g.edges.insert(e.id, e);
        }
        for p in open {
            assert!(g.add_edge(input, p).is_some(), "열린 슬롯 {p:?} 에 이을 수 없다");
        }

        let rep = shape::infer(&g);
        assert!(rep.is_ok(), "형상 추론 실패: {:?}", rep.errors);
        (rep.shape(out_id).unwrap().clone(), out_id, rep)
    }

    #[test]
    fn list_is_stable_and_self_consistent() {
        let all = list();
        assert_eq!(all.len(), 3);
        for t in &all {
            assert_eq!(
                t.default_params.name(),
                t.name,
                "{}: 기본 파라미터가 다른 템플릿",
                t.name
            );
            assert!(
                t.default_params.check().is_ok(),
                "{}: 기본값이 검사를 통과해야 한다",
                t.name
            );
            assert!(!t.label.is_empty() && !t.description.is_empty());
            assert_eq!(spec(t.name), Some(*t));
        }
        // 이름은 서로 달라야 한다 (팔레트 키).
        let mut names: Vec<&str> = all.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        assert_eq!(spec("없는템플릿"), None);
    }

    #[test]
    fn residual_block_keeps_the_feature_width() {
        // [B, 32] 랭크 2.
        let (out, _, _) = infer_after_input(TemplateParams::ResidualBlock { width: 32 }, &[32]);
        assert_eq!(out.sample(), vec![32], "잔차 블록은 폭을 바꾸지 않는다");

        // [B, L, D] 랭크 3 에서도 마지막 차원만 본다.
        let (out, _, _) = infer_after_input(TemplateParams::ResidualBlock { width: 16 }, &[8, 16]);
        assert_eq!(out.sample(), vec![8, 16]);
    }

    #[test]
    fn residual_block_shape_is_the_last_node_and_two_slots_are_open() {
        let params = TemplateParams::ResidualBlock { width: 8 };
        let (nodes, edges) = instantiate(RESIDUAL_BLOCK, [100.0, 50.0], &params).unwrap();
        assert_eq!(nodes.len(), 4);
        assert!(
            matches!(nodes.last().unwrap().kind, LayerKind::Add),
            "마지막 노드가 출력"
        );
        assert_eq!(edges.len(), 3);

        // 본줄기 입구(첫 Linear)와 잔차 우회로(Add 슬롯 0) 둘.
        let open = open_inputs(&nodes, &edges);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0], Port::new(nodes[0].id, 0));
        assert_eq!(open[1], Port::new(nodes[3].id, 0));
    }

    /// 템플릿을 `Input` 뒤에 붙인 그래프. 열린 슬롯은 전부 그 입력에 잇는다.
    fn graph_after_input(params: TemplateParams, input_shape: &[usize]) -> (Graph, Vec<Node>) {
        let (nodes, edges) = instantiate(params.name(), [0.0, 0.0], &params).unwrap();
        let open = open_inputs(&nodes, &edges);
        let mut g = Graph::default();
        let input = g.add_node(Node::new(
            LayerKind::Input {
                shape: input_shape.to_vec(),
            },
            [-260.0, 0.0],
        ));
        for n in nodes.iter().cloned() {
            g.add_node(n);
        }
        for e in edges {
            g.edges.insert(e.id, e);
        }
        for p in open {
            assert!(g.add_edge(input, p).is_some(), "열린 슬롯 {p:?} 에 이을 수 없다");
        }
        (g, nodes)
    }

    #[test]
    fn residual_block_width_must_match_the_upstream() {
        // 폭이 입력과 다르면 잔차 덧셈에서 형상이 어긋난다 — 팔레트가 경고해야 하는 경우다.
        let (g, nodes) = graph_after_input(TemplateParams::ResidualBlock { width: 64 }, &[32]);
        let add = nodes.last().unwrap().id;
        let rep = shape::infer(&g);
        assert!(
            matches!(rep.errors.get(&add), Some(shape::GraphError::ShapeMismatch { .. })),
            "{:?}",
            rep.errors
        );
    }

    #[test]
    fn transformer_block_preserves_the_sequence_shape() {
        let params = TemplateParams::TransformerBlock {
            d_model: 32,
            heads: 4,
            ff_mult: 4,
        };
        let (out, _, rep) = infer_after_input(params, &[16, 32]);
        assert_eq!(out.sample(), vec![16, 32], "트랜스포머 블록은 [L, D] 를 보존한다");

        // 피드포워드 가운데는 d_model × ff_mult 로 넓어졌다가 돌아온다.
        let (nodes, edges) = instantiate(TRANSFORMER_BLOCK, [0.0, 0.0], &params).unwrap();
        assert_eq!(nodes.len(), 8);
        assert_eq!(edges.len(), 8);
        assert!(matches!(nodes.last().unwrap().kind, LayerKind::Add));
        assert!(matches!(nodes[4].kind, LayerKind::Linear { out_features: 128, .. }));
        // 열린 슬롯은 LN 1 과 잔차 1 의 우회로 둘뿐이다 — 두 번째 잔차는 블록 안에서 닫힌다.
        let open = open_inputs(&nodes, &edges);
        assert_eq!(open.len(), 2);
        assert_eq!(open[0], Port::new(nodes[0].id, 0));
        assert_eq!(open[1], Port::new(nodes[2].id, 0));

        // 넓어진 중간 폭이 실제로 추론에도 나타난다.
        let widened = rep.shapes.values().filter(|s| s.sample() == vec![16, 128]).count();
        assert_eq!(widened, 2, "FF 확장과 GELU 두 노드가 넓은 폭이다");
    }

    #[test]
    fn transformer_block_rejects_a_head_count_that_does_not_divide() {
        let bad = TemplateParams::TransformerBlock {
            d_model: 30,
            heads: 4,
            ff_mult: 4,
        };
        let e = instantiate(TRANSFORMER_BLOCK, [0.0, 0.0], &bad).unwrap_err();
        assert!(matches!(e, TemplateError::InvalidParam { what: "heads", .. }), "{e}");
        assert!(e.to_string().contains("나누어떨어지지"), "{e}");
    }

    #[test]
    fn conv_block_halves_the_image_and_sets_the_channel_count() {
        let (out, _, _) = infer_after_input(TemplateParams::ConvBlock { channels: 16 }, &[3, 32, 32]);
        assert_eq!(out.sample(), vec![16, 16, 16], "합성곱은 크기 유지, 풀링이 절반");

        // 홀수 변도 풀링 규칙(내림)을 따른다.
        let (out, _, _) = infer_after_input(TemplateParams::ConvBlock { channels: 8 }, &[1, 9, 7]);
        assert_eq!(out.sample(), vec![8, 4, 3]);

        // 합성곱 편향은 뒤의 BatchNorm 이 지우므로 꺼 둔다.
        let params = TemplateParams::ConvBlock { channels: 4 };
        let (nodes, edges) = instantiate(CONV_BLOCK, [0.0, 0.0], &params).unwrap();
        assert!(matches!(nodes[0].kind, LayerKind::Conv2d { bias: false, .. }));
        // 한 줄 체인이라 열린 슬롯은 하나.
        assert_eq!(open_inputs(&nodes, &edges), vec![Port::new(nodes[0].id, 0)]);
    }

    #[test]
    fn conv_block_needs_a_four_rank_input() {
        // [B, 32] 에 붙이면 합성곱에서 형상 오류가 난다.
        let (g, nodes) = graph_after_input(TemplateParams::ConvBlock { channels: 8 }, &[32]);
        let rep = shape::infer(&g);
        assert!(
            matches!(
                rep.errors.get(&nodes[0].id),
                Some(shape::GraphError::ShapeMismatch { .. })
            ),
            "{:?}",
            rep.errors
        );
    }

    #[test]
    fn nodes_are_laid_out_from_at_and_ids_are_fresh() {
        let params = TemplateParams::TransformerBlock {
            d_model: 8,
            heads: 2,
            ff_mult: 2,
        };
        let (a, _) = instantiate(TRANSFORMER_BLOCK, [400.0, 220.0], &params).unwrap();
        for (i, n) in a.iter().enumerate() {
            assert_eq!(n.pos, [400.0 + i as f32 * COL_GAP, 220.0]);
            assert!(!n.name.is_empty(), "템플릿 노드에는 읽을 이름이 붙는다");
        }

        // 두 번 만들면 id 가 전부 달라 같은 그래프에 겹쳐 넣을 수 있다.
        let (b, _) = instantiate(TRANSFORMER_BLOCK, [0.0, 0.0], &params).unwrap();
        for x in &a {
            assert!(b.iter().all(|y| y.id != x.id));
        }
    }

    #[test]
    fn two_residual_blocks_stack_without_a_shape_change() {
        let params = TemplateParams::ResidualBlock { width: 24 };
        let mut g = Graph::default();
        let input = g.add_node(Node::new(LayerKind::Input { shape: vec![24] }, [0.0, 0.0]));

        let mut prev = input;
        let mut last_out = input;
        for round in 0..2 {
            let (nodes, edges) = instantiate(RESIDUAL_BLOCK, [round as f32 * 1200.0, 0.0], &params).unwrap();
            let open = open_inputs(&nodes, &edges);
            last_out = nodes.last().unwrap().id;
            for n in nodes {
                g.add_node(n);
            }
            for e in edges {
                g.edges.insert(e.id, e);
            }
            for p in open {
                assert!(g.add_edge(prev, p).is_some());
            }
            prev = last_out;
        }
        let out = g.add_node(Node::new(LayerKind::Output, [2600.0, 0.0]));
        g.add_edge(last_out, Port::new(out, 0));

        let rep = shape::infer(&g);
        assert!(rep.is_ok(), "{:?}", rep.errors);
        assert_eq!(rep.shape(out).unwrap().sample(), vec![24]);
    }

    #[test]
    fn unknown_name_and_mismatched_params_are_rejected() {
        let p = TemplateParams::ConvBlock { channels: 8 };
        assert!(matches!(
            instantiate("없는것", [0.0, 0.0], &p),
            Err(TemplateError::Unknown { .. })
        ));
        let e = instantiate(RESIDUAL_BLOCK, [0.0, 0.0], &p).unwrap_err();
        assert!(matches!(e, TemplateError::Mismatch { .. }), "{e}");
        assert!(e.to_string().contains(CONV_BLOCK), "{e}");

        for zero in [
            TemplateParams::ResidualBlock { width: 0 },
            TemplateParams::ConvBlock { channels: 0 },
            TemplateParams::TransformerBlock {
                d_model: 0,
                heads: 1,
                ff_mult: 1,
            },
            TemplateParams::TransformerBlock {
                d_model: 8,
                heads: 0,
                ff_mult: 1,
            },
            TemplateParams::TransformerBlock {
                d_model: 8,
                heads: 2,
                ff_mult: 0,
            },
        ] {
            let e = instantiate(zero.name(), [0.0, 0.0], &zero).unwrap_err();
            assert!(matches!(e, TemplateError::InvalidParam { .. }), "{zero:?} → {e}");
        }

        // 곱이 넘치는 경우도 형상 추론까지 가기 전에 잡는다.
        let huge = TemplateParams::TransformerBlock {
            d_model: usize::MAX / 2,
            heads: 2,
            ff_mult: 4,
        };
        assert!(matches!(
            instantiate(TRANSFORMER_BLOCK, [0.0, 0.0], &huge),
            Err(TemplateError::InvalidParam { .. })
        ));
    }
}
