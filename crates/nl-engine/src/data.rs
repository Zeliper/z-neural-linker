//! 데이터셋 로딩(CSV · 이미지 폴더 · 합성 · 녹화). 학습 루프와 미리보기가 공유한다.

use crate::tensor::HostTensor;
use nl_core::dataset::DatasetInfo;
use nl_core::DatasetSpec;
use std::path::Path;

/// 샘플 하나 (배치 차원 없음: `input.shape` = 샘플 형상).
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub input: HostTensor,
    pub target: HostTensor,
}

/// 소스를 훑어 샘플 수·형상·클래스를 알아낸다 (데이터 뷰 "스캔", 학습 전 점검).
pub fn scan(_spec: &DatasetSpec, _base_dir: &Path) -> anyhow::Result<DatasetInfo> {
    anyhow::bail!("데이터 로더 미구현")
}

/// 앞에서 `n` 개 샘플 (데이터 뷰 미리보기).
pub fn preview(_spec: &DatasetSpec, _base_dir: &Path, _n: usize) -> anyhow::Result<Vec<Sample>> {
    anyhow::bail!("데이터 로더 미구현")
}
