//! 추론 세션. 가중치를 장치에 올려 두고 반복 호출한다.
//!
//! 학습과 달리 autodiff 를 두르지 않은 백엔드로 돌고, Dropout·BatchNorm 은 평가 모드다.

use crate::device::{self, dispatch};
use crate::exec::Model;
use crate::tensor::HostTensor;
use crate::weights;
use anyhow::{Context, Result};
use burn::tensor::backend::Backend;
use nl_core::{DevicePref, ModelDef};
use std::path::Path;

/// 백엔드를 지운 실행 창구.
trait Runner: Send {
    fn run(&mut self, inputs: &[HostTensor]) -> Result<Vec<HostTensor>>;
}

struct Typed<B: Backend> {
    model: Model<B>,
}

impl<B: Backend> Runner for Typed<B> {
    fn run(&mut self, inputs: &[HostTensor]) -> Result<Vec<HostTensor>> {
        let device = self.model.device().clone();
        let mut tensors = Vec::with_capacity(inputs.len());
        for h in inputs {
            tensors.push(crate::exec::to_device::<B>(h, &device)?);
        }
        let out = self.model.forward(tensors, false)?;
        Ok(out.iter().map(|t| t.to_host()).collect())
    }
}

pub struct Session {
    inner: Box<dyn Runner>,
    device_name: String,
    input_shapes: Vec<Vec<usize>>,
    output_shapes: Vec<Vec<usize>>,
}

impl Session {
    /// `weights` 가 `None` 이면 무작위 초기화(구조 확인용).
    pub fn load(model: &ModelDef, weights: Option<&Path>, device: DevicePref) -> anyhow::Result<Self> {
        let (info, handle) = device::resolve_entry(device);
        let (inner, input_shapes, output_shapes) = dispatch!(handle, build, model, weights)?;
        Ok(Self {
            inner,
            device_name: info.name,
            input_shapes,
            output_shapes,
        })
    }

    /// 입력은 `Graph::input_nodes()` 순서, 출력은 `Graph::output_nodes()` 순서. 배치 차원 포함.
    pub fn run(&mut self, inputs: &[HostTensor]) -> anyhow::Result<Vec<HostTensor>> {
        self.inner.run(inputs)
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// 입력 노드별 샘플 형상 (배치 제외), `Graph::input_nodes()` 순서.
    pub fn input_sample_shapes(&self) -> &[Vec<usize>] {
        &self.input_shapes
    }

    /// 출력 노드별 샘플 형상 (배치 제외), `Graph::output_nodes()` 순서.
    pub fn output_sample_shapes(&self) -> &[Vec<usize>] {
        &self.output_shapes
    }
}

#[allow(clippy::type_complexity)]
fn build<B: Backend>(
    device: &B::Device,
    def: &ModelDef,
    w: Option<&Path>,
) -> Result<(Box<dyn Runner>, Vec<Vec<usize>>, Vec<Vec<usize>>)> {
    let mut model = Model::<B>::new(def, device, def.train.seed)?;
    if let Some(path) = w {
        let loaded = weights::load_for(path, Some(def.id))
            .with_context(|| format!("가중치 읽기 실패: {}", crate::paths::short(path)))?;
        model
            .load_host_params(&loaded)
            .with_context(|| format!("가중치 적용 실패: {}", crate::paths::short(path)))?;
    }
    let ins = model.input_sample_shapes();
    let outs = model.output_sample_shapes();
    Ok((Box::new(Typed { model }), ins, outs))
}
