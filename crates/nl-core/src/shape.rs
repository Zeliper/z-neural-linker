//! 형상 추론. 위상정렬(Kahn) → 노드별 규칙. 순환·누락 입력·규칙 위반은 오류로 격리하고 나머지는 계속 추론한다.
//! 배치 차원은 기호 `Dim::Batch` 로 두어 배치 크기와 무관하다.

use crate::ids::NodeId;
use crate::model::{Graph, LayerKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// 엔진이 실행할 수 있는 최대 텐서 랭크 (배치 차원 포함).
///
/// `nl-engine` 의 `DynTensor` 가 랭크 1..5 만 담는다. 여기서 같은 상한을 걸어 두어야 편집기의
/// "문제" 탭이 깨끗한데 학습 버튼에서만 실패하는 일이 없다. 엔진은 이 상수를 참조한다.
pub const MAX_RANK: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dim {
    Batch,
    Fixed(usize),
}

impl fmt::Display for Dim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dim::Batch => write!(f, "B"),
            Dim::Fixed(n) => write!(f, "{n}"),
        }
    }
}

/// 배치 차원을 포함한 전체 형상. `shape[0]` 은 항상 `Dim::Batch`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Shape(pub Vec<Dim>);

impl Shape {
    /// 샘플 형상(배치 제외)으로부터.
    pub fn from_sample(sample: &[usize]) -> Self {
        let mut v = Vec::with_capacity(sample.len() + 1);
        v.push(Dim::Batch);
        v.extend(sample.iter().map(|&d| Dim::Fixed(d)));
        Shape(v)
    }
    /// 샘플 형상(배치 제외).
    pub fn sample(&self) -> Vec<usize> {
        self.0.iter().skip(1).map(|d| match d {
            Dim::Fixed(n) => *n,
            Dim::Batch => 1,
        }).collect()
    }
    /// 배치 크기를 넣은 구체 형상.
    pub fn concrete(&self, batch: usize) -> Vec<usize> {
        self.0.iter().map(|d| match d {
            Dim::Fixed(n) => *n,
            Dim::Batch => batch,
        }).collect()
    }
    pub fn rank(&self) -> usize {
        self.0.len()
    }
    pub fn numel_sample(&self) -> usize {
        self.sample().iter().product()
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{d}")?;
        }
        write!(f, "]")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphError {
    /// 순환에 관여하는 노드.
    Cycle,
    /// 채워지지 않은 입력 슬롯.
    MissingInput { slot: usize },
    /// 입력 노드가 오류라 추론 불가.
    UpstreamError,
    /// 규칙 위반.
    ShapeMismatch { message: String },
    /// 파라미터 값 자체가 잘못됨 (0 크기 등).
    InvalidParam { message: String },
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Cycle => write!(f, "순환 연결에 포함됨"),
            GraphError::MissingInput { slot } => write!(f, "입력 {}번이 비어 있음", slot + 1),
            GraphError::UpstreamError => write!(f, "앞 레이어에 오류가 있음"),
            GraphError::ShapeMismatch { message } => write!(f, "형상 불일치: {message}"),
            GraphError::InvalidParam { message } => write!(f, "잘못된 설정: {message}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShapeReport {
    /// 추론된 출력 형상 (오류 노드는 없음).
    pub shapes: BTreeMap<NodeId, Shape>,
    pub errors: BTreeMap<NodeId, GraphError>,
    /// 실행 순서 (오류·순환 노드 제외).
    pub order: Vec<NodeId>,
    /// 순환에 관여하는 노드 집합.
    pub cycle_nodes: BTreeSet<NodeId>,
}

impl ShapeReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
    pub fn shape(&self, id: NodeId) -> Option<&Shape> {
        self.shapes.get(&id)
    }
}

/// 위상정렬. 잔여(순환) 노드 집합을 함께 돌려준다.
pub fn topo_order(graph: &Graph) -> (Vec<NodeId>, BTreeSet<NodeId>) {
    let mut indeg: BTreeMap<NodeId, usize> = graph.nodes.keys().map(|&k| (k, 0)).collect();
    for e in graph.edges.values() {
        if graph.nodes.contains_key(&e.from) {
            if let Some(d) = indeg.get_mut(&e.to.node) {
                *d += 1;
            }
        }
    }
    let mut queue: VecDeque<NodeId> = indeg.iter().filter(|(_, &d)| d == 0).map(|(&k, _)| k).collect();
    let mut order = Vec::with_capacity(graph.nodes.len());
    while let Some(n) = queue.pop_front() {
        order.push(n);
        let mut outs: Vec<NodeId> = graph.edges.values().filter(|e| e.from == n).map(|e| e.to.node).collect();
        outs.sort();
        for o in outs {
            if let Some(d) = indeg.get_mut(&o) {
                *d -= 1;
                if *d == 0 {
                    queue.push_back(o);
                }
            }
        }
    }
    let ordered: BTreeSet<NodeId> = order.iter().copied().collect();
    let cyc: BTreeSet<NodeId> = graph.nodes.keys().filter(|k| !ordered.contains(k)).copied().collect();
    (order, cyc)
}

pub fn infer(graph: &Graph) -> ShapeReport {
    let (order, cycle_nodes) = topo_order(graph);
    let mut rep = ShapeReport { cycle_nodes: cycle_nodes.clone(), ..Default::default() };
    for &n in &cycle_nodes {
        rep.errors.insert(n, GraphError::Cycle);
    }
    for &id in &order {
        let node = &graph.nodes[&id];
        let spec = node.kind.spec();
        let inputs = graph.inputs_of(id);
        // 입력 수집
        let mut in_shapes: Vec<Shape> = Vec::with_capacity(spec.inputs);
        let mut err: Option<GraphError> = None;
        for slot in 0..spec.inputs {
            match inputs.get(&slot) {
                None => {
                    err = Some(GraphError::MissingInput { slot });
                    break;
                }
                Some(from) => match rep.shapes.get(from) {
                    Some(s) => in_shapes.push(s.clone()),
                    None => {
                        err = Some(GraphError::UpstreamError);
                        break;
                    }
                },
            }
        }
        if let Some(e) = err {
            rep.errors.insert(id, e);
            continue;
        }
        match rule(&node.kind, &in_shapes) {
            Ok(s) => {
                rep.shapes.insert(id, s);
                rep.order.push(id);
            }
            Err(e) => {
                rep.errors.insert(id, e);
            }
        }
    }
    rep
}

fn mismatch(msg: impl Into<String>) -> GraphError {
    GraphError::ShapeMismatch { message: msg.into() }
}
fn invalid(msg: impl Into<String>) -> GraphError {
    GraphError::InvalidParam { message: msg.into() }
}

/// 형상이 엔진 랭크 상한 안인지 본다. 형상을 늘리는 규칙마다 통과시킨다.
fn checked(s: Shape) -> Result<Shape, GraphError> {
    if s.rank() > MAX_RANK {
        return Err(mismatch(format!(
            "랭크 {} 은 실행 가능한 최대 랭크 {MAX_RANK} 를 넘습니다 (배치 차원 포함)",
            s.rank()
        )));
    }
    Ok(s)
}

fn conv_out(n: usize, k: usize, s: usize, p: usize) -> Result<usize, GraphError> {
    if k == 0 || s == 0 {
        return Err(invalid("커널·스트라이드는 0 일 수 없음"));
    }
    let eff = n + 2 * p;
    if eff < k {
        return Err(mismatch(format!("입력 {n}(+패딩 {p}) 이 커널 {k} 보다 작음")));
    }
    Ok((eff - k) / s + 1)
}

/// 레이어 하나의 출력 형상 규칙.
pub fn rule(kind: &LayerKind, inputs: &[Shape]) -> Result<Shape, GraphError> {
    let one = || inputs.first().cloned().ok_or(GraphError::MissingInput { slot: 0 });
    match kind {
        LayerKind::Input { shape } => {
            if shape.is_empty() || shape.contains(&0) {
                return Err(invalid("입력 형상은 비어 있거나 0 을 포함할 수 없음"));
            }
            checked(Shape::from_sample(shape))
        }
        LayerKind::Output
        | LayerKind::Activation { .. }
        | LayerKind::Dropout { .. }
        | LayerKind::LayerNorm { .. } => one(),
        LayerKind::BatchNorm { .. } => {
            let s = one()?;
            if s.rank() < 2 {
                return Err(mismatch("BatchNorm 은 [B, C, …] 입력이 필요"));
            }
            Ok(s)
        }
        LayerKind::Linear { out_features, .. } => {
            if *out_features == 0 {
                return Err(invalid("out_features 는 0 일 수 없음"));
            }
            let mut s = one()?;
            if s.rank() < 2 {
                return Err(mismatch("Linear 는 [B, …, in] 입력이 필요"));
            }
            *s.0.last_mut().unwrap() = Dim::Fixed(*out_features);
            Ok(s)
        }
        LayerKind::Conv2d { out_channels, kernel, stride, padding, .. } => {
            if *out_channels == 0 {
                return Err(invalid("out_channels 는 0 일 수 없음"));
            }
            let s = one()?;
            if s.rank() != 4 {
                return Err(mismatch(format!("Conv2d 는 [B, C, H, W] 입력이 필요 (지금 {s})")));
            }
            let (h, w) = (s.sample()[1], s.sample()[2]);
            let oh = conv_out(h, kernel[0], stride[0], padding[0])?;
            let ow = conv_out(w, kernel[1], stride[1], padding[1])?;
            Ok(Shape::from_sample(&[*out_channels, oh, ow]))
        }
        LayerKind::MaxPool2d { kernel, stride } | LayerKind::AvgPool2d { kernel, stride } => {
            let s = one()?;
            if s.rank() != 4 {
                return Err(mismatch(format!("풀링은 [B, C, H, W] 입력이 필요 (지금 {s})")));
            }
            let sm = s.sample();
            let oh = conv_out(sm[1], kernel[0], stride[0], 0)?;
            let ow = conv_out(sm[2], kernel[1], stride[1], 0)?;
            Ok(Shape::from_sample(&[sm[0], oh, ow]))
        }
        LayerKind::GlobalAvgPool => {
            let s = one()?;
            if s.rank() != 4 {
                return Err(mismatch(format!("GlobalAvgPool 은 [B, C, H, W] 입력이 필요 (지금 {s})")));
            }
            Ok(Shape::from_sample(&[s.sample()[0]]))
        }
        LayerKind::Flatten => {
            let s = one()?;
            Ok(Shape::from_sample(&[s.numel_sample()]))
        }
        LayerKind::Reshape { shape } => {
            let s = one()?;
            if shape.is_empty() || shape.contains(&0) {
                return Err(invalid("Reshape 형상은 비어 있거나 0 을 포함할 수 없음"));
            }
            let want: usize = shape.iter().product();
            if want != s.numel_sample() {
                return Err(mismatch(format!("원소 수 {} ≠ {}", s.numel_sample(), want)));
            }
            checked(Shape::from_sample(shape))
        }
        LayerKind::Add | LayerKind::Mul => {
            if inputs.len() < 2 {
                return Err(GraphError::MissingInput { slot: inputs.len() });
            }
            if inputs.iter().any(|s| s != &inputs[0]) {
                return Err(mismatch(format!("{} vs {}", inputs[0], inputs[1])));
            }
            Ok(inputs[0].clone())
        }
        LayerKind::Concat { dim } => {
            if inputs.len() < 2 {
                return Err(GraphError::MissingInput { slot: inputs.len() });
            }
            let a = inputs[0].sample();
            let b = inputs[1].sample();
            if a.len() != b.len() || *dim >= a.len() {
                return Err(mismatch(format!("랭크가 다르거나 dim {dim} 이 범위 밖: {} vs {}", inputs[0], inputs[1])));
            }
            for i in 0..a.len() {
                if i != *dim && a[i] != b[i] {
                    return Err(mismatch(format!("dim {dim} 외의 차원이 다름: {} vs {}", inputs[0], inputs[1])));
                }
            }
            let mut out = a.clone();
            out[*dim] = a[*dim] + b[*dim];
            checked(Shape::from_sample(&out))
        }
        LayerKind::Embedding { vocab, dim } => {
            if *vocab == 0 || *dim == 0 {
                return Err(invalid("vocab·dim 은 0 일 수 없음"));
            }
            let s = one()?;
            let mut out = s.sample();
            out.push(*dim);
            checked(Shape::from_sample(&out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Act, Node, Port};

    fn add(g: &mut Graph, kind: LayerKind) -> NodeId {
        g.add_node(Node::new(kind, [0.0, 0.0]))
    }
    fn link(g: &mut Graph, a: NodeId, b: NodeId) {
        g.add_edge(a, Port::new(b, 0)).unwrap();
    }

    #[test]
    fn mlp_chain() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![4] });
        let l1 = add(&mut g, LayerKind::Linear { out_features: 8, bias: true });
        let a = add(&mut g, LayerKind::Activation { act: Act::Relu });
        let l2 = add(&mut g, LayerKind::Linear { out_features: 2, bias: true });
        let o = add(&mut g, LayerKind::Output);
        link(&mut g, i, l1);
        link(&mut g, l1, a);
        link(&mut g, a, l2);
        link(&mut g, l2, o);
        let r = infer(&g);
        assert!(r.is_ok(), "{:?}", r.errors);
        assert_eq!(r.shape(o).unwrap().sample(), vec![2]);
        assert_eq!(r.order, vec![i, l1, a, l2, o]);
    }

    #[test]
    fn cnn_chain() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![1, 28, 28] });
        let c = add(&mut g, LayerKind::Conv2d { out_channels: 8, kernel: [3, 3], stride: [1, 1], padding: [1, 1], bias: true });
        let p = add(&mut g, LayerKind::MaxPool2d { kernel: [2, 2], stride: [2, 2] });
        let f = add(&mut g, LayerKind::Flatten);
        let l = add(&mut g, LayerKind::Linear { out_features: 10, bias: true });
        link(&mut g, i, c);
        link(&mut g, c, p);
        link(&mut g, p, f);
        link(&mut g, f, l);
        let r = infer(&g);
        assert!(r.is_ok(), "{:?}", r.errors);
        assert_eq!(r.shape(c).unwrap().sample(), vec![8, 28, 28]);
        assert_eq!(r.shape(p).unwrap().sample(), vec![8, 14, 14]);
        assert_eq!(r.shape(f).unwrap().sample(), vec![8 * 14 * 14]);
        assert_eq!(r.shape(l).unwrap().to_string(), "[B, 10]");
    }

    #[test]
    fn residual_add_and_concat() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![8] });
        let l = add(&mut g, LayerKind::Linear { out_features: 8, bias: true });
        let s = add(&mut g, LayerKind::Add);
        let c = add(&mut g, LayerKind::Concat { dim: 0 });
        link(&mut g, i, l);
        g.add_edge(l, Port::new(s, 0)).unwrap();
        g.add_edge(i, Port::new(s, 1)).unwrap();
        g.add_edge(s, Port::new(c, 0)).unwrap();
        g.add_edge(i, Port::new(c, 1)).unwrap();
        let r = infer(&g);
        assert!(r.is_ok(), "{:?}", r.errors);
        assert_eq!(r.shape(s).unwrap().sample(), vec![8]);
        assert_eq!(r.shape(c).unwrap().sample(), vec![16]);
    }

    #[test]
    fn errors_are_isolated_not_fatal() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![4] });
        let conv = add(&mut g, LayerKind::Conv2d { out_channels: 4, kernel: [3, 3], stride: [1, 1], padding: [0, 0], bias: true });
        let lonely = add(&mut g, LayerKind::Linear { out_features: 2, bias: true });
        let after = add(&mut g, LayerKind::Activation { act: Act::Relu });
        link(&mut g, i, conv);
        link(&mut g, conv, after);
        let r = infer(&g);
        assert!(matches!(r.errors[&conv], GraphError::ShapeMismatch { .. }));
        assert_eq!(r.errors[&lonely], GraphError::MissingInput { slot: 0 });
        assert_eq!(r.errors[&after], GraphError::UpstreamError);
        assert!(r.shapes.contains_key(&i));
    }

    #[test]
    fn cycle_nodes_are_reported() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![4] });
        let a = add(&mut g, LayerKind::Linear { out_features: 4, bias: true });
        let b = add(&mut g, LayerKind::Add);
        link(&mut g, i, a);
        g.add_edge(a, Port::new(b, 0)).unwrap();
        assert!(g.add_edge(b, Port::new(a, 0)).is_none(), "a 의 슬롯 0 은 i 가 점유 → 거부됨");
        // 진짜 순환: b → a 대신 a ↔ b 를 새 노드로
        let c = add(&mut g, LayerKind::Activation { act: Act::Relu });
        g.add_edge(b, Port::new(c, 0)).unwrap();
        g.add_edge(c, Port::new(b, 1)).unwrap();
        let r = infer(&g);
        assert!(r.cycle_nodes.contains(&b) && r.cycle_nodes.contains(&c));
        assert_eq!(r.errors[&b], GraphError::Cycle);
        assert!(r.shapes.contains_key(&a), "순환 밖은 계속 추론된다");
    }

    #[test]
    fn rank_above_the_engine_limit_is_a_shape_error() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![1, 8, 8] });
        // 샘플 랭크 5 → 배치 포함 6 → 실행 불가.
        let r = add(&mut g, LayerKind::Reshape { shape: vec![1, 2, 2, 4, 4] });
        link(&mut g, i, r);
        let rep = infer(&g);
        assert!(matches!(rep.errors[&r], GraphError::ShapeMismatch { .. }), "{:?}", rep.errors);
        assert!(rep.errors[&r].to_string().contains("최대 랭크"));

        // 상한 안(배치 포함 5)은 그대로 통과한다.
        let mut g2 = Graph::default();
        let i2 = add(&mut g2, LayerKind::Input { shape: vec![1, 8, 8] });
        let r2 = add(&mut g2, LayerKind::Reshape { shape: vec![1, 2, 4, 8] });
        link(&mut g2, i2, r2);
        assert!(infer(&g2).is_ok());
    }

    #[test]
    fn embedding_and_concat_respect_the_rank_limit() {
        // Input [a,b,c,d] (랭크 5) → Embedding 이 랭크 6 을 만들려 한다.
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![2, 2, 2, 2] });
        let e = add(&mut g, LayerKind::Embedding { vocab: 10, dim: 4 });
        link(&mut g, i, e);
        assert!(matches!(infer(&g).errors[&e], GraphError::ShapeMismatch { .. }));
    }

    #[test]
    fn reshape_checks_numel() {
        let mut g = Graph::default();
        let i = add(&mut g, LayerKind::Input { shape: vec![16] });
        let ok = add(&mut g, LayerKind::Reshape { shape: vec![1, 4, 4] });
        let bad = add(&mut g, LayerKind::Reshape { shape: vec![3, 3] });
        link(&mut g, i, ok);
        link(&mut g, i, bad);
        let r = infer(&g);
        assert_eq!(r.shape(ok).unwrap().sample(), vec![1, 4, 4]);
        assert!(matches!(r.errors[&bad], GraphError::ShapeMismatch { .. }));
    }
}
