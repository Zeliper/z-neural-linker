//! 체크포인트: safetensors 로 파라미터를 저장·로드한다.
//!
//! 저장 형식은 `"{node_id}.{part}"` 이름의 f32 텐서 모음이고, 헤더 메타데이터에 모델 id 와 포맷 표시를 남긴다.
//! burn 의 `Record` 파생을 쓰지 않는 이유는 그래프가 런타임에 정의되어 정적 모듈 타입이 없기 때문이다.

use crate::limits::{check_file_size, checked_elems, MAX_PARAM_NAME_LEN, MAX_WEIGHTS_BYTES, MAX_WEIGHTS_TENSORS};
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
///
/// 체크포인트는 바깥에서 온 파일이다. 파일 크기·텐서 개수·이름 길이·원소 수에 상한을 걸고,
/// 전체를 `Vec` 으로 읽는 대신 mmap 으로 매핑해 복사를 한 번으로 줄인다.
///
/// `expect_model` 을 주면 헤더의 모델 id 와 대조한다. 다르면 오류이며,
/// `NL_WEIGHTS_FORCE=1` 이면 경고만 남기고 계속 읽는다.
pub fn load(path: &Path) -> Result<BTreeMap<String, HostTensor>> {
    load_for(path, None)
}

/// [`load`] 에 모델 id 확인을 더한 것.
pub fn load_for(path: &Path, expect_model: Option<ModelId>) -> Result<BTreeMap<String, HostTensor>> {
    check_file_size(path, MAX_WEIGHTS_BYTES, "체크포인트")?;
    let file = std::fs::File::open(path).with_context(|| format!("체크포인트 열기 실패: {}", path.display()))?;
    // SAFETY: 읽기 전용 매핑. 읽는 동안 다른 프로세스가 파일을 줄이면 SIGBUS 가 날 수 있는데,
    // 이는 `std::fs::read` 로도 막을 수 없는 동시 수정이며 우리 쓰기는 tmp + rename 으로 원자적이다.
    let mapped =
        unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("체크포인트 매핑 실패: {}", path.display()))?;
    let buf: &[u8] = &mapped;

    let st = safetensors::SafeTensors::deserialize(buf)
        .map_err(|e| anyhow::anyhow!("safetensors 파싱 실패 ({}): {e}", path.display()))?;
    if st.len() > MAX_WEIGHTS_TENSORS {
        bail!(
            "체크포인트의 텐서 {} 개가 상한 {MAX_WEIGHTS_TENSORS} 를 넘습니다",
            st.len()
        );
    }
    check_header(buf, path, expect_model)?;

    let mut out = BTreeMap::new();
    for (name, view) in st.iter() {
        if name.len() > MAX_PARAM_NAME_LEN {
            bail!(
                "파라미터 이름이 {} 바이트로 상한 {MAX_PARAM_NAME_LEN} 를 넘습니다",
                name.len()
            );
        }
        if view.dtype() != Dtype::F32 {
            bail!(
                "파라미터 '{name}' 의 dtype 이 {:?} 입니다 — f32 만 지원합니다",
                view.dtype()
            );
        }
        let shape = view.shape().to_vec();
        let want = checked_elems(&shape, &format!("파라미터 '{name}'"))?;
        let raw = view.data();
        let (words, rest) = raw.as_chunks::<4>();
        if !rest.is_empty() {
            bail!("파라미터 '{name}' 의 바이트 길이 {}가 4의 배수가 아닙니다", raw.len());
        }
        if words.len() != want {
            bail!("파라미터 '{name}' 형상 {shape:?} 과 원소 수 {} 불일치", words.len());
        }
        let data: Vec<f32> = words.iter().map(|c| f32::from_le_bytes(*c)).collect();
        out.insert(name.to_string(), HostTensor::try_new(shape, data)?);
    }
    Ok(out)
}

/// 헤더 메타데이터의 `format` 과 `model` 을 확인한다.
fn check_header(buf: &[u8], path: &Path, expect_model: Option<ModelId>) -> Result<()> {
    let Ok((_, meta)) = safetensors::SafeTensors::read_metadata(buf) else {
        return Ok(());
    };
    let Some(map) = meta.metadata().as_ref() else {
        return Ok(());
    };
    if let Some(fmt) = map.get("format") {
        if fmt != WEIGHTS_FORMAT {
            log::warn!(
                "{}: format 이 '{fmt}' 입니다 (기대값 '{WEIGHTS_FORMAT}') — 그대로 읽습니다",
                path.display()
            );
        }
    }
    let (Some(want), Some(got)) = (expect_model, map.get("model")) else {
        return Ok(());
    };
    if got == &want.to_string() {
        return Ok(());
    }
    if std::env::var("NL_WEIGHTS_FORCE").as_deref() == Ok("1") {
        log::warn!(
            "{}: 다른 모델({got})의 체크포인트지만 NL_WEIGHTS_FORCE=1 이라 그대로 읽습니다",
            path.display()
        );
        return Ok(());
    }
    bail!(
        "이 체크포인트는 다른 모델의 것입니다 — 파일의 모델 id 는 {got} 인데 지금 모델은 {want} 입니다 \
         ({}). 그래도 얹으려면 NL_WEIGHTS_FORCE=1 로 실행하십시오",
        path.display()
    )
}

/// 헤더만 읽어 파라미터 이름·형상을 돌려준다 (모델 관리 뷰).
pub fn summary(path: &Path) -> Result<Vec<(String, Vec<usize>)>> {
    check_file_size(path, MAX_WEIGHTS_BYTES, "체크포인트")?;
    // mmap 으로 매핑만 한다 — 헤더 페이지만 실제로 읽히므로 3 GB 파일도 통째로 메모리에 올리지 않는다.
    // (safetensors 의 `read_metadata` 는 마지막 오프셋이 파일 끝과 맞는지 확인하므로 전체 슬라이스가 필요하다.)
    let file = std::fs::File::open(path).with_context(|| format!("체크포인트 열기 실패: {}", path.display()))?;
    let mapped =
        unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("체크포인트 매핑 실패: {}", path.display()))?;
    let (_, meta) = safetensors::SafeTensors::read_metadata(&mapped)
        .map_err(|e| anyhow::anyhow!("safetensors 헤더 파싱 실패 ({}): {e}", path.display()))?;
    let mut out: Vec<(String, Vec<usize>)> = meta
        .tensors()
        .into_iter()
        .map(|(name, info)| (name, info.shape.clone()))
        .collect();
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
        p.insert(
            "a.weight".to_string(),
            HostTensor::new(vec![2, 3], (0..6).map(|i| i as f32).collect()),
        );
        p.insert("a.bias".to_string(), HostTensor::new(vec![3], vec![-1.5, 0.0, 2.25]));
        save(&path, ModelId::from_u128(7), &p).unwrap();

        let back = load(&path).unwrap();
        assert_eq!(back, p);

        // 다른 모델 id 로 읽으면 막힌다 (S4).
        let e = format!("{:#}", load_for(&path, Some(ModelId::from_u128(9))).unwrap_err());
        assert!(e.contains("다른 모델"), "{e}");
        // 같은 id 면 통과.
        assert!(load_for(&path, Some(ModelId::from_u128(7))).is_ok());

        let s = summary(&path).unwrap();
        assert_eq!(
            s,
            vec![("a.bias".to_string(), vec![3]), ("a.weight".to_string(), vec![2, 3])]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_or_hostile_checkpoints_error_instead_of_panicking() {
        let dir = std::env::temp_dir().join(format!("nl-weights-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 쓰레기 파일.
        let p = dir.join("garbage.safetensors");
        std::fs::write(&p, b"not safetensors at all").unwrap();
        assert!(load(&p).is_err());
        assert!(summary(&p).is_err());

        // 헤더 길이가 파일보다 큰 경우.
        let p2 = dir.join("short.safetensors");
        let mut bytes = u64::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        std::fs::write(&p2, &bytes).unwrap();
        assert!(summary(&p2).is_err(), "헤더 길이 상한에 걸려야 합니다");
        assert!(load(&p2).is_err());

        // 빈 파일.
        let p3 = dir.join("empty.safetensors");
        std::fs::write(&p3, b"").unwrap();
        assert!(load(&p3).is_err());
        assert!(summary(&p3).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
