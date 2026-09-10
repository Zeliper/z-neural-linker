//! `GuiLayout` 렌더러. 런타임은 `RenderMode::Run`, 빌더 디자이너는 `RenderMode::Design` 으로 같은 함수를 부른다.

use nl_core::{GuiLayout, WidgetId};
use nl_engine::Value;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
    /// 위젯이 실제로 동작하고 이벤트를 낸다.
    Run,
    /// 선택·이동·크기 조절 핸들을 그리고 상호작용은 편집용으로만.
    Design,
}

/// 위젯이 낸 이벤트 (버튼 클릭, 슬라이더 변경, 텍스트 확정, 토글).
#[derive(Clone, Debug, PartialEq)]
pub enum GuiEvent {
    Clicked(WidgetId),
    Changed(WidgetId, Value),
    /// Design 모드: 선택 변경.
    Selected(Option<WidgetId>),
    /// Design 모드: 이동/크기 변경 확정 (`rect` 새 값).
    Moved(WidgetId, [f32; 4]),
}

/// 프레임 사이에 유지되는 위젯 상태 (슬라이더 값·텍스트·표시할 값·플롯 히스토리·이미지 텍스처).
#[derive(Default)]
pub struct GuiState {
    pub values: BTreeMap<WidgetId, Value>,
    pub texts: BTreeMap<WidgetId, String>,
    pub history: BTreeMap<WidgetId, Vec<f64>>,
    pub selected: Option<WidgetId>,
    pub textures: BTreeMap<WidgetId, egui::TextureHandle>,
}

impl GuiState {
    /// 파이프라인 싱크(`Sink::GuiWidget`)에서 온 값을 반영한다.
    pub fn push_value(&mut self, widget: WidgetId, value: Value, max_points: usize) {
        if let Value::Number(n) = &value {
            let h = self.history.entry(widget).or_default();
            h.push(*n);
            if h.len() > max_points {
                let cut = h.len() - max_points;
                h.drain(..cut);
            }
        }
        self.values.insert(widget, value);
    }
}

/// `ui` 영역 안에 레이아웃을 절대 좌표로 그린다. 돌려주는 이벤트는 호출자가 파이프라인/편집기에 전달한다.
pub fn render_layout(ui: &mut egui::Ui, layout: &GuiLayout, state: &mut GuiState, mode: RenderMode) -> Vec<GuiEvent> {
    let _ = (ui, layout, state, mode);
    vec![]
}
