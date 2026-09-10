//! 앱 셸: 문서 상태(op 기반 undo/redo), 패널 UI, 파일 IO, 학습 세션.
//! 동기화 서버가 없다는 점만 빼면 trust-pms `pms-app::app` 과 같은 구조다.

use crate::canvas::{CanvasAction, CanvasState, Selection};
use crate::project::{self, Recent};
use crate::sample;
use crate::views::{self, train::TrainSession, ViewAction, ViewCtx, ViewState};
use chrono::{DateTime, Local};
use eframe::egui::{self, Color32, RichText};
use nl_core::dataset::{DataSource, DatasetSpec, SyntheticKind};
use nl_core::shape::{self, ShapeReport};
use nl_core::validate::Where;
use nl_core::{
    apply_ops, diff_ops, inverse_ops, DatasetId, Edge, ModelId, Node, NodeId, Op, Port, Project, RunId, RunStatus,
    Severity,
};
use nl_engine::{DeviceInfo, TrainRequest};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const UNDO_LIMIT: usize = 100;
/// 이 시간(초) 동안 편집이 없으면 하나의 undo 단위(burst)가 끝난 것으로 본다.
pub const BURST_QUIET: f64 = 1.0;
/// 도크 로그·활동 기록의 최대 줄 수.
const LOG_LIMIT: usize = 400;

// ── 문서 상태 ───────────────────────────────────────────────────────

/// undo 한 단위. 문서 스냅샷 대신 "적용할 op / 되돌릴 op" 쌍만 들고 있는다.
struct HistoryEntry {
    forward: Vec<Op>,
    inverse: Vec<Op>,
}

pub struct DocState {
    pub project: Project,
    /// 마지막 안정 상태. burst 편집 중에는 그 시작 시점 문서이고, 그 외에는 project 와 같다.
    baseline: Project,
    undo: Vec<HistoryEntry>,
    redo: Vec<HistoryEntry>,
    in_burst: bool,
    /// burst 가 빈 diff 로 끝났을 때 되돌려 놓을 `modified` 값.
    modified_before_burst: bool,
    last_edit: f64,
    pub file_path: Option<PathBuf>,
    pub modified: bool,
    /// 사용자가 인스펙터에서 직접 친 것이 아닌 경로(undo/redo/스냅샷)로 문서가 바뀐 횟수.
    /// 인스펙터 텍스트 버퍼는 이 값이 바뀌면 다시 채운다 — 안 그러면 되돌린 값이 입력칸에 남는다.
    pub external_edits: u64,
    /// 문서가 바뀔 때마다 오르는 번호(모든 경로). 형상·검증 캐시가 이 번호로 무효화를 판단한다.
    pub edit_seq: u64,
}

impl DocState {
    pub fn new(project: Project) -> Self {
        Self {
            baseline: project.clone(),
            project,
            undo: Vec::new(),
            redo: Vec::new(),
            in_burst: false,
            modified_before_burst: false,
            last_edit: 0.0,
            file_path: None,
            modified: false,
            external_edits: 0,
            edit_seq: 0,
        }
    }

    /// 매 프레임: 조용해지면 burst 를 하나의 undo 항목으로 확정한다.
    pub fn tick(&mut self, now: f64) {
        if self.in_burst && now - self.last_edit > BURST_QUIET {
            self.finish_burst();
        }
    }

    pub fn in_burst(&self) -> bool {
        self.in_burst
    }

    /// 진행 중인 burst 를 baseline 과의 diff 로 확정한다. undo/redo/구조 편집 전에 먼저 호출한다.
    fn finish_burst(&mut self) {
        if !self.in_burst {
            return;
        }
        self.in_burst = false;
        let forward = diff_ops(&self.baseline, &self.project);
        if forward.is_empty() {
            // 썼다 지워서 결국 그대로면 편집이 없었던 것으로 되돌린다.
            self.modified = self.modified_before_burst;
            return;
        }
        let inverse = inverse_ops(&self.baseline, &forward);
        self.push_undo(HistoryEntry { forward, inverse });
        self.baseline = self.project.clone();
    }

    fn begin_burst(&mut self, now: f64) {
        if !self.in_burst {
            self.in_burst = true;
            self.modified_before_burst = self.modified;
        }
        self.last_edit = now;
        self.modified = true;
        self.edit_seq += 1;
    }

    /// 인스펙터에서 `&mut` 로 직접 고친 뒤 호출: burst 로 모아 undo 한 단위가 된다.
    pub fn note_edited(&mut self, now: f64) {
        self.begin_burst(now);
    }

    /// 잦은 편집을 op 로 적용한다 (페이로드 편집기의 DragValue 등). undo 는 burst 로 모인다.
    pub fn apply_burst(&mut self, ops: Vec<Op>, now: f64) {
        if ops.is_empty() {
            return;
        }
        self.begin_burst(now);
        apply_ops(&mut self.project, &ops);
    }

    /// 구조적 편집(생성/삭제/연결): 개별 undo 단위.
    pub fn apply_local(&mut self, ops: Vec<Op>) {
        if ops.is_empty() {
            return;
        }
        self.finish_burst();
        let inverse = inverse_ops(&self.project, &ops);
        apply_ops(&mut self.project, &ops);
        self.baseline = self.project.clone();
        self.modified = true;
        self.edit_seq += 1;
        self.push_undo(HistoryEntry { forward: ops, inverse });
    }

    pub fn set_snapshot(&mut self, project: Project) {
        self.baseline = project.clone();
        self.project = project;
        self.undo.clear();
        self.redo.clear();
        self.in_burst = false;
        self.external_edits += 1;
        self.edit_seq += 1;
    }

    fn push_undo(&mut self, entry: HistoryEntry) {
        self.undo.push(entry);
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty() || (self.in_burst && self.project != self.baseline)
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) {
        self.finish_burst();
        let Some(entry) = self.undo.pop() else { return };
        apply_ops(&mut self.project, &entry.inverse);
        self.baseline = self.project.clone();
        self.redo.push(entry);
        self.modified = true;
        self.external_edits += 1;
        self.edit_seq += 1;
    }

    pub fn redo(&mut self) {
        self.finish_burst();
        let Some(entry) = self.redo.pop() else { return };
        apply_ops(&mut self.project, &entry.forward);
        self.baseline = self.project.clone();
        self.undo.push(entry);
        self.modified = true;
        self.external_edits += 1;
        self.edit_seq += 1;
    }
}

// ── 뷰 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Model,
    Data,
    Train,
    Pipeline,
    Gui,
    Build,
    Resources,
}

impl View {
    /// 뷰 바 순서 = Ctrl+1..Ctrl+7 순서 = 저장되는 인덱스.
    pub const ALL: [View; 7] =
        [View::Model, View::Data, View::Train, View::Pipeline, View::Gui, View::Build, View::Resources];

    pub fn label(self) -> &'static str {
        match self {
            View::Model => "모델",
            View::Data => "데이터",
            View::Train => "학습",
            View::Pipeline => "파이프라인",
            View::Gui => "GUI",
            View::Build => "빌드",
            View::Resources => "자원",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            View::Model => "레이어 그래프 — 우클릭으로 레이어 추가, 포트를 끌어 연결",
            View::Data => "데이터셋과 페이로드 — 스캔·미리보기·인코더 체인",
            View::Train => "학습 설정·손실 플롯·실행 기록",
            View::Pipeline => "소스 → 모델 → 싱크 파이프라인 (2차분)",
            View::Gui => "배포 앱의 위젯 디자이너 (2차분)",
            View::Build => "대상·산출물·도구 상태 (2차분)",
            View::Resources => "CPU·메모리·GPU 사용 현황",
        }
    }

    pub fn index(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }

    /// 저장된 값이 깨져 있어도 앱이 뜨도록 범위 밖은 모델 뷰로 떨어뜨린다.
    pub fn from_index(i: usize) -> View {
        View::ALL.get(i).copied().unwrap_or(View::Model)
    }

    /// 2차분에서 구현할 자리인가.
    pub fn is_placeholder(self) -> bool {
        matches!(self, View::Pipeline | View::Gui | View::Build)
    }
}

const VIEW_KEYS: [egui::Key; 7] = [
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockTab {
    #[default]
    Issues,
    Log,
    Activity,
}

/// 저장하지 않은 변경이 있으면 확인 모달로 한 번 가로채는 동작들.
#[derive(Clone, PartialEq)]
pub enum PendingAction {
    Close,
    New,
    Open,
    OpenPath(PathBuf),
    /// `sample::SAMPLES` 의 인덱스.
    Sample(usize),
}

impl PendingAction {
    fn describe(&self) -> &'static str {
        match self {
            PendingAction::Close => "앱을 닫기",
            PendingAction::New => "새 프로젝트를 만들기",
            PendingAction::Open | PendingAction::OpenPath(_) => "다른 파일을 열기",
            PendingAction::Sample(_) => "샘플 프로젝트를 열기",
        }
    }
}

// ── 앱 ──────────────────────────────────────────────────────────────

pub struct NlApp {
    pub doc: DocState,
    pub canvas: CanvasState,
    pub views: ViewState,
    pub view: View,
    /// `nl_engine::enumerate()` 결과 캐시.
    pub(crate) devices: Vec<DeviceInfo>,
    /// (edit_seq, 모델, 형상 보고). 문서가 바뀔 때만 다시 추론한다.
    shape_cache: Option<(u64, ModelId, ShapeReport)>,
    /// (edit_seq, 검증 결과).
    issues_cache: Option<(u64, Vec<nl_core::Issue>)>,
    toasts: Vec<(String, f64)>,
    /// 도크 로그 탭 (학습 스레드 메시지 등).
    logs: Vec<String>,
    activity: Vec<(DateTime<Local>, String)>,
    pub show_outline: bool,
    pub show_inspector: bool,
    pub show_dock: bool,
    dock_tab: DockTab,
    pending_action: Option<PendingAction>,
    close_confirmed: bool,
    recent: Recent,
    pub training: Option<TrainSession>,
    /// 아웃라인에서 이름을 고치는 중.
    rename: Option<(Selection, String)>,
    /// 인스펙터 텍스트 버퍼가 어느 (선택, 문서 세대)에 맞춰 채워졌는지.
    buf_owner: Option<(Selection, u64)>,
    /// Input/Reshape 형상 입력 버퍼 ("3×28×28").
    pub shape_buf: String,
    /// 시작 직후 몇 프레임을 강제로 그려 패널 크기를 정리한다.
    warmup_frames: u8,
}

impl NlApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_file(cc, None)
    }

    /// `initial_file` 이 있으면(명령줄·파일 연결) 그 문서를, 없으면 마지막 파일, 그것도 없으면 XOR 샘플.
    pub fn new_with_file(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_visuals(nl_gui::dark_visuals());

        let mut startup_error: Option<String> = None;
        let doc = match initial_file {
            Some(path) => match project::load(&path) {
                Ok(l) => {
                    if l.newer {
                        startup_error = Some(format!("이 파일은 앱보다 새 형식입니다 — 모르는 항목은 저장 시 사라집니다: {}", path.display()));
                    }
                    let mut d = DocState::new(l.project);
                    d.file_path = Some(path);
                    d
                }
                Err(e) => {
                    // 엉뚱한 문서를 자기 파일인 줄 알고 편집·저장하는 사고를 막는다.
                    startup_error = Some(format!("파일을 열 수 없습니다: {} — {e}", path.display()));
                    DocState::new(sample::new_project())
                }
            },
            None => cc
                .storage
                .and_then(|s| eframe::get_value::<String>(s, "last_file"))
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .and_then(|path| {
                    project::load(&path).ok().map(|l| {
                        let mut d = DocState::new(l.project);
                        d.file_path = Some(path);
                        d
                    })
                })
                .unwrap_or_else(|| DocState::new(sample::xor_project())),
        };

        let stored = |key: &str, default: bool| {
            cc.storage.and_then(|s| eframe::get_value::<bool>(s, key)).unwrap_or(default)
        };
        let view = cc
            .storage
            .and_then(|s| eframe::get_value::<usize>(s, "view"))
            .map(View::from_index)
            .unwrap_or_default();
        let recent = cc.storage.and_then(|s| eframe::get_value::<Recent>(s, "recent")).unwrap_or_default();

        let mut app = Self {
            doc,
            canvas: CanvasState::new(),
            views: ViewState::default(),
            view,
            devices: nl_engine::enumerate(),
            shape_cache: None,
            issues_cache: None,
            toasts: Vec::new(),
            logs: Vec::new(),
            activity: Vec::new(),
            show_outline: stored("show_outline", true),
            show_inspector: stored("show_inspector", true),
            show_dock: stored("show_dock", false),
            dock_tab: DockTab::Issues,
            pending_action: None,
            close_confirmed: false,
            recent,
            training: None,
            rename: None,
            buf_owner: None,
            shape_buf: String::new(),
            warmup_frames: 3,
        };
        // 첫 선택은 첫 모델 — 인스펙터가 빈 채로 뜨지 않는다.
        if let Some(&id) = app.doc.project.models.keys().next() {
            app.canvas.set_selection(Selection::Model(id));
        } else {
            app.canvas.set_selection(Selection::Project);
        }
        if let Some(e) = startup_error {
            app.toast(e, 0.0);
        }
        app
    }

    // ── 알림 · 기록 ─────────────────────────────────────────────

    pub fn toast(&mut self, msg: impl Into<String>, now: f64) {
        let msg = msg.into();
        self.log_activity(msg.clone());
        self.toasts.push((msg, now + 3.5));
    }

    fn log_activity(&mut self, msg: impl Into<String>) {
        self.activity.push((Local::now(), msg.into()));
        if self.activity.len() > LOG_LIMIT {
            self.activity.remove(0);
        }
    }

    pub fn log(&mut self, line: impl Into<String>) {
        self.logs.push(line.into());
        if self.logs.len() > LOG_LIMIT {
            self.logs.remove(0);
        }
    }

    // ── 캐시 ────────────────────────────────────────────────────

    /// 지금 보고 있는 모델. 선택이 가리키는 것, 없으면 첫 모델.
    pub fn active_model(&self) -> Option<ModelId> {
        self.canvas
            .selection
            .model()
            .filter(|m| self.doc.project.models.contains_key(m))
            .or_else(|| self.doc.project.models.keys().next().copied())
    }

    /// 활성 모델의 형상 추론 결과 (문서가 바뀔 때만 다시 계산).
    pub fn shapes(&mut self) -> ShapeReport {
        let Some(id) = self.active_model() else { return ShapeReport::default() };
        let seq = self.doc.edit_seq;
        if let Some((s, m, rep)) = &self.shape_cache {
            if *s == seq && *m == id {
                return rep.clone();
            }
        }
        let rep = match self.doc.project.models.get(&id) {
            Some(m) => shape::infer(&m.graph),
            None => ShapeReport::default(),
        };
        self.shape_cache = Some((seq, id, rep.clone()));
        rep
    }

    fn issues(&mut self) -> Vec<nl_core::Issue> {
        let seq = self.doc.edit_seq;
        if let Some((s, v)) = &self.issues_cache {
            if *s == seq {
                return v.clone();
            }
        }
        let v = nl_core::validate(&self.doc.project);
        self.issues_cache = Some((seq, v.clone()));
        v
    }

    /// 데이터셋 상대 경로의 기준 폴더. 저장 전이면 현재 작업 폴더.
    pub fn base_dir(&self) -> PathBuf {
        match &self.doc.file_path {
            Some(p) => project::base_dir(p),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    // ── 파일 IO ─────────────────────────────────────────────────

    /// 저장할 문서. 학습 중이면 진행 중인 실행 기록도 함께 담는다.
    fn snapshot_for_save(&self) -> Project {
        let mut p = self.doc.project.clone();
        if let Some(t) = &self.training {
            if t.state.is_live() {
                p.runs.insert(t.run.id, t.run.clone());
            }
        }
        p
    }

    pub fn save(&mut self, now: f64) {
        let Some(path) = self.doc.file_path.clone() else {
            self.save_as(now);
            return;
        };
        let snapshot = self.snapshot_for_save();
        match project::save(&path, &snapshot) {
            Ok(()) => {
                self.doc.modified = false;
                self.recent.push(&path);
                self.toast(format!("저장됨: {}", crate::views::short_path(&path.display().to_string())), now);
            }
            Err(e) => self.toast(format!("저장 실패: {e}"), now),
        }
    }

    pub fn save_as(&mut self, now: f64) {
        let mut dialog = rfd::FileDialog::new().add_filter("Neural Linker 프로젝트", &[nl_core::model::PROJECT_EXT]);
        if let Some(dir) = self.doc.file_path.as_ref().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(dir);
        }
        let name = format!("{}.{}", sanitize(&self.doc.project.name), nl_core::model::PROJECT_EXT);
        if let Some(path) = dialog.set_file_name(name).save_file() {
            let path = project::with_project_ext(path);
            self.doc.file_path = Some(path);
            self.save(now);
        }
    }

    fn open_dialog(&mut self, now: f64) {
        let dialog = rfd::FileDialog::new().add_filter("Neural Linker 프로젝트", &[nl_core::model::PROJECT_EXT]);
        if let Some(path) = dialog.pick_file() {
            self.open_path(&path, now);
        }
    }

    fn open_path(&mut self, path: &Path, now: f64) {
        match project::load(path) {
            Ok(l) => {
                if l.newer {
                    self.toast("이 파일은 앱보다 새 형식입니다 — 모르는 항목은 저장 시 사라집니다", now);
                }
                self.doc.set_snapshot(l.project);
                self.doc.file_path = Some(path.to_path_buf());
                self.doc.modified = false;
                self.after_document_swap();
                self.recent.push(path);
                self.toast(format!("열었습니다: {}", path.display()), now);
            }
            Err(e) => {
                self.recent.remove(&path.display().to_string());
                self.toast(format!("열 수 없습니다: {} — {e}", path.display()), now);
            }
        }
    }

    fn new_project(&mut self, now: f64) {
        self.doc = DocState::new(sample::new_project());
        self.after_document_swap();
        self.toast("새 프로젝트", now);
    }

    fn open_sample(&mut self, index: usize, now: f64) {
        let Some((name, make)) = sample::SAMPLES.get(index) else { return };
        self.doc = DocState::new(make());
        self.after_document_swap();
        self.toast(format!("샘플 열기: {name}"), now);
    }

    /// 문서가 통째로 바뀐 뒤 정리.
    fn after_document_swap(&mut self) {
        self.canvas = CanvasState::new();
        self.views = ViewState::default();
        self.shape_cache = None;
        self.issues_cache = None;
        self.rename = None;
        self.buf_owner = None;
        self.training = None;
        if let Some(&id) = self.doc.project.models.keys().next() {
            self.canvas.set_selection(Selection::Model(id));
        } else {
            self.canvas.set_selection(Selection::Project);
        }
    }

    // ── 저장 확인 모달 ──────────────────────────────────────────

    fn request_action(&mut self, action: PendingAction, ctx: &egui::Context, now: f64) {
        if self.doc.modified {
            self.pending_action = Some(action);
        } else {
            self.run_action(action, ctx, now);
        }
    }

    fn run_action(&mut self, action: PendingAction, ctx: &egui::Context, now: f64) {
        self.pending_action = None;
        match action {
            PendingAction::Close => {
                self.close_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PendingAction::New => self.new_project(now),
            PendingAction::Open => self.open_dialog(now),
            PendingAction::OpenPath(p) => self.open_path(&p, now),
            PendingAction::Sample(i) => self.open_sample(i, now),
        }
    }

    fn unsaved_modal(&mut self, ctx: &egui::Context, now: f64) {
        let Some(action) = self.pending_action.clone() else { return };
        let what = action.describe();
        let modal = egui::Modal::new(egui::Id::new("unsaved-changes")).show(ctx, |ui| {
            ui.set_width(330.0);
            ui.heading("저장하지 않은 변경");
            ui.add_space(6.0);
            ui.label(format!("{what} 전에 변경 내용을 저장할까요?"));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("저장").clicked() {
                    self.save(now);
                    // 저장 대화상자를 취소하면 modified 가 남으므로 모달을 유지한다.
                    if !self.doc.modified {
                        self.run_action(action.clone(), ui.ctx(), now);
                    }
                }
                if ui.button("저장 안 함").clicked() {
                    self.run_action(action.clone(), ui.ctx(), now);
                }
                if ui.button("취소").clicked() {
                    self.pending_action = None;
                }
            });
        });
        if self.pending_action.is_some() && modal.should_close() {
            self.pending_action = None;
        }
    }

    // ── 편집 ────────────────────────────────────────────────────

    fn undo(&mut self) {
        self.doc.undo();
        self.buf_owner = None;
    }

    fn redo(&mut self) {
        self.doc.redo();
        self.buf_owner = None;
    }

    /// 캔버스가 돌려준 편집 의도를 op 로 바꿔 적용한다.
    fn apply_canvas_action(&mut self, model: ModelId, action: CanvasAction, now: f64) {
        let Some(graph) = self.doc.project.models.get(&model).map(|m| m.graph.clone()) else { return };
        match action {
            CanvasAction::AddNode { kind, pos } => {
                let node = Node::new(kind, pos);
                let id = node.id;
                self.doc.apply_local(vec![Op::UpsertNode { model, node }]);
                self.canvas.set_selection(Selection::Node(model, id));
            }
            CanvasAction::MoveNodes(items) => {
                let ops: Vec<Op> = items
                    .iter()
                    .filter_map(|(id, pos)| {
                        let mut n = graph.nodes.get(id)?.clone();
                        n.pos = *pos;
                        Some(Op::UpsertNode { model, node: n })
                    })
                    .collect();
                self.doc.apply_local(ops);
            }
            CanvasAction::Connect { from, to, replace } => {
                let mut ops = Vec::new();
                if let Some(e) = replace {
                    ops.push(Op::DeleteEdge { model, id: e });
                }
                ops.push(Op::UpsertEdge { model, edge: Edge::new(from, to) });
                self.doc.apply_local(ops);
            }
            CanvasAction::DeleteNodes(ids) => {
                let ops: Vec<Op> = ids.iter().map(|id| Op::DeleteNode { model, id: *id }).collect();
                self.doc.apply_local(ops);
                self.canvas.set_selection(Selection::Model(model));
            }
            CanvasAction::DeleteEdges(ids) => {
                let ops: Vec<Op> = ids.iter().map(|id| Op::DeleteEdge { model, id: *id }).collect();
                self.doc.apply_local(ops);
                self.canvas.set_selection(Selection::Model(model));
            }
            CanvasAction::DisconnectNode(id) => {
                let ops: Vec<Op> = graph
                    .edges
                    .values()
                    .filter(|e| e.from == id || e.to.node == id)
                    .map(|e| Op::DeleteEdge { model, id: e.id })
                    .collect();
                if ops.is_empty() {
                    self.toast("연결이 없습니다", now);
                } else {
                    self.doc.apply_local(ops);
                }
            }
            CanvasAction::DuplicateNodes(ids) => {
                let (ops, new_ids) = duplicate_ops(&graph, model, &ids);
                if ops.is_empty() {
                    return;
                }
                self.doc.apply_local(ops);
                self.canvas.select_nodes(model, new_ids, false);
            }
        }
    }

    /// 뷰가 돌려준 명령.
    fn apply_view_action(&mut self, action: ViewAction, ctx: &egui::Context, now: f64) {
        match action {
            ViewAction::Select(sel) => self.canvas.set_selection(sel),
            ViewAction::Ops(ops) => self.doc.apply_local(ops),
            ViewAction::Edit(ops) => self.doc.apply_burst(ops, now),
            ViewAction::Toast(msg) => self.toast(msg, now),
            ViewAction::SetView(v) => self.set_view(v),
            ViewAction::Focus(model, node) => {
                self.set_view(View::Model);
                self.canvas.set_selection(Selection::Node(model, node));
                self.canvas.pending_focus = Some(node);
            }
            ViewAction::PickCsv => self.pick_csv(now),
            ViewAction::PickImageFolder => self.pick_folder_dataset(false, now),
            ViewAction::PickRecordedFolder => self.pick_folder_dataset(true, now),
            ViewAction::AddSynthetic(kind) => self.add_synthetic(kind),
            ViewAction::ScanDataset(id) => self.scan_dataset(id, now),
            ViewAction::PreviewDataset(id) => self.preview_dataset(id, ctx),
            ViewAction::StartTrain { model, dataset } => self.start_train(model, dataset, now),
            ViewAction::PauseTrain => {
                if let Some(t) = &self.training {
                    t.handle.pause();
                }
            }
            ViewAction::ResumeTrain => {
                if let Some(t) = &self.training {
                    t.handle.resume();
                }
            }
            ViewAction::StopTrain => {
                if let Some(t) = &self.training {
                    t.handle.stop();
                }
                self.toast("정지를 요청했습니다", now);
            }
            ViewAction::ApplyRunWeights(id) => self.apply_run_weights(id, now),
        }
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.canvas.cancel_interaction();
    }

    // ── 데이터셋 ────────────────────────────────────────────────

    fn add_synthetic(&mut self, kind: SyntheticKind) {
        let spec = views::data::synthetic_dataset(kind, 1000);
        let id = spec.id;
        self.doc.apply_local(vec![Op::UpsertDataset { dataset: spec }]);
        self.canvas.set_selection(Selection::Dataset(id));
        self.set_view(View::Data);
    }

    fn pick_csv(&mut self, now: f64) {
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv", "tsv", "txt"]).pick_file() else { return };
        match project::csv_columns(&path, true) {
            Ok(names) => {
                let n = names.len();
                let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "CSV".into());
                // 마지막 열을 타깃으로 미리 골라 둔다 — 가장 흔한 배치다.
                let mut inputs = vec![true; n];
                let mut targets = vec![false; n];
                if n > 1 {
                    inputs[n - 1] = false;
                    targets[n - 1] = true;
                }
                self.views.data.csv_form = Some(views::data::CsvForm {
                    path: path.display().to_string(),
                    name,
                    header: true,
                    header_names: names,
                    inputs,
                    targets,
                });
                self.set_view(View::Data);
            }
            Err(e) => self.toast(format!("CSV 를 읽을 수 없습니다: {e}"), now),
        }
    }

    fn pick_folder_dataset(&mut self, recorded: bool, now: f64) {
        let Some(path) = rfd::FileDialog::new().pick_folder() else { return };
        let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "폴더".into());
        let p = path.display().to_string();
        let source = if recorded { DataSource::Recorded { path: p } } else { DataSource::ImageFolder { path: p } };
        let spec = DatasetSpec::new(name, source);
        let id = spec.id;
        self.doc.apply_local(vec![Op::UpsertDataset { dataset: spec }]);
        self.canvas.set_selection(Selection::Dataset(id));
        self.set_view(View::Data);
        self.toast("데이터셋을 추가했습니다 — 스캔으로 내용을 확인하세요", now);
    }

    fn scan_dataset(&mut self, id: DatasetId, now: f64) {
        let Some(spec) = self.doc.project.datasets.get(&id).cloned() else { return };
        let base = self.base_dir();
        match nl_engine::scan(&spec, &base) {
            Ok(info) => {
                let msg = format!(
                    "샘플 {} · 입력 {} · 타깃 {}{}",
                    info.samples,
                    views::shape_text(&info.input_shape),
                    views::shape_text(&info.target_shape),
                    if info.classes.is_empty() { String::new() } else { format!(" · 클래스 {}", info.classes.len()) }
                );
                self.views.data.scan.insert(id, Ok(msg));
                let mut next = spec;
                next.cached_info = Some(info);
                self.doc.apply_local(vec![Op::UpsertDataset { dataset: next }]);
            }
            Err(e) => {
                let msg = e.to_string();
                self.views.data.scan.insert(id, Err(msg.clone()));
                self.log(format!("스캔 실패({}): {msg}", spec.name));
                self.toast(format!("스캔 실패: {msg}"), now);
            }
        }
    }

    fn preview_dataset(&mut self, id: DatasetId, ctx: &egui::Context) {
        let Some(spec) = self.doc.project.datasets.get(&id).cloned() else { return };
        let base = self.base_dir();
        let result = match nl_engine::preview(&spec, &base, 8) {
            Ok(samples) => Ok(samples
                .iter()
                .enumerate()
                .map(|(i, s)| views::data::PreviewItem {
                    texture: sample_texture(ctx, &format!("preview-{}-{i}", id.short()), &s.input),
                    shape: s.input.shape.clone(),
                    head: s.input.data.iter().take(8).copied().collect(),
                    target: target_label(&s.target),
                })
                .collect()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = &result {
            self.log(format!("미리보기 실패({}): {e}", spec.name));
        }
        self.views.data.preview = Some(views::data::Preview { dataset: id, result });
    }

    // ── 학습 ────────────────────────────────────────────────────

    fn start_train(&mut self, model: ModelId, dataset: DatasetId, now: f64) {
        if self.training.as_ref().map(|t| t.state.is_live()).unwrap_or(false) {
            self.toast("이미 학습이 진행 중입니다", now);
            return;
        }
        if self.doc.file_path.is_none() {
            self.toast("학습 기록을 남길 폴더가 필요합니다 — 프로젝트를 먼저 저장하세요", now);
            self.save_as(now);
            if self.doc.file_path.is_none() {
                return;
            }
        }
        // 형상 오류가 있는 그래프는 엔진에 넘기지 않는다 (검증 통과 그래프만 받는 계약).
        let Some(def) = self.doc.project.models.get(&model).cloned() else { return };
        let rep = shape::infer(&def.graph);
        if !rep.is_ok() {
            self.toast(format!("형상 오류 {}개를 먼저 고치세요", rep.errors.len()), now);
            self.show_dock = true;
            self.dock_tab = DockTab::Issues;
            return;
        }
        let Some(spec) = self.doc.project.datasets.get(&dataset).cloned() else { return };
        let path = self.doc.file_path.clone().unwrap();
        let base_dir = project::base_dir(&path);
        let run_id = RunId::new();
        let run_dir = project::runs_dir(&path, &self.doc.project).join(run_id.short());
        let mut def = def;
        def.train.dataset = Some(dataset);
        let req = TrainRequest {
            run_id,
            model: def,
            dataset: spec,
            base_dir,
            run_dir: run_dir.clone(),
            resume_from: None,
        };
        match TrainSession::start(req, run_dir, now) {
            Ok(s) => {
                self.training = Some(s);
                self.views.train.selected_run = None;
                self.set_view(View::Train);
                self.toast("학습을 시작했습니다", now);
            }
            Err(e) => self.toast(format!("학습을 시작할 수 없습니다: {e}"), now),
        }
    }

    /// 매 프레임 학습 이벤트를 소비한다.
    fn tick_training(&mut self, ctx: &egui::Context, now: f64) {
        let Some(poll) = self.training.as_mut().map(|t| t.poll(now)) else { return };
        for line in poll.logs {
            self.log(line);
        }
        if poll.changed {
            ctx.request_repaint();
        }
        let Some(run) = poll.finished else { return };
        let run = *run;
        let mut ops = vec![Op::UpsertRun { run: run.clone() }];
        // 성공한 실행의 체크포인트는 모델의 가중치가 된다.
        if run.status == RunStatus::Finished {
            let base = self.base_dir();
            if let (Some(ckpt), Some(m)) = (run.checkpoint.clone(), self.doc.project.models.get(&run.model)) {
                // `ModelDef::weights` 는 프로젝트 폴더 기준 상대 경로다 — 폴더째 옮겨도 따라간다.
                ops.push(Op::UpsertModelMeta {
                    id: m.id,
                    name: m.name.clone(),
                    description: m.description.clone(),
                    payload: m.payload,
                    weights: Some(relative_to(&base, Path::new(&ckpt))),
                });
            }
        }
        self.doc.apply_local(ops);
        self.views.train.selected_run = Some(run.id);
        let msg = match run.status {
            RunStatus::Finished => "학습이 끝났습니다".to_string(),
            RunStatus::Stopped => "학습을 중지했습니다".to_string(),
            _ => format!("학습 실패: {}", run.error.clone().unwrap_or_default()),
        };
        self.toast(msg, now);
    }

    fn apply_run_weights(&mut self, id: RunId, now: f64) {
        let Some(run) = self.doc.project.runs.get(&id).cloned() else { return };
        let Some(ckpt) = run.checkpoint.clone() else {
            self.toast("이 실행에는 체크포인트가 없습니다", now);
            return;
        };
        let Some(m) = self.doc.project.models.get(&run.model).cloned() else {
            self.toast("실행이 가리키는 모델이 없습니다", now);
            return;
        };
        let weights = relative_to(&self.base_dir(), Path::new(&ckpt));
        self.doc.apply_local(vec![Op::UpsertModelMeta {
            id: m.id,
            name: m.name,
            description: m.description,
            payload: m.payload,
            weights: Some(weights),
        }]);
        self.toast("가중치를 모델에 적용했습니다", now);
    }

    // ── 패널 ────────────────────────────────────────────────────

    fn toolbar(&mut self, ui: &mut egui::Ui, now: f64) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            if ui.button("🗋 새로").on_hover_text("새 프로젝트 (Ctrl+N)").clicked() {
                self.request_action(PendingAction::New, &ctx, now);
            }
            ui.menu_button("📂 열기", |ui| {
                ui.set_min_width(240.0);
                if ui.button("파일에서…  Ctrl+O").clicked() {
                    self.request_action(PendingAction::Open, &ctx, now);
                    ui.close();
                }
                ui.separator();
                ui.label(RichText::new("샘플").weak());
                for (i, (name, _)) in sample::SAMPLES.iter().enumerate() {
                    if ui.button(*name).clicked() {
                        self.request_action(PendingAction::Sample(i), &ctx, now);
                        ui.close();
                    }
                }
                if !self.recent.is_empty() {
                    ui.separator();
                    ui.label(RichText::new("최근").weak());
                    let recents: Vec<String> = self.recent.iter().cloned().collect();
                    for p in recents {
                        if ui.button(views::short_path(&p)).on_hover_text(&p).clicked() {
                            self.request_action(PendingAction::OpenPath(PathBuf::from(p)), &ctx, now);
                            ui.close();
                        }
                    }
                }
            });
            if ui.button("💾 저장").on_hover_text("Ctrl+S").clicked() {
                self.save(now);
            }
            if ui.button("다른 이름").on_hover_text("Ctrl+Shift+S").clicked() {
                self.save_as(now);
            }
            ui.separator();
            ui.add_enabled_ui(self.doc.can_undo(), |ui| {
                if ui.button("⟲ 되돌리기").on_hover_text("Ctrl+Z").clicked() {
                    self.undo();
                }
            });
            ui.add_enabled_ui(self.doc.can_redo(), |ui| {
                if ui.button("⟳ 다시 실행").on_hover_text("Ctrl+Shift+Z").clicked() {
                    self.redo();
                }
            });
            ui.separator();

            // 장치 선택 — 프로젝트 기본 장치를 바꾼다.
            let current = self.doc.project.settings.default_device;
            let label = self
                .devices
                .iter()
                .find(|d| d.pref == current)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| current.label());
            let devices = self.devices.clone();
            let mut chosen: Option<nl_core::DevicePref> = None;
            let mut refresh = false;
            egui::ComboBox::from_id_salt("device-picker").selected_text(format!("🖳 {label}")).show_ui(ui, |ui| {
                if ui.selectable_label(current == nl_core::DevicePref::Auto, "자동 (첫 GPU → CPU)").clicked() {
                    chosen = Some(nl_core::DevicePref::Auto);
                }
                for d in &devices {
                    if ui.selectable_label(current == d.pref, views::device_label(d)).clicked() {
                        chosen = Some(d.pref);
                    }
                }
                ui.separator();
                if ui.button("↻ 다시 찾기").clicked() {
                    refresh = true;
                }
            });
            if refresh {
                self.devices = nl_engine::enumerate();
            }
            if let Some(pref) = chosen {
                let mut settings = self.doc.project.settings.clone();
                settings.default_device = pref;
                self.doc.apply_local(vec![Op::SetSettings { settings }]);
            }

            ui.separator();
            let live = self.training.as_ref().map(|t| t.state).filter(|s| s.is_live());
            match live {
                None => {
                    let ready = self
                        .active_model()
                        .and_then(|m| self.doc.project.models.get(&m))
                        .and_then(|m| m.train.dataset)
                        .is_some();
                    ui.add_enabled_ui(ready, |ui| {
                        if ui
                            .button(RichText::new("▶ 학습 시작").color(views::COL_OK))
                            .on_hover_text(if ready { "학습 뷰에서 자세히" } else { "모델에 데이터셋을 지정하세요" })
                            .clicked()
                        {
                            if let Some(m) = self.active_model() {
                                if let Some(d) = self.doc.project.models.get(&m).and_then(|m| m.train.dataset) {
                                    self.start_train(m, d, now);
                                }
                            }
                        }
                    });
                }
                Some(views::train::SessionState::Paused) => {
                    if ui.button("▶ 재개").clicked() {
                        if let Some(t) = &self.training {
                            t.handle.resume();
                        }
                    }
                    if ui.button(RichText::new("⏹ 학습 정지").color(views::COL_ERROR)).clicked() {
                        if let Some(t) = &self.training {
                            t.handle.stop();
                        }
                    }
                }
                Some(_) => {
                    if ui.button("⏸ 일시정지").clicked() {
                        if let Some(t) = &self.training {
                            t.handle.pause();
                        }
                    }
                    if ui.button(RichText::new("⏹ 학습 정지").color(views::COL_ERROR)).clicked() {
                        if let Some(t) = &self.training {
                            t.handle.stop();
                        }
                    }
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(&self.doc.project.name).strong());
                ui.separator();
                if ui
                    .selectable_label(self.show_inspector, "📋")
                    .on_hover_text("인스펙터 접기/펴기 (Ctrl+Alt+B)")
                    .clicked()
                {
                    self.show_inspector = !self.show_inspector;
                }
                if ui.selectable_label(self.show_dock, "▤").on_hover_text("하단 도크 (Ctrl+J)").clicked() {
                    self.show_dock = !self.show_dock;
                }
                if ui.selectable_label(self.show_outline, "☰").on_hover_text("아웃라인 (Ctrl+B)").clicked() {
                    self.show_outline = !self.show_outline;
                }
            });
        });
    }

    fn view_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (i, view) in View::ALL.iter().enumerate() {
                let text = if view.is_placeholder() {
                    RichText::new(view.label()).color(views::COL_WEAK)
                } else {
                    RichText::new(view.label())
                };
                if ui.selectable_label(self.view == *view, text).on_hover_text(format!("Ctrl+{}", i + 1)).clicked() {
                    self.set_view(*view);
                }
            }
            ui.separator();
            ui.label(RichText::new(self.view.hint()).weak().size(11.0));
        });
    }

    fn dock(&mut self, ui: &mut egui::Ui, now: f64) {
        let issues = self.issues();
        ui.horizontal(|ui| {
            let label = if issues.is_empty() { "⚠ 문제".to_string() } else { format!("⚠ 문제 {}", issues.len()) };
            if ui.selectable_label(self.dock_tab == DockTab::Issues, label).clicked() {
                self.dock_tab = DockTab::Issues;
            }
            if ui.selectable_label(self.dock_tab == DockTab::Log, "☰ 로그").clicked() {
                self.dock_tab = DockTab::Log;
            }
            if ui.selectable_label(self.dock_tab == DockTab::Activity, "🕘 활동").clicked() {
                self.dock_tab = DockTab::Activity;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✖").on_hover_text("도크 닫기 (Ctrl+J)").clicked() {
                    self.show_dock = false;
                }
            });
        });
        ui.separator();
        match self.dock_tab {
            DockTab::Issues => self.dock_issues(ui, &issues),
            DockTab::Log => {
                egui::ScrollArea::vertical().id_salt("dock-log").stick_to_bottom(true).show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(RichText::new("로그가 없습니다").weak());
                    }
                    for l in &self.logs {
                        ui.label(RichText::new(l).size(11.5));
                    }
                });
            }
            DockTab::Activity => {
                egui::ScrollArea::vertical().id_salt("dock-activity").stick_to_bottom(true).show(ui, |ui| {
                    for (at, msg) in &self.activity {
                        ui.label(RichText::new(format!("{} {msg}", at.format("%H:%M:%S"))).size(11.5));
                    }
                });
            }
        }
        let _ = now;
    }

    fn dock_issues(&mut self, ui: &mut egui::Ui, issues: &[nl_core::Issue]) {
        if issues.is_empty() {
            ui.label(RichText::new("문제 없음 — 모델을 학습할 수 있습니다").color(views::COL_OK));
            return;
        }
        let mut jump: Option<(ModelId, Option<NodeId>)> = None;
        egui::ScrollArea::vertical().id_salt("dock-issues").show(ui, |ui| {
            for issue in issues {
                let (icon, color) = match issue.severity {
                    Severity::Error => ("✖", views::COL_ERROR),
                    Severity::Warning => ("⚠", views::COL_WARN),
                };
                let target = match &issue.at {
                    Where::Node(m, n) => Some((*m, Some(*n))),
                    Where::Model(m) => Some((*m, None)),
                    _ => None,
                };
                let resp = ui.add(
                    egui::Label::new(RichText::new(format!("{icon} {}", issue.message)).color(color).size(12.0))
                        .sense(egui::Sense::click()),
                );
                if target.is_some() && resp.on_hover_text("눌러서 해당 노드로 이동").clicked() {
                    jump = target;
                }
            }
        });
        if let Some((model, node)) = jump {
            self.set_view(View::Model);
            match node {
                Some(n) => {
                    self.canvas.set_selection(Selection::Node(model, n));
                    self.canvas.pending_focus = Some(n);
                }
                None => {
                    self.canvas.set_selection(Selection::Model(model));
                    self.canvas.request_fit();
                }
            }
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let file = self
                .doc
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "(저장 안 됨)".to_owned());
            let star = if self.doc.modified { "*" } else { "" };
            ui.label(format!("{file}{star}"));
            ui.separator();
            let pref = self.doc.project.settings.default_device;
            let dev = self.devices.iter().find(|d| d.pref == pref).map(|d| d.name.clone()).unwrap_or_else(|| pref.label());
            ui.label(format!("장치 {dev}"));
            ui.separator();
            if self.view == View::Model {
                ui.label(format!("표시 {}/{} 노드", self.canvas.visible_nodes, self.canvas.total_nodes))
                    .on_hover_text("뷰포트 밖 노드는 그리지 않습니다");
            } else {
                // 캔버스를 그리지 않는 뷰에서는 컬링 수치가 지난 프레임 값이라 뜻이 없다.
                let layers =
                    self.active_model().and_then(|m| self.doc.project.models.get(&m)).map(|m| m.graph.nodes.len()).unwrap_or(0);
                ui.label(format!("레이어 {layers}"));
            }
            let n = self.canvas.selection_count();
            if n > 1 {
                ui.separator();
                ui.label(format!("{n}개 선택"));
            }
            if let Some(t) = &self.training {
                ui.separator();
                let label = match t.state {
                    views::train::SessionState::Running => "학습 중",
                    views::train::SessionState::Paused => "학습 일시정지",
                    views::train::SessionState::Finished => "학습 완료",
                    views::train::SessionState::Failed => "학습 실패",
                };
                ui.label(RichText::new(label).color(if t.state.is_live() { views::COL_SELECT } else { views::COL_WEAK }));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak());
                ui.separator();
                ui.label(format!("{:.0}%", self.canvas.camera.zoom * 100.0));
                if ui.small_button("초기화").on_hover_text("줌 100%").clicked() {
                    self.canvas.reset_zoom();
                }
            });
        });
    }

    // ── 키 ──────────────────────────────────────────────────────

    fn handle_keys(&mut self, ctx: &egui::Context, now: f64) {
        if ctx.egui_wants_keyboard_input() || self.pending_action.is_some() {
            return;
        }
        let k = ctx.input(|i| Keys {
            del: i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace),
            undo: i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
            redo: (i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::Z))
                || (i.modifiers.command && i.key_pressed(egui::Key::Y)),
            save: i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::S),
            save_as: i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::S),
            open: i.modifiers.command && i.key_pressed(egui::Key::O),
            new: i.modifiers.command && i.key_pressed(egui::Key::N),
            fit: i.key_pressed(egui::Key::F) && !i.modifiers.command && !i.modifiers.alt,
            select_all: i.modifiers.command && i.key_pressed(egui::Key::A),
            duplicate: i.modifiers.command && i.key_pressed(egui::Key::D),
            escape: i.key_pressed(egui::Key::Escape),
            outline: i.modifiers.command && !i.modifiers.alt && i.key_pressed(egui::Key::B),
            inspector: i.modifiers.command && i.modifiers.alt && i.key_pressed(egui::Key::B),
            dock: i.modifiers.command && i.key_pressed(egui::Key::J),
            view: i.modifiers.command.then(|| VIEW_KEYS.iter().position(|key| i.key_pressed(*key))).flatten(),
        });

        if k.undo {
            self.undo();
        }
        if k.redo {
            self.redo();
        }
        if k.save {
            self.save(now);
        }
        if k.save_as {
            self.save_as(now);
        }
        if k.open {
            self.request_action(PendingAction::Open, ctx, now);
        }
        if k.new {
            self.request_action(PendingAction::New, ctx, now);
        }
        if k.outline {
            self.show_outline = !self.show_outline;
        }
        if k.inspector {
            self.show_inspector = !self.show_inspector;
        }
        if k.dock {
            self.show_dock = !self.show_dock;
        }
        if let Some(i) = k.view {
            self.set_view(View::from_index(i));
        }
        if k.escape && !ctx.any_popup_open() {
            if self.canvas.interaction_active() {
                self.canvas.cancel_interaction();
            } else if self.rename.is_some() {
                self.rename = None;
            }
        }
        // 캔버스 전용 단축키.
        if self.view != View::Model {
            return;
        }
        if k.fit {
            self.canvas.request_fit();
        }
        let Some(model) = self.active_model() else { return };
        if k.select_all {
            let ids: Vec<NodeId> =
                self.doc.project.models.get(&model).map(|m| m.graph.nodes.keys().copied().collect()).unwrap_or_default();
            self.canvas.select_nodes(model, ids, false);
        }
        if k.duplicate {
            let ids = self.canvas.selected_nodes();
            if !ids.is_empty() {
                self.apply_canvas_action(model, CanvasAction::DuplicateNodes(ids), now);
            }
        }
        if k.del {
            self.delete_selection(model, now);
        }
    }

    fn delete_selection(&mut self, model: ModelId, now: f64) {
        match self.canvas.selection {
            Selection::Edge(m, id) => {
                self.doc.apply_local(vec![Op::DeleteEdge { model: m, id }]);
                self.canvas.set_selection(Selection::Model(m));
            }
            _ => {
                let ids = self.canvas.selected_nodes();
                if !ids.is_empty() {
                    self.apply_canvas_action(model, CanvasAction::DeleteNodes(ids), now);
                }
            }
        }
    }

    fn draw_toasts(&mut self, ctx: &egui::Context, now: f64) {
        self.toasts.retain(|(_, until)| *until > now);
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::Vec2::new(0.0, -46.0))
            .interactable(false)
            .show(ctx, |ui| {
                // 폭을 잡아 주지 않으면 긴 경로가 한 글자씩 세로로 접힌다.
                ui.set_max_width(560.0);
                for (msg, _) in &self.toasts {
                    egui::Frame::new()
                        .fill(Color32::from_rgba_premultiplied(20, 22, 28, 230))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.label(RichText::new(msg).color(Color32::WHITE));
                        });
                    ui.add_space(4.0);
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }

    /// 아웃라인의 이름 변경 버퍼 (outline.rs 가 쓴다).
    pub(crate) fn rename_slot(&mut self) -> &mut Option<(Selection, String)> {
        &mut self.rename
    }

    /// 인스펙터 텍스트 버퍼를 지금 선택에 맞춰 다시 채울지 (`true` 면 채워야 한다).
    pub(crate) fn buffers_stale(&mut self) -> bool {
        let key = (self.canvas.selection, self.doc.external_edits);
        if self.buf_owner == Some(key) {
            false
        } else {
            self.buf_owner = Some(key);
            true
        }
    }
}

struct Keys {
    del: bool,
    undo: bool,
    redo: bool,
    save: bool,
    save_as: bool,
    open: bool,
    new: bool,
    fit: bool,
    select_all: bool,
    duplicate: bool,
    escape: bool,
    outline: bool,
    inspector: bool,
    dock: bool,
    view: Option<usize>,
}

// ── eframe ──────────────────────────────────────────────────────────

impl eframe::App for NlApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        let now = ctx.input(|i| i.time);

        self.tick_training(ctx, now);
        self.doc.tick(now);
        if self.doc.in_burst() {
            // 입력이 멎어도 burst 를 undo 항목으로 확정하려면 한 번 더 깨어나야 한다.
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(BURST_QUIET + 0.05));
        }

        // 저장하지 않은 변경이 있으면 창 닫기를 한 번 붙잡는다.
        if !self.close_confirmed && self.doc.modified && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending_action = Some(PendingAction::Close);
        }

        self.handle_keys(ctx, now);

        egui::Panel::top("toolbar").show(root, |ui| {
            ui.add_space(2.0);
            self.toolbar(ui, now);
            ui.add_space(2.0);
            ui.separator();
            self.view_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(root, |ui| {
            self.status_bar(ui);
        });
        if self.show_dock {
            egui::Panel::bottom("dock")
                .resizable(true)
                .default_size(180.0)
                .size_range(110.0..=460.0)
                .show(root, |ui| self.dock(ui, now));
        }
        if self.show_outline {
            egui::Panel::left("outline")
                .resizable(true)
                .default_size(240.0)
                .size_range(180.0..=400.0)
                .show(root, |ui| self.outline(ui, now));
        }
        if self.show_inspector {
            egui::Panel::right("inspector")
                .resizable(true)
                .default_size(300.0)
                .size_range(240.0..=460.0)
                .show(root, |ui| self.inspector(ui, now));
        }

        let report = self.shapes();
        let actions = egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(root.visuals().panel_fill))
            .show(root, |ui| self.central(ui, &report, now))
            .inner;
        for action in actions {
            self.apply_view_action(action, ctx, now);
        }

        self.unsaved_modal(ctx, now);
        self.draw_toasts(ctx, now);

        if self.warmup_frames > 0 {
            self.warmup_frames -= 1;
            ctx.request_repaint();
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let last_file = self.doc.file_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        eframe::set_value(storage, "last_file", &last_file);
        eframe::set_value(storage, "recent", &self.recent);
        eframe::set_value(storage, "show_outline", &self.show_outline);
        eframe::set_value(storage, "show_inspector", &self.show_inspector);
        eframe::set_value(storage, "show_dock", &self.show_dock);
        eframe::set_value(storage, "view", &self.view.index());
    }
}

impl NlApp {
    /// 중앙 뷰 하나. 캔버스 편집은 여기서 op 로 바꾸고, 나머지는 `ViewAction` 으로 올려보낸다.
    fn central(&mut self, ui: &mut egui::Ui, report: &ShapeReport, now: f64) -> Vec<ViewAction> {
        let mut out = Vec::new();
        match self.view {
            View::Model => {
                // `ViewCtx` 는 doc·devices·training 만 빌리고 캔버스는 따로 빌린다 (필드가 겹치지 않는다).
                let model_out = {
                    let ctx = ViewCtx {
                        project: &self.doc.project,
                        selection: self.canvas.selection,
                        devices: &self.devices,
                        base_dir: self.doc.file_path.as_deref().and_then(|p| p.parent()),
                        training: self.training.as_ref(),
                        now,
                    };
                    views::model::show(ui, &ctx, &mut self.canvas, report)
                };
                out.extend(model_out.actions);
                if let Some(model) = self.active_model() {
                    for a in model_out.canvas {
                        self.apply_canvas_action(model, a, now);
                    }
                }
            }
            View::Data => {
                let ctx = ViewCtx {
                    project: &self.doc.project,
                    selection: self.canvas.selection,
                    devices: &self.devices,
                    base_dir: self.doc.file_path.as_deref().and_then(|p| p.parent()),
                    training: self.training.as_ref(),
                    now,
                };
                out.extend(views::data::show(ui, &ctx, &mut self.views.data));
            }
            View::Train => {
                let ctx = ViewCtx {
                    project: &self.doc.project,
                    selection: self.canvas.selection,
                    devices: &self.devices,
                    base_dir: self.doc.file_path.as_deref().and_then(|p| p.parent()),
                    training: self.training.as_ref(),
                    now,
                };
                out.extend(views::train::show(ui, &ctx, &mut self.views.train));
            }
            View::Resources => {
                let ctx = ViewCtx {
                    project: &self.doc.project,
                    selection: self.canvas.selection,
                    devices: &self.devices,
                    base_dir: self.doc.file_path.as_deref().and_then(|p| p.parent()),
                    training: self.training.as_ref(),
                    now,
                };
                out.extend(views::resources::show(ui, &ctx, &mut self.views.resources));
            }
            View::Pipeline => views::placeholder(
                ui,
                "파이프라인",
                "소스 → 모델 → 싱크 노드 캔버스. 화면 캡처·HTTP·stdio 를 모델에 잇습니다.",
            ),
            View::Gui => views::placeholder(ui, "GUI 디자이너", "배포 앱의 위젯을 배치하고 파이프라인에 바인딩합니다."),
            View::Build => views::placeholder(ui, "빌드", "런타임 바이너리에 번들을 붙여 배포 산출물을 만듭니다."),
        }
        out
    }
}

// ── 헬퍼 ────────────────────────────────────────────────────────────

/// 노드들을 복제하는 op 묶음과 새 id 목록. 복제 집합 안쪽 연결은 함께 복사한다.
pub fn duplicate_ops(graph: &nl_core::Graph, model: ModelId, ids: &[NodeId]) -> (Vec<Op>, Vec<NodeId>) {
    let offset = crate::canvas::duplicate_offset();
    let mut map: BTreeMap<NodeId, NodeId> = BTreeMap::new();
    let mut ops = Vec::new();
    let mut new_ids = Vec::new();
    for id in ids {
        let Some(src) = graph.nodes.get(id) else { continue };
        let mut node = src.clone();
        node.id = NodeId::new();
        node.pos = [src.pos[0] + offset.x, src.pos[1] + offset.y];
        map.insert(*id, node.id);
        new_ids.push(node.id);
        ops.push(Op::UpsertNode { model, node });
    }
    for e in graph.edges.values() {
        if let (Some(&from), Some(&to)) = (map.get(&e.from), map.get(&e.to.node)) {
            ops.push(Op::UpsertEdge { model, edge: Edge::new(from, Port::new(to, e.to.slot)) });
        }
    }
    (ops, new_ids)
}

/// 파일 이름으로 쓸 수 없는 글자를 바꾼다.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let t = cleaned.trim();
    if t.is_empty() {
        "project".into()
    } else {
        t.to_string()
    }
}

/// `base` 아래라면 상대 경로 문자열로, 아니면 절대 경로 그대로.
pub fn relative_to(base: &Path, path: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// 미리보기 타깃 텐서를 한 줄로.
fn target_label(t: &nl_engine::HostTensor) -> String {
    if t.data.is_empty() {
        return "-".into();
    }
    if t.data.len() == 1 {
        return format!("{:.4}", t.data[0]);
    }
    let idx = t.argmax_last().first().copied().unwrap_or(0);
    format!("클래스 {idx}")
}

/// 샘플 입력이 이미지 형상이면 썸네일 텍스처로. 값 범위는 최소·최대로 정규화한다.
fn sample_texture(ctx: &egui::Context, name: &str, t: &nl_engine::HostTensor) -> Option<egui::TextureHandle> {
    let (c, h, w) = match t.shape.as_slice() {
        [c, h, w] => (*c, *h, *w),
        [h, w] => (1, *h, *w),
        _ => return None,
    };
    if !(1..=4).contains(&c) || h == 0 || w == 0 || t.data.len() < c * h * w {
        return None;
    }
    let (min, max) = t.data.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = if (max - min).abs() < 1e-9 { 1.0 } else { max - min };
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let at = |ch: usize| -> u8 {
                let v = t.data[ch * h * w + y * w + x];
                (((v - min) / span).clamp(0.0, 1.0) * 255.0) as u8
            };
            if c >= 3 {
                rgb.extend_from_slice(&[at(0), at(1), at(2)]);
            } else {
                let g = at(0);
                rgb.extend_from_slice(&[g, g, g]);
            }
        }
    }
    let image = egui::ColorImage::from_rgb([w, h], &rgb);
    Some(ctx.load_texture(name, image, egui::TextureOptions::NEAREST))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::{Graph, LayerKind, Node};

    fn doc_with_sample() -> DocState {
        DocState::new(sample::xor_project())
    }

    fn first_model(p: &Project) -> ModelId {
        *p.models.keys().next().unwrap()
    }

    #[test]
    fn view_index_matches_shortcut_order() {
        assert_eq!(View::ALL.len(), VIEW_KEYS.len());
        for (i, v) in View::ALL.iter().enumerate() {
            assert_eq!(v.index(), i);
            assert_eq!(View::from_index(i), *v);
        }
        assert_eq!(View::from_index(99), View::Model);
    }

    #[test]
    fn placeholder_views_are_exactly_the_second_phase_ones() {
        let ph: Vec<View> = View::ALL.iter().copied().filter(|v| v.is_placeholder()).collect();
        assert_eq!(ph, vec![View::Pipeline, View::Gui, View::Build]);
    }

    #[test]
    fn apply_local_undo_redo_round_trips() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let before = doc.project.clone();
        let node = Node::new(LayerKind::Flatten, [10.0, 20.0]);
        let id = node.id;
        doc.apply_local(vec![Op::UpsertNode { model, node }]);
        assert!(doc.project.models[&model].graph.nodes.contains_key(&id));
        assert!(doc.modified);
        doc.undo();
        assert_eq!(doc.project, before);
        doc.redo();
        assert!(doc.project.models[&model].graph.nodes.contains_key(&id));
    }

    #[test]
    fn deleting_a_node_restores_its_edges_on_undo() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let graph = doc.project.models[&model].graph.clone();
        // 사슬 가운데 노드를 지운다 — 엣지 두 개가 함께 사라진다.
        let victim = *graph.nodes.keys().nth(1).unwrap();
        let edges_before = graph.edges.len();
        let before = doc.project.clone();
        doc.apply_local(vec![Op::DeleteNode { model, id: victim }]);
        assert!(doc.project.models[&model].graph.edges.len() < edges_before);
        doc.undo();
        assert_eq!(doc.project, before);
    }

    #[test]
    fn moving_nodes_is_one_undo_step() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let graph = doc.project.models[&model].graph.clone();
        let before = doc.project.clone();
        let ops: Vec<Op> = graph
            .nodes
            .values()
            .map(|n| {
                let mut n = n.clone();
                n.pos = [n.pos[0] + 50.0, n.pos[1] + 10.0];
                Op::UpsertNode { model, node: n }
            })
            .collect();
        doc.apply_local(ops);
        assert!(doc.project != before);
        doc.undo();
        assert_eq!(doc.project, before, "다중 이동은 undo 한 번으로 되돌아온다");
    }

    #[test]
    fn burst_edits_collapse_into_one_undo_entry() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let node_id = *doc.project.models[&model].graph.nodes.keys().next().unwrap();
        let before = doc.project.clone();
        let original = doc.project.models[&model].graph.nodes[&node_id].name.clone();
        // 인스펙터 타이핑처럼 한 글자씩 (같은 burst 안).
        for (i, ch) in "abc".chars().enumerate() {
            doc.project.models.get_mut(&model).unwrap().graph.nodes.get_mut(&node_id).unwrap().name.push(ch);
            doc.note_edited(1.0 + i as f64 * 0.1);
        }
        assert!(doc.in_burst());
        // 1초 넘게 조용해지면 확정.
        doc.tick(5.0);
        assert!(!doc.in_burst());
        assert_eq!(doc.undo.len(), 1, "세 글자가 undo 한 항목");
        doc.undo();
        assert_eq!(doc.project, before);
        doc.redo();
        assert_eq!(doc.project.models[&model].graph.nodes[&node_id].name, format!("{original}abc"));
    }

    #[test]
    fn a_burst_that_reverts_itself_changes_nothing() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let node_id = *doc.project.models[&model].graph.nodes.keys().next().unwrap();
        let original = doc.project.models[&model].graph.nodes[&node_id].name.clone();
        doc.project.models.get_mut(&model).unwrap().graph.nodes.get_mut(&node_id).unwrap().name = "임시".into();
        doc.note_edited(1.0);
        doc.project.models.get_mut(&model).unwrap().graph.nodes.get_mut(&node_id).unwrap().name = original;
        doc.note_edited(1.2);
        doc.tick(5.0);
        assert!(!doc.can_undo(), "결국 그대로면 undo 항목이 생기지 않는다");
        assert!(!doc.modified);
    }

    #[test]
    fn undo_during_a_burst_finalizes_it_first() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let node_id = *doc.project.models[&model].graph.nodes.keys().next().unwrap();
        let before = doc.project.clone();
        doc.project.models.get_mut(&model).unwrap().graph.nodes.get_mut(&node_id).unwrap().name = "새 이름".into();
        doc.note_edited(1.0);
        doc.undo();
        assert_eq!(doc.project, before);
    }

    #[test]
    fn apply_burst_collects_ops_into_one_entry() {
        let mut doc = doc_with_sample();
        let payload = doc.project.payloads.values().next().unwrap().clone();
        let before = doc.project.clone();
        for len in [8usize, 16, 32] {
            let mut next = payload.clone();
            next.inputs[0].kind = nl_core::payload::FieldKind::Vector { len };
            doc.apply_burst(vec![Op::UpsertPayload { payload: next }], 1.0);
        }
        doc.tick(5.0);
        assert_eq!(doc.undo.len(), 1);
        doc.undo();
        assert_eq!(doc.project, before);
    }

    #[test]
    fn edit_seq_advances_on_every_change_path() {
        let mut doc = doc_with_sample();
        let model = first_model(&doc.project);
        let start = doc.edit_seq;
        doc.apply_local(vec![Op::UpsertNode { model, node: Node::new(LayerKind::Flatten, [0.0, 0.0]) }]);
        let after_local = doc.edit_seq;
        assert!(after_local > start);
        doc.undo();
        assert!(doc.edit_seq > after_local);
        doc.redo();
        doc.note_edited(1.0);
        assert!(doc.edit_seq > after_local + 2);
    }

    #[test]
    fn duplicate_copies_inner_edges_only() {
        let mut g = Graph::default();
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = g.add_node(Node::new(LayerKind::Linear { out_features: 2, bias: true }, [220.0, 0.0]));
        let c = g.add_node(Node::new(LayerKind::Output, [440.0, 0.0]));
        g.add_edge(a, Port::new(b, 0)).unwrap();
        g.add_edge(b, Port::new(c, 0)).unwrap();
        let model = ModelId::from_u128(7);
        let (ops, ids) = duplicate_ops(&g, model, &[a, b]);
        assert_eq!(ids.len(), 2);
        let nodes = ops.iter().filter(|o| matches!(o, Op::UpsertNode { .. })).count();
        let edges = ops.iter().filter(|o| matches!(o, Op::UpsertEdge { .. })).count();
        assert_eq!(nodes, 2);
        assert_eq!(edges, 1, "복제 집합 바깥으로 나가는 엣지는 복사하지 않는다");
        // 새 노드는 원본과 다른 id 이고 위치가 어긋나 있다.
        let mut p = Project::new("t");
        p.models.insert(model, nl_core::ModelDef { id: model, graph: g.clone(), ..nl_core::ModelDef::new("m") });
        apply_ops(&mut p, &ops);
        let dup = &p.models[&model].graph;
        assert_eq!(dup.nodes.len(), 5);
        for id in &ids {
            assert!(!g.nodes.contains_key(id));
            assert_ne!(dup.nodes[id].pos, [0.0, 0.0]);
        }
    }

    #[test]
    fn duplicate_of_nothing_is_a_no_op() {
        let g = Graph::default();
        let (ops, ids) = duplicate_ops(&g, ModelId::from_u128(1), &[NodeId::from_u128(9)]);
        assert!(ops.is_empty() && ids.is_empty());
    }

    #[test]
    fn sanitize_makes_a_usable_file_name() {
        assert_eq!(sanitize("XOR 샘플"), "XOR 샘플");
        assert_eq!(sanitize("a/b:c"), "a_b_c");
        assert_eq!(sanitize("   "), "project");
    }

    #[test]
    fn relative_to_strips_the_base_or_keeps_the_absolute_path() {
        let base = Path::new("/proj");
        assert_eq!(relative_to(base, Path::new("/proj/demo.runs/a/w.safetensors")), "demo.runs/a/w.safetensors");
        assert_eq!(relative_to(base, Path::new("/elsewhere/w.safetensors")), "/elsewhere/w.safetensors");
    }
}
