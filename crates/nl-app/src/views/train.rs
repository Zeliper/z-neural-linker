//! 학습 뷰: 모델·데이터셋 선택 → 시작/일시정지/정지 → 손실 플롯 · 진행 바 · 실행 기록 표.
//!
//! 학습은 `nl_engine::start` 가 만든 별도 스레드에서 돌고 UI 스레드는 이벤트만 받는다
//! (GPU 계산 컨텍스트와 GUI 렌더 컨텍스트가 다르므로 — `docs/ARCHITECTURE.md` 플랫폼 주의사항).

use super::{
    fmt_duration, fmt_metric, ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_SELECT, COL_SURFACE, COL_WARN, COL_WEAK,
};
use crate::canvas::Selection;
use eframe::egui::{self, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoints};
use nl_core::{DatasetId, EpochMetrics, ModelId, RunId, RunRecord, RunStatus};
use nl_engine::{TrainEvent, TrainHandle, TrainRequest};
use std::path::PathBuf;

// ── 진행 중인 학습 ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Paused,
    Finished,
    Failed,
}

impl SessionState {
    pub fn is_live(&self) -> bool {
        matches!(self, SessionState::Running | SessionState::Paused)
    }
}

/// 학습 한 번의 UI 쪽 상태. 이벤트를 받아 쌓아 두고 플롯·진행 바가 읽는다.
pub struct TrainSession {
    pub handle: TrainHandle,
    pub model: ModelId,
    pub dataset: DatasetId,
    /// 지금까지의 기록. 끝나면 그대로 `Op::UpsertRun` 으로 문서에 들어간다.
    pub run: RunRecord,
    /// (전역 step, 손실) — 얇은 선.
    pub steps: Vec<[f64; 2]>,
    pub batches_per_epoch: usize,
    pub params: usize,
    pub device: String,
    pub state: SessionState,
    pub error: Option<String>,
    pub started_at: f64,
    pub elapsed: f64,
    pub run_dir: PathBuf,
    /// 마지막 step 이벤트의 epoch·step (진행 바).
    pub last_step: (usize, usize),
}

/// `poll` 한 번의 결과.
#[derive(Default)]
pub struct TrainPoll {
    /// 무언가 들어와 다시 그려야 한다.
    pub changed: bool,
    /// 끝났다 — 이 기록을 문서에 넣는다.
    pub finished: Option<Box<RunRecord>>,
    /// 도크 로그로 보낼 줄.
    pub logs: Vec<String>,
}

/// 스텝 플롯이 무한정 커지지 않도록 유지하는 점 개수.
const MAX_STEP_POINTS: usize = 4000;
/// 학습률 곡선의 높이(px). 손실 곡선(240)보다 낮게 둬 보조 정보임을 드러낸다.
const LR_PLOT_HEIGHT: f32 = 110.0;

/// 학습률 표기. 1e-3 처럼 작은 값이라 고정 소수점으로는 0.001 이 0.00 으로 뭉개진다.
fn fmt_lr(v: f64) -> String {
    if v == 0.0 {
        "0".into()
    } else if v < 0.001 {
        format!("{v:.2e}")
    } else {
        format!("{v:.5}")
    }
}

impl TrainSession {
    pub fn start(req: TrainRequest, run_dir: PathBuf, now: f64) -> anyhow::Result<Self> {
        let run = RunRecord {
            id: req.run_id,
            model: req.model.id,
            dataset: Some(req.dataset.id),
            config: req.model.train.clone(),
            started: chrono::Utc::now(),
            finished: None,
            status: RunStatus::Running,
            device_name: String::new(),
            epochs: Vec::new(),
            checkpoint: None,
            best_checkpoint: None,
            error: None,
            note: String::new(),
        };
        let (model, dataset) = (req.model.id, req.dataset.id);
        let handle = nl_engine::start(req)?;
        Ok(Self {
            handle,
            model,
            dataset,
            run,
            steps: Vec::new(),
            batches_per_epoch: 0,
            params: 0,
            device: String::new(),
            state: SessionState::Running,
            error: None,
            started_at: now,
            elapsed: 0.0,
            run_dir,
            last_step: (0, 0),
        })
    }

    /// 매 프레임 채널을 비운다. 블록하지 않는다.
    pub fn poll(&mut self, now: f64) -> TrainPoll {
        let mut out = TrainPoll::default();
        if self.state.is_live() {
            self.elapsed = now - self.started_at;
            out.changed = true;
        }
        while let Ok(ev) = self.handle.events.try_recv() {
            out.changed = true;
            match ev {
                TrainEvent::Started {
                    device,
                    batches_per_epoch,
                    params,
                } => {
                    self.device = device.clone();
                    self.run.device_name = device;
                    self.batches_per_epoch = batches_per_epoch;
                    self.params = params;
                    out.logs.push(format!(
                        "학습 시작 — {} · 에포크당 {batches_per_epoch} 배치 · 파라미터 {params}개",
                        self.run.device_name
                    ));
                }
                TrainEvent::Step { epoch, step, loss } => {
                    self.last_step = (epoch, step);
                    let x = global_step(epoch, step, self.batches_per_epoch);
                    self.steps.push([x, loss]);
                    if self.steps.len() > MAX_STEP_POINTS {
                        // 앞쪽을 솎아 내 모양은 유지하고 개수만 줄인다.
                        let kept: Vec<[f64; 2]> = self.steps.iter().step_by(2).copied().collect();
                        self.steps = kept;
                    }
                }
                TrainEvent::Epoch(m) => {
                    if let Some(slot) = self.run.epochs.iter_mut().find(|e| e.epoch == m.epoch) {
                        *slot = m;
                    } else {
                        self.run.epochs.push(m);
                    }
                }
                TrainEvent::Checkpoint { path } => {
                    self.run.checkpoint = Some(path.display().to_string());
                    out.logs.push(format!("체크포인트: {}", path.display()));
                }
                TrainEvent::Log(msg) => out.logs.push(msg),
                TrainEvent::Finished { run } => {
                    self.state = SessionState::Finished;
                    self.run = merge_final(std::mem::replace(&mut self.run, blank_run()), run);
                    out.logs.push(format!("학습 종료 — {:?}", self.run.status));
                    out.finished = Some(Box::new(self.run.clone()));
                }
                TrainEvent::Failed { run, error } => {
                    self.state = SessionState::Failed;
                    self.error = Some(error.clone());
                    self.run = merge_final(std::mem::replace(&mut self.run, blank_run()), run);
                    self.run.status = RunStatus::Failed;
                    self.run.error = Some(error.clone());
                    out.logs.push(format!("학습 실패: {error}"));
                    out.finished = Some(Box::new(self.run.clone()));
                }
            }
        }
        if self.state.is_live() {
            self.state = if self.handle.is_paused() {
                SessionState::Paused
            } else {
                SessionState::Running
            };
        }
        out
    }

    /// 0.0..=1.0. 에포크 수를 모르면 `None`.
    pub fn progress(&self) -> Option<f32> {
        let total = self.run.config.epochs;
        if total == 0 {
            return None;
        }
        let done = self.run.epochs.len() as f64;
        let within = if self.batches_per_epoch > 0 {
            (self.last_step.1 as f64 / self.batches_per_epoch as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Some(((done + within) / total as f64).clamp(0.0, 1.0) as f32)
    }
}

fn blank_run() -> RunRecord {
    RunRecord {
        id: RunId::from_u128(0),
        model: ModelId::from_u128(0),
        dataset: None,
        config: Default::default(),
        started: chrono::Utc::now(),
        finished: None,
        status: RunStatus::Stopped,
        device_name: String::new(),
        epochs: Vec::new(),
        checkpoint: None,
        best_checkpoint: None,
        error: None,
        note: String::new(),
    }
}

/// 엔진이 준 최종 기록을 우선하되, 비어 있는 칸은 UI 가 모아 둔 값으로 채운다.
/// (엔진 구현에 따라 epochs 를 최종 기록에 다시 담지 않을 수 있다)
pub fn merge_final(ui: RunRecord, engine: RunRecord) -> RunRecord {
    let mut r = engine;
    if r.epochs.is_empty() {
        r.epochs = ui.epochs;
    }
    if r.device_name.is_empty() {
        r.device_name = ui.device_name;
    }
    if r.checkpoint.is_none() {
        r.checkpoint = ui.checkpoint;
    }
    if r.best_checkpoint.is_none() {
        r.best_checkpoint = ui.best_checkpoint;
    }
    if r.finished.is_none() {
        r.finished = Some(chrono::Utc::now());
    }
    r
}

/// 스텝 이벤트를 하나의 x 축으로 편다.
pub fn global_step(epoch: usize, step: usize, batches_per_epoch: usize) -> f64 {
    (epoch * batches_per_epoch + step) as f64
}

/// 에포크 지표를 스텝 축 위에 놓는다 (에포크 끝 지점).
pub fn epoch_x(epoch: usize, batches_per_epoch: usize) -> f64 {
    if batches_per_epoch == 0 {
        epoch as f64
    } else {
        ((epoch + 1) * batches_per_epoch) as f64
    }
}

// ── 뷰 상태 ─────────────────────────────────────────────────────────

#[derive(Default)]
pub struct TrainViewState {
    /// 실행 기록 표에서 고른 행 (곡선 표시 대상).
    pub selected_run: Option<RunId>,
    /// 스텝 손실도 그린다.
    pub show_steps: bool,
    /// 시작 폼에서 고른 데이터셋 (모델 설정과 다를 수 있다).
    pub dataset_override: Option<DatasetId>,
}

// ── 뷰 ──────────────────────────────────────────────────────────────

pub fn show(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut TrainViewState) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    let model_id = ctx.active_model();

    egui::ScrollArea::vertical().id_salt("train-scroll").show(ui, |ui| {
        controls(ui, ctx, state, model_id, &mut actions);
        ui.add_space(6.0);
        live(ui, ctx);
        ui.add_space(6.0);
        plot(ui, ctx, state);
        ui.add_space(10.0);
        runs_table(ui, ctx, state, &mut actions);
    });
    actions
}

fn controls(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    state: &mut TrainViewState,
    model_id: Option<ModelId>,
    actions: &mut Vec<ViewAction>,
) {
    let Some(model_id) = model_id else {
        ui.label(RichText::new("모델이 없습니다. 모델 뷰에서 먼저 만드세요.").color(COL_WARN));
        return;
    };
    let Some(model) = ctx.project.models.get(&model_id) else {
        return;
    };
    let dataset_id = state
        .dataset_override
        .filter(|d| ctx.project.datasets.contains_key(d))
        .or(model.train.dataset);

    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("모델").color(COL_WEAK));
                egui::ComboBox::from_id_salt("train-model")
                    .selected_text(&model.name)
                    .show_ui(ui, |ui| {
                        for (id, m) in &ctx.project.models {
                            if ui.selectable_label(*id == model_id, &m.name).clicked() {
                                actions.push(ViewAction::Select(Selection::Model(*id)));
                            }
                        }
                    });
                ui.separator();
                ui.label(RichText::new("데이터셋").color(COL_WEAK));
                let label = dataset_id
                    .and_then(|d| ctx.project.datasets.get(&d))
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "(없음)".into());
                egui::ComboBox::from_id_salt("train-dataset")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (id, d) in &ctx.project.datasets {
                            if ui.selectable_label(dataset_id == Some(*id), &d.name).clicked() {
                                state.dataset_override = Some(*id);
                            }
                        }
                    });
            });

            ui.add_space(6.0);
            let c = &model.train;
            ui.horizontal_wrapped(|ui| {
                chip(
                    ui,
                    "옵티마이저",
                    format!("{} lr {}", c.optimizer.label(), c.optimizer.lr()),
                );
                chip(ui, "손실", c.loss.label().to_string());
                chip(ui, "지표", c.metric.label().to_string());
                chip(ui, "에포크", c.epochs.to_string());
                chip(ui, "배치", c.batch_size.to_string());
                chip(ui, "장치", c.device.label());
                chip(ui, "검증", format!("{:.0}%", c.val_split * 100.0));
                if c.grad_clip > 0.0 {
                    chip(ui, "클리핑", format!("{:.2}", c.grad_clip));
                }
                if c.checkpoint_every > 0 {
                    chip(ui, "체크포인트", format!("{}에포크마다", c.checkpoint_every));
                }
            });

            ui.add_space(8.0);
            let live = ctx.training.map(|t| t.state).filter(|s| s.is_live());
            ui.horizontal(|ui| {
                match live {
                    None => {
                        let ready = dataset_id.is_some();
                        ui.add_enabled_ui(ready, |ui| {
                            if ui.button(RichText::new("▶ 학습 시작").color(COL_OK)).clicked() {
                                actions.push(ViewAction::StartTrain {
                                    model: model_id,
                                    dataset: dataset_id.unwrap(),
                                });
                            }
                        });
                        if !ready {
                            ui.label(RichText::new("데이터셋을 고르세요").color(COL_WARN).size(11.5));
                        } else if !ctx.saved() {
                            ui.label(
                                RichText::new("저장되지 않은 프로젝트입니다 — 시작하면 저장을 먼저 요청합니다")
                                    .color(COL_WARN)
                                    .size(11.5),
                            );
                        }
                    }
                    Some(SessionState::Running) => {
                        if ui.button("⏸ 일시정지").clicked() {
                            actions.push(ViewAction::PauseTrain);
                        }
                        if ui.button(RichText::new("⏹ 정지").color(COL_ERROR)).clicked() {
                            actions.push(ViewAction::StopTrain);
                        }
                    }
                    Some(SessionState::Paused) => {
                        if ui.button("▶ 재개").clicked() {
                            actions.push(ViewAction::ResumeTrain);
                        }
                        if ui.button(RichText::new("⏹ 정지").color(COL_ERROR)).clicked() {
                            actions.push(ViewAction::StopTrain);
                        }
                    }
                    Some(_) => {}
                }
                ui.checkbox(&mut state.show_steps, "스텝 손실 표시");
            });
        });
}

fn chip(ui: &mut egui::Ui, key: &str, value: String) {
    egui::Frame::NONE
        .fill(egui::Color32::from_rgb(0x25, 0x29, 0x30))
        .inner_margin(egui::Margin::symmetric(7, 3))
        .corner_radius(3)
        .show(ui, |ui| {
            ui.label(RichText::new(format!("{key} {value}")).size(11.5));
        });
}

/// 진행 바 · 현재 지표 · 경과 시간.
fn live(ui: &mut egui::Ui, ctx: &ViewCtx) {
    let Some(t) = ctx.training else { return };
    ui.horizontal(|ui| {
        let bar = egui::ProgressBar::new(t.progress().unwrap_or(0.0))
            .desired_width(260.0)
            .show_percentage();
        ui.add(bar);
        ui.label(format!("경과 {}", fmt_duration(t.elapsed)));
        if !t.device.is_empty() {
            ui.label(RichText::new(&t.device).color(COL_WEAK));
        }
        if t.params > 0 {
            ui.label(RichText::new(format!("파라미터 {}", t.params)).color(COL_WEAK));
        }
        match t.state {
            SessionState::Paused => {
                ui.label(RichText::new("일시정지").color(COL_WARN));
            }
            SessionState::Failed => {
                ui.label(RichText::new("실패").color(COL_ERROR));
            }
            SessionState::Finished => {
                ui.label(RichText::new("완료").color(COL_OK));
            }
            SessionState::Running => {}
        }
    });
    if let Some(last) = t.run.epochs.last() {
        ui.horizontal(|ui| {
            ui.label(format!("에포크 {}/{}", last.epoch + 1, t.run.config.epochs));
            ui.label(format!("학습 손실 {}", fmt_metric(last.train_loss)));
            if let Some(v) = last.val_loss {
                ui.label(format!("검증 손실 {}", fmt_metric(v)));
            }
            if let Some(m) = last.val_metric {
                ui.label(format!("{} {}", t.run.config.metric.label(), fmt_metric(m)));
            }
        });
    }
    if let Some(e) = &t.error {
        ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR));
    }
}

fn plot(ui: &mut egui::Ui, ctx: &ViewCtx, state: &TrainViewState) {
    // 표에서 고른 실행이 있으면 그것을, 없으면 진행 중인 학습을 그린다.
    let picked = state.selected_run.and_then(|id| ctx.project.runs.get(&id));
    let (epochs, steps, batches, title): (&[EpochMetrics], &[[f64; 2]], usize, String) = match (picked, ctx.training) {
        (Some(r), _) => (&r.epochs, &[], 0, format!("실행 {}", r.id.short())),
        (None, Some(t)) => (&t.run.epochs, &t.steps[..], t.batches_per_epoch, "진행 중".into()),
        (None, None) => (&[], &[], 0, "손실".into()),
    };
    if epochs.is_empty() && steps.is_empty() {
        egui::Frame::NONE
            .fill(COL_SURFACE)
            .inner_margin(14)
            .corner_radius(5)
            .show(ui, |ui| {
                ui.label(
                    RichText::new("아직 그릴 곡선이 없습니다. 학습을 시작하거나 아래 표에서 실행을 고르세요.")
                        .color(COL_WEAK),
                );
            });
        return;
    }
    let train: Vec<[f64; 2]> = epochs
        .iter()
        .map(|e| [epoch_x(e.epoch, batches), e.train_loss])
        .collect();
    let val: Vec<[f64; 2]> = epochs
        .iter()
        .filter_map(|e| e.val_loss.map(|v| [epoch_x(e.epoch, batches), v]))
        .collect();
    let show_steps = state.show_steps && !steps.is_empty();
    let step_points: Vec<[f64; 2]> = if show_steps { steps.to_vec() } else { Vec::new() };

    Plot::new("loss-plot")
        .height(240.0)
        .legend(Legend::default())
        .x_axis_label(if batches > 0 { "스텝" } else { "에포크" })
        .y_axis_label("손실")
        .show(ui, |p| {
            if show_steps {
                p.line(
                    Line::new("스텝 손실", PlotPoints::from(step_points))
                        .width(0.8)
                        .color(COL_WEAK),
                );
            }
            if !train.is_empty() {
                p.line(
                    Line::new("학습 손실", PlotPoints::from(train))
                        .width(2.4)
                        .color(COL_SELECT),
                );
            }
            if !val.is_empty() {
                p.line(Line::new("검증 손실", PlotPoints::from(val)).width(2.4).color(COL_OK));
            }
        });
    ui.label(RichText::new(title).color(COL_WEAK).size(11.0));

    // 학습률은 손실과 자릿수가 딴판이라(1e-3 대 1) 같은 축에 겹치면 한쪽이 납작해진다. 따로 그린다.
    let lr: Vec<[f64; 2]> = epochs
        .iter()
        .filter_map(|e| e.lr.map(|v| [epoch_x(e.epoch, batches), v]))
        .collect();
    if lr.len() >= 2 {
        ui.add_space(6.0);
        Plot::new("lr-plot")
            .height(LR_PLOT_HEIGHT)
            .legend(Legend::default())
            .x_axis_label(if batches > 0 { "스텝" } else { "에포크" })
            .y_axis_label("학습률")
            .show(ui, |p| {
                p.line(Line::new("학습률", PlotPoints::from(lr)).width(2.0).color(COL_WARN));
            });
    }
}

fn runs_table(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut TrainViewState, actions: &mut Vec<ViewAction>) {
    ui.label(RichText::new("실행 기록").size(15.0).strong());
    ui.separator();
    if ctx.project.runs.is_empty() {
        ui.label(RichText::new("아직 실행 기록이 없습니다.").color(COL_WEAK));
        return;
    }
    // 최근 실행이 위로.
    let mut rows: Vec<(&RunId, &RunRecord)> = ctx.project.runs.iter().collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.1.started));

    egui::Grid::new("runs-table")
        .striped(true)
        .num_columns(9)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            for h in [
                "시작",
                "상태",
                "에포크",
                "최종 손실",
                "최고 val",
                "마지막 lr",
                "장치",
                "체크포인트",
                "",
            ] {
                ui.label(RichText::new(h).color(COL_WEAK).size(11.0));
            }
            ui.end_row();
            for (id, r) in rows {
                let selected = state.selected_run == Some(*id);
                let started = r
                    .started
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string();
                if ui.selectable_label(selected, started).clicked() {
                    state.selected_run = if selected { None } else { Some(*id) };
                    actions.push(ViewAction::Select(Selection::Run(*id)));
                }
                ui.label(RichText::new(status_label(r.status)).color(status_color(r.status)));
                ui.label(format!("{}/{}", r.epochs.len(), r.config.epochs));
                ui.label(r.last().map(|e| fmt_metric(e.train_loss)).unwrap_or_else(|| "-".into()));
                ui.label(r.best_val_loss().map(fmt_metric).unwrap_or_else(|| "-".into()));
                // 스케줄이 없으면 엔진이 lr 을 채우지 않는다 — 그때는 빈 칸이 맞다.
                ui.label(r.last().and_then(|e| e.lr).map(fmt_lr).unwrap_or_else(|| "-".into()));
                ui.label(if r.device_name.is_empty() {
                    "-"
                } else {
                    r.device_name.as_str()
                });
                match &r.checkpoint {
                    Some(p) => {
                        // 조기 종료를 켜면 마지막이 아니라 검증 손실이 가장 낮았던 가중치가 남는다.
                        let best = r.best_checkpoint.as_deref() == Some(p.as_str());
                        let label = if best {
                            format!("{} (최고)", super::short_path(p))
                        } else {
                            super::short_path(p)
                        };
                        ui.label(RichText::new(label).size(11.0)).on_hover_text(p);
                    }
                    None => {
                        ui.label("-");
                    }
                }
                ui.horizontal(|ui| {
                    let has = r.checkpoint.is_some() || r.best_checkpoint.is_some();
                    // 조기 종료로 남은 "가장 좋았던" 가중치가 있으면 그쪽을 쓴다.
                    let tip = if r.best_checkpoint.is_some() {
                        "검증 손실이 가장 낮았던 에포크의 가중치를 모델에 적용합니다"
                    } else {
                        "이 실행의 마지막 체크포인트를 모델 가중치로"
                    };
                    ui.add_enabled_ui(has, |ui| {
                        if ui.small_button("가중치 적용").on_hover_text(tip).clicked() {
                            actions.push(ViewAction::ApplyRunWeights(*id));
                        }
                    });
                    // 이 실행의 체크포인트로 바로 ONNX 를 쓴다 — 모델 가중치를 바꾸지 않는다.
                    ui.add_enabled_ui(has, |ui| {
                        if ui
                            .small_button("ONNX")
                            .on_hover_text("이 실행의 가중치로 .onnx 파일을 씁니다")
                            .clicked()
                        {
                            actions.push(ViewAction::ExportOnnx {
                                model: r.model,
                                run: Some(*id),
                            });
                        }
                    });
                    if ui.small_button("🗑").on_hover_text("실행 기록 삭제").clicked() {
                        actions.push(ViewAction::Ops(vec![nl_core::Op::DeleteRun { id: *id }]));
                        if state.selected_run == Some(*id) {
                            state.selected_run = None;
                        }
                    }
                });
                ui.end_row();
            }
        });
    if let Some(r) = state.selected_run.and_then(|id| ctx.project.runs.get(&id)) {
        if let Some(e) = &r.error {
            ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
        }
    }
}

pub fn status_label(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Running => "● 진행 중",
        RunStatus::Finished => "✔ 완료",
        RunStatus::Stopped => "■ 중지",
        RunStatus::Failed => "✖ 실패",
    }
}

pub fn status_color(s: RunStatus) -> egui::Color32 {
    match s {
        RunStatus::Running => COL_SELECT,
        RunStatus::Finished => COL_OK,
        RunStatus::Stopped => COL_WEAK,
        RunStatus::Failed => COL_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_step_lays_epochs_end_to_end() {
        assert_eq!(global_step(0, 0, 10), 0.0);
        assert_eq!(global_step(0, 7, 10), 7.0);
        assert_eq!(global_step(2, 3, 10), 23.0);
        // 배치 수를 모르면 스텝 번호만 남는다.
        assert_eq!(global_step(3, 5, 0), 5.0);
    }

    #[test]
    fn epoch_marks_sit_at_the_end_of_their_epoch() {
        assert_eq!(epoch_x(0, 10), 10.0);
        assert_eq!(epoch_x(4, 10), 50.0);
        assert_eq!(epoch_x(4, 0), 4.0);
    }

    #[test]
    fn merge_final_fills_gaps_from_the_ui_record() {
        let mut ui = blank_run();
        ui.epochs = vec![EpochMetrics {
            epoch: 0,
            train_loss: 1.0,
            ..Default::default()
        }];
        ui.device_name = "CPU".into();
        ui.checkpoint = Some("a.safetensors".into());
        ui.best_checkpoint = Some("best.safetensors".into());
        let ui_best = ui.best_checkpoint.clone();
        let engine = blank_run();
        let merged = merge_final(ui, engine);
        assert_eq!(merged.epochs.len(), 1);
        assert_eq!(merged.device_name, "CPU");
        assert_eq!(merged.checkpoint.as_deref(), Some("a.safetensors"));
        // 조기 종료가 남긴 "가장 좋았던" 가중치도 UI 기록에서 이어받는다.
        assert_eq!(merged.best_checkpoint, ui_best);
        assert!(merged.finished.is_some());
    }

    #[test]
    fn merge_final_prefers_the_engine_record_when_it_has_data() {
        let mut ui = blank_run();
        ui.device_name = "CPU".into();
        let mut engine = blank_run();
        engine.device_name = "GPU 0".into();
        engine.epochs = vec![EpochMetrics {
            epoch: 9,
            train_loss: 0.1,
            ..Default::default()
        }];
        let merged = merge_final(ui, engine);
        assert_eq!(merged.device_name, "GPU 0");
        assert_eq!(merged.epochs.len(), 1);
        assert_eq!(merged.epochs[0].epoch, 9);
    }

    #[test]
    fn session_state_knows_when_it_is_live() {
        assert!(SessionState::Running.is_live());
        assert!(SessionState::Paused.is_live());
        assert!(!SessionState::Finished.is_live());
        assert!(!SessionState::Failed.is_live());
    }
}
