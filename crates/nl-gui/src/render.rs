//! `GuiLayout` 렌더러. 런타임은 `RenderMode::Run`, 빌더 디자이너는 `RenderMode::Design` 으로 같은 함수를 부른다.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2};
use nl_core::{GuiLayout, Widget, WidgetId, WidgetKind};
use nl_engine::Value;
use std::collections::BTreeMap;

/// 그룹 중첩 깊이 상한. 문서가 망가져 부모 사슬이 순환해도 여기서 멈춘다.
const MAX_NESTING: usize = 32;
/// 크기 조절 핸들 한 변 (논리 px).
const HANDLE: f32 = 8.0;
/// Design 모드에서 위젯이 가질 수 있는 최소 크기.
const MIN_W: f32 = 24.0;
const MIN_H: f32 = 16.0;

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
    let full = ui.max_rect();
    let order = draw_order(layout);
    let rects: BTreeMap<WidgetId, Rect> = order
        .iter()
        .map(|w| (w.id, window_rect(layout, w).translate(full.min.to_vec2())))
        .collect();

    // Design 모드: 빈 곳 클릭을 잡을 바닥 레이어를 위젯보다 먼저 등록한다 (나중에 등록한 쪽이 위에 온다).
    let background =
        (mode == RenderMode::Design).then(|| ui.interact(full, ui.id().with("nl_gui_design_bg"), Sense::click()));

    // 위젯 그리기. Design 모드에서는 위젯 자체 상호작용을 끈다.
    let mut builder = UiBuilder::new().max_rect(full).id_salt("nl_gui_widgets");
    if mode == RenderMode::Design {
        builder = builder.disabled();
    }
    let drawn = ui.scope_builder(builder, |ui| {
        let mut evs = Vec::new();
        for w in &order {
            let Some(rect) = rects.get(&w.id).copied() else {
                continue;
            };
            draw_widget(ui, w, rect, state, &mut evs);
        }
        evs
    });

    match mode {
        RenderMode::Run => drawn.inner,
        RenderMode::Design => design_overlay(ui, &order, &rects, state, background.as_ref()),
    }
}

// ───────────────────────────── 배치 ─────────────────────────────

/// 그리기 순서: 중첩 깊이 → z → id. 깊이를 먼저 보므로 그룹 배경이 항상 자식보다 아래에 깔린다.
fn draw_order(layout: &GuiLayout) -> Vec<&Widget> {
    let mut v = layout.ordered();
    v.sort_by(|a, b| {
        depth(layout, a)
            .cmp(&depth(layout, b))
            .then(a.z.cmp(&b.z))
            .then(a.id.cmp(&b.id))
    });
    v
}

fn depth(layout: &GuiLayout, w: &Widget) -> usize {
    let mut n = 0;
    let mut parent = w.parent;
    while let Some(pid) = parent {
        if n >= MAX_NESTING {
            break;
        }
        let Some(p) = layout.widgets.get(&pid) else { break };
        n += 1;
        parent = p.parent;
    }
    n
}

/// 창 기준 절대 좌표. 그룹 자식은 그룹 rect 를 원점으로 삼는다.
fn window_rect(layout: &GuiLayout, w: &Widget) -> Rect {
    let (mut x, mut y) = (w.rect[0], w.rect[1]);
    let mut parent = w.parent;
    for _ in 0..MAX_NESTING {
        let Some(pid) = parent else { break };
        let Some(p) = layout.widgets.get(&pid) else { break };
        x += p.rect[0];
        y += p.rect[1];
        parent = p.parent;
    }
    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w.rect[2].max(1.0), w.rect[3].max(1.0)))
}

// ───────────────────────────── 위젯 그리기 ─────────────────────────────

fn draw_widget(ui: &mut egui::Ui, w: &Widget, rect: Rect, state: &mut GuiState, out: &mut Vec<GuiEvent>) {
    match &w.kind {
        WidgetKind::Label { text } => {
            ui.put(rect, egui::Label::new(text).selectable(false));
        }
        WidgetKind::Button { text } => {
            if ui.put(rect, egui::Button::new(text)).clicked() {
                out.push(GuiEvent::Clicked(w.id));
            }
        }
        WidgetKind::Toggle { text } => {
            let mut on = matches!(state.values.get(&w.id), Some(Value::Number(n)) if *n != 0.0);
            if ui.put(rect, egui::Checkbox::new(&mut on, text)).changed() {
                let v = Value::Number(if on { 1.0 } else { 0.0 });
                state.values.insert(w.id, v.clone());
                out.push(GuiEvent::Changed(w.id, v));
            }
        }
        WidgetKind::Slider { min, max, value } => {
            let lo = *min as f64;
            let hi = (*max as f64).max(lo);
            let mut v = match state.values.get(&w.id) {
                Some(Value::Number(n)) => *n,
                _ => *value as f64,
            }
            .clamp(lo, hi);
            let changed = ui.put(rect, egui::Slider::new(&mut v, lo..=hi)).changed();
            state.values.insert(w.id, Value::Number(v));
            if changed {
                out.push(GuiEvent::Changed(w.id, Value::Number(v)));
            }
        }
        WidgetKind::TextInput { hint } => {
            let text = state.texts.entry(w.id).or_default();
            let resp = ui.put(rect, egui::TextEdit::singleline(text).hint_text(hint.as_str()));
            // Enter 로 확정 (포커스가 빠지면서 Enter 가 눌린 프레임).
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                out.push(GuiEvent::Changed(w.id, Value::Text(text.clone())));
            }
        }
        WidgetKind::Image => draw_image(ui, w.id, rect, state),
        WidgetKind::Plot { max_points } => draw_plot(ui, w.id, rect, state, *max_points),
        WidgetKind::Value { prefix } => {
            let text = format!("{prefix}{}", format_value(state.values.get(&w.id)));
            ui.put(
                rect,
                egui::Label::new(egui::RichText::new(text).size(20.0).strong()).selectable(false),
            );
        }
        WidgetKind::Group { title } => {
            let visuals = ui.visuals().clone();
            let painter = ui.painter();
            painter.rect(
                rect,
                CornerRadius::same(4),
                visuals.faint_bg_color,
                visuals.widgets.noninteractive.bg_stroke,
                StrokeKind::Inside,
            );
            if !title.is_empty() {
                painter.text(
                    rect.min + Vec2::new(8.0, 4.0),
                    Align2::LEFT_TOP,
                    title,
                    FontId::proportional(13.0),
                    visuals.text_color(),
                );
            }
        }
    }
}

fn draw_image(ui: &mut egui::Ui, id: WidgetId, rect: Rect, state: &mut GuiState) {
    if let Some(Value::Image { width, height, rgba }) = state.values.get(&id) {
        let size = [*width as usize, *height as usize];
        if size[0] > 0 && size[1] > 0 && rgba.len() == size[0] * size[1] * 4 {
            let fp_id = egui::Id::new(("nl_gui_img_fp", id));
            let fp = image_fingerprint(*width, *height, rgba);
            let size_changed = state.textures.get(&id).is_none_or(|t| t.size() != size);
            let content_changed = ui.ctx().data(|d| d.get_temp::<u64>(fp_id)) != Some(fp);
            if size_changed || content_changed {
                let img = egui::ColorImage::from_rgba_unmultiplied(size, rgba);
                if let Some(tex) = state.textures.get_mut(&id) {
                    tex.set(img, egui::TextureOptions::LINEAR);
                } else {
                    let tex =
                        ui.ctx()
                            .load_texture(format!("nl-gui-{}", id.short()), img, egui::TextureOptions::LINEAR);
                    state.textures.insert(id, tex);
                }
                ui.ctx().data_mut(|d| d.insert_temp(fp_id, fp));
            }
        }
    }
    match state.textures.get(&id) {
        Some(tex) => {
            ui.put(rect, egui::Image::from_texture(tex).shrink_to_fit());
        }
        None => placeholder(ui, rect, "이미지 없음"),
    }
}

/// 전체 바이트를 훑지 않고도 프레임이 바뀌었는지 알아내는 값싼 지문 (크기 + 일정 간격 표본).
fn image_fingerprint(width: u32, height: u32, rgba: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x100_0000_01b3);
    };
    mix(width as u64);
    mix(height as u64);
    mix(rgba.len() as u64);
    let stride = (rgba.len() / 4096).max(1);
    for (i, b) in rgba.iter().step_by(stride).enumerate() {
        mix((*b as u64) << (i % 8 * 8));
    }
    h
}

fn draw_plot(ui: &mut egui::Ui, id: WidgetId, rect: Rect, state: &GuiState, max_points: usize) {
    let points: Vec<[f64; 2]> = state
        .history
        .get(&id)
        .map(|h| {
            let start = h.len().saturating_sub(max_points.max(1));
            h[start..].iter().enumerate().map(|(i, v)| [i as f64, *v]).collect()
        })
        .unwrap_or_default();
    if points.is_empty() {
        placeholder(ui, rect, "값 없음");
        return;
    }
    ui.scope_builder(UiBuilder::new().max_rect(rect).id_salt(id), |ui| {
        egui_plot::Plot::new(id)
            .width(rect.width())
            .height(rect.height())
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .show_axes(true)
            .show(ui, |p| p.line(egui_plot::Line::new("값", points)));
    });
}

fn placeholder(ui: &egui::Ui, rect: Rect, text: &str) {
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(3),
        visuals.extreme_bg_color,
        Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(12.0),
        visuals.weak_text_color(),
    );
}

/// `Value` 를 위젯에 보여 줄 짧은 문자열로. 숫자는 소수 3자리, 벡터는 앞 8개.
pub fn format_value(value: Option<&Value>) -> String {
    match value {
        None => "—".into(),
        Some(Value::Number(n)) => format!("{n:.3}"),
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Numbers(v)) => {
            let head: Vec<String> = v.iter().take(8).map(|n| format!("{n:.3}")).collect();
            if v.len() > 8 {
                format!("[{}, …]", head.join(", "))
            } else {
                format!("[{}]", head.join(", "))
            }
        }
        Some(Value::Json(j)) => j.to_string(),
        Some(Value::Image { width, height, .. }) => format!("이미지 {width}×{height}"),
        Some(Value::Tensor(t)) => format!("텐서 {:?}", t.shape),
    }
}

// ───────────────────────────── Design 모드 ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Move,
    /// 모서리 번호. bit0 = 오른쪽, bit1 = 아래.
    Resize(u8),
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    widget: WidgetId,
    zone: Zone,
    /// 드래그 시작 시점의 위젯 `rect` (부모 기준 상대 좌표).
    orig: [f32; 4],
    accum: Vec2,
}

fn design_overlay(
    ui: &mut egui::Ui,
    order: &[&Widget],
    rects: &BTreeMap<WidgetId, Rect>,
    state: &mut GuiState,
    background: Option<&egui::Response>,
) -> Vec<GuiEvent> {
    let drag_id = ui.id().with("nl_gui_design_drag");
    let mut drag: Option<Drag> = ui.ctx().data(|d| d.get_temp(drag_id));
    let mut events = Vec::new();
    let mut hit_widget = false;

    for w in order {
        let Some(rect) = rects.get(&w.id).copied() else {
            continue;
        };
        let resp = ui.interact(rect, ui.id().with(("nl_gui_design", w.id)), Sense::click_and_drag());

        if resp.hovered() {
            let pos = ui.ctx().pointer_interact_pos().unwrap_or(rect.center());
            ui.ctx().set_cursor_icon(cursor_for(zone_at(rect, pos)));
        }
        if resp.clicked() {
            hit_widget = true;
            state.selected = Some(w.id);
            events.push(GuiEvent::Selected(Some(w.id)));
        }
        if resp.drag_started() {
            hit_widget = true;
            let press = resp.interact_pointer_pos().unwrap_or(rect.center());
            state.selected = Some(w.id);
            drag = Some(Drag {
                widget: w.id,
                zone: zone_at(rect, press),
                orig: w.rect,
                accum: Vec2::ZERO,
            });
        }
        let dragging_this = drag.is_some_and(|d| d.widget == w.id);
        if dragging_this && resp.dragged() {
            if let Some(d) = drag.as_mut() {
                d.accum += resp.drag_delta();
            }
        }
        if dragging_this && resp.drag_stopped() {
            if let Some(d) = drag.take() {
                events.push(GuiEvent::Moved(w.id, relative_rect(&d, rect)));
            }
        }
    }

    // 드래그 미리보기. 끌던 위젯이 사라졌으면 드래그 상태도 버린다.
    if let Some(d) = drag {
        match rects.get(&d.widget).copied() {
            Some(rect) => {
                let preview = apply_zone(rect, d.zone, d.accum);
                let accent = ui.visuals().selection.bg_fill;
                ui.painter().rect_stroke(
                    preview,
                    CornerRadius::same(2),
                    Stroke::new(1.5, accent),
                    StrokeKind::Outside,
                );
            }
            None => drag = None,
        }
    }

    // 선택 강조 + 모서리 핸들.
    if let Some(sel) = state.selected {
        if let Some(rect) = rects.get(&sel).copied() {
            let accent = ui.visuals().selection.bg_fill;
            let painter = ui.painter();
            painter.rect_stroke(
                rect,
                CornerRadius::same(2),
                Stroke::new(2.0, accent),
                StrokeKind::Outside,
            );
            for corner in handle_rects(rect) {
                painter.rect(
                    corner,
                    CornerRadius::ZERO,
                    Color32::WHITE,
                    Stroke::new(1.0, accent),
                    StrokeKind::Inside,
                );
            }
        }
    }

    // 빈 곳 클릭 → 선택 해제.
    if let Some(bg) = background {
        if bg.clicked() && !hit_widget {
            state.selected = None;
            events.push(GuiEvent::Selected(None));
        }
    }

    ui.ctx().data_mut(|d| match drag {
        Some(v) => {
            d.insert_temp(drag_id, v);
        }
        None => d.remove::<Drag>(drag_id),
    });
    events
}

/// 절대 좌표 미리보기 rect 를 부모 기준 상대 `rect` 로 되돌린다.
fn relative_rect(d: &Drag, abs: Rect) -> [f32; 4] {
    let preview = apply_zone(abs, d.zone, d.accum);
    [
        d.orig[0] + (preview.min.x - abs.min.x),
        d.orig[1] + (preview.min.y - abs.min.y),
        preview.width(),
        preview.height(),
    ]
}

fn apply_zone(rect: Rect, zone: Zone, delta: Vec2) -> Rect {
    match zone {
        Zone::Move => rect.translate(delta),
        Zone::Resize(corner) => {
            let (right, bottom) = (corner & 1 != 0, corner & 2 != 0);
            let mut r = rect;
            if right {
                r.max.x += delta.x;
            } else {
                r.min.x += delta.x;
            }
            if bottom {
                r.max.y += delta.y;
            } else {
                r.min.y += delta.y;
            }
            if r.width() < MIN_W {
                if right {
                    r.max.x = r.min.x + MIN_W;
                } else {
                    r.min.x = r.max.x - MIN_W;
                }
            }
            if r.height() < MIN_H {
                if bottom {
                    r.max.y = r.min.y + MIN_H;
                } else {
                    r.min.y = r.max.y - MIN_H;
                }
            }
            r
        }
    }
}

/// 네 모서리 핸들 사각형 (번호 순: 좌상, 우상, 좌하, 우하).
fn handle_rects(rect: Rect) -> [Rect; 4] {
    let h = Vec2::splat(HANDLE);
    [
        Rect::from_min_size(rect.min, h),
        Rect::from_min_size(Pos2::new(rect.max.x - HANDLE, rect.min.y), h),
        Rect::from_min_size(Pos2::new(rect.min.x, rect.max.y - HANDLE), h),
        Rect::from_min_size(rect.max - h, h),
    ]
}

fn zone_at(rect: Rect, pos: Pos2) -> Zone {
    for (i, h) in handle_rects(rect).into_iter().enumerate() {
        if h.contains(pos) {
            return Zone::Resize(i as u8);
        }
    }
    Zone::Move
}

fn cursor_for(zone: Zone) -> egui::CursorIcon {
    match zone {
        Zone::Move => egui::CursorIcon::Move,
        Zone::Resize(0) | Zone::Resize(3) => egui::CursorIcon::ResizeNwSe,
        Zone::Resize(_) => egui::CursorIcon::ResizeNeSw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::gui::{Widget, WidgetKind};

    fn layout_with_group() -> GuiLayout {
        let mut l = GuiLayout::default();
        let g = l.add(Widget::new(
            WidgetKind::Group { title: "그룹".into() },
            [10.0, 20.0, 200.0, 100.0],
        ));
        let mut child = Widget::new(WidgetKind::Label { text: "안".into() }, [5.0, 6.0, 50.0, 20.0]);
        child.parent = Some(g);
        l.add(child);
        l
    }

    #[test]
    fn child_rect_is_relative_to_group() {
        let l = layout_with_group();
        let child = l.widgets.values().find(|w| w.parent.is_some()).unwrap();
        let r = window_rect(&l, child);
        assert_eq!(r.min, Pos2::new(15.0, 26.0));
        assert_eq!(r.size(), Vec2::new(50.0, 20.0));
    }

    #[test]
    fn groups_are_drawn_before_their_children() {
        let l = layout_with_group();
        let order = draw_order(&l);
        let gi = order
            .iter()
            .position(|w| matches!(w.kind, WidgetKind::Group { .. }))
            .unwrap();
        let ci = order.iter().position(|w| w.parent.is_some()).unwrap();
        assert!(gi < ci, "그룹이 자식보다 먼저 그려져야 합니다");
    }

    #[test]
    fn resize_respects_minimum_size() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0));
        // 우하 모서리를 왼쪽 위로 크게 끌어도 최소 크기 아래로는 줄지 않는다.
        let r = apply_zone(rect, Zone::Resize(3), Vec2::new(-500.0, -500.0));
        assert_eq!(r.width(), MIN_W);
        assert_eq!(r.height(), MIN_H);
        assert_eq!(r.min, Pos2::ZERO);
        // 좌상 모서리를 끌면 오른쪽 아래가 고정된다.
        let r = apply_zone(rect, Zone::Resize(0), Vec2::new(500.0, 500.0));
        assert_eq!(r.max, Pos2::new(100.0, 50.0));
        assert_eq!(r.width(), MIN_W);
    }

    #[test]
    fn move_zone_translates() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(40.0, 30.0));
        let r = apply_zone(rect, Zone::Move, Vec2::new(5.0, -3.0));
        assert_eq!(r.min, Pos2::new(15.0, 7.0));
        assert_eq!(r.size(), Vec2::new(40.0, 30.0));
    }

    #[test]
    fn zone_at_picks_corner_handles() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0));
        assert_eq!(zone_at(rect, Pos2::new(1.0, 1.0)), Zone::Resize(0));
        assert_eq!(zone_at(rect, Pos2::new(99.0, 1.0)), Zone::Resize(1));
        assert_eq!(zone_at(rect, Pos2::new(1.0, 49.0)), Zone::Resize(2));
        assert_eq!(zone_at(rect, Pos2::new(99.0, 49.0)), Zone::Resize(3));
        assert_eq!(zone_at(rect, Pos2::new(50.0, 25.0)), Zone::Move);
    }

    #[test]
    fn push_value_trims_history() {
        let mut s = GuiState::default();
        let id = WidgetId::from_u128(1);
        for i in 0..10 {
            s.push_value(id, Value::Number(i as f64), 4);
        }
        assert_eq!(s.history[&id], vec![6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn value_formatting() {
        assert_eq!(format_value(None), "—");
        assert_eq!(format_value(Some(&Value::Number(1.23456))), "1.235");
        assert_eq!(format_value(Some(&Value::Text("가".into()))), "가");
        let many: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let s = format_value(Some(&Value::Numbers(many)));
        assert!(s.starts_with("[0.000, "), "{s}");
        assert!(s.ends_with(", …]"), "{s}");
    }

    #[test]
    fn image_fingerprint_detects_change() {
        let a = vec![7u8; 64];
        let mut b = a.clone();
        b[0] = 9;
        assert_ne!(image_fingerprint(4, 4, &a), image_fingerprint(4, 4, &b));
        assert_eq!(image_fingerprint(4, 4, &a), image_fingerprint(4, 4, &a));
        assert_ne!(image_fingerprint(4, 4, &a), image_fingerprint(2, 8, &a));
    }
}
