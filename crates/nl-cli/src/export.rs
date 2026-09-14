//! `nl export-onnx` — 학습한 모델을 ONNX 파일로 내보낸다.
//!
//! 다른 도구(ONNX Runtime, tract, Netron 등)로 가져가기 위한 것이다. **추론용 그래프**라
//! `Dropout` 은 빠지고 `BatchNorm` 은 저장된 running 통계로 고정된다.

use crate::common::*;
use anyhow::{bail, Context, Result};
use nl_engine::onnx::{self, Batch, ExportOptions};
use std::path::{Path, PathBuf};

pub struct Args<'a> {
    pub project: &'a Path,
    pub model: &'a str,
    pub out: Option<&'a Path>,
    /// 가중치 파일. 없으면 모델에 적힌 것을 쓴다.
    pub weights: Option<&'a Path>,
    /// 배치를 고정할 크기. 없으면 동적(`"B"` 기호).
    pub batch: Option<usize>,
    pub opset: Option<i64>,
}

pub fn run(args: Args<'_>) -> Result<i32> {
    let l = load_project(args.project)?;
    let model = find_model(&l.project, args.model)?.clone();

    // 형상·레이어 지원 여부를 먼저 본다. 가중치를 찾기 전에 알려 주는 편이 친절하다.
    let pre = onnx::check(&model);
    if !pre.unsupported.is_empty() {
        bail!(
            "이 모델에는 아직 ONNX 로 내보낼 수 없는 레이어가 있다: {}\n\
             순환(LSTM·GRU)·어텐션 레이어는 게이트 순서와 레이아웃 변환이 필요해 준비 중이다.",
            pre.unsupported.join(", ")
        );
    }

    let weights = match args.weights {
        Some(p) => p.to_path_buf(),
        None => {
            let rel = model.weights.as_ref().with_context(|| {
                format!(
                    "모델 '{}' 에 가중치가 없다. 먼저 `nl train` 을 돌리거나 --weights 로 지정해라",
                    model.name
                )
            })?;
            l.base_dir.join(rel)
        }
    };
    if !weights.is_file() {
        bail!("가중치 파일이 없다: {}", weights.display());
    }

    let out = match args.out {
        Some(p) => p.to_path_buf(),
        None => default_out(&l.base_dir, &model.name),
    };

    let opts = ExportOptions {
        opset: args.opset.unwrap_or(onnx::DEFAULT_OPSET),
        batch: match args.batch {
            Some(0) => bail!("--batch 는 1 이상이어야 한다"),
            Some(n) => Batch::Fixed(n),
            None => Batch::Dynamic,
        },
    };
    if opts.opset != onnx::DEFAULT_OPSET {
        eprintln!(
            "{}",
            yellow(&format!(
                "경고: 연산자 선택은 opset {} 기준이다. {} 로 적으면 읽는 쪽에서 깨질 수 있다.",
                onnx::DEFAULT_OPSET,
                opts.opset
            ))
        );
    }

    let report = onnx::export(&model, &weights, &out, opts).context("ONNX 로 내보내지 못했다")?;

    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let batch = match opts.batch {
        Batch::Dynamic => "동적".to_string(),
        Batch::Fixed(n) => n.to_string(),
    };
    println!("{} {}", green("내보냄"), out.display());
    table(&[
        vec![
            "모델".into(),
            "노드".into(),
            "가중치".into(),
            "opset".into(),
            "배치".into(),
            "크기".into(),
        ],
        vec![
            model.name.clone(),
            report.nodes.to_string(),
            report.initializers.to_string(),
            opts.opset.to_string(),
            batch,
            human_size(size),
        ],
    ]);
    Ok(0)
}

/// `<프로젝트 폴더>/<모델 이름>.onnx`. 이름에 쓸 수 없는 글자는 `_` 로 바꾼다.
fn default_out(base_dir: &Path, model_name: &str) -> PathBuf {
    let stem: String = model_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('_').to_string();
    base_dir.join(format!("{}.onnx", if stem.is_empty() { "model" } else { &stem }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_path_is_next_to_the_project() {
        assert_eq!(default_out(Path::new("/p"), "xor"), PathBuf::from("/p/xor.onnx"));
        // 공백·경로 구분자는 파일 이름에 넣지 않는다.
        assert_eq!(
            default_out(Path::new("/p"), "내 모델/v2"),
            PathBuf::from("/p/내_모델_v2.onnx")
        );
        // 이름이 통째로 걸러지면 기본 이름으로.
        assert_eq!(default_out(Path::new("/p"), "///"), PathBuf::from("/p/model.onnx"));
    }

    #[test]
    fn sample_project_reports_no_unsupported_layers() {
        // XOR·CNN 샘플은 지금 지원 범위 안이라 내보내기가 막히지 않아야 한다.
        for p in [nl_core::sample::xor_project(), nl_core::sample::quadrants_cnn_project()] {
            for m in p.models.values() {
                assert!(
                    onnx::check(m).unsupported.is_empty(),
                    "{} 에 미지원 레이어가 잡혔다: {:?}",
                    m.name,
                    onnx::check(m).unsupported
                );
            }
        }
    }
}
