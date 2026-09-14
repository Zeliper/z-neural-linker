//! 페이로드 Transform 실행: 바깥 값 ↔ 텐서.
//!
//! 인코드는 `FieldKind` 가 정하는 자연 표현에서 시작해 `field.encode` 를 차례로 적용하고, 마지막에
//! 배치 1 텐서(`[1, …]`)로 만든다. 디코드는 그 반대로 텐서에서 시작해 `field.decode` 를 적용한다.
//! 결과 형상은 `Field::tensor_shape()` 와 일치한다.

use crate::limits::{check_image_size, checked_elems, decode_image, MAX_CLASSES, MAX_TOKENS};
use crate::tensor::HostTensor;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use nl_core::payload::{Field, FieldKind, Transform};

/// 엔진 경계의 바깥 값.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Numbers(Vec<f32>),
    Text(String),
    Json(serde_json::Value),
    /// RGBA8, 행 우선.
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Tensor(HostTensor),
}

/// 인코드 중간 표현. 이미지 전용 변환(Resize/Grayscale/Crop)이 형상을 알아야 해서 따로 둔다.
#[derive(Clone, Debug)]
enum Mid {
    /// `[c, h, w]` 행 우선, 값 범위는 원본 그대로(0..255).
    Img {
        c: usize,
        h: usize,
        w: usize,
        data: Vec<f32>,
    },
    Vec {
        shape: Vec<usize>,
        data: Vec<f32>,
    },
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
    let mut mid = start(field, value).with_context(|| format!("필드 '{}' 인코딩 시작 실패", field.name))?;
    for (i, t) in field.encode.iter().enumerate() {
        mid = apply_encode(mid, t).with_context(|| format!("필드 '{}' 의 encode[{i}] ({t:?}) 실패", field.name))?;
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
    let mut cur = Dec::Nums {
        shape: sample_shape,
        data,
    };
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
        FieldKind::Image {
            width,
            height,
            channels,
        } => {
            let (w, h, c) = (*width, *height, *channels);
            if c != 1 && c != 3 {
                bail!("이미지 채널은 1 또는 3 만 지원합니다 (지금 {c})");
            }
            check_image_size(w as u32, h as u32, "Image 필드")?;
            image_start(value, w, h, c)
        }
        FieldKind::Tensor { shape, .. } => {
            let data = numbers(value)?;
            let want: usize = shape.iter().product();
            if data.len() != want {
                bail!(
                    "Tensor 필드 형상 {shape:?} 은 원소 {want} 개인데 {} 개를 받았습니다",
                    data.len()
                );
            }
            Ok(Mid::Vec {
                shape: shape.clone(),
                data,
            })
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
            Ok(Mid::Vec {
                shape: vec![*len],
                data,
            })
        }
        FieldKind::ClassLabel { labels } => {
            let idx = match value {
                Value::Text(s) => labels
                    .iter()
                    .position(|l| l == s)
                    .with_context(|| format!("라벨 '{s}' 이 목록에 없습니다"))?,
                Value::Number(n) => *n as usize,
                Value::Numbers(v) if v.len() == 1 => v[0] as usize,
                other => bail!(
                    "ClassLabel 필드에는 Text 나 Number 가 필요합니다 (지금 {})",
                    kind_of(other)
                ),
            };
            if idx >= labels.len() {
                bail!("클래스 인덱스 {idx} 가 라벨 수 {} 를 넘습니다", labels.len());
            }
            let mut v = vec![0.0f32; labels.len()];
            v[idx] = 1.0;
            Ok(Mid::Vec {
                shape: vec![labels.len()],
                data: v,
            })
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

/// `Image` 필드가 받아들이는 값들을 `[c, h, w]` 평면 f32 로 바꾼다.
///
/// 배포 앱의 HTTP 추론 API 는 JSON 만 받으므로 이미지가 여러 모습으로 들어온다.
///
/// - [`Value::Image`] — RGBA8 버퍼. 크기가 다르면 필드 크기로 리샘플링한다.
/// - [`Value::Tensor`] — 랭크 3 이면 자기 형상을 그대로 쓴다 (체인의 `Resize` 가 맞춰도 된다).
/// - [`Value::Numbers`] 와 숫자만 있는 [`Value::Json`] — **평탄하든 중첩이든 `[c][h][w]` 순서**로
///   읽는다. 값은 건드리지 않는다 (0..1 인지 0..255 인지는 `Scale` 변환이 정한다).
/// - 문자열 [`Value::Json`] — `data:image/...;base64,…` 또는 순수 base64 PNG/JPEG.
fn image_start(value: &Value, w: usize, h: usize, c: usize) -> Result<Mid> {
    match value {
        Value::Image {
            width: iw,
            height: ih,
            rgba,
        } => {
            let data = rgba_to_planar(*iw as usize, *ih as usize, rgba, w, h, c)?;
            Ok(Mid::Img { c, h, w, data })
        }
        Value::Tensor(t) if t.shape.len() == 3 => {
            let want = checked_elems(&t.shape, "이미지 텐서")?;
            if want != t.data.len() {
                bail!(
                    "이미지 텐서 형상 {:?} 은 원소 {want} 개인데 데이터는 {} 개입니다",
                    t.shape,
                    t.data.len()
                );
            }
            Ok(Mid::Img {
                c: t.shape[0],
                h: t.shape[1],
                w: t.shape[2],
                data: t.data.clone(),
            })
        }
        Value::Numbers(v) => planar_from_flat(v, w, h, c, "Numbers", None),
        Value::Json(serde_json::Value::String(text)) => {
            let bytes = image_bytes_from_str(text)?;
            let img = decode_image(&bytes, "Image 필드의 base64 이미지")?;
            let rgba = img.to_rgba8();
            let (iw, ih) = (rgba.width() as usize, rgba.height() as usize);
            let data = rgba_to_planar(iw, ih, rgba.as_raw(), w, h, c)?;
            Ok(Mid::Img { c, h, w, data })
        }
        Value::Json(j) => {
            let nesting = json_nesting(j);
            // 중첩이 3 단이면 순서까지 본다. [h][w][c](채널 마지막)는 원소 수가 같아 개수 검사로는
            // 절대 잡히지 않는데, 그대로 받으면 픽셀이 뒤섞인 채 조용히 학습·추론된다.
            if nesting.len() == 3 && nesting != [c, h, w] {
                if nesting == [h, w, c] {
                    bail!(
                        "Image 필드는 [채널][높이][너비] = [{c}][{h}][{w}] 순서가 필요한데 \
                         받은 배열은 [{h}][{w}][{c}] 로 채널이 마지막입니다"
                    );
                }
                bail!(
                    "Image 필드는 [채널][높이][너비] = [{c}][{h}][{w}] 중첩이 필요한데 \
                     받은 배열은 {nesting:?} 입니다"
                );
            }
            let flat = json_numbers(j)?;
            planar_from_flat(&flat, w, h, c, "JSON 숫자 배열", Some(nesting))
        }
        other => bail!(
            "Image 필드에는 Image · 랭크 3 Tensor · 숫자 배열 · base64 이미지 문자열 중 하나가 필요합니다 (지금 {})",
            kind_of(other)
        ),
    }
}

/// 평탄한 값 목록을 `[c, h, w]` 로 받아들인다. 개수가 다르면 기대 형상을 담아 거절한다.
fn planar_from_flat(
    values: &[f32],
    w: usize,
    h: usize,
    c: usize,
    source: &str,
    nesting: Option<Vec<usize>>,
) -> Result<Mid> {
    let want = checked_elems(&[c, h, w], "Image 필드")?;
    if values.len() != want {
        let mut msg = format!(
            "Image 필드는 [{c}, {h}, {w}](채널·높이·너비 순) = {want} 개 값이 필요한데 \
             {source} 은 {} 개를 줬습니다",
            values.len()
        );
        if let Some(dims) = nesting {
            if !dims.is_empty() {
                msg.push_str(&format!(" (받은 중첩 {dims:?})"));
            }
        }
        bail!("{msg}");
    }
    Ok(Mid::Img {
        c,
        h,
        w,
        data: values.to_vec(),
    })
}

/// 중첩 배열의 각 단계 길이 (첫 원소 기준). 숫자면 빈 벡터.
fn json_nesting(j: &serde_json::Value) -> Vec<usize> {
    let mut dims = Vec::new();
    let mut cur = j;
    while let serde_json::Value::Array(a) = cur {
        dims.push(a.len());
        match a.first() {
            Some(next) => cur = next,
            None => break,
        }
        if dims.len() > 8 {
            break;
        }
    }
    dims
}

/// PNG/JPEG 매직 바이트.
fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// 문자열에서 이미지 바이트를 꺼낸다 (`data:` URL 또는 순수 base64).
fn image_bytes_from_str(text: &str) -> Result<Vec<u8>> {
    // base64 는 3 바이트를 4 글자로 늘린다 — 디코드 결과가 상한을 넘을 문자열은 아예 받지 않는다.
    let max_chars = (crate::limits::MAX_IMAGE_ALLOC / 3 * 4) as usize;
    if text.len() > max_chars {
        bail!("base64 이미지 문자열이 {} 자로 너무 깁니다", text.len());
    }

    let (payload, declared) = match text.strip_prefix("data:") {
        Some(rest) => {
            let (meta, data) = rest
                .split_once(',')
                .context("data URL 에 쉼표가 없습니다 (data:image/png;base64,… 형식)")?;
            if !meta.contains("base64") {
                bail!("data URL 이 base64 가 아닙니다 (meta: '{meta}')");
            }
            (data, true)
        }
        None => (text, false),
    };

    let cleaned: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    if cleaned.is_empty() {
        bail!("base64 이미지 문자열이 비어 있습니다");
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = match engine.decode(cleaned.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            // URL-safe 알파벳도 한 번 시도한다.
            match base64::engine::general_purpose::URL_SAFE.decode(cleaned.as_bytes()) {
                Ok(b) => b,
                Err(_) if declared => bail!("data URL 의 base64 를 읽을 수 없습니다: {e}"),
                Err(_) => bail!(
                    "Image 필드의 문자열을 이미지로 읽을 수 없습니다 — \
                     'data:image/png;base64,…' 또는 base64 로 인코딩한 PNG/JPEG 여야 합니다"
                ),
            }
        }
    };
    if !looks_like_image(&bytes) {
        bail!(
            "base64 를 풀었지만 PNG 도 JPEG 도 아닙니다 (앞 바이트: {:02X?})",
            &bytes[..bytes.len().min(8)]
        );
    }
    Ok(bytes)
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
    check_image_size(iw as u32, ih as u32, "들어온 이미지")?;
    check_image_size(w as u32, h as u32, "목표 이미지")?;
    let need = checked_elems(&[iw, ih, 4], "RGBA 버퍼")?;
    if rgba.len() < need {
        bail!(
            "RGBA 버퍼가 {}×{} 에 비해 짧습니다 ({} 바이트, {need} 필요)",
            iw,
            ih,
            rgba.len()
        );
    }
    checked_elems(&[c, h, w], "목표 이미지")?;
    let img = image::RgbaImage::from_raw(iw as u32, ih as u32, rgba[..need].to_vec())
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
            check_image_size(*width as u32, *height as u32, "Resize 목표")?;
            let out = resample(&data, c, h, w, *height, *width)?;
            Ok(Mid::Img {
                c,
                h: *height,
                w: *width,
                data: out,
            })
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
            // 릴리스 빌드의 usize 덧셈은 조용히 래핑한다 — checked_add 로 검사를 통과시키지 않는다.
            let right = x.checked_add(*width).context("Crop 의 x + width 가 넘칩니다")?;
            let bottom = y.checked_add(*height).context("Crop 의 y + height 가 넘칩니다")?;
            if right > w || bottom > h {
                bail!("Crop 영역 ({x},{y},{width},{height}) 이 이미지 {w}×{h} 밖입니다");
            }
            if *width == 0 || *height == 0 {
                bail!("Crop 크기는 1 이상이어야 합니다 (지금 {width}×{height})");
            }
            let n = checked_elems(&[c, *height, *width], "Crop 결과")?;
            let mut out = vec![0.0f32; n];
            for ch in 0..c {
                for row in 0..*height {
                    let src = ch * h * w + (y + row) * w + x;
                    let dst = ch * height * width + row * width;
                    out[dst..dst + width].copy_from_slice(&data[src..src + width]);
                }
            }
            Ok(Mid::Img {
                c,
                h: *height,
                w: *width,
                data: out,
            })
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
            check_classes(*classes)?;
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
            Ok(Mid::Vec {
                shape: vec![*classes],
                data: v,
            })
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
            Ok(Mid::Vec {
                shape: vec![1],
                data: vec![idx as f32],
            })
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
            Ok(Mid::Vec {
                shape: vec![*max_len],
                data: tokenize(&text, vocab, *max_len)?,
            })
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
    if max_len > MAX_TOKENS {
        bail!("Tokenize 의 max_len {max_len} 이 상한 {MAX_TOKENS} 를 넘습니다");
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
        Mid::Img {
            c: shape[0],
            h: shape[1],
            w: shape[2],
            data,
        }
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
    let (channels, per) = if shape.len() == 3 {
        (shape[0], shape[1] * shape[2])
    } else {
        (1, data.len())
    };
    let pick = |i: usize, v: &[f32]| if v.len() == 1 { v[0] } else { v[i % v.len()] };
    if mean.len() != 1 && mean.len() != channels {
        bail!(
            "Normalize mean 길이 {} 가 채널 {} 과 맞지 않습니다",
            mean.len(),
            channels
        );
    }
    if std.len() != 1 && std.len() != channels {
        bail!("Normalize std 길이 {} 가 채널 {} 과 맞지 않습니다", std.len(), channels);
    }
    for ch in 0..channels {
        let (m, s) = (pick(ch, mean), pick(ch, std));
        for i in 0..per {
            let idx = ch * per + i;
            data[idx] = if inverse {
                data[idx] * s + m
            } else {
                (data[idx] - m) / s
            };
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
        Transform::Softmax => Dec::Nums {
            shape,
            data: softmax(&data),
        },
        Transform::Argmax => Dec::Nums {
            shape: vec![1],
            data: vec![argmax(&data) as f32],
        },
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
            // 단일 원소는 뜻이 갈린다. 라벨이 2 개면 이진 분류기의 로짓/확률로 보고 임계 판정하고,
            // 그 밖에는 이미 Argmax 를 거친 인덱스로 본다.
            let idx = if data.len() == 1 {
                if labels.len() == 2 {
                    binary_index(data[0])
                } else {
                    let v = data[0];
                    if v < 0.0 || v.fract() != 0.0 {
                        bail!(
                            "MapLabel 이 받은 단일 값 {v} 를 클래스 인덱스로 볼 수 없습니다 — \
                             Argmax 를 먼저 넣거나, 이진 분류라면 라벨을 2 개로 두십시오"
                        );
                    }
                    v as usize
                }
            } else {
                argmax(&data)
            };
            let name = labels.get(idx).with_context(|| {
                format!(
                    "클래스 인덱스 {idx} 에 해당하는 라벨이 없습니다 (라벨 {} 개)",
                    labels.len()
                )
            })?;
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
            check_classes(*classes)?;
            // 디코드 쪽 OneHot 은 인덱스를 one-hot 벡터로 펼친다.
            let idx = if data.len() == 1 {
                data[0] as usize
            } else {
                argmax(&data)
            };
            if idx >= *classes {
                bail!("OneHot 인덱스 {idx} 가 클래스 수 {classes} 를 넘습니다");
            }
            let mut v = vec![0.0f32; *classes];
            v[idx] = 1.0;
            Dec::Nums {
                shape: vec![*classes],
                data: v,
            }
        }
        Transform::Resize { .. } | Transform::Grayscale | Transform::Crop { .. } => {
            bail!("이미지 변환({t:?})은 디코드에 쓸 수 없습니다")
        }
        Transform::JsonPointer { .. } => bail!("JsonPointer 는 인코드 전용입니다"),
        Transform::Tokenize { .. } => bail!("Tokenize 는 인코드 전용입니다"),
    })
}

// ───────────────────────────── 수치 도우미 ─────────────────────────────

/// 단일 로짓/확률의 이진 판정.
///
/// 값이 `0..=1` 안이면 확률로 보고 0.5, 아니면 로짓으로 보고 0 을 임계로 쓴다
/// (시그모이드가 0.5 를 넘는 지점이 로짓 0 이라 두 규칙은 서로 어긋나지 않는다).
fn binary_index(v: f32) -> usize {
    let positive = if (0.0..=1.0).contains(&v) { v >= 0.5 } else { v >= 0.0 };
    positive as usize
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .fold(
            (0usize, f32::NEG_INFINITY),
            |m, (i, &x)| if x > m.1 { (i, x) } else { m },
        )
        .0
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
fn resample(data: &[f32], c: usize, h: usize, w: usize, nh: usize, nw: usize) -> Result<Vec<f32>> {
    // h/w 가 0 이면 아래 `h - 1` 이 언더플로한다 (릴리스에서는 조용히 래핑해 인덱싱에서 터진다).
    if h == 0 || w == 0 || nh == 0 || nw == 0 {
        bail!("리샘플링 크기가 0 입니다 ({w}×{h} → {nw}×{nh})");
    }
    let need = checked_elems(&[c, h, w], "리샘플링 입력")?;
    if data.len() < need {
        bail!("리샘플링 입력이 [{c}, {h}, {w}] 에 비해 짧습니다 ({} 개)", data.len());
    }
    if h == nh && w == nw {
        return Ok(data.to_vec());
    }
    let out_n = checked_elems(&[c, nh, nw], "리샘플링 결과")?;
    let mut out = vec![0.0f32; out_n];
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
    Ok(out)
}

/// 클래스 수가 상한 안인지. 프로젝트 파일의 값이 그대로 할당 크기가 되므로 반드시 막는다.
fn check_classes(classes: usize) -> Result<()> {
    if classes == 0 {
        bail!("클래스 수는 1 이상이어야 합니다");
    }
    if classes > MAX_CLASSES {
        bail!("클래스 수 {classes} 가 상한 {MAX_CLASSES} 를 넘습니다");
    }
    Ok(())
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
        Value::Image {
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn image_encode_matches_declared_tensor_shape() {
        let p = PayloadSpec::image_classifier("c", 8, 6, vec!["a".into(), "b".into()]);
        let f = &p.inputs[0];
        let t = encode(f, &checkerboard(16, 12)).unwrap();
        assert_eq!(t.shape, vec![1, 3, 6, 8]);
        assert_eq!(f.tensor_shape(), Some(vec![3, 6, 8]));
        assert!(
            t.data.iter().all(|v| (0.0..=1.0).contains(v)),
            "Scale 이 0..1 로 만들어야 한다"
        );
    }

    #[test]
    fn resize_grayscale_crop_chain_follows_field_shape() {
        let mut f = Field::new(
            "img",
            FieldKind::Image {
                width: 16,
                height: 16,
                channels: 3,
            },
        );
        f.encode = vec![
            Transform::Resize { width: 8, height: 8 },
            Transform::Grayscale,
            Transform::Crop {
                x: 1,
                y: 1,
                width: 4,
                height: 4,
            },
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
        f.encode = vec![Transform::JsonPointer {
            pointer: "/data/values".into(),
        }];
        let j = serde_json::json!({"data": {"values": [1.0, 2.0, 3.0]}});
        let t = encode(&f, &Value::Json(j)).unwrap();
        assert_eq!(t.shape, vec![1, 3]);
        assert_eq!(t.data, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn text_tokenizes_into_a_padded_index_tensor() {
        let mut f = Field::new("t", FieldKind::Text);
        f.encode = vec![Transform::Tokenize {
            vocab: "abc".into(),
            max_len: 5,
        }];
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
        f.encode = vec![Transform::Tokenize {
            vocab: String::new(),
            max_len: 4,
        }];
        assert!(encode(&f, &Value::Text("x".into())).is_err(), "빈 vocab");

        f.encode = vec![Transform::Tokenize {
            vocab: "ab".into(),
            max_len: 0,
        }];
        assert!(encode(&f, &Value::Text("x".into())).is_err(), "max_len 0");

        let mut g = Field::new("v", FieldKind::Vector { len: 2 });
        g.encode = vec![Transform::Tokenize {
            vocab: "ab".into(),
            max_len: 2,
        }];
        assert!(
            encode(&g, &Value::Numbers(vec![1.0, 2.0])).is_err(),
            "Text 가 아닌 필드"
        );

        let mut h = Field::new("t", FieldKind::Text);
        h.decode = vec![Transform::Tokenize {
            vocab: "ab".into(),
            max_len: 2,
        }];
        assert!(
            decode(&h, &HostTensor::new(vec![1, 2], vec![1.0, 2.0])).is_err(),
            "디코드 전용 아님"
        );
    }

    // ── Image 입력의 여러 모습 (HTTP 추론 API) ──

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 8) as u8, (y * 8) as u8, 128]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn image_accepts_flat_and_nested_json_numbers() {
        let f = Field::new(
            "img",
            FieldKind::Image {
                width: 2,
                height: 2,
                channels: 1,
            },
        );
        // 평탄 배열.
        let flat = serde_json::json!([1.0, 2.0, 3.0, 4.0]);
        let t = encode(&f, &Value::Json(flat)).unwrap();
        assert_eq!(t.shape, vec![1, 1, 2, 2]);
        assert_eq!(t.data, vec![1.0, 2.0, 3.0, 4.0]);

        // [c][h][w] 중첩 — 같은 결과.
        let nested = serde_json::json!([[[1.0, 2.0], [3.0, 4.0]]]);
        assert_eq!(encode(&f, &Value::Json(nested)).unwrap().data, vec![1.0, 2.0, 3.0, 4.0]);

        // Numbers 도 같은 규칙. 값은 그대로 (Scale 이 없으면 0..255 도 그대로).
        let t = encode(&f, &Value::Numbers(vec![10.0, 20.0, 200.0, 255.0])).unwrap();
        assert_eq!(t.data, vec![10.0, 20.0, 200.0, 255.0]);
    }

    #[test]
    fn image_json_shape_mismatch_names_the_expected_shape() {
        let f = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 3,
                channels: 3,
            },
        );
        let e = format!(
            "{:#}",
            encode(&f, &Value::Json(serde_json::json!([1.0, 2.0]))).unwrap_err()
        );
        assert!(e.contains("[3, 3, 4]"), "기대 형상이 없습니다: {e}");
        assert!(e.contains("36"), "기대 원소 수가 없습니다: {e}");

        // 채널이 마지막인 배열은 원소 수가 같아 개수로는 못 잡는다 — 중첩 순서로 잡아야 한다.
        // 필드는 [c=3][h=2][w=4], 들어온 것은 [h=2][w=4][c=3] (둘 다 24 개).
        let g = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 2,
                channels: 3,
            },
        );
        let hwc = serde_json::json!([
            [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]],
            [[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]]
        ]);
        assert_eq!(json_nesting(&hwc), vec![2, 4, 3]);
        let e = format!("{:#}", encode(&g, &Value::Json(hwc)).unwrap_err());
        assert!(e.contains("채널이 마지막"), "채널 순서를 짚어 주지 않습니다: {e}");

        // 올바른 [c][h][w] 중첩은 통과한다.
        let chw = serde_json::json!([
            [[1.0, 1.0, 1.0, 1.0], [1.0, 1.0, 1.0, 1.0]],
            [[2.0, 2.0, 2.0, 2.0], [2.0, 2.0, 2.0, 2.0]],
            [[3.0, 3.0, 3.0, 3.0], [3.0, 3.0, 3.0, 3.0]]
        ]);
        let t = encode(&g, &Value::Json(chw)).unwrap();
        assert_eq!(t.shape, vec![1, 3, 2, 4]);
        assert_eq!(t.data[0], 1.0);
        assert_eq!(t.data[8], 2.0);
        assert_eq!(t.data[16], 3.0);
    }

    #[test]
    fn image_accepts_base64_png_with_and_without_data_url() {
        let mut f = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 4,
                channels: 3,
            },
        );
        f.encode = vec![Transform::Scale { min: 0.0, max: 255.0 }];
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes(8, 8));

        for text in [format!("data:image/png;base64,{b64}"), b64.clone()] {
            let t = encode(&f, &Value::Json(serde_json::Value::String(text))).unwrap();
            assert_eq!(t.shape, vec![1, 3, 4, 4], "8×8 PNG 가 필드 크기로 리샘플링되어야 한다");
            assert!(t.data.iter().all(|v| (0.0..=1.0).contains(v)));
        }

        // 줄바꿈이 섞인 base64 도 받아들인다.
        let wrapped = format!("{}\n{}", &b64[..b64.len() / 2], &b64[b64.len() / 2..]);
        assert!(encode(&f, &Value::Json(serde_json::Value::String(wrapped))).is_ok());
    }

    #[test]
    fn image_rejects_strings_that_are_not_images() {
        let f = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 4,
                channels: 3,
            },
        );
        // base64 로 풀리지만 이미지가 아니다.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello world hello world");
        let e = format!(
            "{:#}",
            encode(&f, &Value::Json(serde_json::Value::String(b64))).unwrap_err()
        );
        assert!(e.contains("PNG") && e.contains("JPEG"), "{e}");

        // base64 조차 아니다.
        let e = format!(
            "{:#}",
            encode(&f, &Value::Json(serde_json::Value::String("!!! not base64 !!!".into()))).unwrap_err()
        );
        assert!(e.contains("base64"), "{e}");

        // data URL 인데 base64 가 아니다.
        let e = format!(
            "{:#}",
            encode(&f, &Value::Json(serde_json::Value::String("data:image/png,raw".into()))).unwrap_err()
        );
        assert!(e.contains("base64"), "{e}");
    }

    // ── 리뷰 지적 (B6, S6) ──

    #[test]
    fn normalize_validates_std_length_too() {
        let mut f = Field::new(
            "img",
            FieldKind::Image {
                width: 2,
                height: 2,
                channels: 3,
            },
        );
        f.encode = vec![Transform::Normalize {
            mean: vec![0.0; 3],
            std: vec![1.0, 2.0],
        }];
        let e = format!("{:#}", encode(&f, &Value::Numbers(vec![0.0; 12])).unwrap_err());
        assert!(e.contains("std"), "{e}");

        // 길이 1(전체 적용)과 채널 수는 통과한다.
        f.encode = vec![Transform::Normalize {
            mean: vec![0.0],
            std: vec![2.0],
        }];
        assert!(encode(&f, &Value::Numbers(vec![4.0; 12])).is_ok());
        f.encode = vec![Transform::Normalize {
            mean: vec![0.0; 3],
            std: vec![1.0, 2.0, 4.0],
        }];
        assert!(encode(&f, &Value::Numbers(vec![4.0; 12])).is_ok());
    }

    #[test]
    fn map_label_handles_a_single_binary_output() {
        let labels = vec!["아니오".to_string(), "예".to_string()];
        let mut f = Field::new("c", FieldKind::ClassLabel { labels });
        f.decode = vec![Transform::MapLabel];

        // 확률 (0..1): 0.5 기준.
        assert_eq!(
            decode(&f, &HostTensor::new(vec![1, 1], vec![0.7])).unwrap(),
            Value::Text("예".into())
        );
        assert_eq!(
            decode(&f, &HostTensor::new(vec![1, 1], vec![0.3])).unwrap(),
            Value::Text("아니오".into())
        );
        // 로짓 (범위 밖): 0 기준.
        assert_eq!(
            decode(&f, &HostTensor::new(vec![1, 1], vec![2.5])).unwrap(),
            Value::Text("예".into())
        );
        assert_eq!(
            decode(&f, &HostTensor::new(vec![1, 1], vec![-2.5])).unwrap(),
            Value::Text("아니오".into())
        );

        // 라벨이 3 개인데 단일 실수가 오면 모호하므로 오류로 알린다.
        let mut g = Field::new(
            "c",
            FieldKind::ClassLabel {
                labels: vec!["a".into(), "b".into(), "c".into()],
            },
        );
        g.decode = vec![Transform::MapLabel];
        let e = format!("{:#}", decode(&g, &HostTensor::new(vec![1, 1], vec![0.7])).unwrap_err());
        assert!(e.contains("Argmax"), "{e}");
        // 정수 인덱스는 그대로 통한다.
        assert_eq!(
            decode(&g, &HostTensor::new(vec![1, 1], vec![2.0])).unwrap(),
            Value::Text("c".into())
        );
    }

    // ── 보안 (M19): 악성 입력이 패닉이 아니라 Err ──

    #[test]
    fn zero_sized_resize_chain_errors_instead_of_panicking() {
        let mut f = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 4,
                channels: 1,
            },
        );
        f.encode = vec![
            Transform::Resize { width: 0, height: 0 },
            Transform::Resize { width: 4, height: 4 },
        ];
        assert!(encode(&f, &Value::Numbers(vec![0.0; 16])).is_err());
    }

    #[test]
    fn oversized_transform_parameters_are_rejected() {
        let mut f = Field::new(
            "img",
            FieldKind::Image {
                width: 4,
                height: 4,
                channels: 1,
            },
        );
        // 거대 Resize — 할당을 시도하기 전에 거절해야 한다.
        f.encode = vec![Transform::Resize {
            width: 100_000,
            height: 100_000,
        }];
        assert!(encode(&f, &Value::Numbers(vec![0.0; 16])).is_err());

        // Crop 의 x + width 가 usize 를 넘는다.
        f.encode = vec![Transform::Crop {
            x: usize::MAX,
            y: 0,
            width: 4,
            height: 4,
        }];
        assert!(encode(&f, &Value::Numbers(vec![0.0; 16])).is_err());

        // 거대 OneHot.
        let mut g = Field::new("s", FieldKind::Scalar);
        g.encode = vec![Transform::OneHot { classes: 4_000_000_000 }];
        assert!(encode(&g, &Value::Number(0.0)).is_err());

        // 거대 Tokenize.
        let mut h = Field::new("t", FieldKind::Text);
        h.encode = vec![Transform::Tokenize {
            vocab: "ab".into(),
            max_len: usize::MAX,
        }];
        assert!(encode(&h, &Value::Text("a".into())).is_err());
    }

    #[test]
    fn image_field_with_absurd_dimensions_is_rejected() {
        let f = Field::new(
            "img",
            FieldKind::Image {
                width: 100_000,
                height: 100_000,
                channels: 3,
            },
        );
        let e = format!("{:#}", encode(&f, &Value::Numbers(vec![0.0; 4])).unwrap_err());
        assert!(e.contains("상한"), "{e}");
    }

    #[test]
    fn mismatched_tensor_image_is_rejected() {
        let f = Field::new(
            "img",
            FieldKind::Image {
                width: 2,
                height: 2,
                channels: 1,
            },
        );
        // 형상은 [1,2,2](4 개)인데 데이터가 2 개뿐 — 릴리스에서도 잡아야 한다.
        let bad = HostTensor {
            shape: vec![1, 2, 2],
            data: vec![1.0, 2.0],
        };
        assert!(encode(&f, &Value::Tensor(bad)).is_err());
    }

    #[test]
    fn threshold_and_normalize_decode() {
        let mut f = Field::new("v", FieldKind::Vector { len: 3 });
        f.decode = vec![Transform::Threshold { value: 0.5 }];
        let t = HostTensor::new(vec![1, 3], vec![0.2, 0.9, 0.5]);
        assert_eq!(decode(&f, &t).unwrap(), Value::Numbers(vec![0.0, 1.0, 1.0]));

        let mut g = Field::new(
            "i",
            FieldKind::Tensor {
                shape: vec![1, 2, 2],
                dtype: Default::default(),
            },
        );
        g.encode = vec![Transform::Normalize {
            mean: vec![0.5],
            std: vec![0.25],
        }];
        let enc = encode(&g, &Value::Numbers(vec![0.5, 0.75, 0.25, 0.5])).unwrap();
        assert_eq!(enc.data, vec![0.0, 1.0, -1.0, 0.0]);
    }
}
