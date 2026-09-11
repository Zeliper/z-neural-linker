//! 좌측 아웃라인: 프로젝트 트리(모델·데이터셋·페이로드·파이프라인·GUI·실행 기록).
//! 선택·추가·이름 변경·삭제를 한 곳에서 한다. 편집은 op 로만 나간다.

use crate::app::{NlApp, View};
use crate::canvas::Selection;
use crate::inspector::rename_op;
use crate::views::{self, COL_ERROR, COL_OK, COL_WARN, COL_WEAK};
use eframe::egui::{self, RichText};
use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::payload::PayloadSpec;
use nl_core::{ModelDef, ModelId, NodeId, Op, Pipeline};

/// 아웃라인 한 프레임이 모으는 결과. UI 를 그리는 동안 문서를 건드리지 않는다.
#[derive(Default)]
struct OutlineOut {
    ops: Vec<Op>,
    select: Option<Selection>,
    /// 모델 뷰로 가서 이 노드를 비춘다.
    focus: Option<(ModelId, NodeId)>,
    view: Option<View>,
    /// 이름 변경을 시작한다.
    start_rename: Option<(Selection, String)>,
    /// 편집 중인 이름을 확정한다.
    commit_rename: bool,
    cancel_rename: bool,
}

impl NlApp {
    pub(crate) fn outline(&mut self, ui: &mut egui::Ui, now: f64) {
        let mut out = OutlineOut::default();
        let selection = self.sel.primary;
        let rename = self.rename_slot().clone();
        let report = self.shapes();

        egui::ScrollArea::vertical().id_salt("outline-scroll").show(ui, |ui| {
            // ── 프로젝트 ────────────────────────────────────────
            let is_project = selection == Selection::Project;
            let resp = ui.selectable_label(is_project, RichText::new(format!("📁 {}", self.doc.project.name)).strong());
            if resp.clicked() {
                out.select = Some(Selection::Project);
            }
            resp.context_menu(|ui| {
                if ui.button("이름 변경").clicked() {
                    out.start_rename = Some((Selection::Project, self.doc.project.name.clone()));
                    ui.close();
                }
            });
            rename_field(ui, &rename, Selection::Project, &mut out);
            ui.separator();

            // ── 모델 ────────────────────────────────────────────
            section(ui, "모델", self.doc.project.models.len(), |ui| {
                if ui.small_button("＋").on_hover_text("새 모델").clicked() {
                    let m = ModelDef::new(format!("모델 {}", self.doc.project.models.len() + 1));
                    out.select = Some(Selection::Model(m.id));
                    out.ops.extend(nl_core::ops::restore_model(&m));
                }
            });
            for (id, m) in &self.doc.project.models {
                // 이름을 고치는 중이면 헤더 자리에 입력칸을 둔다 — 펼쳐진 하위 목록 아래에 나오면 찾기 어렵다.
                if rename.as_ref().is_some_and(|(s, _)| *s == Selection::Model(*id)) {
                    rename_field(ui, &rename, Selection::Model(*id), &mut out);
                    continue;
                }
                let selected = matches!(selection, Selection::Model(x) | Selection::Node(x, _) | Selection::Edge(x, _) if x == *id);
                let header = egui::CollapsingHeader::new(RichText::new(format!("⚛ {}", m.name)).strong())
                    .id_salt(("model", id))
                    .default_open(selected);
                let resp = header.show(ui, |ui| {
                    if m.graph.nodes.is_empty() {
                        ui.label(RichText::new("레이어 없음").color(COL_WEAK).size(11.0));
                    }
                    // 실행 순서대로, 순서를 못 정한 노드는 뒤에.
                    let mut order = report.order.clone();
                    order.retain(|n| m.graph.nodes.contains_key(n));
                    for n in m.graph.nodes.keys() {
                        if !order.contains(n) {
                            order.push(*n);
                        }
                    }
                    for nid in order {
                        let Some(node) = m.graph.nodes.get(&nid) else { continue };
                        let err = report.errors.get(&nid);
                        let text = RichText::new(format!("  {}", node.display_name()))
                            .color(if err.is_some() { COL_ERROR } else { ui.visuals().text_color() })
                            .size(12.0);
                        let sel = selection == Selection::Node(*id, nid);
                        let r = ui.selectable_label(sel, text);
                        let r = match err {
                            Some(e) => r.on_hover_text(e.to_string()),
                            None => r,
                        };
                        if r.clicked() {
                            out.focus = Some((*id, nid));
                        }
                        r.context_menu(|ui| {
                            if ui.button("이름 변경").clicked() {
                                out.start_rename = Some((Selection::Node(*id, nid), node.name.clone()));
                                ui.close();
                            }
                            if ui.button("삭제").clicked() {
                                out.ops.push(Op::DeleteNode { model: *id, id: nid });
                                out.select = Some(Selection::Model(*id));
                                ui.close();
                            }
                        });
                        rename_field(ui, &rename, Selection::Node(*id, nid), &mut out);
                    }
                });
                if resp.header_response.clicked() {
                    out.select = Some(Selection::Model(*id));
                    out.view = Some(View::Model);
                }
                resp.header_response.context_menu(|ui| {
                    if ui.button("이름 변경").clicked() {
                        out.start_rename = Some((Selection::Model(*id), m.name.clone()));
                        ui.close();
                    }
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeleteModel { id: *id });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
            }

            // ── 데이터셋 ────────────────────────────────────────
            ui.add_space(4.0);
            section(ui, "데이터셋", self.doc.project.datasets.len(), |ui| {
                ui.menu_button("＋", |ui| {
                    for kind in SyntheticKind::ALL {
                        if ui.button(format!("합성 · {}", kind.label())).clicked() {
                            let d = views::data::synthetic_dataset(kind, 1000);
                            out.select = Some(Selection::Dataset(d.id));
                            out.ops.push(Op::UpsertDataset { dataset: d });
                            out.view = Some(View::Data);
                            ui.close();
                        }
                    }
                    ui.separator();
                    ui.label(RichText::new("파일에서 가져오기는 데이터 뷰에서").weak().size(11.0));
                });
            });
            for (id, d) in &self.doc.project.datasets {
                let sel = selection == Selection::Dataset(*id);
                let mark = match &d.cached_info {
                    Some(i) => format!(" ({})", i.samples),
                    None => String::new(),
                };
                let r = ui.selectable_label(sel, format!("{} {}{mark}", dataset_icon(&d.source), d.name));
                let r = r.on_hover_text(views::source_label(&d.source));
                if r.clicked() {
                    out.select = Some(Selection::Dataset(*id));
                    out.view = Some(View::Data);
                }
                r.context_menu(|ui| {
                    if ui.button("이름 변경").clicked() {
                        out.start_rename = Some((Selection::Dataset(*id), d.name.clone()));
                        ui.close();
                    }
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeleteDataset { id: *id });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
                rename_field(ui, &rename, Selection::Dataset(*id), &mut out);
            }

            // ── 페이로드 ────────────────────────────────────────
            ui.add_space(4.0);
            section(ui, "페이로드", self.doc.project.payloads.len(), |ui| {
                if ui.small_button("＋").on_hover_text("빈 페이로드").clicked() {
                    let p = PayloadSpec::new(format!("페이로드 {}", self.doc.project.payloads.len() + 1));
                    out.select = Some(Selection::Payload(p.id));
                    out.ops.push(Op::UpsertPayload { payload: p });
                    out.view = Some(View::Data);
                }
            });
            for (id, p) in &self.doc.project.payloads {
                let sel = selection == Selection::Payload(*id);
                let r = ui.selectable_label(sel, format!("🔌 {}", p.name));
                if r.clicked() {
                    out.select = Some(Selection::Payload(*id));
                    out.view = Some(View::Data);
                }
                r.context_menu(|ui| {
                    if ui.button("이름 변경").clicked() {
                        out.start_rename = Some((Selection::Payload(*id), p.name.clone()));
                        ui.close();
                    }
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeletePayload { id: *id });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
                rename_field(ui, &rename, Selection::Payload(*id), &mut out);
            }

            // ── 파이프라인 ──────────────────────────────────────
            ui.add_space(4.0);
            section(ui, "파이프라인", self.doc.project.pipelines.len(), |ui| {
                if ui.small_button("＋").on_hover_text("새 파이프라인").clicked() {
                    let pl = Pipeline::new(format!("파이프라인 {}", self.doc.project.pipelines.len() + 1));
                    out.select = Some(Selection::Pipeline(pl.id));
                    out.ops.push(Op::UpsertPipelineMeta { id: pl.id, name: pl.name.clone(), tick_hz: pl.tick_hz });
                }
            });
            for (id, pl) in &self.doc.project.pipelines {
                if rename.as_ref().is_some_and(|(s, _)| *s == Selection::Pipeline(*id)) {
                    rename_field(ui, &rename, Selection::Pipeline(*id), &mut out);
                    continue;
                }
                let open = matches!(selection, Selection::Pipeline(x) | Selection::PNode(x, _) | Selection::Link(x, _) if x == *id);
                let header = egui::CollapsingHeader::new(format!("⇄ {} ({})", pl.name, pl.nodes.len()))
                    .id_salt(("pipeline", id))
                    .default_open(open);
                let resp = header.show(ui, |ui| {
                    if pl.nodes.is_empty() {
                        ui.label(RichText::new("노드 없음").color(COL_WEAK).size(11.0));
                    }
                    for n in pl.nodes.values() {
                        let sel = selection == Selection::PNode(*id, n.id);
                        let text = RichText::new(format!("  {}", crate::pcanvas::node_title(n))).size(12.0);
                        if ui.selectable_label(sel, text).on_hover_text(n.kind.label()).clicked() {
                            out.select = Some(Selection::PNode(*id, n.id));
                            out.view = Some(View::Pipeline);
                        }
                    }
                });
                if resp.header_response.clicked() {
                    out.select = Some(Selection::Pipeline(*id));
                    out.view = Some(View::Pipeline);
                }
                resp.header_response.context_menu(|ui| {
                    if ui.button("이름 변경").clicked() {
                        out.start_rename = Some((Selection::Pipeline(*id), pl.name.clone()));
                        ui.close();
                    }
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeletePipeline { id: *id });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
            }

            // ── GUI ─────────────────────────────────────────────
            ui.add_space(4.0);
            section(ui, "GUI", self.doc.project.gui.widgets.len(), |_ui| {});
            let r = ui.selectable_label(false, format!("🖼 {}", self.doc.project.gui.window.title));
            if r.on_hover_text("배포 앱 창 — 누르면 디자이너로").clicked() {
                out.view = Some(View::Gui);
            }
            for (wid, w) in &self.doc.project.gui.widgets {
                let sel = selection == Selection::Widget(*wid);
                let bound = if w.binding.is_some() { " ·" } else { "" };
                let text = RichText::new(format!("  {}{bound}", w.kind.label())).size(11.5);
                let r = ui.selectable_label(sel, text);
                let r = match &w.binding {
                    Some(b) => r.on_hover_text(binding_note(b)),
                    None => r,
                };
                if r.clicked() {
                    out.select = Some(Selection::Widget(*wid));
                    out.view = Some(View::Gui);
                }
                r.context_menu(|ui| {
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeleteWidget { id: *wid });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
            }

            // ── 실행 기록 ───────────────────────────────────────
            ui.add_space(4.0);
            section(ui, "실행 기록", self.doc.project.runs.len(), |_ui| {});
            let mut runs: Vec<_> = self.doc.project.runs.iter().collect();
            runs.sort_by_key(|a| std::cmp::Reverse(a.1.started));
            for (id, r) in runs.into_iter().take(20) {
                let sel = selection == Selection::Run(*id);
                let color = match r.status {
                    nl_core::RunStatus::Finished => COL_OK,
                    nl_core::RunStatus::Failed => COL_ERROR,
                    nl_core::RunStatus::Running => COL_WARN,
                    nl_core::RunStatus::Stopped => COL_WEAK,
                };
                let label = format!(
                    "▶ {} · {}",
                    r.started.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
                    crate::views::train::status_label(r.status)
                );
                let resp = ui.selectable_label(sel, RichText::new(label).color(color).size(11.5));
                if resp.clicked() {
                    out.select = Some(Selection::Run(*id));
                    out.view = Some(View::Train);
                }
                resp.context_menu(|ui| {
                    if ui.button("삭제").clicked() {
                        out.ops.push(Op::DeleteRun { id: *id });
                        out.select = Some(Selection::Project);
                        ui.close();
                    }
                });
            }
        });

        // ── 결과 반영 ───────────────────────────────────────────
        if out.cancel_rename {
            *self.rename_slot() = None;
        }
        if out.commit_rename {
            if let Some((sel, name)) = self.rename_slot().take() {
                let trimmed = name.trim().to_string();
                if !trimmed.is_empty() {
                    if let Some(op) = rename_op(&self.doc.project, sel, trimmed) {
                        self.doc.apply_local(vec![op]);
                    }
                }
            }
        }
        if let Some(start) = out.start_rename {
            *self.rename_slot() = Some(start);
        }
        if !out.ops.is_empty() {
            self.doc.apply_local(out.ops);
        }
        if let Some(v) = out.view {
            if v != View::Model || out.focus.is_none() {
                self.set_view_public(v);
            }
        }
        if let Some(sel) = out.select {
            self.sel.set(sel);
        }
        if let Some((model, node)) = out.focus {
            self.set_view_public(View::Model);
            self.sel.set(Selection::Node(model, node));
            self.canvas.pending_focus = Some(node);
        }
        let _ = now;
    }
}

/// 섹션 제목 줄 + 오른쪽 버튼.
fn section(ui: &mut egui::Ui, title: &str, count: usize, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{title} ({count})")).color(COL_WEAK).size(11.5));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
    });
}

/// 이름 변경 중이면 그 자리에 입력칸을 그린다. Enter 로 확정, Esc·포커스 이탈로 취소.
fn rename_field(ui: &mut egui::Ui, rename: &Option<(Selection, String)>, target: Selection, out: &mut OutlineOut) {
    let Some((sel, name)) = rename else { return };
    if *sel != target {
        return;
    }
    let mut buf = name.clone();
    let resp = ui.add(egui::TextEdit::singleline(&mut buf).desired_width(f32::INFINITY).id_salt("outline-rename"));
    if buf != *name {
        out.start_rename = Some((target, buf));
    }
    // 순서가 중요하다: TextEdit 은 Enter 를 받으면 스스로 포커스를 놓는다. 그 신호를 읽기 **전에**
    // 포커스를 다시 요청하면 `lost_focus()` 가 거짓이 되어 확정이 영영 일어나지 않는다.
    if resp.lost_focus() {
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            out.commit_rename = true;
        } else {
            // 다른 곳을 눌러 빠져나온 것 — 고치던 이름은 버린다.
            out.cancel_rename = true;
        }
    } else if ui.memory(|m| m.focused().is_none()) {
        // 처음 떴을 때만 포커스를 가져온다.
        resp.request_focus();
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        out.cancel_rename = true;
    }
}

impl NlApp {
    /// 아웃라인에서 뷰를 바꿀 때 쓰는 공개 통로 (`set_view` 는 비공개).
    pub(crate) fn set_view_public(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.canvas.cancel_interaction();
        }
    }
}

/// 위젯 바인딩을 한 줄로 (툴팁).
fn binding_note(b: &nl_core::Binding) -> String {
    match b {
        nl_core::Binding::PipelineInput { node } => format!("파이프라인 입력 → {}", node.short()),
        nl_core::Binding::PipelineOutput { node } => format!("파이프라인 출력 ← {}", node.short()),
        nl_core::Binding::ModelOutput { field, .. } => format!("모델 출력 · {field}"),
        nl_core::Binding::Action { action } => format!("내장 동작 · {action:?}"),
    }
}

/// 소스 종류만으로 데이터셋 아이콘을 고른다 (아웃라인·데이터 뷰 공용).
pub fn dataset_icon(s: &DataSource) -> &'static str {
    match s {
        DataSource::Csv { .. } => "🗄",
        DataSource::ImageFolder { .. } => "🖼",
        DataSource::Recorded { .. } => "⏺",
        DataSource::Synthetic { .. } => "✨",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_icons_differ_per_source() {
        let icons = [
            dataset_icon(&DataSource::Csv { path: String::new(), input_cols: vec![], target_cols: vec![], header: true }),
            dataset_icon(&DataSource::ImageFolder { path: String::new() }),
            dataset_icon(&DataSource::Recorded { path: String::new() }),
            dataset_icon(&DataSource::Synthetic { kind: SyntheticKind::Xor, samples: 1 }),
        ];
        let mut sorted = icons;
        sorted.sort_unstable();
        sorted.iter().zip(sorted.iter().skip(1)).for_each(|(a, b)| assert_ne!(a, b));
    }
}
