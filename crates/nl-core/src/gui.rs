//! 배포 앱의 GUI 레이아웃. 렌더러(`nl-app::gui_render`)는 빌더 미리보기와 런타임이 공유한다.

use crate::ids::{ModelId, PNodeId, WidgetId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub title: String,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub dark: bool,
}

impl Default for WindowSpec {
    fn default() -> Self {
        Self {
            title: "Neural Linker App".into(),
            width: 800.0,
            height: 600.0,
            dark: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WidgetKind {
    Label {
        text: String,
    },
    Button {
        text: String,
    },
    Toggle {
        text: String,
    },
    Slider {
        min: f32,
        max: f32,
        #[serde(default)]
        value: f32,
    },
    TextInput {
        #[serde(default)]
        hint: String,
    },
    /// 바인딩된 이미지(화면 캡처·모델 입력 미리보기) 표시.
    Image,
    /// 바인딩된 시계열 값 플롯.
    Plot {
        #[serde(default = "d_pts")]
        max_points: usize,
    },
    /// 바인딩된 값을 큰 글씨로 (분류 결과 등).
    Value {
        #[serde(default)]
        prefix: String,
    },
    Group {
        title: String,
    },
}

fn d_pts() -> usize {
    300
}

impl WidgetKind {
    pub fn palette() -> Vec<WidgetKind> {
        vec![
            WidgetKind::Label { text: "라벨".into() },
            WidgetKind::Button { text: "버튼".into() },
            WidgetKind::Toggle { text: "토글".into() },
            WidgetKind::Slider {
                min: 0.0,
                max: 1.0,
                value: 0.5,
            },
            WidgetKind::TextInput { hint: String::new() },
            WidgetKind::Image,
            WidgetKind::Plot { max_points: 300 },
            WidgetKind::Value { prefix: String::new() },
            WidgetKind::Group { title: "그룹".into() },
        ]
    }
    pub fn label(&self) -> &'static str {
        match self {
            WidgetKind::Label { .. } => "라벨",
            WidgetKind::Button { .. } => "버튼",
            WidgetKind::Toggle { .. } => "토글",
            WidgetKind::Slider { .. } => "슬라이더",
            WidgetKind::TextInput { .. } => "텍스트 입력",
            WidgetKind::Image => "이미지",
            WidgetKind::Plot { .. } => "플롯",
            WidgetKind::Value { .. } => "값",
            WidgetKind::Group { .. } => "그룹",
        }
    }
}

/// 위젯 ↔ 파이프라인/모델 연결.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Binding {
    /// 위젯 이벤트가 이 파이프라인 노드(`Source::GuiEvent`)로 들어간다.
    PipelineInput { node: PNodeId },
    /// 이 파이프라인 노드(`Sink::GuiWidget`)의 값이 위젯에 표시된다.
    PipelineOutput { node: PNodeId },
    /// 모델의 마지막 출력 필드.
    ModelOutput { model: ModelId, field: String },
    /// 내장 동작.
    Action { action: BuiltinAction },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuiltinAction {
    StartPipeline,
    StopPipeline,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Widget {
    pub id: WidgetId,
    pub kind: WidgetKind,
    /// 창 기준 논리 픽셀 `[x, y, w, h]`.
    pub rect: [f32; 4],
    #[serde(default)]
    pub binding: Option<Binding>,
    /// 그룹 위젯 안에 있으면 부모.
    #[serde(default)]
    pub parent: Option<WidgetId>,
    #[serde(default)]
    pub z: i32,
    /// 이 버전이 모르는 필드. 새 버전이 만든 문서를 열고 저장해도 그대로 돌려준다 (보안 리뷰 L3).
    ///
    /// `flatten` 이라 JSON 에서는 이 구조체의 필드와 같은 자리에 평평하게 놓인다. 비어 있으면
    /// 직렬화에도 나타나지 않으므로 기존 파일의 모양은 바뀌지 않는다.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Widget {
    pub fn new(kind: WidgetKind, rect: [f32; 4]) -> Self {
        Self {
            id: WidgetId::new(),
            kind,
            rect,
            binding: None,
            parent: None,
            z: 0,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct GuiLayout {
    #[serde(default)]
    pub window: WindowSpec,
    #[serde(default)]
    pub widgets: BTreeMap<WidgetId, Widget>,
}

impl GuiLayout {
    pub fn add(&mut self, w: Widget) -> WidgetId {
        let id = w.id;
        self.widgets.insert(id, w);
        id
    }
    /// z 순 → id 순 (결정적 그리기 순서).
    pub fn ordered(&self) -> Vec<&Widget> {
        let mut v: Vec<&Widget> = self.widgets.values().collect();
        v.sort_by(|a, b| a.z.cmp(&b.z).then(a.id.cmp(&b.id)));
        v
    }
}
