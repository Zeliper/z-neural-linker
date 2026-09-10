//! 페이로드 = 바깥 세계의 데이터 ↔ 모델 텐서 사이의 계약. 순수 데이터 정의이며 실행은 `nl-engine::codec`.

use crate::ids::PayloadId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Dtype {
    #[default]
    F32,
    I64,
    U8,
    Bool,
}

/// 필드의 바깥쪽 표현.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum FieldKind {
    /// 형상이 정해진 텐서 (샘플 형상).
    Tensor { shape: Vec<usize>, #[serde(default)] dtype: Dtype },
    /// 이미지. 인코더가 `[channels, h, w]` f32 로 만든다.
    Image { width: usize, height: usize, #[serde(default = "three")] channels: usize },
    Scalar,
    Vector { len: usize },
    /// 클래스 라벨 (정수 인덱스 ↔ 이름).
    ClassLabel { labels: Vec<String> },
    Text,
    /// 임의 JSON (파이프라인 소스/싱크 사이 전달용, 모델에 직접 넣을 수는 없음).
    Json,
}

fn three() -> usize {
    3
}

/// 인코드(바깥 → 텐서) / 디코드(텐서 → 바깥) 단계.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Transform {
    Resize { width: usize, height: usize },
    Grayscale,
    Crop { x: usize, y: usize, width: usize, height: usize },
    /// 채널별 (x - mean) / std. 길이 1 이면 전체에 적용.
    Normalize { mean: Vec<f32>, std: Vec<f32> },
    /// 선형 [min, max] → [0, 1].
    Scale { min: f32, max: f32 },
    OneHot { classes: usize },
    Argmax,
    Softmax,
    Threshold { value: f32 },
    /// 인덱스 → 라벨 이름 (ClassLabel 필드와 짝).
    MapLabel,
    /// JSON 포인터로 값 추출 (예: "/data/0/value").
    JsonPointer { pointer: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub kind: FieldKind,
    /// 바깥 → 텐서.
    #[serde(default)]
    pub encode: Vec<Transform>,
    /// 텐서 → 바깥.
    #[serde(default)]
    pub decode: Vec<Transform>,
}

impl Field {
    pub fn new(name: impl Into<String>, kind: FieldKind) -> Self {
        Self { name: name.into(), kind, encode: vec![], decode: vec![] }
    }

    /// 인코딩 후 모델에 들어가는 샘플 형상 (알 수 있을 때).
    pub fn tensor_shape(&self) -> Option<Vec<usize>> {
        let mut shape = match &self.kind {
            FieldKind::Tensor { shape, .. } => shape.clone(),
            FieldKind::Image { width, height, channels } => vec![*channels, *height, *width],
            FieldKind::Scalar => vec![1],
            FieldKind::Vector { len } => vec![*len],
            FieldKind::ClassLabel { labels } => vec![labels.len()],
            FieldKind::Text | FieldKind::Json => return None,
        };
        for t in &self.encode {
            match t {
                Transform::Resize { width, height } if shape.len() == 3 => {
                    shape[1] = *height;
                    shape[2] = *width;
                }
                Transform::Grayscale if shape.len() == 3 => shape[0] = 1,
                Transform::Crop { width, height, .. } if shape.len() == 3 => {
                    shape[1] = *height;
                    shape[2] = *width;
                }
                Transform::OneHot { classes } => shape = vec![*classes],
                _ => {}
            }
        }
        Some(shape)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PayloadSpec {
    pub id: PayloadId,
    pub name: String,
    #[serde(default)]
    pub inputs: Vec<Field>,
    #[serde(default)]
    pub outputs: Vec<Field>,
}

impl PayloadSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self { id: PayloadId::new(), name: name.into(), inputs: vec![], outputs: vec![] }
    }

    /// 분류 프리셋: 이미지 → 클래스.
    pub fn image_classifier(name: &str, w: usize, h: usize, labels: Vec<String>) -> Self {
        let mut p = Self::new(name);
        let mut img = Field::new("image", FieldKind::Image { width: w, height: h, channels: 3 });
        img.encode = vec![Transform::Scale { min: 0.0, max: 255.0 }];
        p.inputs.push(img);
        let mut out = Field::new("class", FieldKind::ClassLabel { labels });
        out.decode = vec![Transform::Softmax, Transform::Argmax, Transform::MapLabel];
        p.outputs.push(out);
        p
    }

    /// 표 데이터 프리셋: 벡터 → 벡터.
    pub fn tabular(name: &str, inputs: usize, outputs: usize) -> Self {
        let mut p = Self::new(name);
        p.inputs.push(Field::new("x", FieldKind::Vector { len: inputs }));
        p.outputs.push(Field::new("y", FieldKind::Vector { len: outputs }));
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_field_shape_follows_encode_chain() {
        let mut f = Field::new("img", FieldKind::Image { width: 640, height: 480, channels: 3 });
        f.encode = vec![Transform::Resize { width: 64, height: 32 }, Transform::Grayscale];
        assert_eq!(f.tensor_shape(), Some(vec![1, 32, 64]));
    }

    #[test]
    fn one_hot_overrides_shape() {
        let mut f = Field::new("y", FieldKind::Scalar);
        f.encode = vec![Transform::OneHot { classes: 10 }];
        assert_eq!(f.tensor_shape(), Some(vec![10]));
    }
}
