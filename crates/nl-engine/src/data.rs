//! 데이터셋 로딩(CSV · 이미지 폴더 · 합성 · 녹화). 학습 루프와 미리보기가 공유한다.

use crate::tensor::HostTensor;
use anyhow::{bail, Context, Result};
use nl_core::dataset::{DataSource, DatasetInfo, SyntheticKind};
use nl_core::DatasetSpec;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::path::{Path, PathBuf};

/// 샘플 하나 (배치 차원 없음: `input.shape` = 샘플 형상).
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub input: HostTensor,
    pub target: HostTensor,
}

/// 합성 데이터의 고정 시드 — `DataSource::Synthetic` 에는 시드 필드가 없고, 같은 스펙이면 같은 데이터여야 한다.
const SYNTHETIC_SEED: u64 = 0x5EED_1234;

/// 소스를 훑어 샘플 수·형상·클래스를 알아낸다 (데이터 뷰 "스캔", 학습 전 점검).
///
/// 이미지 소스는 파일을 전부 디코드하지 않고 개수만 세고 첫 장으로 형상을 잡는다.
pub fn scan(spec: &DatasetSpec, base_dir: &Path) -> Result<DatasetInfo> {
    scan_source(&spec.source, base_dir, None)
}

/// 앞에서 `n` 개 샘플 (데이터 뷰 미리보기).
pub fn preview(spec: &DatasetSpec, base_dir: &Path, n: usize) -> Result<Vec<Sample>> {
    let (samples, _) = load_source(&spec.source, base_dir, None, Some(n))?;
    Ok(samples)
}

/// 학습용 전체 적재. `input_shape_hint` 는 모델 Input 레이어의 샘플 형상(예: `[1, 28, 28]`)이며
/// 이미지 소스의 리사이즈·채널 변환 목표가 된다.
pub fn load_all(
    spec: &DatasetSpec,
    base_dir: &Path,
    input_shape_hint: Option<&[usize]>,
) -> Result<(Vec<Sample>, DatasetInfo)> {
    load_source(&spec.source, base_dir, input_shape_hint, None)
}

/// 소스 하나를 적재한다 (`Split::Separate` 의 검증 소스도 이걸로 읽는다).
pub fn load_source(
    source: &DataSource,
    base_dir: &Path,
    hint: Option<&[usize]>,
    limit: Option<usize>,
) -> Result<(Vec<Sample>, DatasetInfo)> {
    match source {
        DataSource::Synthetic { kind, samples } => synthetic(*kind, limit.map_or(*samples, |l| l.min(*samples))),
        DataSource::Csv { path, input_cols, target_cols, header } => {
            load_csv(&resolve(base_dir, path), input_cols, target_cols, *header, limit)
        }
        DataSource::ImageFolder { path } => load_image_folder(&resolve(base_dir, path), hint, limit),
        DataSource::Recorded { path } => load_recorded(&resolve(base_dir, path), hint, limit),
    }
}

fn scan_source(source: &DataSource, base_dir: &Path, hint: Option<&[usize]>) -> Result<DatasetInfo> {
    match source {
        DataSource::Synthetic { kind, samples } => {
            let (i, t, classes) = kind.shapes();
            Ok(DatasetInfo {
                samples: *samples,
                input_shape: i,
                target_shape: t,
                classes: classes.map(|c| (0..c).map(|k| k.to_string()).collect()).unwrap_or_default(),
            })
        }
        DataSource::ImageFolder { path } => {
            let dir = resolve(base_dir, path);
            let classes = class_dirs(&dir)?;
            let mut samples = 0usize;
            let mut first: Option<PathBuf> = None;
            let mut names = Vec::new();
            for (name, cdir) in &classes {
                names.push(name.clone());
                let files = image_files(cdir)?;
                if first.is_none() {
                    first = files.first().cloned();
                }
                samples += files.len();
            }
            let input_shape = match hint {
                Some(h) if h.len() == 3 => h.to_vec(),
                _ => match &first {
                    Some(p) => {
                        let img = image::open(p).with_context(|| format!("이미지 열기 실패: {}", p.display()))?;
                        let c = if img.color().has_color() { 3 } else { 1 };
                        vec![c, img.height() as usize, img.width() as usize]
                    }
                    None => vec![],
                },
            };
            Ok(DatasetInfo { samples, input_shape, target_shape: vec![1], classes: names })
        }
        _ => {
            // CSV·녹화는 전부 읽어야 정확하다 (행/줄 수 기준이라 비용이 크지 않다).
            Ok(load_source(source, base_dir, hint, None)?.1)
        }
    }
}

fn resolve(base_dir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

// ───────────────────────────── 합성 ─────────────────────────────

fn synthetic(kind: SyntheticKind, n: usize) -> Result<(Vec<Sample>, DatasetInfo)> {
    if n == 0 {
        bail!("합성 데이터 샘플 수가 0 입니다");
    }
    let (in_shape, target_shape, classes) = kind.shapes();
    let mut rng = ChaCha8Rng::seed_from_u64(SYNTHETIC_SEED ^ (kind as u64));
    let mut out = Vec::with_capacity(n);

    for i in 0..n {
        let (input, target) = match kind {
            SyntheticKind::Xor => {
                let x1: f32 = rng.random_range(-1.0..1.0);
                let x2: f32 = rng.random_range(-1.0..1.0);
                let y = ((x1 > 0.0) != (x2 > 0.0)) as usize as f32;
                (vec![x1, x2], vec![y])
            }
            SyntheticKind::Spirals => {
                let cls = i % 2;
                let frac = (i / 2) as f32 / (n as f32 / 2.0).max(1.0);
                let r = 0.15 + 0.85 * frac;
                let theta = frac * 4.0 * std::f32::consts::PI
                    + cls as f32 * std::f32::consts::PI
                    + rng.random_range(-0.08..0.08);
                (vec![r * theta.cos(), r * theta.sin()], vec![cls as f32])
            }
            SyntheticKind::LinearRegression => {
                let x1: f32 = rng.random_range(-1.0..1.0);
                let x2: f32 = rng.random_range(-1.0..1.0);
                let noise: f32 = rng.random_range(-0.03..0.03);
                (vec![x1, x2], vec![3.0 * x1 - 2.0 * x2 + 0.5 + noise])
            }
            SyntheticKind::Quadrants => {
                let q = rng.random_range(0..4usize);
                let mut img = vec![0.0f32; 64];
                // 사분면: 0 = 좌상, 1 = 우상, 2 = 좌하, 3 = 우하.
                let (oy, ox) = ((q / 2) * 4, (q % 2) * 4);
                let y0 = oy + rng.random_range(0..3usize);
                let x0 = ox + rng.random_range(0..3usize);
                for dy in 0..2 {
                    for dx in 0..2 {
                        img[(y0 + dy) * 8 + (x0 + dx)] = 1.0;
                    }
                }
                for v in img.iter_mut() {
                    *v += rng.random_range(-0.02..0.02);
                }
                (img, vec![q as f32])
            }
        };
        out.push(Sample {
            input: HostTensor::new(in_shape.clone(), input),
            target: HostTensor::new(target_shape.clone(), target),
        });
    }

    let info = DatasetInfo {
        samples: out.len(),
        input_shape: in_shape,
        target_shape,
        classes: classes.map(|c| (0..c).map(|k| k.to_string()).collect()).unwrap_or_default(),
    };
    Ok((out, info))
}

// ───────────────────────────── CSV ─────────────────────────────

fn load_csv(
    path: &Path,
    input_cols: &[String],
    target_cols: &[String],
    header: bool,
    limit: Option<usize>,
) -> Result<(Vec<Sample>, DatasetInfo)> {
    if input_cols.is_empty() {
        bail!("CSV 입력 열이 비어 있습니다");
    }
    if target_cols.is_empty() {
        bail!("CSV 타깃 열이 비어 있습니다");
    }
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(header)
        .from_path(path)
        .with_context(|| format!("CSV 열기 실패: {}", path.display()))?;

    let names: Vec<String> = if header {
        rdr.headers()
            .with_context(|| format!("CSV 헤더 읽기 실패: {}", path.display()))?
            .iter()
            .map(|s| s.trim().to_string())
            .collect()
    } else {
        vec![]
    };

    let resolve_col = |spec: &str| -> Result<usize> {
        if let Some(i) = names.iter().position(|n| n == spec.trim()) {
            return Ok(i);
        }
        spec.trim()
            .parse::<usize>()
            .with_context(|| format!("CSV 열 '{spec}' 을 이름으로도 번호로도 찾을 수 없습니다"))
    };
    let in_idx: Vec<usize> = input_cols.iter().map(|c| resolve_col(c)).collect::<Result<_>>()?;
    let tg_idx: Vec<usize> = target_cols.iter().map(|c| resolve_col(c)).collect::<Result<_>>()?;

    let mut out = Vec::new();
    for (row, rec) in rdr.records().enumerate() {
        if let Some(l) = limit {
            if out.len() >= l {
                break;
            }
        }
        let rec = rec.with_context(|| format!("CSV {}행 읽기 실패", row + 1))?;
        let pick = |idx: &[usize]| -> Result<Vec<f32>> {
            idx.iter()
                .map(|&i| {
                    let raw = rec.get(i).with_context(|| format!("CSV {}행에 열 {i} 이 없습니다", row + 1))?;
                    raw.trim()
                        .parse::<f32>()
                        .with_context(|| format!("CSV {}행 열 {i} 의 값 '{raw}' 을 수로 읽을 수 없습니다", row + 1))
                })
                .collect()
        };
        let x = pick(&in_idx)?;
        let y = pick(&tg_idx)?;
        out.push(Sample {
            input: HostTensor::new(vec![in_idx.len()], x),
            target: HostTensor::new(vec![tg_idx.len()], y),
        });
    }
    if out.is_empty() {
        bail!("CSV 에서 읽은 행이 없습니다: {}", path.display());
    }
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: vec![in_idx.len()],
        target_shape: vec![tg_idx.len()],
        classes: vec![],
    };
    Ok((out, info))
}

// ───────────────────────────── 이미지 ─────────────────────────────

fn class_dirs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("폴더 열기 실패: {}", dir.display()))?;
    let mut v: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    if v.is_empty() {
        bail!("이미지 폴더에 클래스 하위 폴더가 없습니다: {}", dir.display());
    }
    Ok(v)
}

fn image_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("폴더 열기 실패: {}", dir.display()))?;
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| matches!(e.to_ascii_lowercase().as_str(), "png" | "jpg" | "jpeg"))
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    Ok(v)
}

/// 이미지 파일 하나를 `[C, H, W]` f32(0..1) 로. `shape` 가 `None` 이면 파일 크기를 그대로 쓴다.
fn load_image(path: &Path, shape: Option<&[usize]>) -> Result<HostTensor> {
    let img = image::open(path).with_context(|| format!("이미지 열기 실패: {}", path.display()))?;
    let (c, h, w) = match shape {
        Some(s) if s.len() == 3 => (s[0], s[1], s[2]),
        Some(s) => bail!("이미지 입력 형상은 [C, H, W] 여야 합니다 (지금 {s:?})"),
        None => {
            let c = if img.color().has_color() { 3 } else { 1 };
            (c, img.height() as usize, img.width() as usize)
        }
    };
    if c != 1 && c != 3 {
        bail!("이미지 채널은 1 또는 3 만 지원합니다 (지금 {c})");
    }
    let resized = if img.width() as usize != w || img.height() as usize != h {
        img.resize_exact(w as u32, h as u32, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let data = if c == 1 {
        resized.to_luma8().into_raw().into_iter().map(|v| v as f32 / 255.0).collect::<Vec<f32>>()
    } else {
        // [H, W, 3] → [3, H, W]
        let rgb = resized.to_rgb8();
        let raw = rgb.as_raw();
        let mut out = vec![0.0f32; 3 * h * w];
        for y in 0..h {
            for x in 0..w {
                for ch in 0..3 {
                    out[ch * h * w + y * w + x] = raw[(y * w + x) * 3 + ch] as f32 / 255.0;
                }
            }
        }
        out
    };
    Ok(HostTensor::new(vec![c, h, w], data))
}

fn load_image_folder(
    dir: &Path,
    hint: Option<&[usize]>,
    limit: Option<usize>,
) -> Result<(Vec<Sample>, DatasetInfo)> {
    let classes = class_dirs(dir)?;
    let mut shape: Option<Vec<usize>> = hint.map(|h| h.to_vec());
    let mut out = Vec::new();
    let mut names = Vec::with_capacity(classes.len());

    for (ci, (name, cdir)) in classes.iter().enumerate() {
        names.push(name.clone());
        for f in image_files(cdir)? {
            if let Some(l) = limit {
                if out.len() >= l {
                    break;
                }
            }
            let t = load_image(&f, shape.as_deref())?;
            if shape.is_none() {
                shape = Some(t.shape.clone());
            }
            out.push(Sample { input: t, target: HostTensor::new(vec![1], vec![ci as f32]) });
        }
    }
    if out.is_empty() {
        bail!("이미지 폴더에서 읽은 파일이 없습니다: {}", dir.display());
    }
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: shape.unwrap_or_default(),
        target_shape: vec![1],
        classes: names,
    };
    Ok((out, info))
}

// ───────────────────────────── 녹화 폴더 ─────────────────────────────

#[derive(serde::Deserialize)]
struct LabelLine {
    frame: String,
    label: i64,
}

fn load_recorded(dir: &Path, hint: Option<&[usize]>, limit: Option<usize>) -> Result<(Vec<Sample>, DatasetInfo)> {
    let labels_path = dir.join("labels.jsonl");
    let frames_dir = dir.join("frames");
    let text = std::fs::read_to_string(&labels_path)
        .with_context(|| format!("라벨 파일 읽기 실패: {}", labels_path.display()))?;

    let mut shape: Option<Vec<usize>> = hint.map(|h| h.to_vec());
    let mut out = Vec::new();
    let mut max_label = 0i64;

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(l) = limit {
            if out.len() >= l {
                break;
            }
        }
        let rec: LabelLine = serde_json::from_str(line)
            .with_context(|| format!("{} {}줄을 읽을 수 없습니다", labels_path.display(), i + 1))?;
        if rec.label < 0 {
            bail!("{} {}줄: 라벨은 0 이상이어야 합니다 (지금 {})", labels_path.display(), i + 1, rec.label);
        }
        max_label = max_label.max(rec.label);
        let f = frames_dir.join(&rec.frame);
        let t = load_image(&f, shape.as_deref())?;
        if shape.is_none() {
            shape = Some(t.shape.clone());
        }
        out.push(Sample { input: t, target: HostTensor::new(vec![1], vec![rec.label as f32]) });
    }
    if out.is_empty() {
        bail!("녹화 폴더에서 읽은 프레임이 없습니다: {}", dir.display());
    }
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: shape.unwrap_or_default(),
        target_shape: vec![1],
        classes: (0..=max_label).map(|i| i.to_string()).collect(),
    };
    Ok((out, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_shapes_follow_the_spec() {
        for kind in SyntheticKind::ALL {
            let (s, info) = synthetic(kind, 32).unwrap();
            let (want_in, want_t, classes) = kind.shapes();
            assert_eq!(s.len(), 32);
            assert_eq!(s[0].input.shape, want_in, "{kind:?}");
            assert_eq!(s[0].target.shape, want_t, "{kind:?}");
            assert_eq!(info.input_shape, want_in);
            assert_eq!(info.classes.len(), classes.unwrap_or(0));
            if let Some(c) = classes {
                for smp in &s {
                    let v = smp.target.data[0];
                    assert!(v >= 0.0 && (v as usize) < c, "클래스 인덱스 범위 밖: {v}");
                }
            }
        }
    }

    #[test]
    fn synthetic_is_deterministic() {
        let a = synthetic(SyntheticKind::Xor, 16).unwrap().0;
        let b = synthetic(SyntheticKind::Xor, 16).unwrap().0;
        assert_eq!(a, b);
    }

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("nl-data-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 회색조 그라데이션 PNG 한 장.
    fn write_png(path: &Path, w: u32, h: u32, base: u8) {
        let img = image::GrayImage::from_fn(w, h, |x, y| {
            image::Luma([base.wrapping_add((x * 7 + y * 13) as u8)])
        });
        img.save(path).unwrap();
    }

    #[test]
    fn image_folder_uses_sorted_class_dirs_and_the_shape_hint() {
        let dir = tmp("imgfolder");
        for name in ["zeta", "alpha"] {
            let c = dir.join(name);
            std::fs::create_dir_all(&c).unwrap();
            write_png(&c.join("b.png"), 6, 4, 10);
            write_png(&c.join("a.png"), 6, 4, 200);
        }

        let (samples, info) = load_image_folder(&dir, Some(&[1, 2, 3]), None).unwrap();
        assert_eq!(info.classes, vec!["alpha".to_string(), "zeta".to_string()]);
        assert_eq!(info.samples, 4);
        assert_eq!(info.input_shape, vec![1, 2, 3]);
        assert!(samples.iter().all(|s| s.input.shape == vec![1, 2, 3]));
        assert!(samples.iter().all(|s| s.input.data.iter().all(|v| (0.0..=1.0).contains(v))));
        // alpha = 0, zeta = 1 (정렬 순).
        assert_eq!(samples[0].target.data, vec![0.0]);
        assert_eq!(samples[3].target.data, vec![1.0]);

        // 힌트가 없으면 첫 이미지 크기를 쓴다.
        let (_, info2) = load_image_folder(&dir, None, None).unwrap();
        assert_eq!(info2.input_shape, vec![1, 4, 6]);

        // scan 은 디코드 없이 개수를 세고 첫 장으로 형상을 잡는다.
        let scanned = scan_source(&DataSource::ImageFolder { path: dir.to_string_lossy().into() }, Path::new("."), None).unwrap();
        assert_eq!(scanned.samples, 4);
        assert_eq!(scanned.input_shape, vec![1, 4, 6]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recorded_folder_reads_frames_and_labels() {
        let dir = tmp("recorded");
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        write_png(&frames.join("000001.png"), 4, 4, 0);
        write_png(&frames.join("000002.png"), 4, 4, 128);
        std::fs::write(
            dir.join("labels.jsonl"),
            "{\"frame\":\"000001.png\",\"label\":3}\n{\"frame\":\"000002.png\",\"label\":1}\n",
        )
        .unwrap();

        let (samples, info) = load_recorded(&dir, None, None).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(info.input_shape, vec![1, 4, 4]);
        assert_eq!(samples[0].target.data, vec![3.0]);
        assert_eq!(samples[1].target.data, vec![1.0]);
        assert_eq!(info.classes.len(), 4, "0..=3 라벨");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn csv_reads_named_and_numbered_columns() {
        let dir = std::env::temp_dir().join(format!("nl-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.csv");
        std::fs::write(&p, "a,b,y\n1,2,3\n4,5,6\n").unwrap();

        let by_name = load_csv(&p, &["a".into(), "b".into()], &["y".into()], true, None).unwrap().0;
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[0].input.data, vec![1.0, 2.0]);
        assert_eq!(by_name[1].target.data, vec![6.0]);

        let by_index = load_csv(&p, &["0".into(), "1".into()], &["2".into()], true, None).unwrap().0;
        assert_eq!(by_index, by_name);

        std::fs::write(&p, "1,2,3\n4,5,6\n").unwrap();
        let no_header = load_csv(&p, &["0".into(), "1".into()], &["2".into()], false, None).unwrap().0;
        assert_eq!(no_header.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
