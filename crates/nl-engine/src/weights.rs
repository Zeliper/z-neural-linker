//! 체크포인트: safetensors 로 파라미터를 저장·로드한다.
//!
//! 저장 형식은 `"{node_id}.{part}"` 이름의 f32 텐서 모음이고, 헤더 메타데이터에 모델 id 와 포맷 표시를 남긴다.
//! burn 의 `Record` 파생을 쓰지 않는 이유는 그래프가 런타임에 정의되어 정적 모듈 타입이 없기 때문이다.

use crate::tensor::HostTensor;
use anyhow::{bail, Context, Result};
use nl_core::ModelId;
use safetensors::tensor::{Dtype, TensorView};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// 메타데이터 `format` 값. 로드할 때 다르면 경고만 남기고 계속 읽는다 (바깥에서 만든 safetensors 도 받는다).
pub const WEIGHTS_FORMAT: &str = "neural-linker/v1";

/// 파라미터를 safetensors 파일로 저장한다 (임시 파일 + rename 으로 원자적).
pub fn save(path: &Path, model: ModelId, params: &BTreeMap<String, HostTensor>) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("폴더 생성 실패: {}", dir.display()))?;
    }

    // TensorView 는 바이트 슬라이스를 빌려가므로 먼저 모든 바이트를 만들어 둔다.
    let bytes: Vec<(String, Vec<usize>, Vec<u8>)> = params
        .iter()
        .map(|(name, t)| {
            let mut b = Vec::with_capacity(t.data.len() * 4);
            for v in &t.data {
                b.extend_from_slice(&v.to_le_bytes());
            }
            (name.clone(), t.shape.clone(), b)
        })
        .collect();

    let mut views = Vec::with_capacity(bytes.len());
    for (name, shape, b) in &bytes {
        let v = TensorView::new(Dtype::F32, shape.clone(), b)
            .map_err(|e| anyhow::anyhow!("텐서 '{name}' 뷰 생성 실패: {e}"))?;
        views.push((name.as_str(), v));
    }

    let mut meta = HashMap::new();
    meta.insert("format".to_string(), WEIGHTS_FORMAT.to_string());
    meta.insert("model".to_string(), model.to_string());

    let buf = safetensors::serialize(views, Some(meta)).map_err(|e| anyhow::anyhow!("safetensors 직렬화 실패: {e}"))?;

    let tmp = path.with_extension("safetensors.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("체크포인트 쓰기 실패: {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("체크포인트 이동 실패: {}", path.display()))?;
    Ok(())
}

/// safetensors 파일에서 파라미터를 읽는다. f32 가 아니면 오류.
pub fn load(path: &Path) -> Result<BTreeMap<String, HostTensor>> {
    let buf = std::fs::read(path).with_context(|| format!("체크포인트 읽기 실패: {}", path.display()))?;
    let st = safetensors::SafeTensors::deserialize(&buf)
        .map_err(|e| anyhow::anyhow!("safetensors 파싱 실패 ({}): {e}", path.display()))?;
    if let Ok((_, meta)) = safetensors::SafeTensors::read_metadata(&buf) {
        if let Some(fmt) = meta.metadata().as_ref().and_then(|m| m.get("format")) {
            if fmt != WEIGHTS_FORMAT {
                log::warn!("{}: format 이 '{fmt}' 입니다 (기대값 '{WEIGHTS_FORMAT}') — 그대로 읽습니다", path.display());
            }
        }
    }
    let mut out = BTreeMap::new();
    for (name, view) in st.iter() {
        if view.dtype() != Dtype::F32 {
            bail!("파라미터 '{name}' 의 dtype 이 {:?} 입니다 — f32 만 지원합니다", view.dtype());
        }
        let raw = view.data();
        let (words, rest) = raw.as_chunks::<4>();
        if !rest.is_empty() {
            bail!("파라미터 '{name}' 의 바이트 길이 {}가 4의 배수가 아닙니다", raw.len());
        }
        let data: Vec<f32> = words.iter().map(|c| f32::from_le_bytes(*c)).collect();
        let shape = view.shape().to_vec();
        if shape.iter().product::<usize>() != data.len() {
            bail!("파라미터 '{name}' 형상 {shape:?} 과 원소 수 {} 불일치", data.len());
        }
        out.insert(name.to_string(), HostTensor::new(shape, data));
    }
    Ok(out)
}

/// 헤더만 읽어 파라미터 이름·형상을 돌려준다 (모델 관리 뷰).
pub fn summary(path: &Path) -> Result<Vec<(String, Vec<usize>)>> {
    let buf = std::fs::read(path).with_context(|| format!("체크포인트 읽기 실패: {}", path.display()))?;
    let (_, meta) = safetensors::SafeTensors::read_metadata(&buf)
        .map_err(|e| anyhow::anyhow!("safetensors 헤더 파싱 실패 ({}): {e}", path.display()))?;
    let mut out: Vec<(String, Vec<usize>)> =
        meta.tensors().into_iter().map(|(name, info)| (name, info.shape.clone())).collect();
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_names_shapes_and_values() {
        let dir = std::env::temp_dir().join(format!("nl-weights-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.safetensors");
        let mut p = BTreeMap::new();
        p.insert("a.weight".to_string(), HostTensor::new(vec![2, 3], (0..6).map(|i| i as f32).collect()));
        p.insert("a.bias".to_string(), HostTensor::new(vec![3], vec![-1.5, 0.0, 2.25]));
        save(&path, ModelId::from_u128(7), &p).unwrap();

        let back = load(&path).unwrap();
        assert_eq!(back, p);

        let s = summary(&path).unwrap();
        assert_eq!(s, vec![("a.bias".to_string(), vec![3]), ("a.weight".to_string(), vec![2, 3])]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
