//! 데이터 뷰: 데이터셋 목록·추가·스캔·미리보기 + 페이로드 필드/Transform 편집기.
//!
//! 스캔·미리보기는 `nl_engine::{scan, preview}` 가 진실이고 여기서는 결과만 보여 준다
//! (엔진이 아직 미구현이면 오류 문구를 그대로 띄운다 — 조용히 성공한 척하지 않는다).

use super::{
    parse_shape_text, shape_text, source_kind_label, source_label, ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_SURFACE,
    COL_WARN, COL_WEAK,
};
use crate::canvas::Selection;
use crate::record::{self, RecordSession};
use eframe::egui::{self, DragValue, RichText};
use nl_core::dataset::{DataSource, DatasetSpec, Split, SyntheticKind};
use nl_core::payload::{Dtype, Field, FieldKind, PayloadSpec, Transform};
use nl_core::pipeline::Region;
use nl_core::{DatasetId, Op, PayloadId};
use std::path::PathBuf;
use std::collections::BTreeMap;

// ── 뷰 상태 ─────────────────────────────────────────────────────────

/// 미리보기 한 칸.
pub struct PreviewItem {
    /// 이미지 형상이면 썸네일.
    pub texture: Option<egui::TextureHandle>,
    /// 입력 샘플 형상.
    pub shape: Vec<usize>,
    /// 벡터 표시용 앞쪽 값들.
    pub head: Vec<f32>,
    pub target: String,
}

pub struct Preview {
    pub dataset: DatasetId,
    pub result: Result<Vec<PreviewItem>, String>,
}

/// CSV 를 고른 뒤 열을 나누는 폼.
pub struct CsvForm {
    pub path: String,
    pub name: String,
    pub header: bool,
    /// 헤더 줄에서 읽은 이름 (헤더를 끄면 번호로 바꿔 보여 준다).
    pub header_names: Vec<String>,
    pub inputs: Vec<bool>,
    pub targets: Vec<bool>,
}

impl CsvForm {
    pub fn columns(&self) -> Vec<String> {
        if self.header {
            self.header_names.clone()
        } else {
            (0..self.header_names.len()).map(|i| i.to_string()).collect()
        }
    }
}

/// 녹화 시작 전 폼.
pub struct RecordForm {
    pub name: String,
    /// 직접 고른 폴더. 비어 있으면 `<프로젝트>/recordings/<이름>`.
    pub dir: Option<PathBuf>,
    pub region: Region,
    pub fps: f32,
    /// 숫자키 0~9 에 붙일 라벨 이름.
    pub labels: Vec<String>,
}

impl Default for RecordForm {
    fn default() -> Self {
        Self {
            name: "녹화".into(),
            dir: None,
            region: Region::default(),
            // 포털 경로는 초당 1~3장이 한계라 기본값을 낮게 잡는다.
            fps: 2.0,
            labels: record::default_labels(),
        }
    }
}

#[derive(Default)]
pub struct DataState {
    /// 데이터셋별 마지막 스캔 결과.
    pub scan: BTreeMap<DatasetId, Result<String, String>>,
    pub preview: Option<Preview>,
    pub csv_form: Option<CsvForm>,
    /// 페이로드 편집기에서 펼친 필드.
    pub open_field: Option<(PayloadId, bool, usize)>,
    /// 녹화 폼 (열려 있을 때만).
    pub record_form: Option<RecordForm>,
}

// ── 뷰 ──────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut DataState) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    if let Some(rec) = ctx.recording {
        recording_panel(ui, ctx, rec, &mut actions);
        return actions;
    }
    if state.record_form.is_some() {
        record_form(ui, ctx, state, &mut actions);
        return actions;
    }
    if state.csv_form.is_some() {
        csv_form(ui, state, &mut actions);
        return actions;
    }
    ui.columns(2, |cols| {
        egui::ScrollArea::vertical().id_salt("datasets-scroll").show(&mut cols[0], |ui| {
            datasets(ui, ctx, state, &mut actions);
        });
        egui::ScrollArea::vertical().id_salt("payloads-scroll").show(&mut cols[1], |ui| {
            payloads(ui, ctx, state, &mut actions);
        });
    });
    actions
}

// ── 데이터셋 ────────────────────────────────────────────────────────

fn datasets(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut DataState, actions: &mut Vec<ViewAction>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("데이터셋").size(15.0).strong());
        ui.menu_button("＋ 추가", |ui| {
            ui.set_min_width(190.0);
            ui.label(RichText::new("합성 (바로 학습 가능)").weak());
            for kind in SyntheticKind::ALL {
                if ui.button(kind.label()).clicked() {
                    actions.push(ViewAction::AddSynthetic(kind));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("CSV 파일…").clicked() {
                actions.push(ViewAction::PickCsv);
                ui.close();
            }
            if ui.button("이미지 폴더…").clicked() {
                actions.push(ViewAction::PickImageFolder);
                ui.close();
            }
            if ui.button("녹화 폴더…").on_hover_text("이미 녹화해 둔 폴더를 가져옵니다").clicked() {
                actions.push(ViewAction::PickRecordedFolder);
                ui.close();
            }
            ui.separator();
            if ui.button("⏺ 녹화 데이터셋 만들기…").clicked() {
                actions.push(ViewAction::StartRecordForm);
                ui.close();
            }
        });
    });
    ui.separator();

    if ctx.project.datasets.is_empty() {
        ui.label(RichText::new("데이터셋이 없습니다. ＋ 추가로 만드세요.").color(COL_WEAK));
        return;
    }

    let selected = match ctx.selection {
        Selection::Dataset(id) => Some(id),
        _ => None,
    };
    for (id, d) in &ctx.project.datasets {
        let is_sel = selected == Some(*id);
        // 한 줄 전체가 선택 위젯이어야 클릭이 확실히 먹는다 (Frame + Label 조합은 히트 판정이 새어 나간다).
        let count = d.cached_info.as_ref().map(|i| format!("  ·  {}개", i.samples)).unwrap_or_default();
        let title = format!("{}  ({}){count}", d.name, source_kind_label(&d.source));
        if ui.selectable_label(is_sel, RichText::new(title).strong()).clicked() {
            actions.push(ViewAction::Select(Selection::Dataset(*id)));
        }
        ui.label(RichText::new(format!("    {}", source_label(&d.source))).color(COL_WEAK).size(11.0));
        if !is_sel {
            continue;
        }
        // 선택된 데이터셋의 도구
        ui.horizontal(|ui| {
            if ui.button("🔍 스캔").on_hover_text("샘플 수·형상·클래스를 알아내 캐시에 저장").clicked() {
                actions.push(ViewAction::ScanDataset(*id));
            }
            if ui.button("👁 미리보기").on_hover_text("앞에서 8개 샘플").clicked() {
                actions.push(ViewAction::PreviewDataset(*id));
            }
            if ui.button("🗑 삭제").clicked() {
                actions.push(ViewAction::Ops(vec![Op::DeleteDataset { id: *id }]));
                actions.push(ViewAction::Select(Selection::None));
            }
        });
        match state.scan.get(id) {
            Some(Ok(msg)) => {
                ui.label(RichText::new(format!("✔ {msg}")).color(COL_OK).size(11.5));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
            }
            None => {}
        }
        if let Some(info) = &d.cached_info {
            ui.label(
                RichText::new(format!(
                    "캐시: 입력 {} · 타깃 {} · 클래스 {}",
                    shape_text(&info.input_shape),
                    shape_text(&info.target_shape),
                    if info.classes.is_empty() { "-".into() } else { info.classes.join(", ") }
                ))
                .color(COL_WEAK)
                .size(11.0),
            );
            // 한 장도 없는 클래스가 있으면 라벨 키를 잘못 눌렀거나 폴더가 빈 것이다.
            if let Some(warn) = info.empty_class_warning() {
                ui.label(RichText::new(format!("⚠ {warn}")).color(COL_WARN).size(11.0));
            }
        }
        if let Some(p) = &state.preview {
            if p.dataset == *id {
                preview_block(ui, p);
            }
        }
        ui.add_space(6.0);
    }
}

fn preview_block(ui: &mut egui::Ui, p: &Preview) {
    ui.separator();
    match &p.result {
        Err(e) => {
            ui.label(RichText::new(format!("미리보기 실패: {e}")).color(COL_ERROR).size(11.5));
        }
        Ok(items) if items.is_empty() => {
            ui.label(RichText::new("샘플이 없습니다").color(COL_WARN));
        }
        Ok(items) => {
            let images = items.iter().any(|i| i.texture.is_some());
            if images {
                ui.horizontal_wrapped(|ui| {
                    for item in items {
                        ui.vertical(|ui| {
                            if let Some(t) = &item.texture {
                                ui.add(egui::Image::new(t).fit_to_exact_size(egui::Vec2::splat(64.0)));
                            }
                            ui.label(RichText::new(&item.target).size(10.5));
                        });
                    }
                });
            } else {
                egui::Grid::new("preview-grid").striped(true).show(ui, |ui| {
                    ui.label(RichText::new("입력").color(COL_WEAK).size(11.0));
                    ui.label(RichText::new("타깃").color(COL_WEAK).size(11.0));
                    ui.end_row();
                    for item in items {
                        let head: Vec<String> = item.head.iter().take(6).map(|v| format!("{v:.3}")).collect();
                        let more = if item.head.len() > 6 { " …" } else { "" };
                        ui.label(RichText::new(format!("[{}]{more}", head.join(", "))).size(11.0));
                        ui.label(RichText::new(&item.target).size(11.0));
                        ui.end_row();
                    }
                });
            }
            ui.label(
                RichText::new(format!("샘플 형상 {}", shape_text(&items[0].shape))).color(COL_WEAK).size(11.0),
            );
        }
    }
}

/// CSV 를 고른 직후의 열 선택 폼. 폼을 잠시 꺼내 그리고 취소·생성이 아니면 되돌려 놓는다.
fn csv_form(ui: &mut egui::Ui, state: &mut DataState, actions: &mut Vec<ViewAction>) {
    let Some(mut form) = state.csv_form.take() else { return };
    let mut close = false;
    ui.heading("CSV 데이터셋 만들기");
    ui.add_space(4.0);
    ui.label(RichText::new(&form.path).color(COL_WEAK).size(11.5));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("이름");
        ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(260.0));
    });
    ui.checkbox(&mut form.header, "첫 줄이 열 이름(헤더)");
    ui.add_space(6.0);
    ui.label(RichText::new("입력 · 타깃 열 고르기").strong());
    let columns = form.columns();
    egui::ScrollArea::vertical().max_height(320.0).id_salt("csv-cols-scroll").show(ui, |ui| {
        egui::Grid::new("csv-cols").striped(true).show(ui, |ui| {
            ui.label(RichText::new("열").color(COL_WEAK));
            ui.label(RichText::new("입력").color(COL_WEAK));
            ui.label(RichText::new("타깃").color(COL_WEAK));
            ui.end_row();
            for (i, name) in columns.iter().enumerate() {
                ui.label(name);
                let mut inp = form.inputs.get(i).copied().unwrap_or(false);
                if ui.checkbox(&mut inp, "").changed() {
                    form.inputs[i] = inp;
                    if inp {
                        form.targets[i] = false;
                    }
                }
                let mut tgt = form.targets.get(i).copied().unwrap_or(false);
                if ui.checkbox(&mut tgt, "").changed() {
                    form.targets[i] = tgt;
                    if tgt {
                        form.inputs[i] = false;
                    }
                }
                ui.end_row();
            }
        });
    });
    ui.add_space(8.0);
    let ok = form.inputs.iter().any(|v| *v) && form.targets.iter().any(|v| *v);
    let mut create = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(ok, |ui| {
            create = ui.button("만들기").clicked();
        });
        if !ok {
            ui.label(RichText::new("입력과 타깃을 각각 하나 이상 고르세요").color(COL_WARN).size(11.5));
        }
        cancel = ui.button("취소").clicked();
    });
    if create {
        let cols = form.columns();
        let pick = |flags: &[bool]| -> Vec<String> {
            cols.iter().zip(flags).filter(|(_, f)| **f).map(|(c, _)| c.clone()).collect()
        };
        let spec = DatasetSpec::new(
            form.name.clone(),
            DataSource::Csv {
                path: form.path.clone(),
                input_cols: pick(&form.inputs),
                target_cols: pick(&form.targets),
                header: form.header,
            },
        );
        let id = spec.id;
        actions.push(ViewAction::Ops(vec![Op::UpsertDataset { dataset: spec }]));
        actions.push(ViewAction::Select(Selection::Dataset(id)));
        close = true;
    }
    if cancel {
        close = true;
    }
    if !close {
        state.csv_form = Some(form);
    }
}

// ── 녹화 ────────────────────────────────────────────────────────────

/// 녹화 시작 폼. 폴더·영역·fps·라벨 키를 정한다.
fn record_form(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut DataState, actions: &mut Vec<ViewAction>) {
    let Some(mut form) = state.record_form.take() else { return };
    let mut close = false;

    ui.heading("녹화 데이터셋 만들기");
    ui.add_space(4.0);
    ui.label(
        RichText::new("화면을 정해진 빠르기로 찍어 프레임과 라벨을 함께 저장합니다. 정지하면 데이터셋이 됩니다.")
            .color(COL_WEAK)
            .size(11.5),
    );
    ui.add_space(8.0);

    egui::Grid::new("record-form").num_columns(2).spacing([12.0, 5.0]).show(ui, |ui| {
        ui.label(RichText::new("이름").color(COL_WEAK));
        ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(240.0));
        ui.end_row();

        ui.label(RichText::new("폴더").color(COL_WEAK));
        ui.horizontal(|ui| {
            let shown = match (&form.dir, ctx.base_dir) {
                (Some(d), _) => d.display().to_string(),
                (None, Some(base)) => record::default_dir(base, &form.name).display().to_string(),
                (None, None) => "(프로젝트를 먼저 저장하거나 폴더를 고르세요)".into(),
            };
            ui.label(RichText::new(shown).size(11.0));
            if ui.small_button("고르기…").clicked() {
                actions.push(ViewAction::PickRecordDir);
            }
            if form.dir.is_some() && ui.small_button("기본값").clicked() {
                form.dir = None;
            }
        });
        ui.end_row();

        ui.label(RichText::new("초당 프레임").color(COL_WEAK));
        ui.add(DragValue::new(&mut form.fps).range(0.2..=60.0).speed(0.2));
        ui.end_row();
    });

    ui.add_space(6.0);
    ui.label(RichText::new("캡처 영역").strong());
    crate::views::pipeline::region_editor(ui, &mut form.region, ctx);

    // 백엔드는 한 장 찍어 봐야 확실히 알 수 있다.
    ui.add_space(6.0);
    shot_block(ui, ctx, form.region, actions);

    ui.add_space(8.0);
    ui.label(RichText::new("라벨 키").strong());
    ui.label(
        RichText::new("녹화 중 빌더 창에서 숫자키를 누르면 그때부터 그 라벨로 기록됩니다. 이름은 비워 둬도 됩니다.")
            .color(COL_WEAK)
            .size(11.0),
    );
    egui::Grid::new("record-labels").num_columns(2).spacing([10.0, 3.0]).show(ui, |ui| {
        for (i, label) in form.labels.iter_mut().enumerate() {
            ui.label(RichText::new(format!("{i}")).color(COL_WEAK).size(11.5));
            ui.add(egui::TextEdit::singleline(label).desired_width(200.0).hint_text("라벨 이름"));
            ui.end_row();
        }
    });

    ui.add_space(10.0);
    let dir = match (&form.dir, ctx.base_dir) {
        (Some(d), _) => Some(d.clone()),
        (None, Some(base)) => Some(record::default_dir(base, &form.name)),
        (None, None) => None,
    };
    let mut start = false;
    let mut cancel = false;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(dir.is_some(), |ui| {
            start = ui.button(RichText::new("⏺ 녹화 시작").color(COL_ERROR)).clicked();
        });
        if dir.is_none() {
            ui.label(RichText::new("프로젝트를 저장하거나 폴더를 고르세요").color(COL_WARN).size(11.5));
        }
        cancel = ui.button("취소").clicked();
    });

    if start {
        if let Some(dir) = dir {
            actions.push(ViewAction::StartRecording {
                dir,
                name: form.name.clone(),
                region: form.region,
                fps: form.fps,
                labels: form.labels.clone(),
            });
            close = true;
        }
    }
    if cancel {
        close = true;
    }
    if !close {
        state.record_form = Some(form);
    }
}

/// "지금 한 장 캡처" 버튼 + 썸네일 + 백엔드 힌트. 캡처 소스 인스펙터도 같은 블록을 쓴다.
pub(crate) fn shot_block(ui: &mut egui::Ui, ctx: &ViewCtx, region: Region, actions: &mut Vec<ViewAction>) {
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!ctx.shot.busy, |ui| {
            if ui.button("📷 지금 한 장 캡처").clicked() {
                actions.push(ViewAction::CaptureShot(region));
            }
        });
        if ctx.shot.busy {
            ui.label(RichText::new("찍는 중… (포털이면 권한 창이 뜰 수 있습니다)").color(COL_WEAK).size(11.0));
        }
        if let Some(b) = ctx.shot.backend {
            ui.label(RichText::new(b.label()).color(COL_OK).size(11.0)).on_hover_text(b.hint());
        }
    });
    if let Some(e) = &ctx.shot.error {
        ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.0));
    }
    if ctx.shot.backend == Some(nl_io::Backend::Portal) {
        ui.label(
            RichText::new("포털 경로라 초당 1~3장이 한계입니다 — fps 를 낮게 잡으세요").color(COL_WARN).size(11.0),
        );
    }
    if let Some(t) = &ctx.shot.texture {
        let (w, h) = ctx.shot.size;
        let side = 260.0;
        let scale = (side / w.max(1) as f32).min(side / h.max(1) as f32).min(1.0);
        ui.add(egui::Image::new(t).fit_to_exact_size(egui::Vec2::new(w as f32 * scale, h as f32 * scale)));
        ui.label(RichText::new(format!("{w}×{h}")).color(COL_WEAK).size(11.0));
    }
}

/// 녹화 중 화면: 미리보기 · 프레임/드롭 수 · 현재 라벨 · 정지.
fn recording_panel(ui: &mut egui::Ui, ctx: &ViewCtx, rec: &RecordSession, actions: &mut Vec<ViewAction>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("⏺ 녹화 중").color(COL_ERROR).size(16.0).strong());
        ui.label(RichText::new(&rec.name).strong());
        if ui.button(RichText::new("⏹ 정지하고 데이터셋 만들기").color(COL_OK)).clicked() {
            actions.push(ViewAction::StopRecording);
        }
    });
    ui.label(RichText::new(rec.dir.display().to_string()).color(COL_WEAK).size(11.0));
    ui.separator();

    let elapsed = (ctx.now - rec.started_at).max(0.0);
    ui.horizontal_wrapped(|ui| {
        super::kv(ui, "경과", super::fmt_duration(elapsed));
        super::kv(ui, "프레임", rec.frames().to_string());
        super::kv(ui, "버린 프레임", rec.dropped().to_string());
        super::kv(ui, "목표 fps", format!("{:.1}", rec.fps));
        let actual = if elapsed > 0.5 { rec.frames() as f64 / elapsed } else { 0.0 };
        super::kv(ui, "실제 fps", format!("{actual:.1}"));
    });
    if let Some(b) = rec.backend {
        ui.label(RichText::new(format!("백엔드 {}", b.label())).color(COL_WEAK).size(11.0)).on_hover_text(b.hint());
        if b == nl_io::Backend::Portal {
            ui.label(RichText::new("포털 경로라 초당 1~3장이 한계입니다").color(COL_WARN).size(11.0));
        }
    }
    if let Some(e) = rec.error() {
        ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
    }

    ui.add_space(6.0);
    let current = rec.label();
    ui.label(RichText::new(format!("현재 라벨: {} ({current})", rec.label_name(current))).size(14.0).strong());
    ui.label(
        RichText::new("빌더 창에 포커스가 있을 때 숫자키 0~9 로 라벨을 바꿉니다 (텍스트 칸에 커서가 있으면 무시).")
            .color(COL_WEAK)
            .size(11.0),
    );
    ui.horizontal_wrapped(|ui| {
        for (i, name) in rec.labels.iter().enumerate() {
            let on = current == i as i64;
            let text = if name.trim().is_empty() { format!("{i}") } else { format!("{i} {name}") };
            ui.label(RichText::new(text).color(if on { COL_OK } else { COL_WEAK }).size(11.5));
        }
    });

    ui.add_space(8.0);
    match &rec.texture {
        Some(t) => {
            let side = 420.0;
            let [w, h] = t.size();
            let scale = (side / w.max(1) as f32).min(side / h.max(1) as f32).min(1.0);
            ui.add(egui::Image::new(t).fit_to_exact_size(egui::Vec2::new(w as f32 * scale, h as f32 * scale)));
        }
        None => {
            ui.label(RichText::new("첫 프레임을 기다리는 중…").color(COL_WEAK));
        }
    }
}

// ── 페이로드 ────────────────────────────────────────────────────────

fn payloads(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut DataState, actions: &mut Vec<ViewAction>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("페이로드").size(15.0).strong());
        ui.menu_button("＋ 프리셋", |ui| {
            ui.set_min_width(200.0);
            if ui.button("이미지 분류 (28×28 → 클래스)").clicked() {
                let p = PayloadSpec::image_classifier(
                    "이미지 분류",
                    28,
                    28,
                    (0..10).map(|i| i.to_string()).collect(),
                );
                let id = p.id;
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: p }]));
                actions.push(ViewAction::Select(Selection::Payload(id)));
                ui.close();
            }
            if ui.button("표 데이터 (벡터 → 벡터)").clicked() {
                let p = PayloadSpec::tabular("표 데이터", 4, 1);
                let id = p.id;
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: p }]));
                actions.push(ViewAction::Select(Selection::Payload(id)));
                ui.close();
            }
            if ui.button("빈 페이로드").clicked() {
                let p = PayloadSpec::new("페이로드");
                let id = p.id;
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: p }]));
                actions.push(ViewAction::Select(Selection::Payload(id)));
                ui.close();
            }
        });
    });
    ui.separator();

    if ctx.project.payloads.is_empty() {
        ui.label(RichText::new("페이로드가 없습니다. 모델 입출력과 바깥 데이터를 잇는 계약입니다.").color(COL_WEAK));
        return;
    }
    let selected = match ctx.selection {
        Selection::Payload(id) => Some(id),
        _ => None,
    };
    for (id, p) in &ctx.project.payloads {
        let is_sel = selected == Some(*id);
        if ui.selectable_label(is_sel, format!("{} (입력 {} · 출력 {})", p.name, p.inputs.len(), p.outputs.len())).clicked() {
            actions.push(ViewAction::Select(Selection::Payload(*id)));
        }
        if is_sel {
            payload_editor(ui, p, state, actions);
        }
    }
}

fn payload_editor(ui: &mut egui::Ui, p: &PayloadSpec, state: &mut DataState, actions: &mut Vec<ViewAction>) {
    egui::Frame::NONE.fill(COL_SURFACE).inner_margin(8).corner_radius(4).show(ui, |ui| {
        for outputs in [false, true] {
            ui.label(RichText::new(if outputs { "출력 필드" } else { "입력 필드" }).strong());
            let list = if outputs { &p.outputs } else { &p.inputs };
            for i in 0..list.len() {
                field_row(ui, p, outputs, i, state, actions);
            }
            if ui.small_button(if outputs { "＋ 출력 필드" } else { "＋ 입력 필드" }).clicked() {
                let mut next = p.clone();
                let f = Field::new(format!("f{}", list.len() + 1), FieldKind::Scalar);
                if outputs {
                    next.outputs.push(f);
                } else {
                    next.inputs.push(f);
                }
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: next }]));
            }
            ui.add_space(6.0);
        }
        if ui.button("🗑 페이로드 삭제").clicked() {
            actions.push(ViewAction::Ops(vec![Op::DeletePayload { id: p.id }]));
            actions.push(ViewAction::Select(Selection::None));
        }
    });
}

fn field_row(
    ui: &mut egui::Ui,
    p: &PayloadSpec,
    outputs: bool,
    index: usize,
    state: &mut DataState,
    actions: &mut Vec<ViewAction>,
) {
    let list = if outputs { &p.outputs } else { &p.inputs };
    let Some(field) = list.get(index) else { return };
    let key = (p.id, outputs, index);
    let open = state.open_field == Some(key);
    ui.horizontal(|ui| {
        if ui.selectable_label(open, format!("{} · {}", field.name, field_kind_label(&field.kind))).clicked() {
            state.open_field = if open { None } else { Some(key) };
        }
        if let Some(s) = field.tensor_shape() {
            ui.label(RichText::new(shape_text(&s)).color(COL_WEAK).size(10.5));
        }
        ui.add_enabled_ui(index > 0, |ui| {
            if ui.small_button("▲").on_hover_text("위로").clicked() {
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: swapped(p, outputs, index, index - 1) }]));
            }
        });
        ui.add_enabled_ui(index + 1 < list.len(), |ui| {
            if ui.small_button("▼").on_hover_text("아래로").clicked() {
                actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: swapped(p, outputs, index, index + 1) }]));
            }
        });
        if ui.small_button("✖").on_hover_text("필드 삭제").clicked() {
            let mut next = p.clone();
            if outputs {
                next.outputs.remove(index);
            } else {
                next.inputs.remove(index);
            }
            actions.push(ViewAction::Ops(vec![Op::UpsertPayload { payload: next }]));
            state.open_field = None;
        }
    });
    if !open {
        return;
    }
    let mut next = p.clone();
    let Some(f) = (if outputs { next.outputs.get_mut(index) } else { next.inputs.get_mut(index) }) else { return };
    let mut changed = false;
    ui.indent(("field", index), |ui| {
        ui.horizontal(|ui| {
            ui.label("이름");
            changed |= ui.add(egui::TextEdit::singleline(&mut f.name).desired_width(120.0)).changed();
        });
        changed |= field_kind_editor(ui, &mut f.kind);
        changed |= transform_chain(ui, "인코드 (바깥 → 텐서)", &mut f.encode, index * 2);
        changed |= transform_chain(ui, "디코드 (텐서 → 바깥)", &mut f.decode, index * 2 + 1);
    });
    if changed {
        actions.push(ViewAction::Edit(vec![Op::UpsertPayload { payload: next }]));
    }
}

fn swapped(p: &PayloadSpec, outputs: bool, a: usize, b: usize) -> PayloadSpec {
    let mut next = p.clone();
    if outputs {
        next.outputs.swap(a, b);
    } else {
        next.inputs.swap(a, b);
    }
    next
}

pub fn field_kind_label(k: &FieldKind) -> &'static str {
    match k {
        FieldKind::Tensor { .. } => "텐서",
        FieldKind::Image { .. } => "이미지",
        FieldKind::Scalar => "스칼라",
        FieldKind::Vector { .. } => "벡터",
        FieldKind::ClassLabel { .. } => "클래스 라벨",
        FieldKind::Text => "텍스트",
        FieldKind::Json => "JSON",
    }
}

fn field_kind_palette() -> Vec<FieldKind> {
    vec![
        FieldKind::Scalar,
        FieldKind::Vector { len: 4 },
        FieldKind::Tensor { shape: vec![1], dtype: Dtype::F32 },
        FieldKind::Image { width: 28, height: 28, channels: 3 },
        FieldKind::ClassLabel { labels: vec!["a".into(), "b".into()] },
        FieldKind::Text,
        FieldKind::Json,
    ]
}

fn field_kind_editor(ui: &mut egui::Ui, kind: &mut FieldKind) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("종류");
        egui::ComboBox::from_id_salt(("field-kind", ui.id())).selected_text(field_kind_label(kind)).show_ui(ui, |ui| {
            for k in field_kind_palette() {
                if ui.selectable_label(std::mem::discriminant(kind) == std::mem::discriminant(&k), field_kind_label(&k)).clicked()
                    && std::mem::discriminant(kind) != std::mem::discriminant(&k)
                {
                    *kind = k;
                    changed = true;
                }
            }
        });
    });
    match kind {
        FieldKind::Tensor { shape, dtype } => {
            let mut text = shape_text(shape);
            ui.horizontal(|ui| {
                ui.label("형상");
                if ui.add(egui::TextEdit::singleline(&mut text).desired_width(110.0)).changed() {
                    if let Some(s) = parse_shape_text(&text) {
                        *shape = s;
                        changed = true;
                    }
                }
                egui::ComboBox::from_id_salt(("dtype", ui.id())).selected_text(format!("{dtype:?}")).show_ui(ui, |ui| {
                    for d in [Dtype::F32, Dtype::I64, Dtype::U8, Dtype::Bool] {
                        if ui.selectable_label(*dtype == d, format!("{d:?}")).clicked() {
                            *dtype = d;
                            changed = true;
                        }
                    }
                });
            });
        }
        FieldKind::Image { width, height, channels } => {
            ui.horizontal(|ui| {
                ui.label("폭");
                changed |= ui.add(egui::DragValue::new(width).range(1..=8192)).changed();
                ui.label("높이");
                changed |= ui.add(egui::DragValue::new(height).range(1..=8192)).changed();
                ui.label("채널");
                changed |= ui.add(egui::DragValue::new(channels).range(1..=4)).changed();
            });
        }
        FieldKind::Vector { len } => {
            ui.horizontal(|ui| {
                ui.label("길이");
                changed |= ui.add(egui::DragValue::new(len).range(1..=1_000_000)).changed();
            });
        }
        FieldKind::ClassLabel { labels } => {
            let mut text = labels.join(", ");
            ui.horizontal(|ui| {
                ui.label("클래스");
                if ui.add(egui::TextEdit::singleline(&mut text).desired_width(200.0)).changed() {
                    *labels = text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    changed = true;
                }
            });
        }
        FieldKind::Scalar | FieldKind::Text | FieldKind::Json => {}
    }
    changed
}

/// Transform 팔레트 (편집기 콤보와 같은 순서).
pub fn transform_palette() -> Vec<Transform> {
    vec![
        Transform::Resize { width: 28, height: 28 },
        Transform::Grayscale,
        Transform::Crop { x: 0, y: 0, width: 28, height: 28 },
        Transform::Normalize { mean: vec![0.5], std: vec![0.5] },
        Transform::Scale { min: 0.0, max: 255.0 },
        Transform::OneHot { classes: 10 },
        Transform::Argmax,
        Transform::Softmax,
        Transform::Threshold { value: 0.5 },
        Transform::MapLabel,
        Transform::JsonPointer { pointer: "/value".into() },
        Transform::Tokenize { vocab: "abcdefghijklmnopqrstuvwxyz ".into(), max_len: 32 },
    ]
}

pub fn transform_label(t: &Transform) -> &'static str {
    match t {
        Transform::Resize { .. } => "크기 변경",
        Transform::Grayscale => "흑백",
        Transform::Crop { .. } => "잘라내기",
        Transform::Normalize { .. } => "정규화",
        Transform::Scale { .. } => "범위 변환",
        Transform::OneHot { .. } => "원핫",
        Transform::Argmax => "argmax",
        Transform::Softmax => "softmax",
        Transform::Threshold { .. } => "임계값",
        Transform::MapLabel => "라벨 이름",
        Transform::JsonPointer { .. } => "JSON 포인터",
        Transform::Tokenize { .. } => "문자 토큰화",
    }
}

fn transform_chain(ui: &mut egui::Ui, title: &str, chain: &mut Vec<Transform>, salt: usize) -> bool {
    let mut changed = false;
    ui.label(RichText::new(title).color(COL_WEAK).size(11.5));
    let mut remove: Option<usize> = None;
    let mut swap: Option<(usize, usize)> = None;
    let len = chain.len();
    for (i, t) in chain.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}.", i + 1)).color(COL_WEAK).size(11.0));
            ui.label(transform_label(t));
            changed |= transform_params(ui, t, salt * 100 + i);
            ui.add_enabled_ui(i > 0, |ui| {
                if ui.small_button("▲").clicked() {
                    swap = Some((i, i - 1));
                }
            });
            ui.add_enabled_ui(i + 1 < len, |ui| {
                if ui.small_button("▼").clicked() {
                    swap = Some((i, i + 1));
                }
            });
            if ui.small_button("✖").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some((a, b)) = swap {
        chain.swap(a, b);
        changed = true;
    }
    if let Some(i) = remove {
        chain.remove(i);
        changed = true;
    }
    ui.menu_button("＋ 단계", |ui| {
        ui.set_min_width(140.0);
        for t in transform_palette() {
            if ui.button(transform_label(&t)).clicked() {
                chain.push(t);
                changed = true;
                ui.close();
            }
        }
    });
    changed
}

fn transform_params(ui: &mut egui::Ui, t: &mut Transform, salt: usize) -> bool {
    let mut changed = false;
    match t {
        Transform::Resize { width, height } | Transform::Crop { width, height, .. } => {
            changed |= ui.add(egui::DragValue::new(width).prefix("w ").range(1..=8192)).changed();
            changed |= ui.add(egui::DragValue::new(height).prefix("h ").range(1..=8192)).changed();
        }
        Transform::Normalize { mean, std } => {
            let mut m = mean.first().copied().unwrap_or(0.0);
            let mut s = std.first().copied().unwrap_or(1.0);
            if ui.add(egui::DragValue::new(&mut m).prefix("mean ").speed(0.01)).changed() {
                *mean = vec![m];
                changed = true;
            }
            if ui.add(egui::DragValue::new(&mut s).prefix("std ").speed(0.01)).changed() {
                *std = vec![s.max(1e-6)];
                changed = true;
            }
        }
        Transform::Scale { min, max } => {
            changed |= ui.add(egui::DragValue::new(min).prefix("min ").speed(0.5)).changed();
            changed |= ui.add(egui::DragValue::new(max).prefix("max ").speed(0.5)).changed();
        }
        Transform::OneHot { classes } => {
            changed |= ui.add(egui::DragValue::new(classes).prefix("n ").range(1..=100_000)).changed();
        }
        Transform::Threshold { value } => {
            changed |= ui.add(egui::DragValue::new(value).speed(0.01)).changed();
        }
        Transform::JsonPointer { pointer } => {
            changed |= ui
                .add(egui::TextEdit::singleline(pointer).desired_width(110.0).id_salt(("ptr", salt)))
                .changed();
        }
        Transform::Tokenize { vocab, max_len } => {
            changed |= ui
                .add(egui::TextEdit::singleline(vocab).desired_width(110.0).id_salt(("vocab", salt)))
                .changed();
            changed |= ui.add(egui::DragValue::new(max_len).prefix("len ").range(1..=4096)).changed();
        }
        Transform::Grayscale | Transform::Argmax | Transform::Softmax | Transform::MapLabel => {}
    }
    changed
}

/// 합성 데이터셋 하나를 만든다 (앱의 `AddSynthetic` 처리와 테스트가 공유).
pub fn synthetic_dataset(kind: SyntheticKind, samples: usize) -> DatasetSpec {
    let mut d = DatasetSpec::new(format!("{} 합성", kind.label()), DataSource::Synthetic { kind, samples });
    d.split = Split::Ratio;
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_dataset_carries_its_kind_and_size() {
        let d = synthetic_dataset(SyntheticKind::Xor, 500);
        assert!(matches!(d.source, DataSource::Synthetic { kind: SyntheticKind::Xor, samples: 500 }));
        assert!(d.name.contains("XOR"));
    }

    #[test]
    fn csv_form_columns_follow_the_header_toggle() {
        let mut f = CsvForm {
            path: "/t/a.csv".into(),
            name: "a".into(),
            header: true,
            header_names: vec!["x".into(), "y".into(), "label".into()],
            inputs: vec![true, true, false],
            targets: vec![false, false, true],
        };
        assert_eq!(f.columns(), vec!["x", "y", "label"]);
        f.header = false;
        assert_eq!(f.columns(), vec!["0", "1", "2"]);
    }

    #[test]
    fn transform_palette_covers_every_variant_once() {
        let p = transform_palette();
        let mut labels: Vec<&str> = p.iter().map(transform_label).collect();
        labels.sort_unstable();
        let n = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), n, "팔레트에 같은 Transform 이 두 번");
        assert_eq!(n, 12, "새 Transform 변형을 팔레트에 추가할 것");
    }

    #[test]
    fn field_kind_palette_has_one_of_each() {
        let p = field_kind_palette();
        assert_eq!(p.len(), 7);
        let mut labels: Vec<&str> = p.iter().map(field_kind_label).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 7);
    }
}
