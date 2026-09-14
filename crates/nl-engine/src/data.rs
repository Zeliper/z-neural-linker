//! 데이터셋 로딩(CSV · 이미지 폴더 · 합성 · 녹화). 학습 루프와 미리보기가 공유한다.

use crate::limits::{
    check_file_size, checked_elems, decode_image_file, MAX_CSV_BYTES, MAX_CSV_COLS, MAX_CSV_ROWS, MAX_DATASET_ELEMS,
    MAX_LABELS_BYTES,
};
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

/// CSV 타깃을 분류로 볼 수 있는 최대 클래스 수. 넘으면 회귀로 본다.
const CSV_MAX_INFERRED_CLASSES: usize = 256;

/// 스캔이 CSV 에서 실제로 읽는 행 수. 나머지는 파일 크기로 어림한다.
const CSV_SCAN_ROWS: usize = 1_000;

/// 클래스별 샘플 수를 세어 하나도 없는 클래스의 인덱스를 돌려준다 (오름차순).
fn empty_class_indices(class_count: usize, targets: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut seen = vec![false; class_count];
    for t in targets {
        if let Some(slot) = seen.get_mut(t) {
            *slot = true;
        }
    }
    seen.iter().enumerate().filter(|(_, &v)| !v).map(|(i, _)| i).collect()
}

/// 샘플들의 타깃을 클래스 인덱스로 읽는다 (타깃이 스칼라 1 개일 때만 의미가 있다).
fn target_classes(samples: &[Sample]) -> impl Iterator<Item = usize> + '_ {
    samples.iter().filter_map(|s| {
        let v = *s.target.data.first()?;
        (v >= 0.0 && v.fract() == 0.0).then_some(v as usize)
    })
}

/// 소스를 훑어 샘플 수·형상·클래스를 알아낸다 (데이터 뷰 "스캔", 학습 전 점검).
///
/// 이미지 소스는 파일을 전부 디코드하지 않고 개수만 세고 첫 장으로 형상을 잡는다.
pub fn scan(spec: &DatasetSpec, base_dir: &Path) -> Result<DatasetInfo> {
    scan_source(&spec.source, base_dir, None)
}

/// 앞에서 `n` 개 샘플 (데이터 뷰 미리보기).
/// 앞에서 `n` 개 샘플 (데이터 뷰 미리보기).
///
/// 클래스가 있는 소스(이미지 폴더·녹화)는 **클래스별로 고르게** 뽑는다. 앞에서 그냥 자르면
/// 첫 클래스만 보이는데, 미리보기의 목적은 데이터가 어떤 모습인지 훑는 것이다.
/// CSV·합성은 클래스를 미리 알 수 없어 앞에서부터 자른다.
pub fn preview(spec: &DatasetSpec, base_dir: &Path, n: usize) -> Result<Vec<Sample>> {
    if n == 0 {
        return Ok(vec![]);
    }
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
        DataSource::Csv {
            path,
            input_cols,
            target_cols,
            header,
        } => load_csv(&resolve(base_dir, path), input_cols, target_cols, *header, limit),
        DataSource::ImageFolder { path } => load_image_folder(&resolve(base_dir, path), hint, limit),
        DataSource::Recorded { path } => load_recorded(&resolve(base_dir, path), hint, limit),
    }
}

fn scan_source(source: &DataSource, base_dir: &Path, hint: Option<&[usize]>) -> Result<DatasetInfo> {
    match source {
        DataSource::Synthetic { kind, samples } => {
            let (i, t, classes) = kind.shapes();
            // 합성 데이터는 규칙상 모든 클래스가 나오므로 빈 클래스가 없다.
            Ok(DatasetInfo {
                samples: *samples,
                input_shape: i,
                target_shape: t,
                classes: classes
                    .map(|c| (0..c).map(|k| k.to_string()).collect())
                    .unwrap_or_default(),
                samples_estimated: false,
                empty_classes: vec![],
            })
        }
        DataSource::ImageFolder { path } => {
            let dir = resolve(base_dir, path);
            let classes = class_dirs(&dir)?;
            let mut samples = 0usize;
            let mut first: Option<PathBuf> = None;
            let mut names = Vec::new();
            let mut empty_classes = Vec::new();
            for (i, (name, cdir)) in classes.iter().enumerate() {
                names.push(name.clone());
                let files = image_files(cdir)?;
                if files.is_empty() {
                    empty_classes.push(i);
                }
                if first.is_none() {
                    first = files.first().cloned();
                }
                samples += files.len();
            }
            let input_shape = match hint {
                Some(h) if h.len() == 3 => h.to_vec(),
                _ => match &first {
                    Some(p) => {
                        let img = decode_image_file(p)?;
                        let c = if img.color().has_color() { 3 } else { 1 };
                        vec![c, img.height() as usize, img.width() as usize]
                    }
                    None => vec![],
                },
            };
            Ok(DatasetInfo {
                samples,
                input_shape,
                target_shape: vec![1],
                classes: names,
                samples_estimated: false,
                empty_classes,
            })
        }
        DataSource::Csv {
            path,
            input_cols,
            target_cols,
            header,
        } => scan_csv(&resolve(base_dir, path), input_cols, target_cols, *header),
        _ => {
            // 녹화는 줄 수만 세면 되므로 전부 읽어도 비용이 크지 않다.
            Ok(load_source(source, base_dir, hint, None)?.1)
        }
    }
}

/// CSV 를 **전부 읽지 않고** 훑는다.
///
/// 앞 [`CSV_SCAN_ROWS`] 행만 읽어 형상·클래스를 보고, 행 수는 그 표본의 평균 바이트로 어림한다.
/// 10 GB CSV 를 스캔 한 번에 전부 메모리에 올리던 동작을 대신한다.
/// 표본 안에서 파일이 끝나면 그 값이 정확하므로 `samples_estimated` 는 거짓이다.
fn scan_csv(path: &Path, input_cols: &[String], target_cols: &[String], header: bool) -> Result<DatasetInfo> {
    check_file_size(path, MAX_CSV_BYTES, "CSV 파일")?;
    let (sample, mut info) = load_csv(path, input_cols, target_cols, header, Some(CSV_SCAN_ROWS))?;

    // 표본이 상한보다 적으면 파일을 다 본 것이다.
    if sample.len() < CSV_SCAN_ROWS {
        return Ok(info);
    }
    let Ok(meta) = std::fs::metadata(path) else {
        info.samples_estimated = true;
        return Ok(info);
    };
    // 표본이 차지한 바이트를 재서 남은 부분의 행 수를 어림한다.
    let sampled_bytes = csv_prefix_bytes(path, header, CSV_SCAN_ROWS)?;
    if sampled_bytes == 0 {
        info.samples_estimated = true;
        return Ok(info);
    }
    let per_row = sampled_bytes as f64 / sample.len() as f64;
    info.samples = ((meta.len() as f64 / per_row).round() as usize).max(sample.len());
    info.samples_estimated = true;
    // 클래스·빈 클래스는 표본에서 본 것뿐이라 단정할 수 없다.
    info.empty_classes.clear();
    Ok(info)
}

/// 헤더와 앞 `rows` 개 레코드가 차지하는 바이트 수. 줄바꿈이 따옴표 안에 있을 수 있어
/// 줄 수로 세지 않고 CSV 리더의 위치를 쓴다.
fn csv_prefix_bytes(path: &Path, header: bool, rows: usize) -> Result<u64> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(header)
        .from_path(path)
        .with_context(|| format!("CSV 열기 실패: {}", path.display()))?;
    let mut rec = csv::StringRecord::new();
    let mut n = 0usize;
    while n < rows {
        match rdr.read_record(&mut rec) {
            Ok(true) => n += 1,
            Ok(false) => break,
            Err(e) => bail!("CSV 훑기 실패 ({}): {e}", path.display()),
        }
    }
    Ok(rdr.position().byte())
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

    let names: Vec<String> = classes
        .map(|c| (0..c).map(|k| k.to_string()).collect())
        .unwrap_or_default();
    let empty_classes = empty_class_indices(names.len(), target_classes(&out));
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: in_shape,
        target_shape,
        classes: names,
        samples_estimated: false,
        empty_classes,
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
    check_file_size(path, MAX_CSV_BYTES, "CSV 파일")?;
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
    // 오류 메시지에 사용자가 쓴 지정자를 그대로 보여 주기 위한 대응표.
    let in_spec: Vec<&str> = input_cols.iter().map(|c| c.as_str()).collect();
    let tg_spec: Vec<&str> = target_cols.iter().map(|c| c.as_str()).collect();

    if in_idx.len() + tg_idx.len() > MAX_CSV_COLS {
        bail!(
            "CSV 열 수 {} 가 상한 {MAX_CSV_COLS} 를 넘습니다",
            in_idx.len() + tg_idx.len()
        );
    }
    let per_row = in_idx.len() + tg_idx.len();
    let max_rows = limit.unwrap_or(MAX_CSV_ROWS).min(MAX_CSV_ROWS);

    let mut out = Vec::new();
    for (row, rec) in rdr.records().enumerate() {
        if out.len() >= max_rows {
            if limit.is_none() {
                bail!("CSV 행 수가 상한 {MAX_CSV_ROWS} 를 넘습니다: {}", path.display());
            }
            break;
        }
        let rec = rec.with_context(|| format!("CSV {}행 읽기 실패", row + 1))?;
        if rec.len() > MAX_CSV_COLS {
            bail!(
                "CSV {}행의 열 수 {} 가 상한 {MAX_CSV_COLS} 를 넘습니다",
                row + 1,
                rec.len()
            );
        }
        if out.len().saturating_mul(per_row) > MAX_DATASET_ELEMS {
            bail!("CSV 원소 수가 상한 {MAX_DATASET_ELEMS} 를 넘습니다: {}", path.display());
        }
        let pick = |idx: &[usize], spec: &[&str]| -> Result<Vec<f32>> {
            idx.iter()
                .zip(spec)
                .map(|(&i, name)| {
                    let raw = rec
                        .get(i)
                        .with_context(|| format!("CSV {}행에 열 '{name}'(번호 {i}) 이 없습니다", row + 1))?;
                    raw.trim().parse::<f32>().with_context(|| {
                        format!(
                            "CSV {}행 열 '{name}'(번호 {i}) 의 값 '{raw}' 을 수로 읽을 수 없습니다",
                            row + 1
                        )
                    })
                })
                .collect()
        };
        let x = pick(&in_idx, &in_spec)?;
        let y = pick(&tg_idx, &tg_spec)?;
        out.push(Sample {
            input: HostTensor::new(vec![in_idx.len()], x),
            target: HostTensor::new(vec![tg_idx.len()], y),
        });
    }
    if out.is_empty() {
        bail!("CSV 에서 읽은 행이 없습니다: {}", path.display());
    }
    let (classes, empty_classes) = csv_classes(&out, tg_idx.len());
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: vec![in_idx.len()],
        target_shape: vec![tg_idx.len()],
        classes,
        samples_estimated: false,
        empty_classes,
    };
    Ok((out, info))
}

/// CSV 타깃을 분류로 볼 수 있는지 추론한다.
///
/// 타깃이 **한 열**이고 값이 전부 음이 아닌 정수이며 최대값이 작을 때만 분류로 본다
/// (클래스 `0..=max`). 그 밖에는 회귀로 보고 클래스를 비워 둔다 — 실수 타깃이나 여러 열은
/// 분류가 아니다. 분류로 보이면 중간에 빠진 클래스도 함께 알린다.
fn csv_classes(out: &[Sample], target_cols: usize) -> (Vec<String>, Vec<usize>) {
    if target_cols != 1 {
        return (vec![], vec![]);
    }
    let mut max = 0usize;
    for s in out {
        match s.target.data.first() {
            Some(&v) if v >= 0.0 && v.fract() == 0.0 && (v as usize) < CSV_MAX_INFERRED_CLASSES => {
                max = max.max(v as usize)
            }
            _ => return (vec![], vec![]),
        }
    }
    if max == 0 {
        return (vec![], vec![]); // 전부 0 이면 분류라고 단정할 수 없다
    }
    let names: Vec<String> = (0..=max).map(|i| i.to_string()).collect();
    let empty = empty_class_indices(names.len(), target_classes(out));
    (names, empty)
}

// ───────────────────────────── 이미지 ─────────────────────────────

fn class_dirs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("폴더 열기 실패: {}", dir.display()))?;
    let mut v: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .collect();
    v.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    if v.is_empty() {
        bail!("이미지 폴더에 클래스 하위 폴더가 없습니다: {}", dir.display());
    }
    Ok(v)
}

/// 숫자를 값으로 비교하는 정렬 — 폴더 이름이 `0`, `1`, `2`, `10` 일 때 바이트 순서(`0,1,10,2`)를 피한다.
///
/// 클래스 인덱스는 이 순서로 매겨진다. 폴더 이름을 그대로 클래스 번호로 쓰는 사용자가
/// 인덱스가 밀린 채 학습하는 일을 막는다.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                if x.is_ascii_digit() && y.is_ascii_digit() {
                    let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                        let mut s = String::new();
                        while it.peek().is_some_and(|c| c.is_ascii_digit()) {
                            s.push(it.next().expect("peek 했다"));
                        }
                        s
                    };
                    let (xs, ys) = (take(&mut ai), take(&mut bi));
                    // 자릿수가 아주 길면 수로 못 바꾸므로 앞 0 을 떼고 길이·사전 순으로 비교한다.
                    let (xt, yt) = (xs.trim_start_matches('0'), ys.trim_start_matches('0'));
                    let ord = xt
                        .len()
                        .cmp(&yt.len())
                        .then_with(|| xt.cmp(yt))
                        .then_with(|| xs.cmp(&ys));
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                } else {
                    let ord = x.cmp(&y);
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                    ai.next();
                    bi.next();
                }
            }
        }
    }
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
    let img = decode_image_file(path)?;
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
    // 프로젝트 파일이 정한 목표 형상도 상한 안이어야 한다 — 디코더 한도는 이쪽을 보지 않는다.
    crate::limits::check_image_size(w as u32, h as u32, "이미지 입력 형상")?;
    checked_elems(&[c, h, w], "이미지 샘플")?;
    let resized = if img.width() as usize != w || img.height() as usize != h {
        img.resize_exact(w as u32, h as u32, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let data = if c == 1 {
        resized
            .to_luma8()
            .into_raw()
            .into_iter()
            .map(|v| v as f32 / 255.0)
            .collect::<Vec<f32>>()
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

fn load_image_folder(dir: &Path, hint: Option<&[usize]>, limit: Option<usize>) -> Result<(Vec<Sample>, DatasetInfo)> {
    let classes = class_dirs(dir)?;
    let mut shape: Option<Vec<usize>> = hint.map(|h| h.to_vec());
    let mut out = Vec::new();
    let mut names = Vec::with_capacity(classes.len());

    // 미리보기처럼 limit 이 있으면 클래스마다 같은 수만큼 뽑아 첫 클래스로 쏠리지 않게 한다.
    let per_class = limit.map(|l| l.div_ceil(classes.len().max(1)));
    let mut buckets: Vec<Vec<Sample>> = Vec::with_capacity(classes.len());

    for (ci, (name, cdir)) in classes.iter().enumerate() {
        names.push(name.clone());
        let mut bucket = Vec::new();
        for f in image_files(cdir)? {
            if per_class.is_some_and(|p| bucket.len() >= p) {
                break;
            }
            let t = load_image(&f, shape.as_deref())?;
            if shape.is_none() {
                shape = Some(t.shape.clone());
            }
            bucket.push(Sample {
                input: t,
                target: HostTensor::new(vec![1], vec![ci as f32]),
            });
        }
        buckets.push(bucket);
    }

    if limit.is_some() {
        // 클래스를 번갈아 가며 채운다 — 어느 클래스가 몇 장이든 고르게 섞인다.
        let deepest = buckets.iter().map(|b| b.len()).max().unwrap_or(0);
        for i in 0..deepest {
            for b in buckets.iter_mut() {
                if i < b.len() {
                    out.push(b[i].clone());
                }
            }
        }
        if let Some(l) = limit {
            out.truncate(l);
        }
    } else {
        for b in buckets {
            out.extend(b);
        }
    }
    if out.is_empty() {
        bail!("이미지 폴더에서 읽은 파일이 없습니다: {}", dir.display());
    }
    let empty_classes = empty_class_indices(names.len(), target_classes(&out));
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: shape.unwrap_or_default(),
        target_shape: vec![1],
        classes: names,
        samples_estimated: false,
        empty_classes,
    };
    Ok((out, info))
}

// ───────────────────────────── 녹화 폴더 ─────────────────────────────

#[derive(serde::Deserialize)]
struct LabelLine {
    frame: String,
    label: i64,
}

/// 라벨별로 고르게 `limit` 개 줄을 고른다 (돌아가며 한 줄씩). 파싱이 안 되는 줄은 그대로 남겨
/// 본 적재에서 오류가 나게 한다 — 미리보기가 조용히 건너뛰면 문제를 못 본다.
fn balanced_label_lines(text: &str, limit: usize) -> std::collections::BTreeSet<usize> {
    use std::collections::BTreeMap;
    let mut by_label: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    let mut broken: Vec<usize> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<LabelLine>(line) {
            Ok(rec) => by_label.entry(rec.label).or_default().push(i),
            Err(_) => broken.push(i),
        }
    }
    let mut out: std::collections::BTreeSet<usize> = broken.into_iter().take(limit).collect();
    let deepest = by_label.values().map(|v| v.len()).max().unwrap_or(0);
    'fill: for k in 0..deepest {
        for lines in by_label.values() {
            if out.len() >= limit {
                break 'fill;
            }
            if let Some(&i) = lines.get(k) {
                out.insert(i);
            }
        }
    }
    out
}

/// `labels.jsonl` 의 `frame` 을 `frames/` 바로 아래의 단일 파일 이름으로만 받아들인다.
///
/// 이 파일은 외부에서 온 데이터다. `..` 이나 절대 경로를 그대로 `join` 하면 폴더 밖 파일을 학습
/// 데이터로 끌어올 수 있고, 실패 메시지가 파일 존재 여부를 알려 주는 신호가 된다.
fn safe_frame_name(name: &str) -> Result<&str> {
    if name.is_empty() {
        bail!("frame 이름이 비어 있습니다");
    }
    let p = Path::new(name);
    if p.is_absolute() {
        bail!("frame '{name}' 은 절대 경로입니다 — frames/ 아래 파일 이름만 쓸 수 있습니다");
    }
    let mut parts = p.components();
    let only = match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(c)), None) => c,
        _ => bail!("frame '{name}' 은 frames/ 바로 아래의 파일 이름 하나여야 합니다"),
    };
    // Windows 에서 "a:b" 같은 이름이 드라이브로 해석되는 것도 막는다.
    if only.to_str() != Some(name) {
        bail!("frame '{name}' 에 쓸 수 없는 문자가 있습니다");
    }
    Ok(name)
}

fn load_recorded(dir: &Path, hint: Option<&[usize]>, limit: Option<usize>) -> Result<(Vec<Sample>, DatasetInfo)> {
    let labels_path = dir.join("labels.jsonl");
    let frames_dir = dir.join("frames");
    check_file_size(&labels_path, MAX_LABELS_BYTES, "라벨 파일")?;
    let text = std::fs::read_to_string(&labels_path)
        .with_context(|| format!("라벨 파일 읽기 실패: {}", labels_path.display()))?;

    let mut shape: Option<Vec<usize>> = hint.map(|h| h.to_vec());
    let mut out = Vec::new();
    let mut max_label = 0i64;

    // limit 이 있으면 라벨별로 고르게 뽑는다. 줄 파싱은 싸고 이미지 디코드가 비싸므로
    // **디코드하기 전에** 어떤 줄을 쓸지 먼저 정한다.
    let chosen: Option<std::collections::BTreeSet<usize>> = limit.map(|l| balanced_label_lines(&text, l));

    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if chosen.as_ref().is_some_and(|c| !c.contains(&i)) {
            continue;
        }
        let rec: LabelLine = serde_json::from_str(line)
            .with_context(|| format!("{} {}줄을 읽을 수 없습니다", labels_path.display(), i + 1))?;
        if rec.label < 0 {
            bail!(
                "{} {}줄: 라벨은 0 이상이어야 합니다 (지금 {})",
                labels_path.display(),
                i + 1,
                rec.label
            );
        }
        max_label = max_label.max(rec.label);
        let name = safe_frame_name(&rec.frame).with_context(|| format!("{} {}줄", labels_path.display(), i + 1))?;
        let f = frames_dir.join(name);
        let t = load_image(&f, shape.as_deref())?;
        if shape.is_none() {
            shape = Some(t.shape.clone());
        }
        out.push(Sample {
            input: t,
            target: HostTensor::new(vec![1], vec![rec.label as f32]),
        });
    }
    if out.is_empty() {
        bail!("녹화 폴더에서 읽은 프레임이 없습니다: {}", dir.display());
    }
    // 라벨은 0..=max 로 잡되(연속 인덱스 유지), 프레임이 하나도 없는 라벨은 따로 알린다.
    let names: Vec<String> = (0..=max_label).map(|i| i.to_string()).collect();
    let empty_classes = empty_class_indices(names.len(), target_classes(&out));
    let info = DatasetInfo {
        samples: out.len(),
        input_shape: shape.unwrap_or_default(),
        target_shape: vec![1],
        classes: names,
        samples_estimated: false,
        empty_classes,
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
        let img = image::GrayImage::from_fn(w, h, |x, y| image::Luma([base.wrapping_add((x * 7 + y * 13) as u8)]));
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
        assert!(samples
            .iter()
            .all(|s| s.input.data.iter().all(|v| (0.0..=1.0).contains(v))));
        // alpha = 0, zeta = 1 (정렬 순).
        assert_eq!(samples[0].target.data, vec![0.0]);
        assert_eq!(samples[3].target.data, vec![1.0]);

        // 힌트가 없으면 첫 이미지 크기를 쓴다.
        let (_, info2) = load_image_folder(&dir, None, None).unwrap();
        assert_eq!(info2.input_shape, vec![1, 4, 6]);

        // scan 은 디코드 없이 개수를 세고 첫 장으로 형상을 잡는다.
        let scanned = scan_source(
            &DataSource::ImageFolder {
                path: dir.to_string_lossy().into(),
            },
            Path::new("."),
            None,
        )
        .unwrap();
        assert_eq!(scanned.samples, 4);
        assert_eq!(scanned.input_shape, vec![1, 4, 6]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recorded_folder_reports_labels_with_no_frames() {
        let dir = tmp("recorded-gap");
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        write_png(&frames.join("a.png"), 4, 4, 0);
        write_png(&frames.join("b.png"), 4, 4, 64);
        // 라벨 0 과 3 만 쓰인다 → 1, 2 는 비어 있다.
        std::fs::write(
            dir.join("labels.jsonl"),
            "{\"frame\":\"a.png\",\"label\":0}\n{\"frame\":\"b.png\",\"label\":3}\n",
        )
        .unwrap();

        let (_, info) = load_recorded(&dir, None, None).unwrap();
        assert_eq!(
            info.classes,
            vec!["0", "1", "2", "3"],
            "클래스는 0..=max 를 유지해야 한다"
        );
        assert_eq!(info.empty_classes, vec![1, 2]);
        let w = info.empty_class_warning().expect("경고 문장");
        assert!(w.contains('1') && w.contains('2'), "{w}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_folder_reports_empty_class_dirs() {
        let dir = tmp("imgfolder-empty");
        for name in ["cat", "dog", "ghost"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        write_png(&dir.join("cat/1.png"), 4, 4, 0);
        write_png(&dir.join("dog/1.png"), 4, 4, 90);
        // ghost 폴더는 비어 있다.

        let (_, loaded) = load_image_folder(&dir, None, None).unwrap();
        assert_eq!(loaded.classes, vec!["cat", "dog", "ghost"]);
        assert_eq!(loaded.empty_classes, vec![2]);

        // scan 도 파일을 열지 않고 같은 답을 내야 한다.
        let scanned = scan_source(
            &DataSource::ImageFolder {
                path: dir.to_string_lossy().into(),
            },
            Path::new("."),
            None,
        )
        .unwrap();
        assert_eq!(scanned.empty_classes, vec![2]);
        assert_eq!(scanned.samples, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn csv_infers_classes_only_when_targets_look_categorical() {
        let dir = tmp("csv-classes");
        let p = dir.join("c.csv");

        // 정수 라벨 0, 1, 3 → 클래스 0..=3, 2 는 비어 있다.
        std::fs::write(&p, "x,y\n0.5,0\n0.6,1\n0.7,3\n").unwrap();
        let (_, info) = load_csv(&p, &["x".into()], &["y".into()], true, None).unwrap();
        assert_eq!(info.classes, vec!["0", "1", "2", "3"]);
        assert_eq!(info.empty_classes, vec![2]);

        // 실수 타깃은 회귀 — 클래스를 만들지 않는다.
        std::fs::write(&p, "x,y\n0.5,1.5\n0.6,2.5\n").unwrap();
        let (_, info) = load_csv(&p, &["x".into()], &["y".into()], true, None).unwrap();
        assert!(info.classes.is_empty() && info.empty_classes.is_empty());

        // 타깃이 여러 열이어도 회귀.
        std::fs::write(&p, "x,a,b\n0.5,0,1\n0.6,1,0\n").unwrap();
        let (_, info) = load_csv(&p, &["x".into()], &["a".into(), "b".into()], true, None).unwrap();
        assert!(info.classes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn synthetic_has_no_empty_classes() {
        for kind in SyntheticKind::ALL {
            let (_, info) = synthetic(kind, 200).unwrap();
            assert!(
                info.empty_classes.is_empty(),
                "{kind:?} 에 빈 클래스: {:?}",
                info.empty_classes
            );
        }
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
    fn class_dirs_sort_numerically_not_bytewise() {
        // 바이트 순이면 0,1,10,11,2 — 폴더 이름을 클래스 번호로 쓰는 사용자가 밀린 인덱스로 학습한다.
        let mut v = ["10", "2", "0", "11", "1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["0", "1", "2", "10", "11"]);

        // 접두사가 있어도 숫자 부분을 값으로 본다.
        let mut w = ["class10", "class2", "class1"];
        w.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(w, ["class1", "class2", "class10"]);

        // 숫자가 없으면 평범한 사전 순.
        let mut x = ["dog", "cat", "bird"];
        x.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(x, ["bird", "cat", "dog"]);

        // 앞 0 은 값으로는 같다 — 그래도 전순서라야 하므로 어느 한쪽으로 확정되고 대칭이어야 한다.
        let a = natural_cmp("007", "7");
        assert_ne!(
            a,
            std::cmp::Ordering::Equal,
            "서로 다른 이름이 같은 순위를 가지면 정렬이 불안정하다"
        );
        assert_eq!(natural_cmp("7", "007"), a.reverse());
        // 값이 다르면 앞 0 과 무관하게 값 순서를 따른다.
        assert_eq!(natural_cmp("007", "10"), std::cmp::Ordering::Less);
    }

    #[test]
    fn recorded_rejects_frame_names_that_escape_the_folder() {
        for bad in ["../../../etc/hosts", "/etc/hosts", "sub/dir.png", "..", ""] {
            assert!(safe_frame_name(bad).is_err(), "'{bad}' 를 받아들이면 안 됩니다");
        }
        assert_eq!(safe_frame_name("000001.png").unwrap(), "000001.png");
    }

    #[test]
    fn recorded_folder_rejects_traversal_in_labels() {
        let dir = tmp("recorded-escape");
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        write_png(&frames.join("a.png"), 4, 4, 0);
        // 폴더 밖 파일을 가리키는 줄.
        std::fs::write(
            dir.join("labels.jsonl"),
            "{\"frame\":\"a.png\",\"label\":0}\n{\"frame\":\"../../secret.png\",\"label\":1}\n",
        )
        .unwrap();

        let e = format!("{:#}", load_recorded(&dir, None, None).unwrap_err());
        assert!(e.contains("frames/"), "경계 위반을 알려야 합니다: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn csv_column_errors_name_the_user_spec() {
        let dir = tmp("csv-err");
        let p = dir.join("c.csv");
        std::fs::write(&p, "a,b\n1,x\n").unwrap();
        // 값이 수가 아니면 사용자가 쓴 열 이름이 메시지에 있어야 한다.
        let e = format!(
            "{:#}",
            load_csv(&p, &["a".into()], &["b".into()], true, None).unwrap_err()
        );
        assert!(e.contains("'b'"), "사용자가 쓴 열 이름이 없습니다: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_image_shape_hint_is_rejected() {
        let dir = tmp("img-bomb");
        std::fs::create_dir_all(dir.join("c0")).unwrap();
        write_png(&dir.join("c0/a.png"), 8, 8, 0);
        // 프로젝트 파일이 정한 목표 형상이 터무니없으면 할당 전에 거절한다.
        let e = format!(
            "{:#}",
            load_image_folder(&dir, Some(&[3, 100_000, 100_000]), None).unwrap_err()
        );
        assert!(e.contains("상한"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_spreads_samples_across_classes() {
        let dir = tmp("preview-balance");
        for (ci, name) in ["a", "b", "c"].iter().enumerate() {
            let c = dir.join(name);
            std::fs::create_dir_all(&c).unwrap();
            for k in 0..10 {
                write_png(&c.join(format!("{k}.png")), 4, 4, (ci * 40 + k) as u8);
            }
        }
        let spec = DatasetSpec::new(
            "p",
            DataSource::ImageFolder {
                path: dir.to_string_lossy().into(),
            },
        );
        let got = preview(&spec, Path::new("."), 6).unwrap();
        assert_eq!(got.len(), 6);
        let mut seen: Vec<f32> = got.iter().map(|s| s.target.data[0]).collect();
        seen.sort_by(|a, b| a.partial_cmp(b).unwrap());
        seen.dedup();
        assert_eq!(seen, vec![0.0, 1.0, 2.0], "미리보기가 첫 클래스에 쏠렸습니다");

        // 클래스 수보다 적게 요청해도 서로 다른 클래스에서 온다.
        let two = preview(&spec, Path::new("."), 2).unwrap();
        assert_eq!(two.len(), 2);
        assert_ne!(two[0].target.data[0], two[1].target.data[0]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recorded_preview_spreads_across_labels() {
        let dir = tmp("preview-recorded");
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        let mut labels = String::new();
        // 라벨 0 이 앞에 몰려 있다 — 앞에서 자르면 라벨 1 이 안 보인다.
        for k in 0..8 {
            write_png(&frames.join(format!("a{k}.png")), 4, 4, k as u8);
            labels.push_str(&format!("{{\"frame\":\"a{k}.png\",\"label\":0}}\n"));
        }
        for k in 0..8 {
            write_png(&frames.join(format!("b{k}.png")), 4, 4, (100 + k) as u8);
            labels.push_str(&format!("{{\"frame\":\"b{k}.png\",\"label\":1}}\n"));
        }
        std::fs::write(dir.join("labels.jsonl"), labels).unwrap();

        let spec = DatasetSpec::new(
            "r",
            DataSource::Recorded {
                path: dir.to_string_lossy().into(),
            },
        );
        let got = preview(&spec, Path::new("."), 4).unwrap();
        assert_eq!(got.len(), 4);
        assert!(got.iter().any(|s| s.target.data[0] == 0.0));
        assert!(
            got.iter().any(|s| s.target.data[0] == 1.0),
            "라벨 1 이 미리보기에 없습니다"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_csv_does_not_read_the_whole_file() {
        let dir = tmp("scan-csv");
        let p = dir.join("big.csv");

        // 상한보다 적으면 정확한 값이어야 한다.
        let mut small = String::from("a,y\n");
        for i in 0..50 {
            small.push_str(&format!("{}.5,1\n", i));
        }
        std::fs::write(&p, small).unwrap();
        let info = scan_csv(&p, &["a".into()], &["y".into()], true).unwrap();
        assert_eq!(info.samples, 50);
        assert!(!info.samples_estimated, "다 읽었으면 추정이 아니다");

        // 상한을 넘으면 추정치이되 실제와 크게 다르지 않아야 한다.
        let rows = CSV_SCAN_ROWS * 3;
        let mut big = String::from("a,y\n");
        for i in 0..rows {
            big.push_str(&format!("{}.5,1\n", i % 100));
        }
        std::fs::write(&p, big).unwrap();
        let info = scan_csv(&p, &["a".into()], &["y".into()], true).unwrap();
        assert!(info.samples_estimated, "표본만 읽었으면 추정이다");
        assert!(
            info.samples >= CSV_SCAN_ROWS,
            "표본 수보다 적을 수 없다: {}",
            info.samples
        );
        let err = (info.samples as f64 - rows as f64).abs() / rows as f64;
        assert!(err < 0.2, "추정 {} vs 실제 {rows} (오차 {err:.2})", info.samples);
        // 형상은 표본에서 정확히 나온다.
        assert_eq!(info.input_shape, vec![1]);
        assert_eq!(info.target_shape, vec![1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_does_not_claim_empty_classes_from_a_sample() {
        let dir = tmp("scan-classes");
        let p = dir.join("c.csv");
        // 앞 표본에는 라벨 0 만, 뒤쪽에 라벨 1 이 나온다.
        let mut text = String::from("a,y\n");
        for i in 0..(CSV_SCAN_ROWS + 500) {
            let label = if i < CSV_SCAN_ROWS { 0 } else { 1 };
            text.push_str(&format!("{}.5,{label}\n", i % 100));
        }
        std::fs::write(&p, text).unwrap();
        let info = scan_csv(&p, &["a".into()], &["y".into()], true).unwrap();
        // 표본만 보고 "클래스 1 은 비어 있다" 고 단정하면 안 된다.
        assert!(
            info.empty_classes.is_empty(),
            "표본 기반으로 빈 클래스를 단정했습니다: {:?}",
            info.empty_classes
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn csv_reads_named_and_numbered_columns() {
        let dir = std::env::temp_dir().join(format!("nl-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.csv");
        std::fs::write(&p, "a,b,y\n1,2,3\n4,5,6\n").unwrap();

        let by_name = load_csv(&p, &["a".into(), "b".into()], &["y".into()], true, None)
            .unwrap()
            .0;
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[0].input.data, vec![1.0, 2.0]);
        assert_eq!(by_name[1].target.data, vec![6.0]);

        let by_index = load_csv(&p, &["0".into(), "1".into()], &["2".into()], true, None)
            .unwrap()
            .0;
        assert_eq!(by_index, by_name);

        std::fs::write(&p, "1,2,3\n4,5,6\n").unwrap();
        let no_header = load_csv(&p, &["0".into(), "1".into()], &["2".into()], false, None)
            .unwrap()
            .0;
        assert_eq!(no_header.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }
}
