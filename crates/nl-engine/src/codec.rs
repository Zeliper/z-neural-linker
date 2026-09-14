//! 페이로드 Transform 실행: 바깥 값 ↔ 텐서.
//!
//! 인코드는 `FieldKind` 가 정하는 자연 표현에서 시작해 `field.encode` 를 차례로 적용하고, 마지막에
//! 배치 1 텐서(`[1, …]`)로 만든다. 디코드는 그 반대로 텐서에서 시작해 `field.decode` 를 적용한다.
//! 결과 형상은 `Field::tensor_shape()` 와 일치한다.

use crate::tensor::HostTensor;
use anyhow::{bail, Context, Result};
use nl_core::payload::{Field, FieldKind, Transform};

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

/// 인코드 중간 표현. 이미지 전용 변환(Resize/Grayscale/Crop)이 형상을 알아야 해서 따로 둔다.
#[derive(Clone, Debug)]
enum Mid {
    /// `[c, h, w]` 행 우선, 값 범위는 원본 그대로(0..255).
    Img { c: usize, h: usize, w: usize, data: Vec<f32> },
    Vec { shape: Vec<usize>, data: Vec<f32> },
    Json(serde_json::Value),
    /// `Tokenize` 가 정수 인덱스로 바꿔 주기를 기다리는 문자열.
    Text(String),
}

impl Mid {
    fn into_parts(self) -> Result<(Vec<usize>, Vec<f32>)> {
        match self {
            Mid::Img { c, h, w, data } => Ok((vec![c, h, w], data)),
            Mid::Vec { shape, data } => Ok((shape, data)),
            Mid::Json(_) => bail!("JSON 값은 JsonPointer 로 숫자를 꺼낸 뒤에야 텐서가 됩니다"),
            Mid::Text(_) => bail!("Text 값은 Tokenize 로 인덱스를 만든 뒤에야 텐서가 됩니다"),
        }
    }
}

/// `field.encode` 체인을 적용해 배치 1 텐서를 만든다.
pub fn encode(field: &Field, value: &Value) -> Result<HostTensor> {
    let mut mid = start(field, value)
        .with_context(|| format!("필드 '{}' 인코딩 시작 실패", field.name))?;
    for (i, t) in field.encode.iter().enumerate() {
        mid = apply_encode(mid, t)
            .with_context(|| format!("필드 '{}' 의 encode[{i}] ({t:?}) 실패", field.name))?;
    }
    let (shape, data) = mid.into_parts()?;
    let mut full = vec![1usize];
    full.extend(shape);
    Ok(HostTensor::new(full, data))
}

/// `field.decode` 체인을 적용해 바깥 값으로 (배치 1 가정, 배치 >1 이면 첫 샘플).
pub fn decode(field: &Field, tensor: &HostTensor) -> Result<Value> {
    // 배치 차원을 떼고 첫 샘플만 본다.
    let (sample_shape, data) = first_sample(tensor);
    let mut cur = Dec::Nums { shape: sample_shape, data };
    for (i, t) in field.decode.iter().enumerate() {
        cur = apply_decode(cur, t, field)
            .with_context(|| format!("필드 '{}' 의 decode[{i}] ({t:?}) 실패", field.name))?;
    }
    Ok(match cur {
        Dec::Text(s) => Value::Text(s),
        Dec::Nums { shape, data } => {
            if shape.iter().product::<usize>() == 1 {
                Value::Number(data.first().copied().unwrap_or(0.0) as f64)
            } else {
                Value::Numbers(data)
            }
        }
    })
}

fn first_sample(t: &HostTensor) -> (Vec<usize>, Vec<f32>) {
    if t.shape.len() <= 1 {
        return (t.shape.clone(), t.data.clone());
    }
    let per: usize = t.shape[1..].iter().product();
    (t.shape[1..].to_vec(), t.data.iter().take(per).copied().collect())
}

// ───────────────────────────── 인코드 ─────────────────────────────

fn start(field: &Field, value: &Value) -> Result<Mid> {
    match &field.kind {
        FieldKind::Image { width, height, channels } => {
            let (w, h, c) = (*width, *height, *channels);
            if c != 1 && c != 3 {
                bail!("이미지 채널은 1 또는 3 만 지원합니다 (지금 {c})");
            }
            match value {
                Value::Image { width: iw, height: ih, rgba } => {
                    let data = rgba_to_planar(*iw as usize, *ih as usize, rgba, w, h, c)?;
                    Ok(Mid::Img { c, h, w, data })
                }
                Value::Tensor(t) if t.shape.len() == 3 => {
                    Ok(Mid::Img { c: t.shape[0], h: t.shape[1], w: t.shape[2], data: t.data.clone() })
                }
                other => bail!("Image 필드에는 Image 값이 필요합니다 (지금 {})", kind_of(other)),
            }
        }
        FieldKind::Tensor { shape, .. } => {
            let data = numbers(value)?;
            let want: usize = shape.iter().product();
            if data.len() != want {
                bail!("Tensor 필드 형상 {shape:?} 은 원소 {want} 개인데 {} 개를 받았습니다", data.len());
            }
            Ok(Mid::Vec { shape: shape.clone(), data })
        }
        FieldKind::Scalar => {
            let data = numbers(value)?;
            if data.len() != 1 {
                bail!("Scalar 필드에는 값 1 개가 필요합니다 (지금 {})", data.len());
            }
            Ok(Mid::Vec { shape: vec![1], data })
        }
        FieldKind::Vector { len } => {
            let data = numbers(value)?;
            if data.len() != *len {
                bail!("Vector 필드 길이 {len} 인데 {} 개를 받았습니다", data.len());
            }
            Ok(Mid::Vec { shape: vec![*len], data })
        }
        FieldKind::ClassLabel { labels } => {
            let idx = match value {
                Value::Text(s) => labels
                    .iter()
                    .position(|l| l == s)
                    .with_context(|| format!("라벨 '{s}' 이 목록에 없습니다"))?,
                Value::Number(n) => *n as usize,
                Value::Numbers(v) if v.len() == 1 => v[0] as usize,
                other => bail!("ClassLabel 필드에는 Text 나 Number 가 필요합니다 (지금 {})", kind_of(other)),
            };
            if idx >= labels.len() {
                bail!("클래스 인덱스 {idx} 가 라벨 수 {} 를 넘습니다", labels.len());
            }
            let mut v = vec![0.0f32; labels.len()];
            v[idx] = 1.0;
            Ok(Mid::Vec { shape: vec![labels.len()], data: v })
        }
        FieldKind::Json => match value {
            Value::Json(j) => Ok(Mid::Json(j.clone())),
            other => bail!("Json 필드에는 Json 값이 필요합니다 (지금 {})", kind_of(other)),
        },
        FieldKind::Text => match value {
            Value::Text(t) => Ok(Mid::Text(t.clone())),
            Value::Number(n) => Ok(Mid::Text(n.to_string())),
            Value::Json(serde_json::Value::String(t)) => Ok(Mid::Text(t.clone())),
            other => bail!("Text 필드에는 Text 값이 필요합니다 (지금 {})", kind_of(other)),
        },
    }
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Number(_) => "Number",
        Value::Numbers(_) => "Numbers",
        Value::Text(_) => "Text",
        Value::Json(_) => "Json",
        Value::Image { .. } => "Image",
        Value::Tensor(_) => "Tensor",
    }
}

fn numbers(v: &Value) -> Result<Vec<f32>> {
    Ok(match v {
        Value::Number(n) => vec![*n as f32],
        Value::Numbers(v) => v.clone(),
        Value::Tensor(t) => t.data.clone(),
        Value::Json(j) => json_numbers(j)?,
        other => bail!("숫자로 바꿀 수 없는 값입니다 ({})", kind_of(other)),
    })
}

fn json_numbers(j: &serde_json::Value) -> Result<Vec<f32>> {
    match j {
        serde_json::Value::Number(n) => Ok(vec![n.as_f64().context("JSON 수를 f64 로 읽을 수 없음")? as f32]),
        serde_json::Value::Bool(b) => Ok(vec![if *b { 1.0 } else { 0.0 }]),
        serde_json::Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                out.extend(json_numbers(v)?);
            }
            Ok(out)
        }
        serde_json::Value::String(s) => Ok(vec![s
            .trim()
            .parse::<f32>()
            .with_context(|| format!("JSON 문자열 '{s}' 을 수로 읽을 수 없습니다"))?]),
        other => bail!("JSON 값 {other} 을 숫자로 바꿀 수 없습니다"),
    }
}

/// RGBA8 → `[c, h, w]` (0..255 f32). 필요하면 최근접 리샘플링으로 크기를 맞춘다.
fn rgba_to_planar(iw: usize, ih: usize, rgba: &[u8], w: usize, h: usize, c: usize) -> Result<Vec<f32>> {
    if iw == 0 || ih == 0 {
        bail!("이미지 크기가 0 입니다");
    }
    if rgba.len() < iw * ih * 4 {
        bail!("RGBA 버퍼가 {}×{} 에 비해 짧습니다 ({} 바이트)", iw, ih, rgba.len());
    }
    let img = image::RgbaImage::from_raw(iw as u32, ih as u32, rgba[..iw * ih * 4].to_vec())
        .context("RGBA 버퍼를 이미지로 만들 수 없습니다")?;
    let dyn_img = image::DynamicImage::ImageRgba8(img);
    let resized = if iw != w || ih != h {
        dyn_img.resize_exact(w as u32, h as u32, image::imageops::FilterType::Triangle)
    } else {
        dyn_img
    };
    Ok(if c == 1 {
        resized.to_luma8().into_raw().into_iter().map(|v| v as f32).collect()
    } else {
        let rgb = resized.to_rgb8();
        let raw = rgb.as_raw();
        let mut out = vec![0.0f32; 3 * h * w];
        for y in 0..h {
            for x in 0..w {
                for ch in 0..3 {
                    out[ch * h * w + y * w + x] = raw[(y * w + x) * 3 + ch] as f32;
                }
            }
        }
        out
    })
}

fn apply_encode(mid: Mid, t: &Transform) -> Result<Mid> {
    match t {
        Transform::Resize { width, height } => {
            let Mid::Img { c, h, w, data } = mid else {
                bail!("Resize 는 이미지 필드에만 쓸 수 있습니다");
            };
            let out = resample(&data, c, h, w, *height, *width);
            Ok(Mid::Img { c, h: *height, w: *width, data: out })
        }
        Transform::Grayscale => {
            let Mid::Img { c, h, w, data } = mid else {
                bail!("Grayscale 은 이미지 필드에만 쓸 수 있습니다");
            };
            if c == 1 {
                return Ok(Mid::Img { c, h, w, data });
            }
            if c != 3 {
                bail!("Grayscale 은 채널 3 에서만 쓸 수 있습니다 (지금 {c})");
            }
            let n = h * w;
            let mut out = vec![0.0f32; n];
            for i in 0..n {
                out[i] = 0.299 * data[i] + 0.587 * data[n + i] + 0.114 * data[2 * n + i];
            }
            Ok(Mid::Img { c: 1, h, w, data: out })
        }
        Transform::Crop { x, y, width, height } => {
            let Mid::Img { c, h, w, data } = mid else {
                bail!("Crop 은 이미지 필드에만 쓸 수 있습니다");
            };
            if x + width > w || y + height > h {
                bail!("Crop 영역 ({x},{y},{width},{height}) 이 이미지 {w}×{h} 밖입니다");
            }
            let mut out = vec![0.0f32; c * height * width];
            for ch in 0..c {
                for row in 0..*height {
                    let src = ch * h * w + (y + row) * w + x;
                    let dst = ch * height * width + row * width;
                    out[dst..dst + width].copy_from_slice(&data[src..src + width]);
                }
            }
            Ok(Mid::Img { c, h: *height, w: *width, data: out })
        }
        Transform::Normalize { mean, std } => {
            let (shape, mut data) = mid.into_parts()?;
            normalize(&mut data, &shape, mean, std, false)?;
            Ok(rebuild(shape, data))
        }
        Transform::Scale { min, max } => {
            let (shape, mut data) = mid.into_parts()?;
            if (max - min).abs() < f32::EPSILON {
                bail!("Scale 의 min 과 max 가 같습니다");
            }
            for v in data.iter_mut() {
                *v = (*v - min) / (max - min);
            }
            Ok(rebuild(shape, data))
        }
        Transform::OneHot { classes } => {
            let (_, data) = mid.into_parts()?;
            if data.len() != 1 {
                bail!("OneHot 은 스칼라에만 쓸 수 있습니다 (지금 {} 개)", data.len());
            }
            let idx = data[0] as usize;
            if idx >= *classes {
                bail!("OneHot 인덱스 {idx} 가 클래스 수 {classes} 를 넘습니다");
            }
            let mut v = vec![0.0f32; *classes];
            v[idx] = 1.0;
            Ok(Mid::Vec { shape: vec![*classes], data: v })
        }
        Transform::JsonPointer { pointer } => {
            let Mid::Json(j) = mid else {
                bail!("JsonPointer 는 Json 필드에만 쓸 수 있습니다");
            };
            let picked = j
                .pointer(pointer)
                .with_context(|| format!("JSON 포인터 '{pointer}' 가 가리키는 값이 없습니다"))?;
            let data = json_numbers(picked)?;
            let len = data.len();
            Ok(Mid::Vec { shape: vec![len], data })
        }
        Transform::Argmax => {
            let (_, data) = mid.into_parts()?;
            let idx = argmax(&data);
            Ok(Mid::Vec { shape: vec![1], data: vec![idx as f32] })
        }
        Transform::Softmax => {
            let (shape, data) = mid.into_parts()?;
            Ok(rebuild(shape, softmax(&data)))
        }
        Transform::Threshold { value } => {
            let (shape, mut data) = mid.into_parts()?;
            for v in data.iter_mut() {
                *v = if *v >= *value { 1.0 } else { 0.0 };
            }
            Ok(rebuild(shape, data))
        }
        Transform::Tokenize { vocab, max_len } => {
            let Mid::Text(text) = mid else {
                bail!("Tokenize 는 Text 필드에만 쓸 수 있습니다");
            };
            Ok(Mid::Vec { shape: vec![*max_len], data: tokenize(&text, vocab, *max_len)? })
        }
        Transform::MapLabel => bail!("MapLabel 은 디코드 전용입니다"),
    }
}

/// 문자 단위 토큰화. 인덱스는 1 부터 (0 = 패딩 겸 미지 문자), 길이는 `max_len` 으로 맞춘다.
fn tokenize(text: &str, vocab: &str, max_len: usize) -> Result<Vec<f32>> {
    if vocab.is_empty() {
        bail!("Tokenize 의 vocab 이 비어 있습니다");
    }
    if max_len == 0 {
        bail!("Tokenize 의 max_len 은 1 이상이어야 합니다");
    }
    let table: Vec<char> = vocab.chars().collect();
    let mut out = vec![0.0f32; max_len];
    for (slot, ch) in out.iter_mut().zip(text.chars()) {
        // 없는 문자는 0 (미지) 으로 둔다.
        *slot = table.iter().position(|&v| v == ch).map_or(0.0, |i| (i + 1) as f32);
    }
    Ok(out)
}

fn rebuild(shape: Vec<usize>, data: Vec<f32>) -> Mid {
    if shape.len() == 3 {
        Mid::Img { c: shape[0], h: shape[1], w: shape[2], data }
    } else {
        Mid::Vec { shape, data }
    }
}

/// 채널별 (x - mean) / std. `mean`/`std` 길이가 1 이면 전체에 적용. `inverse` 면 x * std + mean.
fn normalize(data: &mut [f32], shape: &[usize], mean: &[f32], std: &[f32], inverse: bool) -> Result<()> {
    if mean.is_empty() || std.is_empty() {
        bail!("Normalize 의 mean/std 가 비어 있습니다");
    }
    if std.iter().any(|s| s.abs() < f32::EPSILON) {
        bail!("Normalize 의 std 에 0 이 있습니다");
    }
    let (channels, per) = if shape.len() == 3 { (shape[0], shape[1] * shape[2]) } else { (1, data.len()) };
    let pick = |i: usize, v: &[f32]| if v.len() == 1 { v[0] } else { v[i % v.len()] };
    if mean.len() != 1 && mean.len() != channels {
        bail!("Normalize mean 길이 {} 가 채널 {} 과 맞지 않습니다", mean.len(), channels);
    }
    for ch in 0..channels {
        let (m, s) = (pick(ch, mean), pick(ch, std));
        for i in 0..per {
            let idx = ch * per + i;
            data[idx] = if inverse { data[idx] * s + m } else { (data[idx] - m) / s };
        }
    }
    Ok(())
}

// ───────────────────────────── 디코드 ─────────────────────────────

#[derive(Clone, Debug)]
enum Dec {
    Nums { shape: Vec<usize>, data: Vec<f32> },
    Text(String),
}

fn apply_decode(cur: Dec, t: &Transform, field: &Field) -> Result<Dec> {
    let (shape, mut data) = match cur {
        Dec::Nums { shape, data } => (shape, data),
        Dec::Text(_) => bail!("이미 문자열이 된 값에는 더 이상 변환을 적용할 수 없습니다"),
    };
    Ok(match t {
        Transform::Softmax => Dec::Nums { shape, data: softmax(&data) },
        Transform::Argmax => Dec::Nums { shape: vec![1], data: vec![argmax(&data) as f32] },
        Transform::Threshold { value } => {
            for v in data.iter_mut() {
                *v = if *v >= *value { 1.0 } else { 0.0 };
            }
            Dec::Nums { shape, data }
        }
        Transform::MapLabel => {
            let labels = match &field.kind {
                FieldKind::ClassLabel { labels } => labels,
                _ => bail!("MapLabel 은 ClassLabel 필드에만 쓸 수 있습니다"),
            };
            // 아직 Argmax 를 거치지 않았다면 여기서 최대 인덱스를 고른다.
            let idx = if data.len() == 1 { data[0] as usize } else { argmax(&data) };
            let name = labels
                .get(idx)
                .with_context(|| format!("클래스 인덱스 {idx} 에 해당하는 라벨이 없습니다 (라벨 {} 개)", labels.len()))?;
            Dec::Text(name.clone())
        }
        // 인코드의 역변환.
        Transform::Scale { min, max } => {
            for v in data.iter_mut() {
                *v = *v * (max - min) + min;
            }
            Dec::Nums { shape, data }
        }
        Transform::Normalize { mean, std } => {
            normalize(&mut data, &shape, mean, std, true)?;
            Dec::Nums { shape, data }
        }
        Transform::OneHot { classes } => {
            // 디코드 쪽 OneHot 은 인덱스를 one-hot 벡터로 펼친다.
            let idx = if data.len() == 1 { data[0] as usize } else { argmax(&data) };
            if idx >= *classes {
                bail!("OneHot 인덱스 {idx} 가 클래스 수 {classes} 를 넘습니다");
            }
            let mut v = vec![0.0f32; *classes];
            v[idx] = 1.0;
            Dec::Nums { shape: vec![*classes], data: v }
        }
        Transform::Resize { .. } | Transform::Grayscale | Transform::Crop { .. } => {
            bail!("이미지 변환({t:?})은 디코드에 쓸 수 없습니다")
        }
        Transform::JsonPointer { .. } => bail!("JsonPointer 는 인코드 전용입니다"),
        Transform::Tokenize { .. } => bail!("Tokenize 는 인코드 전용입니다"),
    })
}

// ───────────────────────────── 수치 도우미 ─────────────────────────────

fn argmax(v: &[f32]) -> usize {
    v.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |m, (i, &x)| if x > m.1 { (i, x) } else { m }).0
}

fn softmax(v: &[f32]) -> Vec<f32> {
    let max = v.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = v.iter().map(|x| (x - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum <= 0.0 {
        return vec![0.0; v.len()];
    }
    exps.into_iter().map(|e| e / sum).collect()
}

/// 중간 표현(f32 평면)의 이중선형 보간.
///
/// 소스 이미지의 첫 리사이즈는 `image` 크레이트(`resize_exact`, Triangle)가 맡는다. 다만 체인 중간의
/// `Transform::Resize` 는 앞선 Scale/Normalize 때문에 값이 0..255 를 벗어날 수 있어 u8 왕복을 할 수 없다.
fn resample(data: &[f32], c: usize, h: usize, w: usize, nh: usize, nw: usize) -> Vec<f32> {
    if h == nh && w == nw {
        return data.to_vec();
    }
    let mut out = vec![0.0f32; c * nh * nw];
    let sy = h as f32 / nh as f32;
    let sx = w as f32 / nw as f32;
    for ch in 0..c {
        for y in 0..nh {
            let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0);
            let y0 = fy.floor() as usize;
            let y1 = (y0 + 1).min(h - 1);
            let wy = fy - y0 as f32;
            for x in 0..nw {
                let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0);
                let x0 = fx.floor() as usize;
                let x1 = (x0 + 1).min(w - 1);
                let wx = fx - x0 as f32;
                let base = ch * h * w;
                let p00 = data[base + y0 * w + x0];
                let p01 = data[base + y0 * w + x1];
                let p10 = data[base + y1 * w + x0];
                let p11 = data[base + y1 * w + x1];
                let top = p00 + (p01 - p00) * wx;
                let bot = p10 + (p11 - p10) * wx;
                out[ch * nh * nw + y * nw + x] = top + (bot - top) * wy;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::payload::PayloadSpec;

    fn checkerboard(w: u32, h: u32) -> Value {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let on = (x + y) % 2 == 0;
                let v = if on { 255 } else { 0 };
                rgba.extend_from_slice(&[v, v / 2, 0, 255]);
            }
        }
        Value::Image { width: w, height: h, rgba }
    }

    #[test]
    fn image_encode_matches_declared_tensor_shape() {
        let p = PayloadSpec::image_classifier("c", 8, 6, vec!["a".into(), "b".into()]);
        let f = &p.inputs[0];
        let t = encode(f, &checkerboard(16, 12)).unwrap();
        assert_eq!(t.shape, vec![1, 3, 6, 8]);
        assert_eq!(f.tensor_shape(), Some(vec![3, 6, 8]));
        assert!(t.data.iter().all(|v| (0.0..=1.0).contains(v)), "Scale 이 0..1 로 만들어야 한다");
    }

    #[test]
    fn resize_grayscale_crop_chain_follows_field_shape() {
        let mut f = Field::new("img", FieldKind::Image { width: 16, height: 16, channels: 3 });
        f.encode = vec![
            Transform::Resize { width: 8, height: 8 },
            Transform::Grayscale,
            Transform::Crop { x: 1, y: 1, width: 4, height: 4 },
        ];
        let t = encode(&f, &checkerboard(16, 16)).unwrap();
        assert_eq!(t.shape, vec![1, 1, 4, 4]);
        assert_eq!(f.tensor_shape(), Some(vec![1, 4, 4]));
    }

    #[test]
    fn map_label_decodes_logits_to_name() {
        let p = PayloadSpec::image_classifier("c", 4, 4, vec!["고양이".into(), "개".into(), "새".into()]);
        let out = &p.outputs[0];
        let logits = HostTensor::new(vec![1, 3], vec![0.1, 2.5, -1.0]);
        assert_eq!(decode(out, &logits).unwrap(), Value::Text("개".into()));
    }

    #[test]
    fn scale_decode_is_the_inverse_of_encode() {
        let mut f = Field::new("v", FieldKind::Vector { len: 2 });
        f.encode = vec![Transform::Scale { min: 0.0, max: 10.0 }];
        f.decode = vec![Transform::Scale { min: 0.0, max: 10.0 }];
        let t = encode(&f, &Value::Numbers(vec![2.5, 7.5])).unwrap();
        assert_eq!(t.data, vec![0.25, 0.75]);
        assert_eq!(decode(&f, &t).unwrap(), Value::Numbers(vec![2.5, 7.5]));
    }

    #[test]
    fn json_pointer_extracts_numbers() {
        let mut f = Field::new("j", FieldKind::Json);
        f.encode = vec![Transform::JsonPointer { pointer: "/data/values".into() }];
        let j = serde_json::json!({"data": {"values": [1.0, 2.0, 3.0]}});
        let t = encode(&f, &Value::Json(j)).unwrap();
        assert_eq!(t.shape, vec![1, 3]);
        assert_eq!(t.data, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn text_tokenizes_into_a_padded_index_tensor() {
        let mut f = Field::new("t", FieldKind::Text);
        f.encode = vec![Transform::Tokenize { vocab: "abc".into(), max_len: 5 }];
        // 인덱스는 1 부터, 없는 문자('z')와 남는 자리는 0.
        let t = encode(&f, &Value::Text("cabz".into())).unwrap();
        assert_eq!(t.shape, vec![1, 5]);
        assert_eq!(t.data, vec![3.0, 1.0, 2.0, 0.0, 0.0]);
        assert_eq!(f.tensor_shape(), Some(vec![5]), "형상 계산이 Tokenize 를 반영해야 한다");

        // 긴 문자열은 잘린다.
        let t = encode(&f, &Value::Text("aaaaaaa".into())).unwrap();
        assert_eq!(t.data, vec![1.0; 5]);
    }

    #[test]
    fn text_without_tokenize_is_rejected() {
        let f = Field::new("t", FieldKind::Text);
        assert_eq!(f.tensor_shape(), None);
        let e = encode(&f, &Value::Text("hi".into())).unwrap_err().to_string();
        assert!(e.contains("Tokenize"), "{e}");
    }

    #[test]
    fn tokenize_rejects_bad_settings_and_wrong_fields() {
        let mut f = Field::new("t", FieldKind::Text);
        f.encode = vec![Transform::Tokenize { vocab: String::new(), max_len: 4 }];
        assert!(encode(&f, &Value::Text("x".into())).is_err(), "빈 vocab");

        f.encode = vec![Transform::Tokenize { vocab: "ab".into(), max_len: 0 }];
        assert!(encode(&f, &Value::Text("x".into())).is_err(), "max_len 0");

        let mut g = Field::new("v", FieldKind::Vector { len: 2 });
        g.encode = vec![Transform::Tokenize { vocab: "ab".into(), max_len: 2 }];
        assert!(encode(&g, &Value::Numbers(vec![1.0, 2.0])).is_err(), "Text 가 아닌 필드");

        let mut h = Field::new("t", FieldKind::Text);
        h.decode = vec![Transform::Tokenize { vocab: "ab".into(), max_len: 2 }];
        assert!(decode(&h, &HostTensor::new(vec![1, 2], vec![1.0, 2.0])).is_err(), "디코드 전용 아님");
    }

    #[test]
    fn threshold_and_normalize_decode() {
        let mut f = Field::new("v", FieldKind::Vector { len: 3 });
        f.decode = vec![Transform::Threshold { value: 0.5 }];
        let t = HostTensor::new(vec![1, 3], vec![0.2, 0.9, 0.5]);
        assert_eq!(decode(&f, &t).unwrap(), Value::Numbers(vec![0.0, 1.0, 1.0]));

        let mut g = Field::new("i", FieldKind::Tensor { shape: vec![1, 2, 2], dtype: Default::default() });
        g.encode = vec![Transform::Normalize { mean: vec![0.5], std: vec![0.25] }];
        let enc = encode(&g, &Value::Numbers(vec![0.5, 0.75, 0.25, 0.5])).unwrap();
        assert_eq!(enc.data, vec![0.0, 1.0, -1.0, 0.0]);
    }
}
