//! 데이터셋 스펙. 실제 로딩은 `nl-engine::data`.

use crate::ids::{DatasetId, PayloadId};
use serde::{Deserialize, Serialize};

/// 데이터 소스.
///
/// # 모델 Input 이 여럿일 때
///
/// 샘플의 입력은 하나의 평탄한 텐서다. 모델 Input 이 여럿이면 엔진이 이를 `Graph::input_nodes()`
/// 순서(이름 → id)대로 각 Input 의 원소 수만큼 앞에서부터 잘라 넣는다.
/// `Csv` 라면 `input_cols` 가 그 순서를 정한다 — 예를 들어 Input 이 `[2]`, `[3]` 이면
/// `input_cols` 는 5 개여야 하고 앞 2 개가 첫 Input, 뒤 3 개가 두 번째 Input 으로 간다.
/// Input 이 하나면 데이터 형상이 그 Input 형상과 **정확히** 같아야 한다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DataSource {
    /// CSV. 열 이름(헤더) 또는 0 기반 번호 문자열.
    /// 다입력 모델에서는 `input_cols` 순서가 Input 노드 순서로 매핑된다(위 설명 참고).
    Csv { path: String, input_cols: Vec<String>, target_cols: Vec<String>, #[serde(default = "yes")] header: bool },
    /// `path/<class>/*.png|jpg`. 클래스 = 하위 폴더 이름(정렬 순).
    ImageFolder { path: String },
    /// 빌더의 녹화 기능이 만든 폴더(`frames/*.png` + `labels.jsonl`).
    Recorded { path: String },
    /// 합성 데이터 (테스트·튜토리얼).
    Synthetic { kind: SyntheticKind, samples: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyntheticKind {
    /// 2 입력 → 1 출력 (0/1).
    Xor,
    /// 2D 두 나선 분류.
    Spirals,
    /// y = 3x₁ − 2x₂ + 0.5 + 잡음.
    LinearRegression,
    /// 8×8 이미지 안의 밝은 사각형 위치 4분면 분류.
    Quadrants,
}

impl SyntheticKind {
    pub const ALL: [SyntheticKind; 4] =
        [SyntheticKind::Xor, SyntheticKind::Spirals, SyntheticKind::LinearRegression, SyntheticKind::Quadrants];
    pub fn label(&self) -> &'static str {
        match self {
            SyntheticKind::Xor => "XOR",
            SyntheticKind::Spirals => "두 나선",
            SyntheticKind::LinearRegression => "선형 회귀",
            SyntheticKind::Quadrants => "사분면 이미지",
        }
    }
    /// (입력 샘플 형상, 타깃 샘플 형상, 분류 클래스 수).
    pub fn shapes(&self) -> (Vec<usize>, Vec<usize>, Option<usize>) {
        match self {
            SyntheticKind::Xor => (vec![2], vec![1], Some(2)),
            SyntheticKind::Spirals => (vec![2], vec![1], Some(2)),
            SyntheticKind::LinearRegression => (vec![2], vec![1], None),
            SyntheticKind::Quadrants => (vec![1, 8, 8], vec![1], Some(4)),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Split {
    /// 학습 설정의 `val_split` 비율로 자른다.
    Ratio,
    /// 별도 검증 소스.
    Separate { validation: Box<DataSource> },
}

impl Default for Split {
    fn default() -> Self {
        Split::Ratio
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DatasetSpec {
    pub id: DatasetId,
    pub name: String,
    pub source: DataSource,
    /// 인코더 체인을 정하는 페이로드. 없으면 소스의 자연 표현(CSV 열 → f32, 이미지 → [C,H,W]/255).
    #[serde(default)]
    pub payload: Option<PayloadId>,
    #[serde(default)]
    pub split: Split,
    #[serde(default = "yes")]
    pub shuffle: bool,
    /// 최근 스캔 결과(샘플 수·클래스) — 캐시일 뿐 진실은 소스.
    #[serde(default)]
    pub cached_info: Option<DatasetInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct DatasetInfo {
    pub samples: usize,
    pub input_shape: Vec<usize>,
    pub target_shape: Vec<usize>,
    #[serde(default)]
    pub classes: Vec<String>,
}

impl DatasetSpec {
    pub fn new(name: impl Into<String>, source: DataSource) -> Self {
        Self { id: DatasetId::new(), name: name.into(), source, payload: None, split: Split::Ratio, shuffle: true, cached_info: None }
    }
}
