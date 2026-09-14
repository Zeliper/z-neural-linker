//! 엔진 경계에서 오가는 CPU 측 텐서. burn 텐서는 크레이트 밖으로 나가지 않는다.

use crate::limits::checked_elems;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct HostTensor {
    /// 배치 차원 포함 전체 형상 (예: `[1, 3, 28, 28]`).
    pub shape: Vec<usize>,
    /// 행 우선(row-major) f32. 정수 라벨도 f32 로 담는다.
    pub data: Vec<f32>,
}

impl HostTensor {
    /// 형상과 데이터 길이가 맞아야 한다. **릴리스에서도 검사한다** — 어긋난 채로 통과하면
    /// 나중에 인덱싱에서 터지고, 그 지점은 원인과 한참 떨어져 있다.
    /// 바깥 입력으로 만들 때는 [`HostTensor::try_new`] 를 써서 오류로 받는다.
    pub fn new(shape: Vec<usize>, data: Vec<f32>) -> Self {
        match Self::try_new(shape, data) {
            Ok(t) => t,
            Err(e) => panic!("{e}"),
        }
    }

    /// 검사에 실패하면 `Err`. 신뢰할 수 없는 입력에서 텐서를 만들 때 쓴다.
    pub fn try_new(shape: Vec<usize>, data: Vec<f32>) -> anyhow::Result<Self> {
        let want = checked_elems(&shape, "텐서")?;
        if want != data.len() {
            anyhow::bail!("형상 {shape:?} 은 원소 {want} 개인데 데이터는 {} 개입니다", data.len());
        }
        Ok(Self { shape, data })
    }
    pub fn zeros(shape: Vec<usize>) -> Self {
        let n = checked_elems(&shape, "텐서").expect("zeros 형상");
        Self { shape, data: vec![0.0; n] }
    }
    pub fn scalar(v: f32) -> Self {
        Self { shape: vec![1], data: vec![v] }
    }
    pub fn numel(&self) -> usize {
        self.data.len()
    }
    pub fn batch(&self) -> usize {
        self.shape.first().copied().unwrap_or(0)
    }
    /// 마지막 차원 argmax (배치별).
    pub fn argmax_last(&self) -> Vec<usize> {
        let last = *self.shape.last().unwrap_or(&1);
        if last == 0 {
            return vec![];
        }
        self.data
            .chunks(last)
            .map(|row| row.iter().enumerate().fold((0, f32::NEG_INFINITY), |m, (i, &v)| if v > m.1 { (i, v) } else { m }).0)
            .collect()
    }
}
