//! ONNX 가져오기 — **선택 기능**. 바깥에서 받은 `.onnx` 를 추론 전용으로 돌린다.
//!
//! ```text
//! cargo build -p nl-engine --features onnx-import
//! ```
//!
//! ## 왜 기본이 꺼져 있나
//!
//! 실행은 `tract-onnx` 가 한다. 순수 Rust 라 시스템 의존성은 없지만 **배포 바이너리가 34 MiB
//! 늘어난다**(현재 `nl-runtime` 74 MiB 대비 +46%). 기능 플래그로도 줄일 수 없다 —
//! `tract-onnx` 0.23.7 에는 `optional` 의존성이 하나도 없다. 실측과 판단 근거는
//! `docs/research/onnx-2026-09-14.md` 에 있다.
//!
//! 그래서 **쓰는 사람만 켠다.** 기본 배포판에는 들어가지 않는다.
//!
//! ## 이것이 하지 않는 것
//!
//! - **학습.** 가져온 모델은 가중치를 고칠 수 없다. `ModelDef` 로 되돌리는 변환은 없고,
//!   캔버스에도 그려지지 않는다. "추론 전용" 이 그 뜻이다.
//! - **파이프라인 노드.** `PNodeKind::OnnxModel` 같은 변형은 아직 `nl-core` 에 없다 —
//!   `LayerKind`/`PNodeKind` 를 전수 match 하는 앱 코드가 함께 고쳐져야 하기 때문이다.
//!   지금은 이 모듈을 직접 부르는 것만 된다.
//!
//! ## 쓰는 법
//!
//! ```no_run
//! use nl_engine::onnx_import::OnnxSession;
//! use nl_engine::HostTensor;
//!
//! let mut s = OnnxSession::load(std::path::Path::new("model.onnx"))?;
//! let y = s.run(&[HostTensor::new(vec![1, 4], vec![0.0; 4])])?;
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! 입력·출력 순서는 **ONNX 그래프에 적힌 순서**다. 우리 `Session` 이 `Graph::input_nodes()`
//! 순서를 쓰는 것과 같은 자리이므로, 우리가 내보낸 파일을 다시 읽으면 순서가 그대로 맞는다.
//!
//! ## 계획은 첫 `run` 때 만든다
//!
//! `load` 는 파일을 읽기만 하고, 실행 계획은 **입력 형상을 아는 순간**에 만든다. 동적 배치로
//! 내보낸 모델은 배치가 기호(`dim_param`)로 남아 있어 그대로는 최적화할 수 없기 때문이다.
//!
//! 이건 이론이 아니라 실측이다 — **tract 는 기호 배치가 남은 LSTM 을 최적화하려다 패닉한다**
//! (`UndeterminedSymbol("B")`). 그래서 계획 생성을 `catch_unwind` 로 감싼다. 바깥에서 받은
//! 파일 하나 때문에 앱이 죽으면 안 된다. 같은 형상이 다시 오면 만들어 둔 계획을 그대로 쓴다.

use crate::limits::{check_file_size, checked_elems, MAX_TENSOR_ELEMS, MAX_WEIGHTS_BYTES};
use crate::paths;
use crate::tensor::HostTensor;
use anyhow::{bail, Result};
use std::path::Path;
use tract_onnx::prelude::*;

/// 입력 형상 하나에 맞춰 최적화된 tract 실행 계획.
type Plan = std::sync::Arc<tract_onnx::prelude::TypedRunnableModel>;

/// 가져온 ONNX 모델 하나. 우리 [`crate::Session`] 과 같은 자리에 놓이지만 **추론만** 한다.
///
/// **계획은 첫 [`run`](Self::run) 때 만든다.** 동적 배치로 내보낸 모델은 배치가 기호로 남아 있어
/// 그대로는 최적화할 수 없다 — 실제로 tract 의 LSTM 최적화는 기호 차원을 만나면
/// `UndeterminedSymbol` 로 **패닉**한다. 그래서 입력 형상을 아는 순간에 계획을 만들고,
/// 같은 형상이 다시 오면 그대로 쓴다.
pub struct OnnxSession {
    /// 파싱만 한 모델. 형상을 박을 때마다 복제해 특수화한다.
    model: InferenceModel,
    /// 파일이 선언한 입력 형상. 기호 차원(동적 배치)은 `None` 이다.
    inputs: Vec<Vec<Option<usize>>>,
    name: String,
    /// 마지막으로 쓴 입력 형상과 그에 맞춰 만든 계획.
    cached: Option<(Vec<Vec<usize>>, Plan)>,
}

impl OnnxSession {
    /// `.onnx` 파일을 읽는다. 실행 계획은 첫 [`run`](Self::run) 때 만들어진다.
    pub fn load(path: &Path) -> Result<Self> {
        // protobuf 는 중첩 깊이·크기 공격이 가능한 포맷이라 가중치와 같은 상한을 건다.
        check_file_size(path, MAX_WEIGHTS_BYTES, "ONNX 모델")?;
        let shown = paths::short(path);

        // 선언된 형상은 protobuf 에서 바로 읽는다 — 기호 차원(`dim_param`)을 그대로 구별할 수 있다.
        let proto = tract_onnx::onnx()
            .proto_model_for_path(path)
            .map_err(|e| anyhow::anyhow!("ONNX 를 읽지 못했습니다 ({shown}): {e}"))?;
        let inputs = declared_input_shapes(&proto, &shown)?;
        if inputs.is_empty() {
            bail!("{shown} 에 입력이 없습니다");
        }

        let model = tract_onnx::onnx()
            .model_for_path(path)
            .map_err(|e| anyhow::anyhow!("ONNX 그래프를 해석하지 못했습니다 ({shown}): {e}"))?;

        Ok(Self {
            model,
            inputs,
            name: shown,
            cached: None,
        })
    }

    /// 입력을 ONNX 그래프의 입력 순서대로 넣고 출력 순서대로 받는다.
    ///
    /// 처음 보는 입력 형상이면 그 형상에 맞춰 계획을 새로 만든다(그리고 기억해 둔다).
    pub fn run(&mut self, inputs: &[HostTensor]) -> Result<Vec<HostTensor>> {
        if inputs.len() != self.inputs.len() {
            bail!(
                "{} 의 입력은 {} 개인데 {} 개를 받았습니다",
                self.name,
                self.inputs.len(),
                inputs.len()
            );
        }
        let mut tv: TVec<TValue> = tvec!();
        let mut shapes = Vec::with_capacity(inputs.len());
        for (i, (t, want)) in inputs.iter().zip(&self.inputs).enumerate() {
            check_against(i, t, want, &self.name)?;
            checked_elems(&t.shape, "ONNX 입력")?;
            let tensor = Tensor::from_shape(&t.shape, &t.data)
                .map_err(|e| anyhow::anyhow!("{} 의 입력 {i} 를 텐서로 만들지 못했습니다: {e}", self.name))?;
            tv.push(tensor.into());
            shapes.push(t.shape.clone());
        }

        if self.cached.as_ref().is_none_or(|(s, _)| s != &shapes) {
            let plan = self.build_plan(&shapes)?;
            self.cached = Some((shapes, plan));
        }
        let (_, plan) = self.cached.as_ref().expect("방금 넣었다");

        let out = plan
            .run(tv)
            .map_err(|e| anyhow::anyhow!("{} 추론 실패: {e}", self.name))?;

        let mut host = Vec::with_capacity(out.len());
        for (i, o) in out.iter().enumerate() {
            let shape = o.shape().to_vec();
            let elems = checked_elems(&shape, "ONNX 출력")?;
            if elems > MAX_TENSOR_ELEMS {
                bail!("{} 의 출력 {i} 가 원소 {elems} 개로 너무 큽니다", self.name);
            }
            let view = o
                .to_plain_array_view::<f32>()
                .map_err(|e| anyhow::anyhow!("{} 의 출력 {i} 가 f32 가 아닙니다: {e}", self.name))?;
            host.push(HostTensor::try_new(shape, view.iter().copied().collect())?);
        }
        Ok(host)
    }

    /// 구체 입력 형상을 박고 최적화까지 한 계획을 만든다.
    ///
    /// **`catch_unwind` 로 감싼다.** 바깥에서 받은 ONNX 가 tract 안에서 패닉할 수 있고
    /// (기호 차원의 LSTM 이 그렇다), 남의 파일 하나 때문에 앱이 죽으면 안 된다.
    fn build_plan(&self, shapes: &[Vec<usize>]) -> Result<Plan> {
        let name = &self.name;
        let model = self.model.clone();
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || -> TractResult<Plan> {
            let mut m = model;
            for (i, s) in shapes.iter().enumerate() {
                m = m.with_input_fact(i, f32::fact(s.as_slice()).into())?;
            }
            m.into_optimized()?.into_runnable()
        }));
        match built {
            Ok(Ok(plan)) => Ok(plan),
            Ok(Err(e)) => {
                bail!("{name} 를 이 입력 형상으로 준비하지 못했습니다: {e} — 지원하지 않는 연산자일 수 있습니다")
            }
            Err(e) => bail!("{name} 를 준비하다 tract 가 패닉했습니다: {}", panic_text(e.as_ref())),
        }
    }

    /// 파일이 선언한 입력 형상. 기호 차원(동적 배치)은 `None` 이다.
    pub fn input_shapes(&self) -> &[Vec<Option<usize>>] {
        &self.inputs
    }

    /// 오류 메시지에 쓰는 짧은 이름 (파일 이름이나 `~/…`).
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 오류".to_string()
    }
}

/// 파일이 선언한 입력 형상. 고정 차원은 `Some(n)`, 기호(`dim_param`)는 `None`.
///
/// 옛 ONNX(IR 3 이하)는 이니셜라이저까지 `graph.input` 에 넣기도 한다 — 그건 진짜 입력이 아니라
/// 상수이므로 뺀다.
fn declared_input_shapes(proto: &tract_onnx::pb::ModelProto, shown: &str) -> Result<Vec<Vec<Option<usize>>>> {
    let graph = proto
        .graph
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{shown} 에 그래프가 없습니다"))?;
    let constants: std::collections::BTreeSet<&str> = graph.initializer.iter().map(|t| t.name.as_str()).collect();

    let mut out = Vec::new();
    for vi in &graph.input {
        if constants.contains(vi.name.as_str()) {
            continue;
        }
        let Some(tract_onnx::pb::type_proto::Value::TensorType(t)) = vi.r#type.as_ref().and_then(|t| t.value.as_ref())
        else {
            bail!("{shown} 의 입력 '{}' 이 텐서가 아닙니다", vi.name);
        };
        let dims = t.shape.as_ref().map_or_else(Vec::new, |sh| {
            sh.dim
                .iter()
                .map(|d| match &d.value {
                    Some(tract_onnx::pb::tensor_shape_proto::dimension::Value::DimValue(v)) => usize::try_from(*v).ok(),
                    // `dim_param` 이거나 비어 있으면 기호 — 무엇이든 받는다.
                    _ => None,
                })
                .collect()
        });
        out.push(dims);
    }
    Ok(out)
}

/// 준 입력이 그래프가 선언한 형상과 맞는지. 기호 차원은 무엇이든 받는다.
fn check_against(i: usize, got: &HostTensor, want: &[Option<usize>], name: &str) -> Result<()> {
    if want.is_empty() {
        return Ok(()); // 형상을 선언하지 않은 그래프 — 실행할 때 tract 가 판단한다.
    }
    if got.shape.len() != want.len() {
        bail!(
            "{name} 의 입력 {i} 는 랭크 {} 를 기대하는데 {} 를 받았습니다 ({:?})",
            want.len(),
            got.shape.len(),
            got.shape
        );
    }
    for (axis, (&g, w)) in got.shape.iter().zip(want).enumerate() {
        if let Some(expect) = w {
            if g != *expect {
                bail!(
                    "{name} 의 입력 {i} 의 {axis} 번 축이 {expect} 여야 하는데 {g} 입니다 ({:?})",
                    got.shape
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_check_allows_symbolic_dims_and_rejects_fixed_mismatches() {
        let want = vec![None, Some(4)]; // [B, 4]
        let ok = HostTensor::new(vec![7, 4], vec![0.0; 28]);
        assert!(check_against(0, &ok, &want, "t").is_ok(), "동적 배치를 막았다");

        let bad_dim = HostTensor::new(vec![7, 5], vec![0.0; 35]);
        let e = check_against(0, &bad_dim, &want, "t").unwrap_err().to_string();
        assert!(e.contains("1 번 축"), "{e}");

        let bad_rank = HostTensor::new(vec![7], vec![0.0; 7]);
        let e = check_against(0, &bad_rank, &want, "t").unwrap_err().to_string();
        assert!(e.contains("랭크"), "{e}");

        // 형상을 선언하지 않은 그래프는 통과시킨다.
        assert!(check_against(0, &bad_rank, &[], "t").is_ok());
    }

    #[test]
    fn loading_a_non_onnx_file_fails_without_leaking_the_path() {
        let dir = std::env::temp_dir().join(format!("nl-onnx-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("가짜.onnx");
        std::fs::write(&p, "이건 protobuf 가 아니다").unwrap();
        // `OnnxSession` 은 Debug 가 없어 `unwrap_err()` 를 못 쓴다 — 명시적으로 가른다.
        let e = match OnnxSession::load(&p) {
            Ok(_) => panic!("protobuf 가 아닌 파일을 읽어 버렸다"),
            Err(e) => format!("{e:#}"),
        };
        assert!(e.contains("가짜.onnx"), "파일 이름은 알려 줘야 한다: {e}");
        assert!(!e.contains(&dir.display().to_string()), "절대 경로가 새어 나왔다: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
