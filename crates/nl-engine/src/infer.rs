//! 추론 세션. 가중치를 장치에 올려 두고 반복 호출한다.

use crate::tensor::HostTensor;
use nl_core::{DevicePref, ModelDef};
use std::path::Path;

pub struct Session {
    _private: (),
}

impl Session {
    /// `weights` 가 `None` 이면 무작위 초기화(구조 확인용).
    pub fn load(_model: &ModelDef, _weights: Option<&Path>, _device: DevicePref) -> anyhow::Result<Self> {
        anyhow::bail!("추론 엔진 미구현")
    }
    /// 입력은 `Graph::input_nodes()` 순서, 출력은 `Graph::output_nodes()` 순서. 배치 차원 포함.
    pub fn run(&mut self, _inputs: &[HostTensor]) -> anyhow::Result<Vec<HostTensor>> {
        anyhow::bail!("추론 엔진 미구현")
    }
    pub fn device_name(&self) -> &str {
        ""
    }
}
