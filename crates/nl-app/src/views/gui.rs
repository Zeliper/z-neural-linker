//! GUI 디자이너: 위젯 팔레트 · 창 캔버스(`nl_gui::render_layout` Design) · 바인딩 편집 · 미리보기.
//!
//! 캔버스는 런타임과 **같은 렌더러**를 쓴다. 미리보기를 켜면 `RenderMode::Run` 으로 바꾸고 선택한
//! 파이프라인을 실행기에 붙이므로, 빌더에서 보이는 것이 배포판에서 도는 것과 같다.

use super::{ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_SELECT, COL_SURFACE, COL_WARN, COL_WEAK};
use crate::canvas::Selection;
use eframe::egui::{self, DragValue, RichText, UiBuilder};
use nl_core::gui::{Binding, BuiltinAction};
use nl_core::pipeline::{PNode, PNodeKind, Sink, Source};
use nl_core::{ModelId, Op, PNodeId, PayloadId, Pipeline, Widget, WidgetId, WidgetKind};
use nl_gui::{GuiEvent, GuiState, RenderMode};

/// 이동·크기 조절이 걸리는 격자 (논리 px).
pub const SNAP: f32 = 8.0;

pub struct GuiViewState {
    pub snap: bool,
}

impl Default for GuiViewState {
    fn default() -> Self {
        // 격자는 기본으로 켠다 — 손으로 끌어도 자리가 어긋나지 않는다.
        Self { snap: true }
    }
}

#[derive(Default)]
pub struct GuiViewOut {
    pub actions: Vec<ViewAction>,
    /// 미리보기(Run) 모드에서 위젯이 낸 이벤트. 앱이 바인딩을 보고 실행기에 넘긴다.
    pub events: Vec<GuiEvent>,
}

/// 값을 격자에 맞춘다.
pub fn snap_rect(rect: [f32; 4], snap: bool) -> [f32; 4] {
    if !snap {
        return rect.map(|v| (v * 100.0).round() / 100.0);
    }
    let q = |v: f32| (v / SNAP).round() * SNAP;
    [
        q(rect[0]),
        q(rect[1]),
        q(rect[2]).max(SNAP * 2.0),
        q(rect[3]).max(SNAP * 2.0),
    ]
}

pub fn show(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    state: &mut GuiViewState,
    gui: &mut GuiState,
    preview: bool,
    // 지금 돌고 있는 파이프라인 이름. 없으면 미리보기가 값을 받지 못한다.
    running: Option<&str>,
) -> GuiViewOut {
    let mut out = GuiViewOut::default();
    let layout = &ctx.project.gui;

    // ── 상단 바 ─────────────────────────────────────────────────
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("창").color(COL_WEAK));
                let mut win = layout.window.clone();
                let mut changed = false;
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut win.title).desired_width(180.0))
                    .changed();
                changed |= ui
                    .add(
                        DragValue::new(&mut win.width)
                            .range(120.0..=4096.0)
                            .speed(4.0)
                            .prefix("w "),
                    )
                    .changed();
                changed |= ui
                    .add(
                        DragValue::new(&mut win.height)
                            .range(120.0..=4096.0)
                            .speed(4.0)
                            .prefix("h "),
                    )
                    .changed();
                changed |= ui.checkbox(&mut win.dark, "다크").changed();
                if changed {
                    out.actions
                        .push(ViewAction::Edit(vec![Op::SetGuiWindow { window: win }]));
                }
                ui.separator();
                ui.checkbox(&mut state.snap, format!("{SNAP:.0}px 격자"));
                ui.separator();
                let mut on = preview;
                if ui
                    .checkbox(&mut on, "▶ 미리보기")
                    .on_hover_text("런타임과 같은 Run 모드로 바꾸고 선택한 파이프라인을 실행합니다")
                    .changed()
                {
                    out.actions.push(ViewAction::SetGuiPreview(on));
                }
                if preview {
                    // 어느 파이프라인이 위젯을 먹이는지 적어 준다. 이것이 없으면 값 위젯이 `—` 인 것이
                    // 고장인지 "그 파이프라인이 이 위젯을 안 먹인다" 인지 구별할 수 없다.
                    match running {
                        Some(name) => {
                            ui.label(
                                RichText::new(format!("● 미리보기 중 · {name} 실행 — 편집은 꺼짐")).color(COL_SELECT),
                            );
                        }
                        None => {
                            ui.label(
                                RichText::new("● 미리보기 중 — 실행 중인 파이프라인이 없어 값이 들어오지 않습니다")
                                    .color(COL_WARN),
                            );
                        }
                    }
                }
            });
        });
    ui.separator();

    // ── 팔레트 + 캔버스 ─────────────────────────────────────────
    let avail = ui.available_rect_before_wrap();
    let palette_w = 150.0_f32.min(avail.width() * 0.28);
    ui.horizontal_top(|ui| {
        // 부모가 가로 배치라 명시하지 않으면 팔레트 버튼이 옆으로 흘러 캔버스를 밀어낸다.
        let palette_layout = egui::Layout::top_down(egui::Align::Min);
        ui.allocate_ui_with_layout(egui::Vec2::new(palette_w, avail.height()), palette_layout, |ui| {
            ui.label(RichText::new("위젯").strong());
            ui.separator();
            ui.add_enabled_ui(!preview, |ui| {
                for kind in WidgetKind::palette() {
                    if ui
                        .add(egui::Button::new(kind.label()).min_size(egui::Vec2::new(palette_w - 12.0, 0.0)))
                        .clicked()
                    {
                        let rect = default_rect(&kind, layout);
                        let w = Widget::new(kind.clone(), rect);
                        let id = w.id;
                        out.actions.push(ViewAction::Ops(vec![Op::UpsertWidget { widget: w }]));
                        out.actions.push(ViewAction::Select(Selection::Widget(id)));
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("{}개 배치됨", layout.widgets.len()))
                    .color(COL_WEAK)
                    .size(11.0),
            );
            if preview {
                ui.label(
                    RichText::new("미리보기를 끄면 편집할 수 있습니다")
                        .color(COL_WEAK)
                        .size(11.0),
                );
            }
        });
        ui.separator();

        // 창 크기만큼의 프레임. 남는 자리가 좁으면 스크롤로 본다.
        egui::ScrollArea::both().id_salt("gui-canvas-scroll").show(ui, |ui| {
            let size = egui::Vec2::new(layout.window.width.max(80.0), layout.window.height.max(80.0));
            let (rect, _) = ui.allocate_exact_size(size + egui::Vec2::new(2.0, 26.0), egui::Sense::hover());
            let title_h = 24.0;
            let title_rect = egui::Rect::from_min_size(rect.min, egui::Vec2::new(rect.width(), title_h));
            let win_rect = egui::Rect::from_min_size(rect.min + egui::Vec2::new(1.0, title_h + 1.0), size);
            let painter = ui.painter();
            painter.rect_filled(title_rect, egui::CornerRadius::same(3), COL_SURFACE);
            painter.text(
                title_rect.left_center() + egui::Vec2::new(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &layout.window.title,
                egui::FontId::proportional(12.0),
                ui.visuals().text_color(),
            );
            let bg = if layout.window.dark {
                egui::Color32::from_rgb(0x1e, 0x21, 0x27)
            } else {
                egui::Color32::from_rgb(0xf2, 0xf3, 0xf5)
            };
            painter.rect(
                win_rect,
                egui::CornerRadius::ZERO,
                bg,
                egui::Stroke::new(1.0, COL_WEAK),
                egui::StrokeKind::Outside,
            );

            let mode = if preview { RenderMode::Run } else { RenderMode::Design };
            let events = ui
                .scope_builder(UiBuilder::new().max_rect(win_rect).id_salt("gui-design"), |ui| {
                    nl_gui::render_layout(ui, layout, gui, mode)
                })
                .inner;

            for ev in events {
                match ev {
                    GuiEvent::Selected(Some(id)) => out.actions.push(ViewAction::Select(Selection::Widget(id))),
                    GuiEvent::Selected(None) => out.actions.push(ViewAction::Select(Selection::Project)),
                    GuiEvent::Moved(id, rect) => {
                        if let Some(w) = layout.widgets.get(&id) {
                            let mut next = w.clone();
                            next.rect = snap_rect(rect, state.snap);
                            out.actions
                                .push(ViewAction::Ops(vec![Op::UpsertWidget { widget: next }]));
                        }
                    }
                    other => out.events.push(other),
                }
            }
            if layout.widgets.is_empty() {
                ui.painter().text(
                    win_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "왼쪽 팔레트에서 위젯을 고르세요",
                    egui::FontId::proportional(13.0),
                    COL_WEAK,
                );
            }
        });
    });
    out
}

/// 새 위젯의 기본 자리: 창 가운데에서 조금씩 어긋나게 쌓는다.
fn default_rect(kind: &WidgetKind, layout: &nl_core::GuiLayout) -> [f32; 4] {
    let (w, h) = match kind {
        WidgetKind::Group { .. } => (220.0, 140.0),
        WidgetKind::Plot { .. } => (260.0, 140.0),
        WidgetKind::Image => (200.0, 150.0),
        WidgetKind::Value { .. } => (180.0, 48.0),
        WidgetKind::Slider { .. } | WidgetKind::TextInput { .. } => (200.0, 28.0),
        _ => (140.0, 28.0),
    };
    let n = layout.widgets.len() as f32;
    let x = (layout.window.width * 0.5 - w * 0.5 + (n % 5.0) * SNAP * 2.0).max(SNAP);
    let y = (layout.window.height * 0.35 + (n % 7.0) * SNAP * 2.0).max(SNAP);
    snap_rect([x, y, w, h], true)
}

// ───────────────────────────── 인스펙터 ─────────────────────────────

/// `gui` 는 실행 중 위젯이 받은 값(이미지 텍스처 포함)을 보여 주기 위한 것이다.
pub fn inspect_widget(ui: &mut egui::Ui, ctx: &ViewCtx, id: WidgetId, gui: &GuiState) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    let layout = &ctx.project.gui;
    let Some(w) = layout.widgets.get(&id) else {
        ui.label(RichText::new("위젯이 없습니다").color(COL_ERROR));
        return actions;
    };
    let mut next = w.clone();
    let mut changed = false;

    ui.label(RichText::new(w.kind.label()).size(15.0).strong());
    ui.separator();

    ui.label(RichText::new("종류").color(COL_WEAK).size(11.5));
    egui::ComboBox::from_id_salt("widget-kind")
        .selected_text(next.kind.label())
        .show_ui(ui, |ui| {
            for k in WidgetKind::palette() {
                let same = std::mem::discriminant(&next.kind) == std::mem::discriminant(&k);
                if ui.selectable_label(same, k.label()).clicked() && !same {
                    next.kind = k;
                    changed = true;
                }
            }
        });
    changed |= kind_fields(ui, &mut next.kind);

    ui.add_space(6.0);
    ui.label(RichText::new("자리 (창 기준 논리 px)").color(COL_WEAK).size(11.5));
    ui.horizontal(|ui| {
        changed |= ui
            .add(DragValue::new(&mut next.rect[0]).prefix("x ").speed(1.0))
            .changed();
        changed |= ui
            .add(DragValue::new(&mut next.rect[1]).prefix("y ").speed(1.0))
            .changed();
    });
    ui.horizontal(|ui| {
        changed |= ui
            .add(
                DragValue::new(&mut next.rect[2])
                    .prefix("w ")
                    .speed(1.0)
                    .range(8.0..=4096.0),
            )
            .changed();
        changed |= ui
            .add(
                DragValue::new(&mut next.rect[3])
                    .prefix("h ")
                    .speed(1.0)
                    .range(8.0..=4096.0),
            )
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label("z");
        changed |= ui
            .add(DragValue::new(&mut next.z).speed(1.0))
            .on_hover_text("클수록 위에 그려집니다")
            .changed();
    });

    // 그룹 부모.
    ui.label(RichText::new("그룹").color(COL_WEAK).size(11.5));
    let groups: Vec<(WidgetId, String)> = layout
        .widgets
        .values()
        .filter(|g| matches!(g.kind, WidgetKind::Group { .. }) && g.id != id)
        .map(|g| {
            let title = match &g.kind {
                WidgetKind::Group { title } => title.clone(),
                _ => String::new(),
            };
            (g.id, if title.is_empty() { g.id.short() } else { title })
        })
        .collect();
    let parent_label = next
        .parent
        .and_then(|p| groups.iter().find(|(gid, _)| *gid == p))
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| "(없음)".into());
    egui::ComboBox::from_id_salt("widget-parent")
        .selected_text(parent_label)
        .show_ui(ui, |ui| {
            if ui.selectable_label(next.parent.is_none(), "(없음)").clicked() && next.parent.is_some() {
                next.parent = None;
                changed = true;
            }
            for (gid, name) in &groups {
                if ui.selectable_label(next.parent == Some(*gid), name).clicked() && next.parent != Some(*gid) {
                    next.parent = Some(*gid);
                    changed = true;
                }
            }
        });

    ui.add_space(8.0);
    ui.label(RichText::new("바인딩").size(13.0).strong());
    ui.separator();
    changed |= binding_editor(ui, &mut next, ctx, &mut actions);

    // 실행 중 이 위젯이 받은 값. 이미지는 렌더러가 만들어 둔 텍스처를 그대로 보여 준다
    // (파이프라인 값 이벤트는 이미지를 싣지 않는다 — `Sink::GuiWidget` 으로만 온다).
    let current = gui.values.get(&id);
    if current.is_some() || gui.textures.contains_key(&id) {
        ui.add_space(8.0);
        ui.separator();
        super::kv(ui, "마지막 값", nl_gui::format_value(current));
        if let Some(tex) = gui.textures.get(&id) {
            let [w, h] = tex.size();
            let side = 160.0;
            let scale = (side / w.max(1) as f32).min(side / h.max(1) as f32).min(1.0);
            ui.add(egui::Image::new(tex).fit_to_exact_size(egui::Vec2::new(w as f32 * scale, h as f32 * scale)));
            ui.label(RichText::new(format!("{w}×{h}")).color(COL_WEAK).size(11.0));
        }
    }

    ui.add_space(10.0);
    if ui.button(RichText::new("🗑 위젯 삭제").color(COL_ERROR)).clicked() {
        actions.push(ViewAction::Ops(vec![Op::DeleteWidget { id }]));
        actions.push(ViewAction::Select(Selection::Project));
    }

    if changed {
        actions.push(ViewAction::Edit(vec![Op::UpsertWidget { widget: next }]));
    }
    actions
}

fn kind_fields(ui: &mut egui::Ui, kind: &mut WidgetKind) -> bool {
    let mut changed = false;
    match kind {
        WidgetKind::Label { text } | WidgetKind::Button { text } | WidgetKind::Toggle { text } => {
            ui.label(RichText::new("문구").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY))
                .changed();
        }
        WidgetKind::Group { title } => {
            ui.label(RichText::new("제목").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(title).desired_width(f32::INFINITY))
                .changed();
        }
        WidgetKind::Value { prefix } => {
            ui.label(RichText::new("앞에 붙일 말").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(prefix).desired_width(f32::INFINITY))
                .changed();
        }
        WidgetKind::TextInput { hint } => {
            ui.label(RichText::new("힌트").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(hint).desired_width(f32::INFINITY))
                .changed();
        }
        WidgetKind::Slider { min, max, value } => {
            ui.horizontal(|ui| {
                changed |= ui.add(DragValue::new(min).prefix("min ").speed(0.1)).changed();
                changed |= ui.add(DragValue::new(max).prefix("max ").speed(0.1)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("초기값");
                changed |= ui.add(DragValue::new(value).speed(0.01)).changed();
            });
        }
        WidgetKind::Plot { max_points } => {
            ui.horizontal(|ui| {
                ui.label("점 개수");
                changed |= ui.add(DragValue::new(max_points).range(2..=100_000)).changed();
            });
        }
        WidgetKind::Image => {}
    }
    changed
}

/// 바인딩 콤보 + "없으면 만들기" 버튼.
fn binding_editor(ui: &mut egui::Ui, w: &mut Widget, ctx: &ViewCtx, actions: &mut Vec<ViewAction>) -> bool {
    let mut changed = false;
    let current = binding_tag(&w.binding);
    egui::ComboBox::from_id_salt("widget-binding")
        .selected_text(binding_tag_label(current))
        .show_ui(ui, |ui| {
            for tag in [
                BindingTag::None,
                BindingTag::Input,
                BindingTag::Output,
                BindingTag::ModelOutput,
                BindingTag::Action,
            ] {
                if ui.selectable_label(current == tag, binding_tag_label(tag)).clicked() && current != tag {
                    w.binding = default_binding(tag, ctx, w.id);
                    changed = true;
                }
            }
        });

    match &mut w.binding {
        None => {
            ui.label(
                RichText::new("아무 데도 연결되지 않았습니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        }
        Some(Binding::Action { action }) => {
            for a in [
                BuiltinAction::StartPipeline,
                BuiltinAction::StopPipeline,
                BuiltinAction::Quit,
            ] {
                let text = match a {
                    BuiltinAction::StartPipeline => "파이프라인 시작",
                    BuiltinAction::StopPipeline => "파이프라인 정지",
                    BuiltinAction::Quit => "앱 종료",
                };
                if ui.selectable_label(*action == a, text).clicked() && *action != a {
                    *action = a;
                    changed = true;
                }
            }
        }
        Some(Binding::PipelineInput { node }) => {
            changed |= node_picker(ui, node, ctx, NodeRole::GuiEventSource, w.id, actions, "bind-in");
        }
        Some(Binding::PipelineOutput { node }) => {
            changed |= node_picker(ui, node, ctx, NodeRole::GuiWidgetSink, w.id, actions, "bind-out");
        }
        Some(Binding::ModelOutput { model, field }) => {
            changed |= model_output_picker(ui, model, field, ctx);
        }
    }
    changed
}

/// `Binding::ModelOutput` 편집기: 모델 콤보 + 그 모델 페이로드의 출력 필드 콤보.
///
/// 파이프라인을 거치지 않고 모델의 마지막 추론 결과를 위젯에 바로 꽂는 바인딩이다. 값이 어디서
/// 오는지는 배포 앱이 정하므로, 여기서는 **가리키는 대상이 실제로 있는지**만 책임진다.
fn model_output_picker(ui: &mut egui::Ui, model: &mut ModelId, field: &mut String, ctx: &ViewCtx) -> bool {
    let mut changed = false;

    ui.label(RichText::new("모델").color(COL_WEAK).size(11.0));
    let current = ctx.project.models.get(model);
    let label = current
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "(없는 모델)".to_string());
    egui::ComboBox::from_id_salt("bind-model")
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (id, m) in &ctx.project.models {
                if ui.selectable_label(*model == *id, &m.name).clicked() && *model != *id {
                    *model = *id;
                    // 모델이 바뀌면 옛 필드 이름은 대개 없는 이름이 된다 — 새 모델의 첫 출력으로 옮긴다.
                    *field = first_output_field(ctx, *id).unwrap_or_default();
                    changed = true;
                }
            }
        });
    let Some(m) = ctx.project.models.get(model) else {
        ui.label(
            RichText::new("✖ 이 모델이 프로젝트에 없습니다")
                .color(COL_ERROR)
                .size(11.0),
        );
        return changed;
    };

    ui.label(RichText::new("출력 필드").color(COL_WEAK).size(11.0));
    let fields = output_fields(ctx, m.payload);
    if fields.is_empty() {
        ui.label(
            RichText::new("이 모델의 페이로드에 출력 필드가 없습니다 — 페이로드를 먼저 정하세요")
                .color(COL_WARN)
                .size(11.0),
        );
        return changed;
    }
    let shown = if field.is_empty() {
        "(고르세요)".to_string()
    } else {
        field.clone()
    };
    egui::ComboBox::from_id_salt("bind-model-field")
        .selected_text(shown)
        .show_ui(ui, |ui| {
            for name in &fields {
                if ui.selectable_label(field == name, name).clicked() && field != name {
                    *field = name.clone();
                    changed = true;
                }
            }
        });
    if !field.is_empty() && !fields.iter().any(|f| f == field) {
        ui.label(
            RichText::new(format!("✖ '{field}' 은 이 페이로드에 없는 출력입니다"))
                .color(COL_ERROR)
                .size(11.0),
        );
    }
    ui.label(
        RichText::new("모델의 마지막 추론 값을 이 위젯에 보여 줍니다.")
            .color(COL_WEAK)
            .size(11.0),
    );
    changed
}

/// 모델 페이로드의 출력 필드 이름들.
fn output_fields(ctx: &ViewCtx, payload: Option<PayloadId>) -> Vec<String> {
    payload
        .and_then(|id| ctx.project.payloads.get(&id))
        .map(|p| p.outputs.iter().map(|f| f.name.clone()).collect())
        .unwrap_or_default()
}

fn first_output_field(ctx: &ViewCtx, model: ModelId) -> Option<String> {
    let payload = ctx.project.models.get(&model)?.payload;
    output_fields(ctx, payload).into_iter().next()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingTag {
    None,
    Input,
    Output,
    ModelOutput,
    Action,
}

fn binding_tag(b: &Option<Binding>) -> BindingTag {
    match b {
        None => BindingTag::None,
        Some(Binding::PipelineInput { .. }) => BindingTag::Input,
        Some(Binding::PipelineOutput { .. }) => BindingTag::Output,
        Some(Binding::ModelOutput { .. }) => BindingTag::ModelOutput,
        Some(Binding::Action { .. }) => BindingTag::Action,
    }
}

fn binding_tag_label(t: BindingTag) -> &'static str {
    match t {
        BindingTag::None => "없음",
        BindingTag::Input => "파이프라인 입력 (위젯 → 파이프라인)",
        BindingTag::Output => "파이프라인 출력 (파이프라인 → 위젯)",
        BindingTag::ModelOutput => "모델 출력 (마지막 추론 값 → 위젯)",
        BindingTag::Action => "내장 동작",
    }
}

fn default_binding(tag: BindingTag, ctx: &ViewCtx, widget: WidgetId) -> Option<Binding> {
    match tag {
        BindingTag::None => None,
        BindingTag::Action => Some(Binding::Action {
            action: BuiltinAction::StartPipeline,
        }),
        BindingTag::Input => {
            let node = find_node(ctx, NodeRole::GuiEventSource, widget).unwrap_or_default();
            Some(Binding::PipelineInput { node })
        }
        BindingTag::Output => {
            let node = find_node(ctx, NodeRole::GuiWidgetSink, widget).unwrap_or_default();
            Some(Binding::PipelineOutput { node })
        }
        BindingTag::ModelOutput => {
            // 첫 모델과 그 페이로드의 첫 출력으로 시작한다 — 고를 것이 없으면 빈 값이라도 둬야
            // 편집기가 열리고 사용자가 바꿀 수 있다.
            let model = ctx.project.models.keys().next().copied().unwrap_or_default();
            let field = first_output_field(ctx, model).unwrap_or_default();
            Some(Binding::ModelOutput { model, field })
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeRole {
    /// `Source::GuiEvent { widget }`
    GuiEventSource,
    /// `Sink::GuiWidget { widget }`
    GuiWidgetSink,
}

/// 이 위젯에 이미 묶인 노드를 찾는다.
fn find_node(ctx: &ViewCtx, role: NodeRole, widget: WidgetId) -> Option<PNodeId> {
    ctx.project
        .pipelines
        .values()
        .flat_map(|p| p.nodes.values())
        .find_map(|n| match (&n.kind, role) {
            (
                PNodeKind::Source {
                    source: Source::GuiEvent { widget: w },
                },
                NodeRole::GuiEventSource,
            ) if *w == widget => Some(n.id),
            (
                PNodeKind::Sink {
                    sink: Sink::GuiWidget { widget: w },
                },
                NodeRole::GuiWidgetSink,
            ) if *w == widget => Some(n.id),
            _ => None,
        })
}

/// 역할에 맞는 파이프라인 노드 콤보 + 없을 때 만들기.
fn node_picker(
    ui: &mut egui::Ui,
    node: &mut PNodeId,
    ctx: &ViewCtx,
    role: NodeRole,
    widget: WidgetId,
    actions: &mut Vec<ViewAction>,
    salt: &str,
) -> bool {
    let mut changed = false;
    let candidates: Vec<(PNodeId, String)> = ctx
        .project
        .pipelines
        .values()
        .flat_map(|p| p.nodes.values().map(move |n| (p, n)))
        .filter(|(_, n)| matches_role(&n.kind, role))
        .map(|(p, n)| (n.id, format!("{} · {}", p.name, crate::pcanvas::node_title(n))))
        .collect();

    let label = candidates
        .iter()
        .find(|(id, _)| id == node)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| "(고르세요)".into());
    egui::ComboBox::from_id_salt(salt)
        .selected_text(label)
        .show_ui(ui, |ui| {
            if candidates.is_empty() {
                ui.label(RichText::new("맞는 노드가 없습니다").weak());
            }
            for (id, name) in &candidates {
                if ui.selectable_label(node == id, name).clicked() && node != id {
                    *node = *id;
                    changed = true;
                }
            }
        });

    if !candidates.iter().any(|(id, _)| id == node) {
        let what = match role {
            NodeRole::GuiEventSource => "이 위젯용 GuiEvent 소스 만들기",
            NodeRole::GuiWidgetSink => "이 위젯용 GuiWidget 싱크 만들기",
        };
        ui.label(RichText::new("연결된 노드가 없습니다").color(COL_WARN).size(11.0));
        if ui.button(what).clicked() {
            let (ops, new_id) = create_node_for_widget(ctx, role, widget);
            actions.push(ViewAction::Ops(ops));
            *node = new_id;
            changed = true;
        }
    } else {
        ui.label(RichText::new("✔ 파이프라인 노드와 연결됨").color(COL_OK).size(11.0));
    }
    changed
}

fn matches_role(kind: &PNodeKind, role: NodeRole) -> bool {
    matches!(
        (kind, role),
        (
            PNodeKind::Source {
                source: Source::GuiEvent { .. }
            },
            NodeRole::GuiEventSource
        ) | (
            PNodeKind::Sink {
                sink: Sink::GuiWidget { .. }
            },
            NodeRole::GuiWidgetSink
        )
    )
}

/// 위젯에 묶인 노드를 만든다. 파이프라인이 하나도 없으면 같이 만든다.
fn create_node_for_widget(ctx: &ViewCtx, role: NodeRole, widget: WidgetId) -> (Vec<Op>, PNodeId) {
    let mut ops = Vec::new();
    let pid = match ctx.active_pipeline() {
        Some(p) => p,
        None => {
            let pl = Pipeline::new("파이프라인 1");
            ops.push(Op::UpsertPipelineMeta {
                id: pl.id,
                name: pl.name.clone(),
                tick_hz: pl.tick_hz,
            });
            pl.id
        }
    };
    let count = ctx.project.pipelines.get(&pid).map(|p| p.nodes.len()).unwrap_or(0);
    let kind = match role {
        NodeRole::GuiEventSource => PNodeKind::Source {
            source: Source::GuiEvent { widget },
        },
        NodeRole::GuiWidgetSink => PNodeKind::Sink {
            sink: Sink::GuiWidget { widget },
        },
    };
    let row = match role {
        NodeRole::GuiEventSource => 60.0,
        NodeRole::GuiWidgetSink => 260.0,
    };
    let node = PNode::new(kind, crate::pcanvas::chain_pos(count, row));
    let id = node.id;
    ops.push(Op::UpsertPNode { pipeline: pid, node });
    (ops, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_quantises_and_keeps_a_minimum_size() {
        assert_eq!(snap_rect([3.0, 12.0, 100.0, 30.0], true), [0.0, 16.0, 104.0, 32.0]);
        // 너무 작아지면 격자 두 칸으로 올린다.
        let r = snap_rect([0.0, 0.0, 2.0, 2.0], true);
        assert!(r[2] >= SNAP * 2.0 && r[3] >= SNAP * 2.0);
        // 끄면 소수점만 정리한다.
        assert_eq!(snap_rect([3.004, 12.0, 100.0, 30.0], false), [3.0, 12.0, 100.0, 30.0]);
    }

    #[test]
    fn binding_tags_round_trip() {
        assert_eq!(binding_tag(&None), BindingTag::None);
        assert_eq!(
            binding_tag(&Some(Binding::PipelineInput {
                node: PNodeId::from_u128(1)
            })),
            BindingTag::Input
        );
        assert_eq!(
            binding_tag(&Some(Binding::PipelineOutput {
                node: PNodeId::from_u128(1)
            })),
            BindingTag::Output
        );
        assert_eq!(
            binding_tag(&Some(Binding::Action {
                action: BuiltinAction::Quit
            })),
            BindingTag::Action
        );
        // 모델 출력은 파이프라인 출력과 다른 종류다 — 같은 칸에 묶으면 콤보에서 고를 수 없다.
        assert_eq!(
            binding_tag(&Some(Binding::ModelOutput {
                model: ModelId::from_u128(1),
                field: "out".into()
            })),
            BindingTag::ModelOutput
        );
    }

    /// 모델 출력 바인딩을 고르면 실제로 있는 모델과 출력 필드로 시작해야 한다.
    #[test]
    fn a_new_model_output_binding_points_at_something_real() {
        let project = nl_core::sample::xor_project();
        let devices: Vec<nl_engine::DeviceInfo> = Vec::new();
        let monitors: Vec<nl_io::MonitorInfo> = Vec::new();
        let shot = crate::record::ShotPreview::default();
        let dir = std::path::PathBuf::from("/tmp");
        let ctx = ViewCtx {
            project: &project,
            selection: Selection::None,
            devices: &devices,
            base_dir: None,
            training: None,
            monitors: &monitors,
            monitors_error: None,
            recording: None,
            shot: &shot,
            update_check: false,
            update_state: None,
            autosave_file: false,
            recovery_dir: &dir,
            now: 0.0,
        };

        let b = default_binding(BindingTag::ModelOutput, &ctx, WidgetId::from_u128(1));
        let Some(Binding::ModelOutput { model, field }) = b else {
            panic!("모델 출력 바인딩")
        };
        assert!(project.models.contains_key(&model), "있는 모델을 가리켜야 한다");
        // 샘플 모델에는 페이로드가 붙어 있고 그 출력 필드가 기본값이 된다.
        assert!(!field.is_empty(), "첫 출력 필드가 채워져야 한다");
        assert!(output_fields(&ctx, project.models[&model].payload).contains(&field));
    }

    #[test]
    fn role_matching_is_exclusive() {
        let src = PNodeKind::Source {
            source: Source::GuiEvent {
                widget: WidgetId::from_u128(1),
            },
        };
        let sink = PNodeKind::Sink {
            sink: Sink::GuiWidget {
                widget: WidgetId::from_u128(1),
            },
        };
        assert!(matches_role(&src, NodeRole::GuiEventSource));
        assert!(!matches_role(&src, NodeRole::GuiWidgetSink));
        assert!(matches_role(&sink, NodeRole::GuiWidgetSink));
        assert!(!matches_role(&sink, NodeRole::GuiEventSource));
        // 다른 소스는 어느 역할에도 맞지 않는다.
        let other = PNodeKind::Source { source: Source::Manual };
        assert!(!matches_role(&other, NodeRole::GuiEventSource));
    }

    #[test]
    fn default_rects_are_inside_the_window_and_snapped() {
        let layout = nl_core::GuiLayout::default();
        for kind in WidgetKind::palette() {
            let r = default_rect(&kind, &layout);
            assert!(r[0] >= 0.0 && r[1] >= 0.0, "{kind:?} 가 창 밖에서 시작한다");
            assert_eq!(r[0] % SNAP, 0.0);
            assert_eq!(r[1] % SNAP, 0.0);
            assert!(r[2] > 0.0 && r[3] > 0.0);
        }
    }
}
