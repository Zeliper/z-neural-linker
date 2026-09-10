//! 엔진 경계에서 오가는 CPU 측 텐서. burn 텐서는 크레이트 밖으로 나가지 않는다.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct HostTensor {
    /// 배치 차원 포함 전체 형상 (예: `[1, 3, 28, 28]`).
    pub shape: Vec<usize>,
    /// 행 우선(row-major) f32. 정수 라벨도 f32 로 담는다.
    pub data: Vec<f32>,
}

impl HostTensor {
    pub fn new(shape: Vec<usize>, data: Vec<f32>) -> Self {
        debug_assert_eq!(shape.iter().product::<usize>(), data.len(), "형상과 데이터 길이 불일치");
        Self { shape, data }
    }
    pub fn zeros(shape: Vec<usize>) -> Self {
        let n = shape.iter().product();
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
