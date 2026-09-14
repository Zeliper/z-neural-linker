//! 런타임 정의 그래프 인터프리터.
//!
//! burn 의 텐서는 랭크가 const generic 이라 문서에서 읽은 그래프를 그대로 실행할 수 없다.
//! 그래서 랭크 1..5 를 [`DynTensor`] 로 감싸고, `LayerKind` 17 종을 `B: Backend` 제네릭 연산으로 옮긴다.
//! 순전파는 `Backend`, 학습은 `Autodiff<B>` — **같은 코드**가 돈다.
//!
//! 실행 순서는 `nl_core::shape::infer(&graph).order`, 입력 텐서 순서는 `Graph::input_nodes()`,
//! 출력은 `Graph::output_nodes()` 다.
//!
//! 파라미터 이름 규약: `"{node_id}.{weight|bias|gamma|beta|running_mean|running_var}"`.

use crate::tensor::HostTensor;
use anyhow::{bail, Context, Result};
use burn::tensor::backend::{AutodiffBackend, Backend};
use burn::tensor::ops::ConvOptions;
use burn::tensor::{activation, module, Distribution, ElementConversion, Int, Shape, Tensor, TensorData};
use nl_core::model::{Act, Graph, LayerKind, ModelDef};
use nl_core::shape::{self, ShapeReport, MAX_RANK};
use nl_core::NodeId;
use std::collections::{BTreeMap, BTreeSet};

// ───────────────────────────── DynTensor ─────────────────────────────

/// 랭크를 런타임 값으로 들고 다니는 텐서. 지원 랭크는 1..=[`MAX_RANK`] (배치 차원 포함).
///
/// 상한은 `nl_core::shape::MAX_RANK` 와 같은 값이어야 한다 — 그래야 편집기의 형상 검사와
/// 실행이 같은 계약을 본다. 아래 `const _:` 가 둘이 어긋나면 컴파일을 막는다.
#[derive(Clone, Debug)]
pub enum DynTensor<B: Backend> {
    R1(Tensor<B, 1>),
    R2(Tensor<B, 2>),
    R3(Tensor<B, 3>),
    R4(Tensor<B, 4>),
    R5(Tensor<B, 5>),
}

// DynTensor 는 R1..R5 만 담는다. nl-core 의 상한이 바뀌면 여기도 함께 바꿔야 한다.
const _: () = assert!(
    MAX_RANK == 5,
    "DynTensor 의 변형 수와 nl_core::shape::MAX_RANK 가 어긋납니다"
);

/// 랭크별 단형화를 한 곳에 모으는 매크로 — 단항 연산.
macro_rules! map_t {
    ($t:expr, |$x:ident| $body:expr) => {
        match $t {
            $crate::exec::DynTensor::R1($x) => $crate::exec::DynTensor::R1($body),
            $crate::exec::DynTensor::R2($x) => $crate::exec::DynTensor::R2($body),
            $crate::exec::DynTensor::R3($x) => $crate::exec::DynTensor::R3($body),
            $crate::exec::DynTensor::R4($x) => $crate::exec::DynTensor::R4($body),
            $crate::exec::DynTensor::R5($x) => $crate::exec::DynTensor::R5($body),
        }
    };
}

/// 랭크가 같은 두 텐서에 대한 이항 연산.
macro_rules! zip_t {
    ($a:expr, $b:expr, |$x:ident, $y:ident| $body:expr) => {
        match ($a, $b) {
            ($crate::exec::DynTensor::R1($x), $crate::exec::DynTensor::R1($y)) => {
                Ok($crate::exec::DynTensor::R1($body))
            }
            ($crate::exec::DynTensor::R2($x), $crate::exec::DynTensor::R2($y)) => {
                Ok($crate::exec::DynTensor::R2($body))
            }
            ($crate::exec::DynTensor::R3($x), $crate::exec::DynTensor::R3($y)) => {
                Ok($crate::exec::DynTensor::R3($body))
            }
            ($crate::exec::DynTensor::R4($x), $crate::exec::DynTensor::R4($y)) => {
                Ok($crate::exec::DynTensor::R4($body))
            }
            ($crate::exec::DynTensor::R5($x), $crate::exec::DynTensor::R5($y)) => {
                Ok($crate::exec::DynTensor::R5($body))
            }
            (a, b) => Err(::anyhow::anyhow!("랭크가 다른 텐서 연산: {} vs {}", a.rank(), b.rank())),
        }
    };
}

impl<B: Backend> DynTensor<B> {
    pub fn rank(&self) -> usize {
        match self {
            DynTensor::R1(_) => 1,
            DynTensor::R2(_) => 2,
            DynTensor::R3(_) => 3,
            DynTensor::R4(_) => 4,
            DynTensor::R5(_) => 5,
        }
    }

    pub fn dims(&self) -> Vec<usize> {
        match self {
            DynTensor::R1(t) => t.dims().to_vec(),
            DynTensor::R2(t) => t.dims().to_vec(),
            DynTensor::R3(t) => t.dims().to_vec(),
            DynTensor::R4(t) => t.dims().to_vec(),
            DynTensor::R5(t) => t.dims().to_vec(),
        }
    }

    pub fn numel(&self) -> usize {
        self.dims().iter().product()
    }

    pub fn device(&self) -> B::Device {
        match self {
            DynTensor::R1(t) => t.device(),
            DynTensor::R2(t) => t.device(),
            DynTensor::R3(t) => t.device(),
            DynTensor::R4(t) => t.device(),
            DynTensor::R5(t) => t.device(),
        }
    }

    /// 원소 수가 같아야 한다. 목표 랭크는 1..5.
    pub fn reshape(self, target: &[usize]) -> Result<Self> {
        let want: usize = target.iter().product();
        if want != self.numel() {
            bail!("reshape 원소 수 불일치: {:?} → {:?}", self.dims(), target);
        }
        macro_rules! go {
            ($t:expr) => {{
                let sh = Shape::from(target.to_vec());
                match target.len() {
                    1 => DynTensor::R1($t.reshape(sh)),
                    2 => DynTensor::R2($t.reshape(sh)),
                    3 => DynTensor::R3($t.reshape(sh)),
                    4 => DynTensor::R4($t.reshape(sh)),
                    5 => DynTensor::R5($t.reshape(sh)),
                    n => bail!("지원하지 않는 랭크 {n} (최대 {MAX_RANK})"),
                }
            }};
        }
        Ok(match self {
            DynTensor::R1(t) => go!(t),
            DynTensor::R2(t) => go!(t),
            DynTensor::R3(t) => go!(t),
            DynTensor::R4(t) => go!(t),
            DynTensor::R5(t) => go!(t),
        })
    }

    /// `dim` 차원에서 `[start, start + len)` 만 잘라낸다 (뷰).
    pub fn narrow_dim(self, dim: usize, start: usize, len: usize) -> Result<Self> {
        let dims = self.dims();
        if dim >= dims.len() {
            bail!("narrow dim {dim} 이 랭크 {} 밖", dims.len());
        }
        if start + len > dims[dim] {
            bail!(
                "narrow 범위 [{start}, {}) 가 차원 {dim} 크기 {} 를 넘습니다",
                start + len,
                dims[dim]
            );
        }
        Ok(map_t!(self, |t| t.narrow(dim, start, len)))
    }

    /// 0번(배치) 차원에서 `indices` 순서로 행을 고른다 — 에포크 셔플에 쓴다.
    pub fn select_rows(self, indices: &Tensor<B, 1, Int>) -> Self {
        map_t!(self, |t| t.select(0, indices.clone()))
    }

    pub fn into_r1(self) -> Result<Tensor<B, 1>> {
        match self {
            DynTensor::R1(t) => Ok(t),
            o => bail!("랭크 1 텐서가 필요한데 랭크 {}", o.rank()),
        }
    }
    pub fn into_r2(self) -> Result<Tensor<B, 2>> {
        match self {
            DynTensor::R2(t) => Ok(t),
            o => bail!("랭크 2 텐서가 필요한데 랭크 {}", o.rank()),
        }
    }
    pub fn into_r4(self) -> Result<Tensor<B, 4>> {
        match self {
            DynTensor::R4(t) => Ok(t),
            o => bail!("랭크 4 텐서가 필요한데 랭크 {}", o.rank()),
        }
    }

    pub fn square(self) -> Self {
        map_t!(self, |t| {
            let c = t.clone();
            t * c
        })
    }
    pub fn sqrt(self) -> Self {
        map_t!(self, |t| t.sqrt())
    }
    pub fn abs(self) -> Self {
        map_t!(self, |t| t.abs())
    }
    pub fn detach(self) -> Self {
        map_t!(self, |t| t.detach())
    }
    pub fn mul_scalar(self, v: f64) -> Self {
        map_t!(self, |t| t.mul_scalar(v))
    }
    pub fn add_scalar(self, v: f64) -> Self {
        map_t!(self, |t| t.add_scalar(v))
    }
    pub fn zeros_like(&self) -> Self {
        map_t!(self.clone(), |t| t.zeros_like())
    }
    /// 랭크·형상이 맞아야 하므로 `std::ops` 트레이트 대신 `Result` 를 돌려주는 메서드다.
    pub fn try_add(self, other: Self) -> Result<Self> {
        zip_t!(self, other, |a, b| a + b)
    }
    pub fn try_sub(self, other: Self) -> Result<Self> {
        zip_t!(self, other, |a, b| a - b)
    }
    pub fn try_mul(self, other: Self) -> Result<Self> {
        zip_t!(self, other, |a, b| a * b)
    }
    pub fn try_div(self, other: Self) -> Result<Self> {
        zip_t!(self, other, |a, b| a / b)
    }
    pub fn negate(self) -> Self {
        map_t!(self, |t| t.neg())
    }
    pub fn exp(self) -> Self {
        map_t!(self, |t| t.exp())
    }
    pub fn log1p(self) -> Self {
        map_t!(self, |t| t.log1p())
    }
    pub fn clamp_min(self, v: f64) -> Self {
        map_t!(self, |t| t.clamp_min(v))
    }
    /// 모든 원소의 평균 (스칼라 텐서).
    pub fn mean_all(self) -> Tensor<B, 1> {
        match self {
            DynTensor::R1(t) => t.mean(),
            DynTensor::R2(t) => t.mean(),
            DynTensor::R3(t) => t.mean(),
            DynTensor::R4(t) => t.mean(),
            DynTensor::R5(t) => t.mean(),
        }
    }
    /// 모든 원소 제곱합을 **장치 텐서로** 돌려준다.
    ///
    /// 값을 읽지 않으므로 장치 동기화가 없다. 파라미터마다 스칼라를 읽으면 GPU 에서는 그 동기화가
    /// 스텝 비용을 지배한다 — 여러 개를 텐서로 더한 뒤 한 번만 읽는다.
    pub fn sum_squares_tensor(&self) -> Tensor<B, 1> {
        match self.clone().square() {
            DynTensor::R1(t) => t.sum(),
            DynTensor::R2(t) => t.sum(),
            DynTensor::R3(t) => t.sum(),
            DynTensor::R4(t) => t.sum(),
            DynTensor::R5(t) => t.sum(),
        }
    }

    /// 모든 원소 제곱합 (호스트 값). **장치를 동기화한다** — 루프 안에서 반복하지 말 것.
    pub fn sum_squares(&self) -> f64 {
        self.sum_squares_tensor().into_scalar().elem::<f64>()
    }

    pub fn to_host(&self) -> HostTensor {
        let dims = self.dims();
        let data = match self.clone() {
            DynTensor::R1(t) => t.into_data(),
            DynTensor::R2(t) => t.into_data(),
            DynTensor::R3(t) => t.into_data(),
            DynTensor::R4(t) => t.into_data(),
            DynTensor::R5(t) => t.into_data(),
        };
        let v = data.convert::<f32>().to_vec::<f32>().unwrap_or_default();
        HostTensor::new(dims, v)
    }

    pub fn from_host(h: &HostTensor, device: &B::Device) -> Result<Self> {
        if h.shape.iter().product::<usize>() != h.data.len() {
            bail!("HostTensor 형상 {:?} 과 데이터 길이 {} 불일치", h.shape, h.data.len());
        }
        let data = TensorData::new(h.data.clone(), Shape::from(h.shape.clone()));
        Ok(match h.shape.len() {
            1 => DynTensor::R1(Tensor::from_data(data, device)),
            2 => DynTensor::R2(Tensor::from_data(data, device)),
            3 => DynTensor::R3(Tensor::from_data(data, device)),
            4 => DynTensor::R4(Tensor::from_data(data, device)),
            5 => DynTensor::R5(Tensor::from_data(data, device)),
            n => bail!("지원하지 않는 랭크 {n} (최대 {MAX_RANK})"),
        })
    }

    pub fn require_grad(self) -> Self {
        map_t!(self, |t| t.require_grad())
    }
}

impl<B: AutodiffBackend> DynTensor<B> {
    /// autodiff 정보를 벗긴다.
    pub fn inner(self) -> DynTensor<B::InnerBackend> {
        match self {
            DynTensor::R1(t) => DynTensor::R1(t.inner()),
            DynTensor::R2(t) => DynTensor::R2(t.inner()),
            DynTensor::R3(t) => DynTensor::R3(t.inner()),
            DynTensor::R4(t) => DynTensor::R4(t.inner()),
            DynTensor::R5(t) => DynTensor::R5(t.inner()),
        }
    }

    pub fn from_inner(inner: DynTensor<B::InnerBackend>) -> Self {
        match inner {
            DynTensor::R1(t) => DynTensor::R1(Tensor::from_inner(t)),
            DynTensor::R2(t) => DynTensor::R2(Tensor::from_inner(t)),
            DynTensor::R3(t) => DynTensor::R3(Tensor::from_inner(t)),
            DynTensor::R4(t) => DynTensor::R4(Tensor::from_inner(t)),
            DynTensor::R5(t) => DynTensor::R5(Tensor::from_inner(t)),
        }
    }

    /// `Gradients` 에서 이 텐서의 grad 를 꺼낸다 (없으면 `None`).
    pub fn grad_remove(&self, grads: &mut B::Gradients) -> Option<DynTensor<B::InnerBackend>> {
        match self {
            DynTensor::R1(t) => t.grad_remove(grads).map(DynTensor::R1),
            DynTensor::R2(t) => t.grad_remove(grads).map(DynTensor::R2),
            DynTensor::R3(t) => t.grad_remove(grads).map(DynTensor::R3),
            DynTensor::R4(t) => t.grad_remove(grads).map(DynTensor::R4),
            DynTensor::R5(t) => t.grad_remove(grads).map(DynTensor::R5),
        }
    }
}

// ───────────────────────────── 파라미터 이름 ─────────────────────────────

pub const P_WEIGHT: &str = "weight";
pub const P_BIAS: &str = "bias";
pub const P_GAMMA: &str = "gamma";
pub const P_BETA: &str = "beta";
pub const P_RUNNING_MEAN: &str = "running_mean";
pub const P_RUNNING_VAR: &str = "running_var";
/// 순환 레이어: 입력→게이트 가중치 `[D, gates·H]`.
pub const P_WEIGHT_IH: &str = "weight_ih";
/// 순환 레이어: 은닉→게이트 가중치 `[H, gates·H]`.
pub const P_WEIGHT_HH: &str = "weight_hh";
/// 순환 레이어: 입력 쪽 편향 `[gates·H]`.
pub const P_BIAS_IH: &str = "bias_ih";
/// 순환 레이어: 은닉 쪽 편향 `[gates·H]`. GRU 의 `n` 게이트가 `r` 과 곱해야 해 따로 둔다.
pub const P_BIAS_HH: &str = "bias_hh";
/// 양방향의 역방향 파라미터에 붙는 꼬리표 (`weight_ih_reverse` 처럼).
pub const P_REVERSE_SUFFIX: &str = "_reverse";

/// `"{node_id}.{part}"`.
pub fn param_name(node: NodeId, part: &str) -> String {
    format!("{node}.{part}")
}

// ───────────────────────────── 모델 ─────────────────────────────

/// 그래프 + 파라미터. 순전파(`forward`)와 파라미터 관리를 담당한다.
pub struct Model<B: Backend> {
    graph: Graph,
    report: ShapeReport,
    input_nodes: Vec<NodeId>,
    output_nodes: Vec<NodeId>,
    device: B::Device,
    /// 파라미터 (학습 대상 + BatchNorm running 통계).
    params: BTreeMap<String, DynTensor<B>>,
    /// 학습 대상 파라미터 이름 (running 통계는 빠진다).
    trainable: BTreeSet<String>,
}

impl<B: Backend> Model<B> {
    /// 그래프를 검사하고 파라미터를 무작위 초기화한다. `seed` 로 재현성을 준다.
    pub fn new(def: &ModelDef, device: &B::Device, seed: u64) -> Result<Self> {
        B::seed(device, seed);
        Self::build(def, device)
    }

    /// 시드를 다시 심지 않는 생성 (이미 심은 뒤 여러 모델을 만들 때).
    pub fn build(def: &ModelDef, device: &B::Device) -> Result<Self> {
        let graph = def.graph.clone();
        let report = shape::infer(&graph);
        if !report.errors.is_empty() {
            bail!("{}", describe_graph_errors(&graph, &report));
        }
        let input_nodes = graph.input_nodes();
        let output_nodes = graph.output_nodes();
        if input_nodes.is_empty() {
            bail!("Input 레이어가 없습니다");
        }
        if output_nodes.is_empty() {
            bail!("Output 레이어가 없습니다");
        }
        let mut m = Self {
            graph,
            report,
            input_nodes,
            output_nodes,
            device: device.clone(),
            params: BTreeMap::new(),
            trainable: BTreeSet::new(),
        };
        m.init_params()?;
        Ok(m)
    }

    pub fn device(&self) -> &B::Device {
        &self.device
    }
    pub fn graph(&self) -> &Graph {
        &self.graph
    }
    pub fn input_nodes(&self) -> &[NodeId] {
        &self.input_nodes
    }
    pub fn output_nodes(&self) -> &[NodeId] {
        &self.output_nodes
    }
    /// 입력 노드별 샘플 형상 (배치 제외), `input_nodes()` 순서.
    pub fn input_sample_shapes(&self) -> Vec<Vec<usize>> {
        self.input_nodes
            .iter()
            .filter_map(|id| self.report.shape(*id))
            .map(|s| s.sample())
            .collect()
    }
    /// 출력 노드별 샘플 형상 (배치 제외), `output_nodes()` 순서.
    pub fn output_sample_shapes(&self) -> Vec<Vec<usize>> {
        self.output_nodes
            .iter()
            .filter_map(|id| self.report.shape(*id))
            .map(|s| s.sample())
            .collect()
    }
    pub fn trainable_names(&self) -> Vec<String> {
        self.trainable.iter().cloned().collect()
    }
    pub fn param(&self, name: &str) -> Option<&DynTensor<B>> {
        self.params.get(name)
    }
    pub fn set_param(&mut self, name: &str, t: DynTensor<B>) {
        self.params.insert(name.to_string(), t);
    }
    /// 학습 대상 파라미터의 원소 수 합.
    pub fn trainable_count(&self) -> usize {
        self.trainable
            .iter()
            .filter_map(|n| self.params.get(n))
            .map(|t| t.numel())
            .sum()
    }

    /// 모든 파라미터를 호스트 텐서로 (safetensors 저장용).
    pub fn host_params(&self) -> BTreeMap<String, HostTensor> {
        self.params.iter().map(|(k, v)| (k.clone(), v.to_host())).collect()
    }

    /// 저장된 파라미터를 얹는다. 이름·형상이 맞지 않으면 오류.
    pub fn load_host_params(&mut self, map: &BTreeMap<String, HostTensor>) -> Result<()> {
        let extra: Vec<&String> = map.keys().filter(|k| !self.params.contains_key(*k)).collect();
        if !extra.is_empty() {
            let shown: Vec<&str> = extra.iter().take(5).map(|s| s.as_str()).collect();
            log::warn!(
                "체크포인트에 이 모델에 없는 파라미터가 {} 개 있습니다 (무시합니다): {}{}",
                extra.len(),
                shown.join(", "),
                if extra.len() > shown.len() { " …" } else { "" }
            );
        }
        for (name, want) in self
            .params
            .iter()
            .map(|(k, v)| (k.clone(), v.dims()))
            .collect::<Vec<_>>()
        {
            let got = map
                .get(&name)
                .with_context(|| format!("체크포인트에 파라미터 '{name}' 이 없습니다"))?;
            if got.shape != want {
                bail!(
                    "파라미터 '{name}' 형상 불일치: 체크포인트 {:?} vs 모델 {:?}",
                    got.shape,
                    want
                );
            }
            let t = DynTensor::from_host(got, &self.device)?;
            self.params.insert(name, t);
        }
        Ok(())
    }

    /// 학습 대상 파라미터에 `require_grad` 를 건다 (autodiff 백엔드에서만 의미가 있다).
    pub fn require_grad_all(&mut self) {
        for name in self.trainable.iter().cloned().collect::<Vec<_>>() {
            if let Some(t) = self.params.remove(&name) {
                self.params.insert(name, t.require_grad());
            }
        }
    }

    // ── 초기화 ──

    fn in_shape(&self, node: NodeId, slot: usize) -> Result<Vec<usize>> {
        let from = *self
            .graph
            .inputs_of(node)
            .get(&slot)
            .with_context(|| format!("노드 {node} 의 입력 {slot} 이 비어 있음"))?;
        Ok(self
            .report
            .shape(from)
            .with_context(|| format!("노드 {from} 의 형상을 알 수 없음"))?
            .sample())
    }

    fn uniform(&self, shape: Vec<usize>, bound: f64) -> Result<DynTensor<B>> {
        let d = Distribution::Uniform(-bound, bound);
        random_dyn::<B>(&shape, d, &self.device)
    }

    fn init_params(&mut self) -> Result<()> {
        let ids: Vec<NodeId> = self.graph.nodes.keys().copied().collect();
        for id in ids {
            let kind = self.graph.nodes[&id].kind.clone();
            match &kind {
                LayerKind::Linear { out_features, bias } => {
                    let s = self.in_shape(id, 0)?;
                    let fan_in = *s.last().context("Linear 입력 형상이 비어 있음")?;
                    let bound = 1.0 / (fan_in as f64).sqrt();
                    let w = self.uniform(vec![fan_in, *out_features], bound)?;
                    self.insert_trainable(param_name(id, P_WEIGHT), w);
                    if *bias {
                        let b = self.uniform(vec![*out_features], bound)?;
                        self.insert_trainable(param_name(id, P_BIAS), b);
                    }
                }
                LayerKind::Conv2d {
                    out_channels,
                    kernel,
                    bias,
                    ..
                } => {
                    let s = self.in_shape(id, 0)?;
                    if s.len() != 3 {
                        bail!("Conv2d 입력은 [C, H, W] 여야 합니다 (지금 {s:?})");
                    }
                    let fan_in = s[0] * kernel[0] * kernel[1];
                    let bound = 1.0 / (fan_in as f64).sqrt();
                    let w = self.uniform(vec![*out_channels, s[0], kernel[0], kernel[1]], bound)?;
                    self.insert_trainable(param_name(id, P_WEIGHT), w);
                    if *bias {
                        let b = self.uniform(vec![*out_channels], bound)?;
                        self.insert_trainable(param_name(id, P_BIAS), b);
                    }
                }
                LayerKind::BatchNorm { .. } => {
                    let s = self.in_shape(id, 0)?;
                    let c = *s.first().context("BatchNorm 입력 형상이 비어 있음")?;
                    let dev = self.device.clone();
                    self.insert_trainable(param_name(id, P_GAMMA), ones1::<B>(c, &dev));
                    self.insert_trainable(param_name(id, P_BETA), zeros1::<B>(c, &dev));
                    // running 통계는 학습 파라미터가 아니다 (grad 없음, 저장은 된다).
                    self.params.insert(param_name(id, P_RUNNING_MEAN), zeros1::<B>(c, &dev));
                    self.params.insert(param_name(id, P_RUNNING_VAR), ones1::<B>(c, &dev));
                }
                LayerKind::LayerNorm { .. } => {
                    let s = self.in_shape(id, 0)?;
                    let n = *s.last().context("LayerNorm 입력 형상이 비어 있음")?;
                    let dev = self.device.clone();
                    self.insert_trainable(param_name(id, P_GAMMA), ones1::<B>(n, &dev));
                    self.insert_trainable(param_name(id, P_BETA), zeros1::<B>(n, &dev));
                }
                LayerKind::Lstm {
                    hidden, bidirectional, ..
                }
                | LayerKind::Gru {
                    hidden, bidirectional, ..
                } => {
                    let sm = self.in_shape(id, 0)?;
                    if sm.len() != 2 {
                        bail!("순환 레이어 입력은 [L, D] 여야 합니다 (지금 {sm:?})");
                    }
                    let (d, h) = (sm[1], *hidden);
                    let gates = if matches!(kind, LayerKind::Lstm { .. }) { 4 } else { 3 };
                    // PyTorch 와 같은 초기화: 모든 가중치·편향이 U(-1/√H, 1/√H).
                    let bound = 1.0 / (h as f64).sqrt();
                    for dir in directions(*bidirectional) {
                        let w_ih = self.uniform(vec![d, gates * h], bound)?;
                        let w_hh = self.uniform(vec![h, gates * h], bound)?;
                        let b_ih = self.uniform(vec![gates * h], bound)?;
                        let b_hh = self.uniform(vec![gates * h], bound)?;
                        self.insert_trainable(rnn_name(id, P_WEIGHT_IH, dir), w_ih);
                        self.insert_trainable(rnn_name(id, P_WEIGHT_HH, dir), w_hh);
                        self.insert_trainable(rnn_name(id, P_BIAS_IH, dir), b_ih);
                        self.insert_trainable(rnn_name(id, P_BIAS_HH, dir), b_hh);
                    }
                }
                LayerKind::MultiHeadAttention { heads, .. } => {
                    let sm = self.in_shape(id, 0)?;
                    if sm.len() != 2 {
                        bail!("어텐션 입력은 [L, D] 여야 합니다 (지금 {sm:?})");
                    }
                    let d = sm[1];
                    if *heads == 0 || d % *heads != 0 {
                        bail!("특징 차원 {d} 가 헤드 수 {heads} 로 나누어떨어지지 않습니다");
                    }
                    let bound = 1.0 / (d as f64).sqrt();
                    for part in ["q", "k", "v", "out"] {
                        let w = self.uniform(vec![d, d], bound)?;
                        let b = self.uniform(vec![d], bound)?;
                        self.insert_trainable(param_name(id, &format!("{part}_weight")), w);
                        self.insert_trainable(param_name(id, &format!("{part}_bias")), b);
                    }
                }
                LayerKind::Embedding { vocab, dim } => {
                    let w = random_dyn::<B>(&[*vocab, *dim], Distribution::Normal(0.0, 1.0), &self.device)?;
                    self.insert_trainable(param_name(id, P_WEIGHT), w);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn insert_trainable(&mut self, name: String, t: DynTensor<B>) {
        self.trainable.insert(name.clone());
        self.params.insert(name, t);
    }

    // ── 순전파 ──

    fn p(&self, node: NodeId, part: &str) -> Result<DynTensor<B>> {
        self.params
            .get(&param_name(node, part))
            .cloned()
            .with_context(|| format!("파라미터 '{}' 가 없습니다", param_name(node, part)))
    }

    /// 입력은 `input_nodes()` 순서, 출력은 `output_nodes()` 순서. 배치 차원 포함.
    /// `train` 이 true 면 Dropout 이 걸리고 BatchNorm 이 배치 통계를 쓰며 running 통계를 갱신한다.
    pub fn forward(&mut self, inputs: Vec<DynTensor<B>>, train: bool) -> Result<Vec<DynTensor<B>>> {
        if inputs.len() != self.input_nodes.len() {
            bail!(
                "입력 텐서 {} 개가 필요한데 {} 개를 받았습니다",
                self.input_nodes.len(),
                inputs.len()
            );
        }
        let mut slot: BTreeMap<NodeId, DynTensor<B>> = BTreeMap::new();
        for (id, t) in self.input_nodes.iter().zip(inputs) {
            let want = self.report.shape(*id).map(|s| s.sample()).unwrap_or_default();
            let got = t.dims();
            if got.len() != want.len() + 1 || got[1..] != want[..] {
                bail!(
                    "입력 노드 {} 형상 불일치: [B, {:?}] 이어야 하는데 {:?}",
                    self.graph.nodes[id].display_name(),
                    want,
                    got
                );
            }
            slot.insert(*id, t);
        }

        let order = self.report.order.clone();
        for id in order {
            let kind = self.graph.nodes[&id].kind.clone();
            if matches!(kind, LayerKind::Input { .. }) {
                continue; // 이미 채워져 있다
            }
            let n_in = kind.spec().inputs;
            let sources = self.graph.inputs_of(id);
            let mut ins = Vec::with_capacity(n_in);
            for s in 0..n_in {
                let from = *sources.get(&s).with_context(|| format!("입력 {s} 가 비어 있음"))?;
                ins.push(
                    slot.get(&from)
                        .cloned()
                        .with_context(|| format!("노드 {from} 의 값이 없음"))?,
                );
            }
            let out = self
                .apply(id, &kind, ins, train)
                .with_context(|| format!("레이어 '{}' 실행 실패", self.graph.nodes[&id].display_name()))?;
            slot.insert(id, out);
        }

        self.output_nodes
            .iter()
            .map(|id| {
                slot.get(id)
                    .cloned()
                    .with_context(|| format!("출력 노드 {id} 의 값이 없음"))
            })
            .collect()
    }

    fn apply(&mut self, id: NodeId, kind: &LayerKind, ins: Vec<DynTensor<B>>, train: bool) -> Result<DynTensor<B>> {
        let n_in = kind.spec().inputs;
        if ins.len() < n_in {
            bail!("입력 {n_in} 개가 필요한데 {} 개", ins.len());
        }
        let x = || -> Result<DynTensor<B>> { ins.first().cloned().context("입력이 없습니다") };
        match kind {
            LayerKind::Input { .. } => x(),
            LayerKind::Output => x(),

            LayerKind::Linear { bias, .. } => {
                let w = self.p(id, P_WEIGHT)?.into_r2()?;
                let b = if *bias {
                    Some(self.p(id, P_BIAS)?.into_r1()?)
                } else {
                    None
                };
                Ok(map_t!(x()?, |t| module::linear(t, w.clone(), b.clone())))
            }

            LayerKind::Conv2d {
                stride, padding, bias, ..
            } => {
                let t = x()?.into_r4()?;
                let w = self.p(id, P_WEIGHT)?.into_r4()?;
                let b = if *bias {
                    Some(self.p(id, P_BIAS)?.into_r1()?)
                } else {
                    None
                };
                let opt = ConvOptions::new(*stride, *padding, [1, 1], 1);
                Ok(DynTensor::R4(module::conv2d(t, w, b, opt)))
            }

            LayerKind::MaxPool2d { kernel, stride } => {
                let t = x()?.into_r4()?;
                Ok(DynTensor::R4(module::max_pool2d(
                    t,
                    *kernel,
                    *stride,
                    [0, 0],
                    [1, 1],
                    false,
                )))
            }

            LayerKind::AvgPool2d { kernel, stride } => {
                let t = x()?.into_r4()?;
                Ok(DynTensor::R4(module::avg_pool2d(
                    t,
                    *kernel,
                    *stride,
                    [0, 0],
                    true,
                    false,
                )))
            }

            LayerKind::GlobalAvgPool => {
                let t = x()?.into_r4()?;
                let d = t.dims();
                let pooled = t.mean_dim(2).mean_dim(3);
                DynTensor::R4(pooled).reshape(&[d[0], d[1]])
            }

            LayerKind::Flatten => {
                let t = x()?;
                let d = t.dims();
                let rest: usize = d[1..].iter().product();
                t.reshape(&[d[0], rest])
            }

            LayerKind::Reshape { shape } => {
                let t = x()?;
                let mut target = vec![t.dims()[0]];
                target.extend(shape.iter().copied());
                t.reshape(&target)
            }

            LayerKind::Activation { act } => Ok(activate(x()?, *act)),

            LayerKind::Dropout { p } => {
                let t = x()?;
                if !train || *p <= 0.0 {
                    return Ok(t);
                }
                if *p >= 1.0 {
                    bail!("Dropout p 는 1 보다 작아야 합니다 (지금 {p})");
                }
                let keep = 1.0 - *p as f64;
                Ok(map_t!(t, |v| {
                    let mask = v.random_like(Distribution::Bernoulli(keep));
                    v * mask * (1.0 / keep)
                }))
            }

            LayerKind::BatchNorm { eps, momentum } => {
                let t = x()?;
                let gamma = self.p(id, P_GAMMA)?;
                let beta = self.p(id, P_BETA)?;
                let rmean = self.p(id, P_RUNNING_MEAN)?;
                let rvar = self.p(id, P_RUNNING_VAR)?;
                let (y, updated) = batch_norm(t, gamma, beta, rmean, rvar, *eps, *momentum, train)?;
                if let Some((m, v)) = updated {
                    self.params.insert(param_name(id, P_RUNNING_MEAN), m);
                    self.params.insert(param_name(id, P_RUNNING_VAR), v);
                }
                Ok(y)
            }

            LayerKind::LayerNorm { eps } => {
                let t = x()?;
                let gamma = self.p(id, P_GAMMA)?;
                let beta = self.p(id, P_BETA)?;
                layer_norm(t, gamma, beta, *eps)
            }

            LayerKind::Add => ins[0].clone().try_add(ins[1].clone()),
            LayerKind::Mul => ins[0].clone().try_mul(ins[1].clone()),
            LayerKind::Concat { dim } => concat(ins[0].clone(), ins[1].clone(), dim + 1),

            LayerKind::Embedding { vocab, dim } => {
                let t = x()?;
                let d = t.dims();
                if d.len() < 2 {
                    bail!("Embedding 입력은 [B, …] 여야 합니다 (지금 {d:?})");
                }
                let len: usize = d[1..].iter().product();
                let flat = t.reshape(&[d[0], len])?;
                // 인덱스는 작은 텐서라 한 번 호스트로 내려 검사한다. 검사가 없으면 범위를 넘길 때
                // 백엔드 깊은 곳에서 터지고(ndarray) wgpu 는 조용히 엉뚱한 값을 읽는다.
                check_embedding_indices(&flat.to_host(), *vocab)?;
                let idx = flat.into_r2()?.int();
                let w = self.p(id, P_WEIGHT)?.into_r2()?;
                let out = module::embedding(w, idx);
                let mut target = d.clone();
                target.push(*dim);
                DynTensor::R3(out).reshape(&target)
            }

            LayerKind::Lstm {
                hidden,
                bidirectional,
                return_sequence,
            } => {
                let x3 = to_rank3(x()?, "LSTM")?;
                let w = self.rnn_params(id, *bidirectional)?;
                Ok(recurrent(x3, &w, *hidden, *return_sequence, Cell::Lstm))
            }
            LayerKind::Gru {
                hidden,
                bidirectional,
                return_sequence,
            } => {
                let x3 = to_rank3(x()?, "GRU")?;
                let w = self.rnn_params(id, *bidirectional)?;
                Ok(recurrent(x3, &w, *hidden, *return_sequence, Cell::Gru))
            }
            LayerKind::MultiHeadAttention { heads, dropout } => {
                let x3 = to_rank3(x()?, "어텐션")?;
                let mut proj = Vec::with_capacity(4);
                for part in ["q", "k", "v", "out"] {
                    proj.push((
                        self.p(id, &format!("{part}_weight"))?.into_r2()?,
                        self.p(id, &format!("{part}_bias"))?.into_r1()?,
                    ));
                }
                Ok(DynTensor::R3(self_attention(x3, *heads, *dropout, train, &proj)?))
            }
        }
    }

    /// 순환 레이어의 방향별 파라미터를 꺼낸다 (정방향 먼저).
    fn rnn_params(&self, id: NodeId, bidirectional: bool) -> Result<Vec<RnnWeights<B>>> {
        directions(bidirectional)
            .into_iter()
            .map(|dir| {
                Ok(RnnWeights {
                    w_ih: self.p(id, &rnn_part(P_WEIGHT_IH, dir))?.into_r2()?,
                    w_hh: self.p(id, &rnn_part(P_WEIGHT_HH, dir))?.into_r2()?,
                    b_ih: self.p(id, &rnn_part(P_BIAS_IH, dir))?.into_r1()?,
                    b_hh: self.p(id, &rnn_part(P_BIAS_HH, dir))?.into_r1()?,
                })
            })
            .collect()
    }
}

// ───────────────────────────── 순환 · 어텐션 ─────────────────────────────

/// 방향 표식. `false` = 정방향, `true` = 역방향.
type Direction = bool;

/// 이 레이어가 쓰는 방향 목록 (정방향 먼저).
fn directions(bidirectional: bool) -> Vec<Direction> {
    if bidirectional {
        vec![false, true]
    } else {
        vec![false]
    }
}

/// `weight_ih` → `weight_ih_reverse` 처럼 방향 꼬리표를 붙인다.
fn rnn_part(base: &str, dir: Direction) -> String {
    if dir {
        format!("{base}{P_REVERSE_SUFFIX}")
    } else {
        base.to_string()
    }
}

/// `"{node_id}.{part}[_reverse]"`.
pub fn rnn_name(node: NodeId, base: &str, dir: Direction) -> String {
    param_name(node, &rnn_part(base, dir))
}

/// 순환 셀 종류.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cell {
    Lstm,
    Gru,
}

/// 한 방향의 파라미터.
struct RnnWeights<B: Backend> {
    w_ih: Tensor<B, 2>,
    w_hh: Tensor<B, 2>,
    b_ih: Tensor<B, 1>,
    b_hh: Tensor<B, 1>,
}

fn to_rank3<B: Backend>(t: DynTensor<B>, what: &str) -> Result<Tensor<B, 3>> {
    match t {
        DynTensor::R3(t) => Ok(t),
        o => bail!("{what} 은 [B, L, D] 입력이 필요합니다 (지금 랭크 {})", o.rank()),
    }
}

/// LSTM/GRU 를 게이트 수식 그대로 편다.
///
/// 그래프가 런타임에 정해지므로 burn 의 `nn::Lstm` 모듈(정적 `Module` 파생)을 쓸 수 없다.
/// 대신 파라미터 텐서를 직접 들고 시간축을 한 스텝씩 도는데, 이는 `Linear`·`Conv2d` 와 같은 방식이다.
///
/// 게이트 배치는 PyTorch 와 같다. LSTM 은 `[i, f, g, o]`, GRU 는 `[r, z, n]` 순서로
/// `[D, gates·H]` 안에 이어 붙는다.
fn recurrent<B: Backend>(
    x: Tensor<B, 3>,
    weights: &[RnnWeights<B>],
    hidden: usize,
    return_sequence: bool,
    cell: Cell,
) -> DynTensor<B> {
    let [batch, len, _] = x.dims();
    let device = x.device();
    let mut per_direction: Vec<Vec<Tensor<B, 2>>> = Vec::with_capacity(weights.len());

    for (dir, w) in weights.iter().enumerate() {
        let reverse = dir == 1;
        let mut h = Tensor::<B, 2>::zeros([batch, hidden], &device);
        let mut c = Tensor::<B, 2>::zeros([batch, hidden], &device);
        let mut outs: Vec<Tensor<B, 2>> = Vec::with_capacity(len);

        for step in 0..len {
            let t = if reverse { len - 1 - step } else { step };
            let xt = x.clone().narrow(1, t, 1).squeeze_dim::<2>(1);
            let gi = module::linear(xt, w.w_ih.clone(), Some(w.b_ih.clone()));
            let gh = module::linear(h.clone(), w.w_hh.clone(), Some(w.b_hh.clone()));
            let take = |t: &Tensor<B, 2>, k: usize| t.clone().narrow(1, k * hidden, hidden);

            match cell {
                Cell::Lstm => {
                    let g = gi + gh;
                    let i = activation::sigmoid(take(&g, 0));
                    let f = activation::sigmoid(take(&g, 1));
                    let gg = activation::tanh(take(&g, 2));
                    let o = activation::sigmoid(take(&g, 3));
                    c = f * c + i * gg;
                    h = o * activation::tanh(c.clone());
                }
                Cell::Gru => {
                    // n 게이트는 `r` 이 은닉 쪽 편향까지 곱해야 해서 gi/gh 를 따로 둔다.
                    let r = activation::sigmoid(take(&gi, 0) + take(&gh, 0));
                    let z = activation::sigmoid(take(&gi, 1) + take(&gh, 1));
                    let n = activation::tanh(take(&gi, 2) + r * take(&gh, 2));
                    h = (z.clone().neg().add_scalar(1.0)) * n + z * h;
                }
            }
            outs.push(h.clone());
        }
        if reverse {
            outs.reverse(); // 시간 순서로 되돌린다
        }
        per_direction.push(outs);
    }

    if return_sequence {
        // 시각마다 방향을 이어 붙인 뒤 [B, L, H'] 로 쌓는다.
        let steps: Vec<Tensor<B, 3>> = (0..len)
            .map(|t| {
                let parts: Vec<Tensor<B, 2>> = per_direction.iter().map(|d| d[t].clone()).collect();
                Tensor::cat(parts, 1).unsqueeze_dim(1)
            })
            .collect();
        DynTensor::R3(Tensor::cat(steps, 1))
    } else {
        // 각 방향의 마지막 상태: 정방향은 t=L-1, 역방향은 t=0 에서 계산된 것.
        let parts: Vec<Tensor<B, 2>> = per_direction
            .iter()
            .enumerate()
            .map(|(dir, d)| if dir == 1 { d[0].clone() } else { d[len - 1].clone() })
            .collect();
        DynTensor::R2(Tensor::cat(parts, 1))
    }
}

/// 멀티헤드 셀프 어텐션. `proj` 는 q·k·v·out 투영 (가중치, 편향).
fn self_attention<B: Backend>(
    x: Tensor<B, 3>,
    heads: usize,
    dropout: f32,
    train: bool,
    proj: &[(Tensor<B, 2>, Tensor<B, 1>)],
) -> Result<Tensor<B, 3>> {
    let [batch, len, d] = x.dims();
    if heads == 0 || d % heads != 0 {
        bail!("특징 차원 {d} 가 헤드 수 {heads} 로 나누어떨어지지 않습니다");
    }
    let head_dim = d / heads;
    let split = |t: Tensor<B, 3>| t.reshape([batch, len, heads, head_dim]).swap_dims(1, 2);

    let q = split(module::linear(x.clone(), proj[0].0.clone(), Some(proj[0].1.clone())));
    let k = split(module::linear(x.clone(), proj[1].0.clone(), Some(proj[1].1.clone())));
    let v = split(module::linear(x, proj[2].0.clone(), Some(proj[2].1.clone())));

    // [B, heads, L, L]
    let scores = q.matmul(k.swap_dims(2, 3)).div_scalar((head_dim as f64).sqrt());
    let mut attn = activation::softmax(scores, 3);
    if train && dropout > 0.0 {
        if dropout >= 1.0 {
            bail!("어텐션 dropout 은 1 보다 작아야 합니다 (지금 {dropout})");
        }
        let keep = 1.0 - dropout as f64;
        let mask = attn.random_like(Distribution::Bernoulli(keep));
        attn = attn * mask * (1.0 / keep);
    }

    let ctx = attn.matmul(v).swap_dims(1, 2).reshape([batch, len, d]);
    Ok(module::linear(ctx, proj[3].0.clone(), Some(proj[3].1.clone())))
}

// ───────────────────────────── 레이어 연산 ─────────────────────────────

fn activate<B: Backend>(t: DynTensor<B>, act: Act) -> DynTensor<B> {
    match act {
        Act::Relu => map_t!(t, |x| activation::relu(x)),
        Act::LeakyRelu { slope } => map_t!(t, |x| activation::leaky_relu(x, slope as f64)),
        Act::Gelu => map_t!(t, |x| activation::gelu(x)),
        Act::Silu => map_t!(t, |x| activation::silu(x)),
        Act::Sigmoid => map_t!(t, |x| activation::sigmoid(x)),
        Act::Tanh => map_t!(t, |x| activation::tanh(x)),
        Act::Softmax => map_t!(t, |x| {
            let d = x.dims().len() - 1;
            activation::softmax(x, d)
        }),
        Act::LogSoftmax => map_t!(t, |x| {
            let d = x.dims().len() - 1;
            activation::log_softmax(x, d)
        }),
    }
}

fn concat<B: Backend>(a: DynTensor<B>, b: DynTensor<B>, dim: usize) -> Result<DynTensor<B>> {
    if a.rank() != b.rank() {
        bail!("Concat 랭크 불일치: {} vs {}", a.rank(), b.rank());
    }
    if dim >= a.rank() {
        bail!("Concat dim {} 이 랭크 {} 밖", dim, a.rank());
    }
    Ok(match (a, b) {
        (DynTensor::R1(x), DynTensor::R1(y)) => DynTensor::R1(Tensor::cat(vec![x, y], dim)),
        (DynTensor::R2(x), DynTensor::R2(y)) => DynTensor::R2(Tensor::cat(vec![x, y], dim)),
        (DynTensor::R3(x), DynTensor::R3(y)) => DynTensor::R3(Tensor::cat(vec![x, y], dim)),
        (DynTensor::R4(x), DynTensor::R4(y)) => DynTensor::R4(Tensor::cat(vec![x, y], dim)),
        (DynTensor::R5(x), DynTensor::R5(y)) => DynTensor::R5(Tensor::cat(vec![x, y], dim)),
        _ => unreachable!("랭크는 위에서 확인했다"),
    })
}

/// 채널(첫 샘플 차원 = 텐서 dim 1) 기준 배치 정규화.
/// 학습 모드면 갱신된 `(running_mean, running_var)` 를 함께 돌려준다.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn batch_norm<B: Backend>(
    x: DynTensor<B>,
    gamma: DynTensor<B>,
    beta: DynTensor<B>,
    running_mean: DynTensor<B>,
    running_var: DynTensor<B>,
    eps: f32,
    momentum: f32,
    train: bool,
) -> Result<(DynTensor<B>, Option<(DynTensor<B>, DynTensor<B>)>)> {
    let dims = x.dims();
    let rank = dims.len();
    if rank < 2 {
        bail!("BatchNorm 은 [B, C, …] 입력이 필요합니다 (지금 {dims:?})");
    }
    let c = dims[1];
    let mut pshape = vec![1usize; rank];
    pshape[1] = c;
    let reduce: Vec<usize> = (0..rank).filter(|d| *d != 1).collect();

    let (normed, updated) = if train {
        let mean = map_t!(x.clone(), |t| t.mean_dims(&reduce));
        let centered = x.try_sub(mean.clone())?;
        let var = map_t!(centered.clone().square(), |t| t.mean_dims(&reduce));
        // running 통계는 학습 대상이 아니므로 그래프에서 떼어 낸다.
        let mom = momentum as f64;
        let m_flat = mean.detach().reshape(&[c])?;
        // running_var 만 불편 추정치(n/(n-1))로 갱신한다 — PyTorch 와 같은 규약이다.
        // 정규화 자체는 편향 분산을 쓴다(위 `var`). n = 채널당 표본 수.
        let n: usize = reduce.iter().map(|&d| dims[d]).product();
        let unbiased = if n > 1 { n as f64 / (n - 1) as f64 } else { 1.0 };
        let v_flat = var.clone().detach().reshape(&[c])?.mul_scalar(unbiased);
        // running 통계는 학습 대상이 아니다. require_grad 가 걸린 적이 없어 현재 백엔드에서는
        // 노드가 생기지 않지만, 백엔드 동작에 기대지 않도록 명시적으로 떼어 둔다.
        let new_mean = running_mean
            .detach()
            .mul_scalar(1.0 - mom)
            .try_add(m_flat.mul_scalar(mom))?
            .detach();
        let new_var = running_var
            .detach()
            .mul_scalar(1.0 - mom)
            .try_add(v_flat.mul_scalar(mom))?
            .detach();
        (
            centered.try_div(var.add_scalar(eps as f64).sqrt())?,
            Some((new_mean, new_var)),
        )
    } else {
        let mean = running_mean.reshape(&pshape)?;
        let var = running_var.reshape(&pshape)?;
        (x.try_sub(mean)?.try_div(var.add_scalar(eps as f64).sqrt())?, None)
    };

    let y = normed
        .try_mul(gamma.reshape(&pshape)?)?
        .try_add(beta.reshape(&pshape)?)?;
    Ok((y, updated))
}

/// 마지막 차원 기준 레이어 정규화.
fn layer_norm<B: Backend>(x: DynTensor<B>, gamma: DynTensor<B>, beta: DynTensor<B>, eps: f32) -> Result<DynTensor<B>> {
    let dims = x.dims();
    let rank = dims.len();
    let last = rank - 1;
    let mut pshape = vec![1usize; rank];
    pshape[last] = dims[last];

    let mean = map_t!(x.clone(), |t| t.mean_dim(last));
    let centered = x.try_sub(mean)?;
    let var = map_t!(centered.clone().square(), |t| t.mean_dim(last));
    let normed = centered.try_div(var.add_scalar(eps as f64).sqrt())?;
    normed.try_mul(gamma.reshape(&pshape)?)?.try_add(beta.reshape(&pshape)?)
}

// ───────────────────────────── 작은 도우미 ─────────────────────────────

/// 그래프 오류를 사람이 읽을 한 덩어리로 만든다.
///
/// 출력에서 **거꾸로 도달할 수 있는** 노드를 먼저 보여 준다. 캔버스에 떠 있는 연결되지 않은 노드는
/// 실행을 막는 진짜 원인이 아닌데, id 순으로 아무거나 고르면 그런 노드가 지목되곤 했다.
fn describe_graph_errors(graph: &Graph, report: &ShapeReport) -> String {
    let contributing = contributing_nodes(graph);
    let name_of = |id: &NodeId| {
        graph
            .nodes
            .get(id)
            .map(|n| n.display_name())
            .unwrap_or_else(|| "?".into())
    };

    let mut rows: Vec<(bool, NodeId, String)> = report
        .errors
        .iter()
        .map(|(id, e)| {
            (
                !contributing.contains(id),
                *id,
                format!("레이어 '{}': {e}", name_of(id)),
            )
        })
        .collect();
    // 출력에 기여하는 노드가 앞, 그다음 id 순 (결정적).
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    const SHOW: usize = 3;
    let shown: Vec<&str> = rows.iter().take(SHOW).map(|r| r.2.as_str()).collect();
    let mut msg = format!("그래프에 오류가 있어 실행할 수 없습니다 — {}", shown.join("; "));
    if rows.len() > shown.len() {
        msg.push_str(&format!(" (그 밖에 {} 개 더)", rows.len() - shown.len()));
    }
    let stray = rows.iter().filter(|r| r.0).count();
    if stray > 0 && stray < rows.len() {
        msg.push_str(&format!(" — 이 중 {stray} 개는 출력에 이어지지 않은 노드입니다"));
    }
    msg
}

/// Output 에서 엣지를 거꾸로 따라가 도달하는 노드 집합 (= 결과에 기여하는 노드).
fn contributing_nodes(graph: &Graph) -> BTreeSet<NodeId> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<NodeId> = graph.output_nodes();
    while let Some(n) = stack.pop() {
        if !seen.insert(n) {
            continue;
        }
        for (_, from) in graph.inputs_of(n) {
            stack.push(from);
        }
    }
    seen
}

/// Embedding 인덱스가 `0..vocab` 안의 정수인지 확인한다.
///
/// 실수 입력은 `Tensor::int()` 가 **0 방향으로 절단**하므로 0.9 가 조용히 0 이 된다.
/// 정규화를 거친 값을 Embedding 에 물리는 흔한 실수를 여기서 잡는다.
fn check_embedding_indices(host: &HostTensor, vocab: usize) -> Result<()> {
    for (i, &v) in host.data.iter().enumerate() {
        if !v.is_finite() || v.fract() != 0.0 {
            bail!(
                "Embedding 입력 {i} 번째 값 {v} 가 정수가 아닙니다 — 인덱스는 0 이상 {vocab} 미만의                  정수여야 합니다 (소수부는 버려지므로 조용히 틀린 결과가 됩니다)"
            );
        }
        if v < 0.0 || v as usize >= vocab {
            bail!("Embedding 입력 {i} 번째 값 {v} 가 vocab {vocab} 범위 밖입니다 (0 이상 {vocab} 미만)");
        }
    }
    Ok(())
}

fn random_dyn<B: Backend>(shape: &[usize], d: Distribution, device: &B::Device) -> Result<DynTensor<B>> {
    let sh = Shape::from(shape.to_vec());
    Ok(match shape.len() {
        1 => DynTensor::R1(Tensor::random(sh, d, device)),
        2 => DynTensor::R2(Tensor::random(sh, d, device)),
        3 => DynTensor::R3(Tensor::random(sh, d, device)),
        4 => DynTensor::R4(Tensor::random(sh, d, device)),
        5 => DynTensor::R5(Tensor::random(sh, d, device)),
        n => bail!("지원하지 않는 랭크 {n} (최대 {MAX_RANK})"),
    })
}

fn ones1<B: Backend>(n: usize, device: &B::Device) -> DynTensor<B> {
    DynTensor::R1(Tensor::ones(Shape::from(vec![n]), device))
}

fn zeros1<B: Backend>(n: usize, device: &B::Device) -> DynTensor<B> {
    DynTensor::R1(Tensor::zeros(Shape::from(vec![n]), device))
}

/// `HostTensor` → 장치 텐서 (배치 차원 포함).
pub fn to_device<B: Backend>(h: &HostTensor, device: &B::Device) -> Result<DynTensor<B>> {
    DynTensor::from_host(h, device)
}
