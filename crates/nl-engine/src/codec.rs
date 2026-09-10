//! 페이로드 Transform 실행: 바깥 값 ↔ 텐서.

use crate::tensor::HostTensor;
use nl_core::payload::Field;

/// 엔진 경계의 바깥 값.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Numbers(Vec<f32>),
    Text(String),
    Json(serde_json::Value),
    /// RGBA8, 행 우선.
    Image { width: u32, height: u32, rgba: Vec<u8> },
    Tensor(HostTensor),
}

/// `field.encode` 체인을 적용해 배치 1 텐서를 만든다.
pub fn encode(_field: &Field, _value: &Value) -> anyhow::Result<HostTensor> {
    anyhow::bail!("codec 미구현")
}

/// `field.decode` 체인을 적용해 바깥 값으로 (배치 1 가정, 배치 >1 이면 첫 샘플).
pub fn decode(_field: &Field, _tensor: &HostTensor) -> anyhow::Result<Value> {
    anyhow::bail!("codec 미구현")
}
