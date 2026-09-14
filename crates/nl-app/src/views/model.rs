//! 모델 뷰 = 모델 선택 줄 + 레이어 그래프 캔버스.

use super::{ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_WEAK};
use crate::canvas::{CanvasAction, CanvasState, Selection, SelectionState};
use eframe::egui::{self, RichText};
use nl_core::shape::ShapeReport;
use nl_core::{ModelDef, ModelId};

/// 캔버스 편집과 앱 명령을 함께 돌려준다.
#[derive(Default)]
pub struct ModelViewOut {
    pub canvas: Vec<CanvasAction>,
    pub actions: Vec<ViewAction>,
}

pub fn show(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    canvas: &mut CanvasState,
    report: &ShapeReport,
    sel: &mut SelectionState,
) -> ModelViewOut {
    let mut out = ModelViewOut::default();
    let active = ctx.active_model();

    // ── 모델 선택 줄 ────────────────────────────────────────────
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("모델").color(COL_WEAK));
                let current = active.and_then(|m| ctx.project.models.get(&m));
                let label = current.map(|m| m.name.clone()).unwrap_or_else(|| "(없음)".into());
                egui::ComboBox::from_id_salt("model-picker")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (id, m) in &ctx.project.models {
                            if ui.selectable_label(active == Some(*id), &m.name).clicked() {
                                out.actions.push(ViewAction::Select(Selection::Model(*id)));
                            }
                        }
                    });
                if ui.button("＋ 새 모델").clicked() {
                    let m = ModelDef::new(format!("모델 {}", ctx.project.models.len() + 1));
                    let id = m.id;
                    out.actions.push(ViewAction::Ops(nl_core::ops::restore_model(&m)));
                    out.actions.push(ViewAction::Select(Selection::Model(id)));
                }
                if let Some(id) = active {
                    if ui.button("⛶ 전체 보기").on_hover_text("F").clicked() {
                        canvas.request_fit();
                    }
                    onnx_button(ui, ctx, id, report, &mut out.actions);
                    ui.separator();
                    summary(ui, ctx, id, report);
                }
            });
        });
    ui.separator();

    // ── 캔버스 ──────────────────────────────────────────────────
    match active {
        Some(id) => {
            let graph = &ctx.project.models[&id].graph;
            out.canvas = canvas.show(ui, id, graph, report, sel);
        }
        None => {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.35);
                ui.label(RichText::new("모델이 없습니다").size(18.0));
                ui.add_space(8.0);
                if ui.button("＋ 첫 모델 만들기").clicked() {
                    let m = ModelDef::new("모델 1");
                    let id = m.id;
                    out.actions.push(ViewAction::Ops(nl_core::ops::restore_model(&m)));
                    out.actions.push(ViewAction::Select(Selection::Model(id)));
                }
            });
        }
    }
    out
}

/// "ONNX 로 내보내기…" — 내보낼 수 없는 그래프면 회색으로 두고 이유를 툴팁에 적는다.
fn onnx_button(ui: &mut egui::Ui, ctx: &ViewCtx, id: ModelId, report: &ShapeReport, actions: &mut Vec<ViewAction>) {
    let Some(m) = ctx.project.models.get(&id) else { return };
    // `check` 는 가중치를 읽지 않는다 — 학습 전에도 부를 수 있다.
    let unsupported = nl_engine::onnx::check(m).unsupported;
    let blocked = if !unsupported.is_empty() {
        Some(format!(
            "아직 ONNX 로 내보낼 수 없는 레이어가 있습니다: {}",
            unsupported.join(", ")
        ))
    } else if !report.errors.is_empty() {
        Some("형상 오류를 먼저 고쳐야 내보낼 수 있습니다 — 하단 도크의 문제 탭을 보세요".to_string())
    } else if m.graph.nodes.is_empty() {
        Some("레이어가 없는 모델은 내보낼 것이 없습니다".to_string())
    } else {
        None
    };
    let tip = match &blocked {
        Some(why) => why.clone(),
        None if m.weights.is_none() => "가중치가 아직 없습니다 — 누르면 어떻게 만드는지 알려 줍니다".to_string(),
        None => "학습한 가중치를 담아 .onnx 파일로 씁니다 (opset 17, 배치 차원은 동적)".to_string(),
    };
    ui.add_enabled_ui(blocked.is_none(), |ui| {
        if ui.button("ONNX 로 내보내기…").on_hover_text(&tip).clicked() {
            actions.push(ViewAction::ExportOnnx { model: id, run: None });
        }
    });
}

/// 노드·엣지 수와 형상 추론 결과 한 줄.
fn summary(ui: &mut egui::Ui, ctx: &ViewCtx, id: ModelId, report: &ShapeReport) {
    let Some(m) = ctx.project.models.get(&id) else { return };
    ui.label(RichText::new(format!("레이어 {} · 연결 {}", m.graph.nodes.len(), m.graph.edges.len())).color(COL_WEAK));
    let errors = report.errors.len();
    if errors == 0 {
        let out = m.graph.output_nodes();
        let text = match out.first().and_then(|o| report.shape(*o)) {
            Some(s) => format!("출력 {s}"),
            None => "형상 정상".into(),
        };
        ui.label(RichText::new(format!("✔ {text}")).color(COL_OK));
    } else {
        ui.label(RichText::new(format!("✖ 오류 {errors}개")).color(COL_ERROR))
            .on_hover_text("하단 도크의 문제 탭에서 항목을 누르면 해당 노드로 이동합니다");
    }
}
