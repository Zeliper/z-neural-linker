//! 앱 셸: 문서 상태(op 기반 undo/redo), 패널 UI, 파일 IO, 학습 세션.
//! 동기화 서버가 없다는 점만 빼면 trust-pms `pms-app::app` 과 같은 구조다.

use crate::canvas::{CanvasAction, CanvasState, Selection, SelectionState};
use crate::pcanvas::{PipelineAction, PipelineCanvas};
use crate::project::{self, Recent};
use crate::record::{self, RecordSession, Shot, ShotPreview};
use crate::sample;
use crate::session::{self, RunnerSession};
use crate::tools::{self, Plan, ToolEvent, ToolState};
use crate::views::build::{BuildEvent, BuildRequest};
use crate::views::{self, train::TrainSession, ViewAction, ViewCtx, ViewState};
use chrono::{DateTime, Local};
use eframe::egui::{self, Color32, RichText};
use nl_core::dataset::{DataSource, DatasetSpec, SyntheticKind};
use nl_core::shape::{self, ShapeReport};
use nl_core::validate::Where;
use nl_core::{
    apply_ops, diff_ops, inverse_ops, BuildSpec, BuildTarget, DatasetId, DevicePref, Edge, Link, LinkId, ModelId, Node,
    NodeId, Op, PNode, PNodeId, Port, Project, RunId, RunStatus, Severity,
};
use nl_engine::{DeviceInfo, TrainRequest};
use nl_gui::{GuiEvent, GuiState};
use nl_io::runner::RunnerInput;
use nl_io::MonitorInfo;
use nl_core::pipeline::Region;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

/// 뷰가 받는 읽기 전용 컨텍스트를 만든다.
///
/// 매크로인 이유: 필드를 하나하나 직접 빌려야 `&mut self.canvas` 나 `&mut self.views` 와 겹치지 않는다.
/// 메서드로 빼면 `&self` 전체를 빌려 버려서 캔버스를 함께 넘길 수 없다.
macro_rules! view_ctx {
    ($app:ident, $now:expr) => {
        ViewCtx {
            project: &$app.doc.project,
            selection: $app.sel.primary,
            devices: &$app.devices,
            base_dir: $app.doc.file_path.as_deref().and_then(|p| p.parent()),
            training: $app.training.as_ref(),
            monitors: &$app.monitors,
            monitors_error: $app.monitors_error.as_deref(),
            recording: $app.recording.as_ref(),
            shot: &$app.shot,
            update_check: $app.update_check,
            update_state: $app.updater.as_ref().map(|u| u.state()),
            now: $now,
        }
    };
}
pub(crate) use view_ctx;

const UNDO_LIMIT: usize = 100;
/// 이 시간(초) 동안 편집이 없으면 하나의 undo 단위(burst)가 끝난 것으로 본다.
pub const BURST_QUIET: f64 = 1.0;
/// 도크 로그·활동 기록의 최대 줄 수.
const LOG_LIMIT: usize = 400;
/// 백그라운드 작업(도구 설치·빌드) 채널을 다시 볼 간격.
const REPOLL: std::time::Duration = std::time::Duration::from_millis(80);
/// 실행 중 Esc 를 이만큼 누르고 있으면 킬 스위치가 동작한다.
const KILL_HOLD: f64 = 0.5;
/// 설치 동의 모달의 고정 폭. 긴 주소·경로는 이 폭에 맞춰 접힌다.
const MODAL_WIDTH: f32 = 460.0;
/// 모달의 왼쪽 이름 칸 폭.
const MODAL_KEY_WIDTH: f32 = 52.0;
/// 단계 목록이 이보다 길어지면 모달 안에서 스크롤한다.
const MODAL_STEPS_HEIGHT: f32 = 180.0;

/// 모달 한 줄: 왼쪽에 이름, 오른쪽에 남은 폭으로 접히는 값.
///
/// `egui::Grid` 는 칸에 무한 폭을 주기 때문에 안에 넣은 라벨이 줄바꿈되지 않는다.
/// 설치 계획에는 URL·경로·설명처럼 긴 문자열이 들어가 그대로 두면 모달이 화면 밖까지 커진다.
fn plan_row(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(MODAL_KEY_WIDTH, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.label(RichText::new(key).color(views::COL_WEAK));
            },
        );
        ui.add(egui::Label::new(value).wrap());
    });
}

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
            View::Pipeline => "소스 → 모델 → 싱크 — 우클릭으로 노드 추가, 시험 실행으로 확인",
            View::Gui => "배포 앱의 위젯 배치와 파이프라인 바인딩",
            View::Build => "대상·도구 상태·산출물 — 런타임에 번들을 붙여 배포본을 만든다",
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

    /// 노드 캔버스를 쓰는 뷰인가 (전체 보기·다중 선택 단축키가 뜻을 가진다).
    pub fn uses_canvas(self) -> bool {
        matches!(self, View::Model | View::Pipeline)
    }
}

/// 설정을 실제 장치로 푼다. **백그라운드 스레드에서만 부른다.**
/// 프로젝트 폴더 밖에 파일을 쓰기 전에 묻는 내용 (보안 리뷰 M22).
#[derive(Clone, Debug, PartialEq)]
pub struct OutsideAsk {
    /// 무엇을 쓰는 폴더인지 ("산출물").
    pub what: String,
    /// 실제로 쓰게 될 폴더.
    pub dir: PathBuf,
}

/// 백그라운드 장치 확인이 UI 로 보내는 소식.
enum DeviceMsg {
    /// 열거된 장치 목록 (콤보·자원 뷰).
    List(Vec<DeviceInfo>),
    /// 자동 선택까지 끝났다. 이 뒤로는 `nl_engine::resolve_cached` 가 곧바로 답한다.
    Ready { note: String },
}

/// 녹화 라벨 스위치로 쓰는 숫자키 0~9.
const DIGIT_KEYS: [egui::Key; 10] = [
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
];

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
    /// 앱 전체가 공유하는 선택 상태 (두 캔버스·아웃라인·인스펙터가 같은 것을 본다).
    pub sel: SelectionState,
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
    /// 파이프라인 노드 캔버스.
    pub pcanvas: PipelineCanvas,
    /// GUI 디자이너·미리보기가 공유하는 위젯 상태.
    pub gui_state: GuiState,
    /// 시험 실행 중인 파이프라인.
    pub runner: Option<RunnerSession>,
    /// 마우스·키보드 싱크 무장 (기본 꺼짐).
    pub arm_input: bool,
    /// GUI 뷰 미리보기 모드.
    pub gui_preview: bool,
    /// 실행 중 Esc 를 누르기 시작한 시각 (킬 스위치).
    esc_since: Option<f64>,
    /// 화면 캡처 편집용 모니터 목록 캐시.
    pub(crate) monitors: Vec<MonitorInfo>,
    pub(crate) monitors_error: Option<String>,
    /// 도구 상태 캐시와 그 캐시를 만든 대상 목록.
    tool_states: Vec<ToolState>,
    /// 그 목록을 만든 대상들. `None` 이면 아직 검사하지 않았다.
    tool_targets: Option<Vec<BuildTarget>>,
    /// 동의를 기다리는 설치 계획.
    /// 동의를 기다리는 설치 계획. 테스트가 모달 레이아웃을 확인하려고 직접 채운다.
    pub pending_plan: Option<Plan>,
    /// 계획을 만드는 중(네트워크) / 설치 중 / 빌드 중인 작업 채널.
    plan_job: Option<Receiver<Result<Plan, String>>>,
    tool_job: Option<Receiver<ToolEvent>>,
    tool_progress: Option<f32>,
    /// 진행 중인 설치를 멈추라는 신호. 청크 사이에서 확인된다.
    tool_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 프로젝트 폴더 밖에 쓰려 할 때 띄우는 확인 질문.
    outside_ask: Option<OutsideAsk>,
    /// 사용자가 방금 승인한 질문. 한 번의 빌드에만 쓰인다.
    outside_confirmed: Option<OutsideAsk>,
    build_job: Option<Receiver<BuildEvent>>,
    /// 장치 열거·검사 백그라운드 작업. UI 스레드는 이 둘을 절대 직접 부르지 않는다.
    device_job: Option<Receiver<DeviceMsg>>,
    /// 백그라운드 확인을 이미 맡겼는가. 한 번이면 충분하다 — 그 뒤로는 엔진 캐시가 답한다.
    device_asked: bool,
    /// 장치 확인이 한 번이라도 끝났는가 (하네스 마커를 한 번만 찍으려고).
    devices_ready: bool,
    /// 백그라운드 장치 확인을 할지. 테스트에서만 끈다.
    probe_enabled: bool,
    /// 첫 화면을 다 그렸는가 (하네스 마커를 한 번만 찍으려고).
    ready_logged: bool,
    /// 창이 키보드 포커스를 받은 적이 있는가 (하네스 마커를 한 번만 찍으려고).
    focus_logged: bool,
    /// 진행 중인 화면 녹화.
    pub recording: Option<RecordSession>,
    /// "지금 한 장 캡처" 결과와 진행 중인 캡처 작업.
    pub shot: ShotPreview,
    shot_job: Option<Receiver<Result<Shot, String>>>,
    /// 빌더 자체 업데이트.
    pub(crate) updater: Option<nl_update::Updater>,
    update_show: bool,
    /// 시작할 때 업데이트를 확인할지 (설정에 저장).
    pub(crate) update_check: bool,
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
            sel: SelectionState::default(),
            canvas: CanvasState::new(),
            views: ViewState::default(),
            view,
            // 장치 열거는 어댑터를 실제로 여는 일이라 수 초가 걸린다. 백그라운드에서 채운다.
            devices: Vec::new(),
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
            pcanvas: PipelineCanvas::new(),
            gui_state: GuiState::default(),
            runner: None,
            arm_input: false,
            gui_preview: false,
            esc_since: None,
            monitors: Vec::new(),
            monitors_error: None,
            tool_states: Vec::new(),
            tool_targets: None,
            pending_plan: None,
            plan_job: None,
            tool_job: None,
            tool_progress: None,
            tool_cancel: None,
            outside_ask: None,
            outside_confirmed: None,
            build_job: None,
            device_job: None,
            device_asked: false,
            devices_ready: false,
            probe_enabled: true,
            ready_logged: false,
            focus_logged: false,
            recording: None,
            shot: ShotPreview::default(),
            shot_job: None,
            updater: None,
            update_show: false,
            update_check: stored("update_check", true),
            rename: None,
            buf_owner: None,
            shape_buf: String::new(),
            warmup_frames: 3,
        };
        // 첫 선택은 첫 모델 — 인스펙터가 빈 채로 뜨지 않는다.
        if let Some(&id) = app.doc.project.models.keys().next() {
            app.sel.set(Selection::Model(id));
        } else {
            app.sel.set(Selection::Project);
        }
        app.views.build.manifest_url = cc
            .storage
            .and_then(|s| eframe::get_value::<String>(s, "runtime_manifest"))
            .unwrap_or_default();
        app.refresh_monitors();
        app.start_device_probe();
        app.start_update_check();
        if let Some(e) = startup_error {
            app.toast(e, 0.0);
        }
        app
    }

    // ── 장치·모니터 ─────────────────────────────────────────────

    /// 장치를 열거하고 기본 장치가 실제로 도는지까지 **백그라운드에서** 확인한다.
    ///
    /// `enumerate()` 는 어댑터를 열고 `resolve(Auto)` 는 장치마다 학습 경로를 한 번 태워 본다.
    /// 드라이버가 깨진 GPU 는 패닉하거나 20초 타임아웃까지 버티므로, UI 스레드에서 부르면
    /// 창이 그 시간만큼 통째로 멈춘다. 그동안 UI 는 "확인 중…" 으로 즉시 그려진다.
    fn start_device_probe(&mut self) {
        if !self.probe_enabled || self.device_asked {
            return;
        }
        self.device_asked = true;
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("nl-devices".into()).spawn(move || {
            let _ = tx.send(DeviceMsg::List(nl_engine::enumerate()));
            // 자동 선택만 한 번 돌려 둔다. 구체 장치는 목록이 채워진 순간부터 `resolve_cached` 가
            // 검사 없이 답하므로 설정을 바꿔도 다시 물을 일이 없다.
            let _ = nl_engine::resolve(DevicePref::Auto);
            let _ = tx.send(DeviceMsg::Ready { note: nl_engine::describe(DevicePref::Auto) });
        });
        match spawned {
            Ok(_) => self.device_job = Some(rx),
            // 스레드를 못 만들면 장치 이름 없이 계속 간다 — 앱을 세우는 것보다 낫다.
            Err(e) => self.log(format!("장치 확인을 시작하지 못했습니다: {e}")),
        }
    }

    /// 백그라운드 장치 확인 결과를 받아 넣는다.
    fn tick_devices(&mut self, ctx: &egui::Context) {
        let msgs: Vec<DeviceMsg> = match &self.device_job {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        let mut ready_now = false;
        for msg in msgs {
            match msg {
                DeviceMsg::List(list) => self.devices = list,
                DeviceMsg::Ready { note } => {
                    self.log(format!("장치 확인: {note}"));
                    ready_now = true;
                }
            }
        }
        if ready_now {
            self.device_job = None;
            if !self.devices_ready {
                self.devices_ready = true;
                log::info!("장치 확인 완료");
                // RUST_LOG 와 무관하게 보이는 하네스 마커.
                eprintln!("[nl-app] devices ready");
            }
            ctx.request_repaint();
        }
        if self.device_job.is_some() {
            ctx.request_repaint_after(REPOLL);
        }
    }

    /// 장치 설명 한 줄. `describe` 는 이미 아는 것만 말하고 어댑터를 새로 열지 않아 UI 스레드에서 안전하다.
    fn device_note(&self, pref: DevicePref) -> String {
        nl_engine::describe(pref)
    }

    fn refresh_monitors(&mut self) {
        match nl_io::monitors() {
            Ok(list) => {
                self.monitors = list;
                self.monitors_error = None;
            }
            Err(e) => {
                self.monitors.clear();
                self.monitors_error = Some(format!("{e:#}"));
            }
        }
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
        self.sel
            .primary
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
        let mut v = nl_core::validate(&self.doc.project);
        // 남에게 받은 프로젝트가 바깥 파일을 가리킬 수 있다. 막지는 않고 눈에 띄게만 한다.
        v.extend(crate::paths::issues(&self.doc.project));
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
        // 남은 실행기가 옛 문서의 노드로 실제 입력을 보내면 안 된다.
        if let Some(r) = self.runner.take() {
            r.stop_and_wait();
        }
        if let Some(rec) = self.recording.take() {
            rec.stop();
        }
        self.sel = SelectionState::default();
        self.canvas = CanvasState::new();
        self.pcanvas = PipelineCanvas::new();
        self.gui_state = GuiState::default();
        self.gui_preview = false;
        self.views = ViewState::default();
        self.tool_targets = None;
        self.tool_states.clear();
        self.shape_cache = None;
        self.issues_cache = None;
        self.rename = None;
        self.buf_owner = None;
        self.training = None;
        if let Some(&id) = self.doc.project.models.keys().next() {
            self.sel.set(Selection::Model(id));
        } else {
            self.sel.set(Selection::Project);
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
                self.sel.set(Selection::Node(model, id));
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
                self.sel.set(Selection::Model(model));
            }
            CanvasAction::DeleteEdges(ids) => {
                let ops: Vec<Op> = ids.iter().map(|id| Op::DeleteEdge { model, id: *id }).collect();
                self.doc.apply_local(ops);
                self.sel.set(Selection::Model(model));
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
                self.sel.select_nodes(model, new_ids, false);
            }
        }
    }

    /// 뷰가 돌려준 명령.
    fn apply_view_action(&mut self, action: ViewAction, ctx: &egui::Context, now: f64) {
        match action {
            ViewAction::Select(sel) => self.sel.set(sel),
            ViewAction::Ops(ops) => self.doc.apply_local(ops),
            ViewAction::Edit(ops) => self.doc.apply_burst(ops, now),
            ViewAction::Toast(msg) => self.toast(msg, now),
            ViewAction::SetView(v) => self.set_view(v),
            ViewAction::Focus(model, node) => {
                self.set_view(View::Model);
                self.sel.set(Selection::Node(model, node));
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
            ViewAction::StartPipeline(pid) => self.start_pipeline(pid, now),
            ViewAction::StopPipeline => self.stop_pipeline(now),
            ViewAction::SetArmInput(on) => {
                self.arm_input = on;
                // 실행 중이어도 바로 먹는다 — 무장을 끄는 것은 안전 장치라 기다리게 하면 안 된다.
                if let Some(r) = &self.runner {
                    if r.is_running() {
                        r.handle.set_armed(on);
                        self.log(if on { "입력 무장: 켬 — 실제 입력을 보냅니다" } else { "입력 무장: 끔" }.to_string());
                    }
                }
            }
            ViewAction::SendManual { node, value } => match &self.runner {
                Some(r) if r.is_running() => {
                    if !r.send(RunnerInput::Manual { node, value }) {
                        self.toast("입력 채널이 닫혔습니다", now);
                    }
                }
                _ => self.toast("파이프라인이 멈춰 있습니다", now),
            },
            ViewAction::SetGuiPreview(on) => self.set_gui_preview(on, now),
            ViewAction::BuildStart => self.start_build(now),
            ViewAction::ToolPlan(target) => self.request_tool_plan(target, now),
            ViewAction::ToolPlanInno => self.request_inno_plan(now),
            ViewAction::RecheckTools => {
                self.tool_targets = None;
                self.refresh_tools_if_needed();
            }
            ViewAction::OpenPath(p) => {
                if let Err(e) = tools::open_in_file_manager(&p) {
                    self.toast(e, now);
                }
            }
            ViewAction::RunArtifact(p) => self.run_artifact(&p, now),
            ViewAction::PickIcon => self.pick_icon(now),
            ViewAction::StartRecordForm => {
                self.views.data.record_form = Some(views::data::RecordForm::default());
                self.set_view(View::Data);
            }
            ViewAction::PickRecordDir => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    if let Some(f) = self.views.data.record_form.as_mut() {
                        f.dir = Some(dir);
                    }
                }
            }
            ViewAction::StartRecording { dir, name, region, fps, labels } => {
                self.start_recording(dir, name, region, fps, labels, now)
            }
            ViewAction::StopRecording => self.stop_recording(now),
            ViewAction::CaptureShot(region) => self.start_shot(region),
            ViewAction::ShowUpdateWindow(on) => self.update_show = on,
            ViewAction::SetUpdateCheck(on) => {
                self.update_check = on;
                if on {
                    self.start_update_check();
                } else {
                    // 이미 돌고 있는 확인은 버린다 — 꺼 달라고 했으면 결과도 보여 주지 않는다.
                    self.updater = None;
                }
            }
            ViewAction::CheckUpdateNow => match self.updater.as_mut() {
                Some(u) => u.check(),
                None => {
                    // 설정이 꺼져 있어도 눌렀으면 이번 한 번은 확인한다.
                    let was = self.update_check;
                    self.update_check = true;
                    self.start_update_check();
                    self.update_check = was;
                }
            },
        }
    }

    /// 앱 아이콘 PNG 고르기. 프로젝트 폴더 아래면 상대 경로로 저장한다.
    fn pick_icon(&mut self, now: f64) {
        let Some(path) = rfd::FileDialog::new().add_filter("PNG 이미지", &["png"]).pick_file() else { return };
        let stored = match self.doc.file_path.as_deref() {
            Some(p) => relative_to(&project::base_dir(p), &path),
            None => path.display().to_string(),
        };
        let mut settings = self.doc.project.settings.clone();
        let mut spec = settings.build.clone().unwrap_or_else(|| BuildSpec::from_project(&self.doc.project));
        spec.icon = Some(stored);
        settings.build = Some(spec);
        self.doc.apply_local(vec![Op::SetSettings { settings }]);
        self.views.build.icon_dirty = true;
        self.toast("아이콘을 골랐습니다", now);
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.canvas.cancel_interaction();
        self.pcanvas.cancel_interaction();
        log::info!("뷰 전환: {}", view.label());
        // 하네스가 `wait-log` 로 기다린다 — 고정 `sleep` 은 키가 씹혀도 지나가 버린다.
        eprintln!("[nl-app] view {}", view.label());
    }

    // ── 데이터셋 ────────────────────────────────────────────────

    fn add_synthetic(&mut self, kind: SyntheticKind) {
        let spec = views::data::synthetic_dataset(kind, 1000);
        let id = spec.id;
        self.doc.apply_local(vec![Op::UpsertDataset { dataset: spec }]);
        self.sel.set(Selection::Dataset(id));
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
        self.sel.set(Selection::Dataset(id));
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

    // ── 파이프라인 ──────────────────────────────────────────────

    /// 지금 보고 있는 파이프라인.
    pub fn active_pipeline(&self) -> Option<nl_core::PipelineId> {
        self.sel
            .primary
            .pipeline()
            .filter(|p| self.doc.project.pipelines.contains_key(p))
            .or_else(|| self.doc.project.pipelines.keys().next().copied())
    }

    /// 파이프라인 캔버스가 돌려준 편집을 op 로 적용한다.
    fn apply_pipeline_action(&mut self, pid: nl_core::PipelineId, action: PipelineAction, now: f64) {
        let Some(pl) = self.doc.project.pipelines.get(&pid).cloned() else { return };
        match action {
            PipelineAction::AddNode { kind, pos } => {
                let node = PNode::new(kind, pos);
                let id = node.id;
                self.doc.apply_local(vec![Op::UpsertPNode { pipeline: pid, node }]);
                self.sel.set(Selection::PNode(pid, id));
            }
            PipelineAction::MoveNodes(items) => {
                let ops: Vec<Op> = items
                    .iter()
                    .filter_map(|(id, pos)| {
                        let mut n = pl.nodes.get(id)?.clone();
                        n.pos = *pos;
                        Some(Op::UpsertPNode { pipeline: pid, node: n })
                    })
                    .collect();
                self.doc.apply_local(ops);
            }
            PipelineAction::Link { from, to } => {
                let link = Link { id: LinkId::new(), from, to };
                self.doc.apply_local(vec![Op::UpsertLink { pipeline: pid, link }]);
            }
            PipelineAction::DeleteNodes(ids) => {
                let ops: Vec<Op> = ids.iter().map(|id| Op::DeletePNode { pipeline: pid, id: *id }).collect();
                self.doc.apply_local(ops);
                self.sel.set(Selection::Pipeline(pid));
            }
            PipelineAction::DeleteLinks(ids) => {
                let ops: Vec<Op> = ids.iter().map(|id| Op::DeleteLink { pipeline: pid, id: *id }).collect();
                self.doc.apply_local(ops);
                self.sel.set(Selection::Pipeline(pid));
            }
            PipelineAction::DisconnectNode(id) => {
                let ops: Vec<Op> = pl
                    .links
                    .values()
                    .filter(|l| l.from == id || l.to == id)
                    .map(|l| Op::DeleteLink { pipeline: pid, id: l.id })
                    .collect();
                if ops.is_empty() {
                    self.toast("연결이 없습니다", now);
                } else {
                    self.doc.apply_local(ops);
                }
            }
            PipelineAction::DuplicateNodes(ids) => {
                let (ops, new_ids) = duplicate_pnode_ops(&pl, pid, &ids);
                if ops.is_empty() {
                    return;
                }
                self.doc.apply_local(ops);
                self.sel.select_pnodes(pid, new_ids, false);
            }
        }
    }

    /// 시험 실행. 저장 안 된 프로젝트는 임시 폴더를 기준으로 돌린다(가중치 상대 경로는 못 푼다).
    fn start_pipeline(&mut self, pid: nl_core::PipelineId, now: f64) {
        if self.runner.as_ref().map(|r| r.is_running()).unwrap_or(false) {
            self.toast("이미 실행 중입니다", now);
            return;
        }
        let Some(pl) = self.doc.project.pipelines.get(&pid).cloned() else { return };
        let (base_dir, temp) = match self.doc.file_path.as_deref() {
            Some(p) => (project::base_dir(p), None),
            None => {
                let d = session::temp_run_dir();
                let _ = std::fs::create_dir_all(&d);
                self.toast("저장되지 않은 프로젝트라 임시 폴더에서 돕니다 — 모델 가중치 경로는 풀리지 않습니다", now);
                (d.clone(), Some(d))
            }
        };
        let device = self.doc.project.settings.default_device;
        let arm = self.arm_input;
        if arm {
            self.log("마우스·키보드 싱크가 무장된 채로 시작합니다 — Esc 를 길게 누르면 즉시 멈춥니다");
        }
        match RunnerSession::start(&self.doc.project, &pl, base_dir, device, arm, temp, now) {
            Ok(s) => {
                self.runner = Some(s);
                self.show_dock = true;
                self.dock_tab = DockTab::Log;
                self.toast(format!("시험 실행: {}", pl.name), now);
            }
            Err(e) => self.toast(format!("실행할 수 없습니다: {e:#}"), now),
        }
    }

    fn stop_pipeline(&mut self, now: f64) {
        let Some(r) = &self.runner else { return };
        r.stop();
        self.toast("정지를 요청했습니다", now);
    }

    /// 매 프레임 실행기 이벤트를 소비한다.
    fn tick_runner(&mut self, ctx: &egui::Context) {
        let Some(session) = self.runner.as_mut() else { return };
        let layout = self.doc.project.gui.clone();
        let poll = session.poll(ctx, &mut self.gui_state, &layout);
        for line in poll.logs {
            self.log(line);
        }
        if poll.changed {
            ctx.request_repaint();
        }
        if poll.stopped {
            self.log("시험 실행이 끝났습니다");
        }
    }

    /// GUI 미리보기에서 위젯이 낸 이벤트를 바인딩대로 처리한다 (런타임과 같은 규칙).
    fn handle_widget_event(&mut self, ev: GuiEvent, ctx: &egui::Context, now: f64) {
        let (id, value) = match ev {
            GuiEvent::Clicked(id) => (id, nl_engine::Value::Number(1.0)),
            GuiEvent::Changed(id, v) => (id, v),
            GuiEvent::Selected(_) | GuiEvent::Moved(..) => return,
        };
        let layout = self.doc.project.gui.clone();
        let binding = match &self.runner {
            Some(r) if r.is_running() => r.route_widget_event(&layout, id, value),
            _ => layout.widgets.get(&id).and_then(|w| w.binding.clone()),
        };
        match binding {
            Some(nl_core::Binding::Action { action }) => match action {
                nl_core::gui::BuiltinAction::StartPipeline => {
                    if let Some(pid) = self.active_pipeline() {
                        self.start_pipeline(pid, now);
                    }
                }
                nl_core::gui::BuiltinAction::StopPipeline => self.stop_pipeline(now),
                nl_core::gui::BuiltinAction::Quit => {
                    self.toast("배포판에서는 앱이 종료됩니다 (미리보기에서는 무시)", now);
                }
            },
            Some(nl_core::Binding::PipelineInput { .. })
                if self.runner.as_ref().map(|r| r.is_running()) != Some(true) =>
            {
                self.toast("파이프라인이 멈춰 있어 입력을 보내지 않았습니다", now);
            }
            _ => {}
        }
        let _ = ctx;
    }

    /// GUI 미리보기 켜기/끄기. 켜면 선택한 파이프라인을 함께 돌린다.
    fn set_gui_preview(&mut self, on: bool, now: f64) {
        self.gui_preview = on;
        if on {
            match self.active_pipeline() {
                Some(pid) if !self.runner.as_ref().map(|r| r.is_running()).unwrap_or(false) => {
                    self.start_pipeline(pid, now)
                }
                Some(_) => {}
                None => self.toast("실행할 파이프라인이 없습니다 — 파이프라인 뷰에서 먼저 만드세요", now),
            }
        } else if self.runner.is_some() {
            self.stop_pipeline(now);
        }
    }

    // ── 도구 · 빌드 ─────────────────────────────────────────────

    /// 빌드 설정의 대상이 바뀌었을 때만 도구를 다시 검사한다 (파일 시스템을 매 프레임 훑지 않게).
    fn refresh_tools_if_needed(&mut self) {
        let targets = self
            .doc
            .project
            .settings
            .build
            .as_ref()
            .map(|b| b.targets.clone())
            .unwrap_or_default();
        if self.tool_targets.as_ref() == Some(&targets) {
            return;
        }
        self.tool_states = tools::check(&targets);
        self.tool_targets = Some(targets);
    }

    /// Inno Setup 설치 계획은 네트워크 없이 바로 만들 수 있다.
    fn request_inno_plan(&mut self, now: f64) {
        if self.tool_job.is_some() {
            self.toast("이미 진행 중인 도구 작업이 있습니다", now);
            return;
        }
        self.pending_plan = Some(tools::plan_inno_setup());
    }

    /// 도구 설치 계획을 백그라운드에서 만든다 (매니페스트를 받아야 해서 네트워크를 탄다).
    fn request_tool_plan(&mut self, target: BuildTarget, now: f64) {
        if self.plan_job.is_some() || self.tool_job.is_some() {
            self.toast("이미 진행 중인 도구 작업이 있습니다", now);
            return;
        }
        let url = tools::manifest_url(Some(self.views.build.manifest_url.as_str()));
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("nl-tool-plan".into()).spawn(move || {
            let _ = tx.send(tools::plan_runtime(target, &url));
        });
        match spawned {
            Ok(_) => {
                self.plan_job = Some(rx);
                self.views.build.log(format!("{} 설치 방법을 확인하는 중…", target.label()));
            }
            Err(e) => self.toast(format!("작업 스레드를 만들지 못했습니다: {e}"), now),
        }
    }

    fn tick_tools(&mut self, ctx: &egui::Context, now: f64) {
        // 계획 만들기 결과.
        if let Some(rx) = &self.plan_job {
            match rx.try_recv() {
                Ok(Ok(plan)) => {
                    self.plan_job = None;
                    self.pending_plan = Some(plan);
                    ctx.request_repaint();
                }
                Ok(Err(e)) => {
                    self.plan_job = None;
                    self.views.build.log(format!("설치 방법을 찾지 못했습니다: {e}"));
                    self.toast(format!("설치 방법을 찾지 못했습니다: {e}"), now);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(REPOLL),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.plan_job = None,
            }
        }
        // 설치 진행.
        let mut finished = false;
        if let Some(rx) = &self.tool_job {
            loop {
                match rx.try_recv() {
                    Ok(ToolEvent::Log(l)) => self.views.build.log(l),
                    Ok(ToolEvent::Progress(p)) => self.tool_progress = Some(p),
                    Ok(ToolEvent::Done(path)) => {
                        self.views.build.log(format!("준비됨: {}", path.display()));
                        finished = true;
                        break;
                    }
                    Ok(ToolEvent::Failed(e)) => {
                        self.views.build.log(format!("실패: {e}"));
                        self.views.build.error = Some(e);
                        finished = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        ctx.request_repaint_after(REPOLL);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        finished = true;
                        break;
                    }
                }
            }
        }
        if finished {
            self.tool_job = None;
            self.tool_progress = None;
            self.tool_cancel = None;
            self.tool_targets = None;
            // 진행 창은 끝나면 스스로 닫힌다.
            self.pending_plan = None;
            self.refresh_tools_if_needed();
            ctx.request_repaint();
        }
    }

    /// 산출물 폴더가 프로젝트 폴더 밖이면 물어볼 내용을 만든다.
    fn outside_build_dir(&self, project_file: &Path) -> Option<OutsideAsk> {
        let spec = self.doc.project.settings.build.clone()?;
        let raw = spec.output_dir.clone().unwrap_or_else(|| nl_core::bundle::DEFAULT_OUTPUT_DIR.to_string());
        crate::paths::outside_project(&raw)?;
        let base = project::base_dir(project_file);
        Some(OutsideAsk { what: "산출물".into(), dir: views::build::resolve_out_dir(&base, &spec) })
    }

    /// 프로젝트 폴더 밖에 쓰기 전 확인 모달.
    fn outside_modal(&mut self, ctx: &egui::Context, now: f64) {
        let Some(ask) = self.outside_ask.clone() else { return };
        let modal = egui::Modal::new(egui::Id::new("outside-dir")).show(ctx, |ui| {
            ui.set_width(MODAL_WIDTH);
            ui.heading("프로젝트 폴더 밖에 씁니다");
            ui.add_space(6.0);
            plan_row(ui, "무엇을", &format!("{} 폴더", ask.what));
            plan_row(ui, "어디에", &ask.dir.display().to_string());
            ui.add_space(6.0);
            ui.label(
                RichText::new("이 폴더를 만들고 그 안에 파일을 씁니다. 뜻한 자리가 맞는지 확인하세요.")
                    .color(views::COL_WARN)
                    .size(11.5),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("계속").clicked() {
                    self.outside_confirmed = Some(ask.clone());
                    self.outside_ask = None;
                    self.start_build(now);
                }
                if ui.button("취소").clicked() {
                    self.views.build.log("산출물 폴더가 프로젝트 밖이라 빌드를 멈췄습니다");
                    self.outside_ask = None;
                }
            });
        });
        if self.outside_ask.is_some() && modal.should_close() {
            self.outside_ask = None;
        }
    }

    /// 설치 동의 모달. 승인 전에는 아무것도 내려받지 않는다.
    fn tool_modal(&mut self, ctx: &egui::Context, now: f64) {
        let Some(plan) = self.pending_plan.clone() else { return };
        let modal = egui::Modal::new(egui::Id::new("tool-consent")).show(ctx, |ui| {
            ui.set_width(MODAL_WIDTH);
            ui.heading("도구 설치 동의");
            ui.add_space(6.0);
            ui.label(RichText::new(plan.tool.label()).strong());
            ui.add_space(6.0);
            // 주소·경로·단계는 길다. `Grid` 는 칸에 무한 폭을 줘 줄바꿈이 안 되므로 직접 폭을 나눈다.
            plan_row(ui, "무엇을", &plan.what);
            plan_row(ui, "어디서", &plan.from);
            plan_row(ui, "어디에", &plan.to.display().to_string());
            plan_row(
                ui,
                "크기",
                &if plan.size > 0 { views::fmt_bytes(plan.size) } else { "모름".into() },
            );
            // 무엇을 확인하고 무엇을 확인하지 않는지 — 빠진 항목이 보여야 승인 여부를 판단할 수 있다.
            ui.add_space(8.0);
            ui.label(RichText::new("확인하는 것").color(views::COL_WEAK));
            let note = plan.verification_note();
            for (line, ok) in plan.verification().lines_with(note.as_deref()) {
                let mark = if ok { "✔" } else { "✖" };
                let color = if ok { views::COL_OK } else { views::COL_WARN };
                ui.label(RichText::new(format!("{mark} {line}")).color(color).size(11.5));
            }
            if !plan.steps.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("이렇게 진행합니다").color(views::COL_WEAK));
                egui::ScrollArea::vertical().max_height(MODAL_STEPS_HEIGHT).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (i, step) in plan.steps.iter().enumerate() {
                        plan_row(ui, &format!("{}.", i + 1), step);
                    }
                });
            }
            ui.add_space(10.0);
            match self.tool_progress {
                // 승인한 뒤에는 같은 창이 진행 상황을 보여 준다. 끝나면 스스로 닫힌다.
                Some(p) => {
                    ui.add(egui::ProgressBar::new(p).show_percentage());
                    let cancelling = self
                        .tool_cancel
                        .as_ref()
                        .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!cancelling, egui::Button::new("취소")).clicked() {
                            if let Some(c) = &self.tool_cancel {
                                c.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                            self.views.build.log("설치를 취소하는 중…");
                        }
                        if cancelling {
                            ui.label(RichText::new("취소하는 중…").color(views::COL_WEAK).size(11.5));
                        }
                    });
                }
                None => {
                    ui.horizontal(|ui| {
                        if ui.button("승인하고 설치").clicked() {
                            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            self.tool_job = Some(tools::spawn(plan.clone(), cancel.clone()));
                            self.tool_cancel = Some(cancel);
                            self.tool_progress = Some(0.0);
                            self.views.build.error = None;
                        }
                        if ui.button("거부").clicked() {
                            self.views.build.log("설치를 거부했습니다");
                            self.pending_plan = None;
                        }
                    });
                }
            }
        });
        // 진행 중에는 바깥을 눌러도 닫지 않는다 — 창이 사라지면 취소할 방법이 없어진다.
        if self.tool_job.is_none() && self.pending_plan.is_some() && modal.should_close() {
            self.pending_plan = None;
        }
        let _ = now;
    }

    fn start_build(&mut self, now: f64) {
        if self.build_job.is_some() {
            self.toast("이미 빌드 중입니다", now);
            return;
        }
        let Some(path) = self.doc.file_path.clone() else {
            self.toast("산출물 폴더를 정하려면 프로젝트를 먼저 저장하세요", now);
            return;
        };
        // 산출물 폴더가 프로젝트 밖이면 한 번 묻는다 — 빌드는 그 폴더를 만들고 파일을 쓴다.
        if self.outside_confirmed.is_none() {
            if let Some(q) = self.outside_build_dir(&path) {
                self.outside_ask = Some(q);
                return;
            }
        }
        self.outside_confirmed = None;
        let spec = self
            .doc
            .project
            .settings
            .build
            .clone()
            .unwrap_or_else(|| BuildSpec::from_project(&self.doc.project));
        let base_dir = project::base_dir(&path);
        let out_dir = views::build::resolve_out_dir(&base_dir, &spec);
        let mut runtimes = BTreeMap::new();
        for t in &spec.targets {
            match tools::find_runtime(*t) {
                Some(p) => {
                    runtimes.insert(*t, p);
                }
                None => {
                    self.toast(format!("{} 런타임 바이너리가 없습니다", t.label()), now);
                    return;
                }
            }
        }
        let icon = spec.icon.as_deref().map(|rel| views::build::resolve_path(Some(&base_dir), rel));
        if let Some(p) = &icon {
            if !p.is_file() {
                self.toast(format!("아이콘 파일이 없습니다: {}", p.display()), now);
                return;
            }
        }
        let req = BuildRequest {
            project: self.doc.project.clone(),
            spec,
            base_dir,
            out_dir,
            runtimes,
            built_with: format!("nl-app {}", env!("CARGO_PKG_VERSION")),
            icon,
            publisher: "Neural Linker".into(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nl-build".into())
            .spawn(move || views::build::run_build(req, &tx));
        match spawned {
            Ok(_) => {
                self.build_job = Some(rx);
                self.views.build.running = true;
                self.views.build.progress = Some(0.0);
                self.views.build.error = None;
                self.views.build.artifacts.clear();
                self.views.build.log.clear();
                self.views.build.log("빌드를 시작합니다");
            }
            Err(e) => self.toast(format!("빌드 스레드를 만들지 못했습니다: {e}"), now),
        }
    }

    fn tick_build(&mut self, ctx: &egui::Context, now: f64) {
        let Some(rx) = &self.build_job else { return };
        let mut done = false;
        let mut toast: Option<String> = None;
        loop {
            match rx.try_recv() {
                Ok(BuildEvent::Log(l)) => self.views.build.log(l),
                Ok(BuildEvent::Progress(p)) => self.views.build.progress = Some(p),
                Ok(BuildEvent::Artifact(a)) => self.views.build.artifacts.push(a),
                Ok(BuildEvent::Done) => {
                    toast = Some(format!("빌드 완료 — 산출물 {}개", self.views.build.artifacts.len()));
                    done = true;
                    break;
                }
                Ok(BuildEvent::Failed(e)) => {
                    self.views.build.log(format!("실패: {e}"));
                    self.views.build.error = Some(e.clone());
                    toast = Some(format!("빌드 실패: {e}"));
                    done = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(REPOLL);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        if done {
            self.build_job = None;
            self.views.build.running = false;
            self.views.build.progress = None;
            ctx.request_repaint();
        }
        if let Some(t) = toast {
            self.toast(t, now);
        }
    }

    /// 만든 tar.gz 를 임시 폴더에 풀어 실행한다 (Linux 호스트).
    fn run_artifact(&mut self, archive: &Path, now: f64) {
        match extract_and_run(archive) {
            Ok(exe) => self.toast(format!("실행: {}", exe.display()), now),
            Err(e) => self.toast(format!("실행하지 못했습니다: {e}"), now),
        }
    }

    // ── 녹화 · 한 장 캡처 ───────────────────────────────────────

    fn start_recording(&mut self, dir: PathBuf, name: String, region: Region, fps: f32, labels: Vec<String>, now: f64) {
        if self.recording.is_some() {
            self.toast("이미 녹화 중입니다", now);
            return;
        }
        match RecordSession::start(dir.clone(), name, region, fps, labels, now) {
            Ok(s) => {
                self.recording = Some(s);
                self.set_view(View::Data);
                self.toast(format!("녹화 시작: {}", dir.display()), now);
            }
            Err(e) => self.toast(format!("녹화를 시작할 수 없습니다: {e}"), now),
        }
    }

    /// 녹화를 멈추고 그 폴더를 데이터셋으로 등록한다.
    fn stop_recording(&mut self, now: f64) {
        let Some(rec) = self.recording.take() else { return };
        rec.stop();
        rec.handle.wait_done(std::time::Duration::from_secs(3));
        let frames = rec.frames();
        let dropped = rec.dropped();
        let dir = rec.dir.clone();
        let name = rec.name.clone();
        if frames == 0 {
            let why = rec.error().unwrap_or_else(|| "프레임을 한 장도 얻지 못했습니다".into());
            self.toast(format!("녹화 실패: {why}"), now);
            self.log(format!("녹화 {}: 프레임 0 — {why}", dir.display()));
            return;
        }
        // 데이터셋 경로는 프로젝트 폴더 기준 상대 경로로 — 폴더째 옮겨도 따라간다.
        let rel = match self.doc.file_path.as_deref() {
            Some(p) => relative_to(&project::base_dir(p), &dir),
            None => dir.display().to_string(),
        };
        let spec = nl_core::DatasetSpec::new(name, nl_core::DataSource::Recorded { path: rel });
        let id = spec.id;
        self.doc.apply_local(vec![Op::UpsertDataset { dataset: spec }]);
        self.sel.set(Selection::Dataset(id));
        self.log(format!("녹화 {}: 프레임 {frames}장 (버림 {dropped}장)", dir.display()));
        self.toast(format!("데이터셋을 만들었습니다 — 프레임 {frames}장"), now);
    }

    /// 매 프레임 미리보기·오류를 살핀다.
    fn tick_recording(&mut self, ctx: &egui::Context, now: f64) {
        let Some(rec) = self.recording.as_mut() else { return };
        rec.tick(ctx, now);
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        // 캡처가 끊기면(화면 잠김·권한 회수) 스레드가 스스로 끝난다 — 그때 남은 프레임으로 마무리한다.
        if !rec.is_running() {
            self.log("녹화가 스스로 멈췄습니다 — 데이터셋으로 마무리합니다");
            self.stop_recording(now);
        }
    }

    /// 숫자키로 녹화 라벨을 바꾼다.
    fn apply_label_key(&mut self, value: i64, now: f64) {
        let Some(rec) = &self.recording else { return };
        rec.set_label(value);
        let name = rec.label_name(value);
        self.toast(format!("라벨 {value} ({name})"), now);
    }

    fn start_shot(&mut self, region: Region) {
        if self.shot_job.is_some() {
            return;
        }
        self.shot.busy = true;
        self.shot.error = None;
        self.shot_job = Some(record::spawn_shot(region));
    }

    fn tick_shot(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.shot_job else { return };
        match rx.try_recv() {
            Ok(Ok(shot)) => {
                self.shot.set(ctx, shot);
                self.shot_job = None;
                ctx.request_repaint();
            }
            Ok(Err(e)) => {
                self.log(format!("캡처 실패: {e}"));
                self.shot.fail(e);
                self.shot_job = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => ctx.request_repaint_after(REPOLL),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.shot.fail("캡처 작업이 사라졌습니다".into());
                self.shot_job = None;
            }
        }
    }

    // ── 빌더 자체 업데이트 ──────────────────────────────────────

    /// 시작할 때 한 번 확인한다. 서버가 없으면 조용히 실패하고 로그만 남는다.
    fn start_update_check(&mut self) {
        if !self.update_check {
            return;
        }
        let url = std::env::var("NL_UPDATE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| crate::update_key::UPDATE_URL.to_string());
        let mut u = nl_update::Updater::new(url, nl_update::current_version!())
            .with_public_key(crate::update_key::PUBLIC_KEY);
        u.check();
        self.updater = Some(u);
    }

    /// 백그라운드 장치 확인을 끄고 진행 중인 확인도 버린다.
    ///
    /// 확인이 도는 동안은 결과를 받으려고 계속 다시 그리기를 요청하므로, 헤드리스 하네스가
    /// "화면이 멎었다" 고 판단하지 못한다. 테스트가 이것을 먼저 부른다.
    pub fn disable_device_probe(&mut self) {
        self.probe_enabled = false;
        self.device_job = None;
        self.devices_ready = true;
    }

    /// 업데이트 자동 확인을 끄고 진행 중인 확인도 버린다.
    ///
    /// 네트워크가 없는 헤드리스 하네스에서는 확인이 타임아웃까지 다시 그리기를 계속 요청해
    /// `Harness::run` 이 "화면이 멎었다" 고 판단하지 못한다. 테스트가 이것을 먼저 부른다.
    pub fn disable_update_check(&mut self) {
        self.update_check = false;
        self.updater = None;
    }

    fn tick_updater(&mut self, ctx: &egui::Context) {
        let Some(u) = self.updater.as_mut() else { return };
        let events = u.poll();
        let busy = u.is_busy();
        let mut lines = Vec::new();
        for ev in events {
            match ev {
                nl_update::Event::UpToDate => lines.push("업데이트: 최신입니다".to_string()),
                nl_update::Event::Available(a) => lines.push(format!("업데이트: 새 버전 v{}", a.version)),
                nl_update::Event::Downloaded(p) => lines.push(format!("업데이트: 내려받음 {}", p.display())),
                nl_update::Event::Applied(a) => lines.push(format!("업데이트: {}", a.message())),
                nl_update::Event::Failed(e) => lines.push(format!("업데이트 실패: {e}")),
                // 공개키가 없거나 주소가 https 가 아니면 확인 자체를 하지 않는다.
                nl_update::Event::Disabled(why) => lines.push(format!("업데이트 사용 불가: {why}")),
                nl_update::Event::Checking | nl_update::Event::Applying | nl_update::Event::Progress(_) => {}
            }
        }
        for l in lines {
            self.log(l);
        }
        if busy {
            ctx.request_repaint_after(REPOLL);
        }
    }

    /// 업데이트 창 (trust-pms 의 업데이트 구역과 같은 흐름).
    fn update_window(&mut self, ctx: &egui::Context, now: f64) {
        if !self.update_show {
            return;
        }
        let mut open = true;
        let state = self.updater.as_ref().map(|u| u.state().clone());
        let mut action: Option<UpdateAction> = None;
        egui::Window::new("빌더 업데이트").open(&mut open).resizable(false).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(format!("현재 v{}", env!("CARGO_PKG_VERSION")));
            match &state {
                None => {
                    ui.label(RichText::new("업데이트 확인이 꺼져 있습니다").color(views::COL_WEAK));
                }
                Some(s) => {
                    ui.label(s.message());
                    if let nl_update::State::Available(a) = s {
                        if !a.notes.trim().is_empty() {
                            ui.add_space(4.0);
                            ui.label(RichText::new(a.notes.trim()).size(11.5));
                        }
                    }
                    if let nl_update::State::Downloading { received, total } = s {
                        let p = nl_update::Progress { received: *received, total: *total };
                        match p.fraction() {
                            Some(f) => {
                                ui.add(egui::ProgressBar::new(f).desired_width(260.0).show_percentage());
                            }
                            None => {
                                ui.add(egui::ProgressBar::new(0.0).desired_width(260.0).text("내려받는 중"));
                            }
                        }
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let busy = self.updater.as_ref().map(|u| u.is_busy()).unwrap_or(false);
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button("지금 확인").clicked() {
                        action = Some(UpdateAction::Check);
                    }
                    if matches!(state, Some(nl_update::State::Available(_))) && ui.button("⬇ 내려받기").clicked() {
                        action = Some(UpdateAction::Download);
                    }
                    if matches!(state, Some(nl_update::State::Downloaded { .. }))
                        && ui.button(RichText::new("⬆ 적용하고 다시 시작").color(views::COL_OK)).clicked()
                    {
                        action = Some(UpdateAction::Apply);
                    }
                });
            });
            ui.add_space(6.0);
            let mut check = self.update_check;
            if ui.checkbox(&mut check, "시작할 때 확인").changed() {
                action = Some(UpdateAction::SetCheck(check));
            }
        });
        if !open {
            self.update_show = false;
        }
        match action {
            Some(UpdateAction::Check) => {
                if self.updater.is_none() {
                    self.update_check = true;
                    self.start_update_check();
                } else if let Some(u) = self.updater.as_mut() {
                    u.check();
                }
            }
            Some(UpdateAction::Download) => {
                let dir = update_download_dir();
                if let Some(u) = self.updater.as_mut() {
                    u.download(dir);
                }
            }
            Some(UpdateAction::Apply) => {
                if self.doc.modified {
                    self.toast("저장하지 않은 변경이 있습니다 — 먼저 저장하세요", now);
                } else if let Some(u) = self.updater.as_mut() {
                    u.apply();
                }
            }
            Some(UpdateAction::SetCheck(v)) => self.update_check = v,
            None => {}
        }
    }

    /// 툴바 배지 문구. 알릴 것이 없으면 `None`.
    fn update_badge(&self) -> Option<String> {
        match self.updater.as_ref()?.state() {
            nl_update::State::Available(a) => Some(format!("⬆ v{}", a.version)),
            nl_update::State::Downloading { .. } => Some("⬆ 내려받는 중".into()),
            nl_update::State::Downloaded { .. } => Some("⬆ 적용 준비됨".into()),
            nl_update::State::Applying => Some("⬆ 적용 중".into()),
            nl_update::State::Applied(_) => Some("⬆ 적용됨".into()),
            _ => None,
        }
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
                .unwrap_or_else(|| {
                    if self.devices.is_empty() {
                        "확인 중…".to_string()
                    } else {
                        current.label()
                    }
                });
            let devices = self.devices.clone();
            let mut chosen: Option<nl_core::DevicePref> = None;
            let mut refresh = false;
            egui::ComboBox::from_id_salt("device-picker").selected_text(format!("🖳 {label}")).show_ui(ui, |ui| {
                if ui.selectable_label(current == nl_core::DevicePref::Auto, "자동 (첫 GPU → CPU)").clicked() {
                    chosen = Some(nl_core::DevicePref::Auto);
                }
                if devices.is_empty() {
                    ui.label(RichText::new("장치를 찾는 중…").color(views::COL_WEAK));
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
                // 열거도 검사도 백그라운드로 — 목록이 길면 UI 스레드가 그만큼 멈춘다.
                self.device_asked = false;
                self.start_device_probe();
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
                if let Some(badge) = self.update_badge() {
                    if ui
                        .button(RichText::new(badge).color(views::COL_OK))
                        .on_hover_text("빌더 업데이트")
                        .clicked()
                    {
                        self.update_show = true;
                    }
                    ui.separator();
                }
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
                let resp = ui
                    .selectable_label(self.view == *view, view.label())
                    .on_hover_text(format!("Ctrl+{}  ·  {}", i + 1, view.hint()));
                if resp.clicked() {
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
                    self.sel.set(Selection::Node(model, n));
                    self.canvas.pending_focus = Some(n);
                }
                None => {
                    self.sel.set(Selection::Model(model));
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
            // `resolve_cached` 는 이미 정해진 결과만 본다 — 열거도 검사도 스레드 생성도 하지 않는다.
            // 아직 아무것도 정해지지 않았으면 `None` 이고, 그 값은 시작할 때 띄운 스레드가 채운다.
            let dev = match nl_engine::resolve_cached(pref) {
                Some(r) if pref == DevicePref::Auto => format!("자동 → {}", r.info.name),
                Some(r) => r.info.name,
                None => "확인 중…".to_string(),
            };
            ui.label(format!("장치 {dev}")).on_hover_text(self.device_note(pref));
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
            let n = self.sel.count();
            if n > 1 {
                ui.separator();
                ui.label(format!("{n}개 선택"));
            }
            if let Some(r) = &self.runner {
                ui.separator();
                let (label, color) = if r.is_running() {
                    ("파이프라인 실행 중", views::COL_SELECT)
                } else {
                    ("파이프라인 정지", views::COL_WEAK)
                };
                ui.label(RichText::new(label).color(color));
                // 실행기가 초당 한 번 보고하는 실제 틱 속도 — 목표 Hz 와 얼마나 벌어지는지 보인다.
                if let Some(st) = r.stats.filter(|_| r.is_running()) {
                    ui.label(RichText::new(st.label()).color(views::COL_WEAK));
                }
                if r.errors > 0 {
                    ui.label(RichText::new(format!("오류 {}", r.errors)).color(views::COL_ERROR));
                }
                if self.arm_input {
                    ui.label(RichText::new("⚠ 입력 무장").color(views::COL_WARN));
                }
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
        // 킬 스위치는 무엇보다 먼저 본다 — 텍스트 칸에 포커스가 있어도, 모달이 떠 있어도 멈춰야 한다.
        self.tick_kill_switch(ctx, now);
        // 모달이 떠 있으면 아래 단축키는 전부 막는다 — 모달이 먼저 답을 받아야 한다.
        if self.pending_action.is_some() || self.pending_plan.is_some() || self.outside_ask.is_some() {
            return;
        }
        let typing = ctx.egui_wants_keyboard_input();
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
            digit: (!i.modifiers.command && !i.modifiers.alt)
                .then(|| DIGIT_KEYS.iter().position(|k| i.key_pressed(*k)))
                .flatten(),
            view: i.modifiers.command.then(|| VIEW_KEYS.iter().position(|key| i.key_pressed(*key))).flatten(),
        });
        let k = if typing { k.while_typing() } else { k };

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
        // 녹화 중에는 숫자키가 라벨 스위치다 (텍스트 칸에 커서가 있으면 위에서 이미 돌아갔다).
        if self.recording.is_some() {
            if let Some(d) = k.digit {
                self.apply_label_key(d as i64, now);
            }
        }
        if k.escape && !ctx.any_popup_open() {
            if self.canvas.interaction_active() {
                self.canvas.cancel_interaction();
            } else if self.pcanvas.interaction_active() {
                self.pcanvas.cancel_interaction();
            } else if self.rename.is_some() {
                self.rename = None;
            }
        }
        // 뷰별 편집 단축키.
        match self.view {
            View::Model => {
                if k.fit {
                    self.canvas.request_fit();
                }
                let Some(model) = self.active_model() else { return };
                if k.select_all {
                    let ids: Vec<NodeId> = self
                        .doc
                        .project
                        .models
                        .get(&model)
                        .map(|m| m.graph.nodes.keys().copied().collect())
                        .unwrap_or_default();
                    self.sel.select_nodes(model, ids, false);
                }
                if k.duplicate {
                    let ids = self.sel.node_list();
                    if !ids.is_empty() {
                        self.apply_canvas_action(model, CanvasAction::DuplicateNodes(ids), now);
                    }
                }
                if k.del {
                    self.delete_selection(model, now);
                }
            }
            View::Pipeline => {
                if k.fit {
                    self.pcanvas.request_fit();
                }
                let Some(pid) = self.active_pipeline() else { return };
                if k.select_all {
                    let ids: Vec<PNodeId> = self
                        .doc
                        .project
                        .pipelines
                        .get(&pid)
                        .map(|p| p.nodes.keys().copied().collect())
                        .unwrap_or_default();
                    self.sel.select_pnodes(pid, ids, false);
                }
                if k.duplicate {
                    let ids = self.sel.pnode_list();
                    if !ids.is_empty() {
                        self.apply_pipeline_action(pid, PipelineAction::DuplicateNodes(ids), now);
                    }
                }
                if k.del {
                    self.delete_pipeline_selection(pid, now);
                }
            }
            View::Gui => {
                if self.gui_preview {
                    return;
                }
                if let Selection::Widget(id) = self.sel.primary {
                    if k.del {
                        self.doc.apply_local(vec![Op::DeleteWidget { id }]);
                        self.sel.set(Selection::Project);
                    }
                    if k.duplicate {
                        if let Some(w) = self.doc.project.gui.widgets.get(&id) {
                            let mut copy = w.clone();
                            copy.id = nl_core::WidgetId::new();
                            copy.rect[0] += views::gui::SNAP * 2.0;
                            copy.rect[1] += views::gui::SNAP * 2.0;
                            let new_id = copy.id;
                            self.doc.apply_local(vec![Op::UpsertWidget { widget: copy }]);
                            self.sel.set(Selection::Widget(new_id));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// 실행 중 Esc 를 길게 누르면 즉시 멈춘다. 실제 마우스·키보드를 움직이는 동안의 마지막 안전장치다.
    fn tick_kill_switch(&mut self, ctx: &egui::Context, now: f64) {
        let running = self.runner.as_ref().map(|r| r.is_running()).unwrap_or(false);
        let down = ctx.input(|i| i.key_down(egui::Key::Escape));
        if !running || !down {
            self.esc_since = None;
            return;
        }
        let since = *self.esc_since.get_or_insert(now);
        // 누르고 있는 동안 계속 깨어 있어야 시간을 잴 수 있다.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        if now - since >= KILL_HOLD {
            self.esc_since = None;
            if let Some(r) = &self.runner {
                r.stop();
            }
            self.toast("킬 스위치 — 파이프라인을 멈췄습니다", now);
        }
    }

    fn delete_pipeline_selection(&mut self, pid: nl_core::PipelineId, now: f64) {
        match self.sel.primary {
            Selection::Link(p, id) => {
                self.doc.apply_local(vec![Op::DeleteLink { pipeline: p, id }]);
                self.sel.set(Selection::Pipeline(p));
            }
            _ => {
                let ids = self.sel.pnode_list();
                if !ids.is_empty() {
                    self.apply_pipeline_action(pid, PipelineAction::DeleteNodes(ids), now);
                }
            }
        }
    }

    fn delete_selection(&mut self, model: ModelId, now: f64) {
        match self.sel.primary {
            Selection::Edge(m, id) => {
                self.doc.apply_local(vec![Op::DeleteEdge { model: m, id }]);
                self.sel.set(Selection::Model(m));
            }
            _ => {
                let ids = self.sel.node_list();
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
        let key = (self.sel.primary, self.doc.external_edits);
        if self.buf_owner == Some(key) {
            false
        } else {
            self.buf_owner = Some(key);
            true
        }
    }
}

#[derive(Default)]
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
    /// 숫자키 0~9 (녹화 라벨).
    digit: Option<usize>,
}

impl Keys {
    /// 텍스트 칸을 편집하는 중에도 살아 있어야 하는 것만 남긴다.
    ///
    /// 이름을 고치다가 Ctrl+S 를 눌렀는데 저장이 안 되면 글을 잃는다. 뷰 전환·패널 토글도 마찬가지로
    /// 글자를 넣는 동작이 아니라 막을 이유가 없다. 반대로 Ctrl+A·Ctrl+Z·Delete·Esc 는 `TextEdit` 이
    /// 스스로 쓰는 것이라 가로채면 편집이 망가진다 — 그것들은 egui 에 넘긴다.
    fn while_typing(self) -> Self {
        Keys {
            save: self.save,
            save_as: self.save_as,
            open: self.open,
            new: self.new,
            outline: self.outline,
            inspector: self.inspector,
            dock: self.dock,
            view: self.view,
            ..Keys::default()
        }
    }
}

// ── eframe ──────────────────────────────────────────────────────────

impl eframe::App for NlApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        let now = ctx.input(|i| i.time);

        self.tick_training(ctx, now);
        self.tick_runner(ctx);
        self.tick_recording(ctx, now);
        self.tick_shot(ctx);
        self.tick_tools(ctx, now);
        self.tick_build(ctx, now);
        self.tick_updater(ctx);
        self.tick_devices(ctx);
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
        let mut inspector_actions: Vec<ViewAction> = Vec::new();
        if self.show_inspector {
            inspector_actions = egui::Panel::right("inspector")
                .resizable(true)
                .default_size(320.0)
                .size_range(240.0..=520.0)
                .show(root, |ui| self.inspector(ui, now))
                .inner;
        }

        let report = self.shapes();
        let actions = egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(root.visuals().panel_fill))
            .show(root, |ui| self.central(ui, &report, now))
            .inner;
        for action in inspector_actions.into_iter().chain(actions) {
            self.apply_view_action(action, ctx, now);
        }

        self.unsaved_modal(ctx, now);
        self.tool_modal(ctx, now);
        self.outside_modal(ctx, now);
        self.update_window(ctx, now);
        self.draw_toasts(ctx, now);

        if self.warmup_frames > 0 {
            self.warmup_frames -= 1;
            ctx.request_repaint();
        } else if !self.ready_logged {
            // 패널 크기가 자리를 잡은 첫 프레임. 하네스가 `wait-log` 로 이 줄을 기다린다.
            self.ready_logged = true;
            log::info!("UI 준비 완료");
            eprintln!("[nl-app] ready");
        }
        // 화면이 그려진 것과 키를 받을 수 있는 것은 다르다. 컴포지터가 포커스를 주기 전에
        // 보낸 단축키는 그냥 버려지므로, 하네스는 키를 넣기 전에 이 줄을 기다려야 한다.
        if !self.focus_logged && ctx.input(|i| i.focused) {
            self.focus_logged = true;
            log::info!("창 포커스 받음");
            eprintln!("[nl-app] focused");
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
        eframe::set_value(storage, "runtime_manifest", &self.views.build.manifest_url);
        eframe::set_value(storage, "update_check", &self.update_check);
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
                    let ctx = view_ctx!(self, now);
                    views::model::show(ui, &ctx, &mut self.canvas, report, &mut self.sel)
                };
                out.extend(model_out.actions);
                if let Some(model) = self.active_model() {
                    for a in model_out.canvas {
                        self.apply_canvas_action(model, a, now);
                    }
                }
            }
            View::Data => {
                let ctx = view_ctx!(self, now);
                out.extend(views::data::show(ui, &ctx, &mut self.views.data));
            }
            View::Train => {
                let ctx = view_ctx!(self, now);
                out.extend(views::train::show(ui, &ctx, &mut self.views.train));
            }
            View::Resources => {
                let ctx = view_ctx!(self, now);
                out.extend(views::resources::show(ui, &ctx, &mut self.views.resources));
            }
            View::Pipeline => {
                let live_running = self.runner.as_ref().map(|r| r.is_running()).unwrap_or(false);
                let run_state = if live_running {
                    views::pipeline::RunState::Running
                } else {
                    views::pipeline::RunState::Idle
                };
                let empty = crate::pcanvas::LiveView::default();
                let pipe_out = {
                    let live = self.runner.as_ref().map(|r| &r.live).unwrap_or(&empty);
                    let ctx = view_ctx!(self, now);
                    views::pipeline::show(ui, &ctx, &mut self.pcanvas, live, run_state, self.arm_input, &mut self.sel)
                };
                out.extend(pipe_out.actions);
                if let Some(pid) = self.active_pipeline() {
                    for a in pipe_out.canvas {
                        self.apply_pipeline_action(pid, a, now);
                    }
                }
            }
            View::Gui => {
                let gui_out = {
                    let ctx = view_ctx!(self, now);
                    views::gui::show(ui, &ctx, &mut self.views.gui, &mut self.gui_state, self.gui_preview)
                };
                out.extend(gui_out.actions);
                for ev in gui_out.events {
                    self.handle_widget_event(ev, ui.ctx(), now);
                }
            }
            View::Build => {
                self.refresh_tools_if_needed();
                let ctx = view_ctx!(self, now);
                out.extend(views::build::show(ui, &ctx, &mut self.views.build, &self.tool_states));
            }
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

/// 업데이트 창에서 누른 것.
#[derive(Clone, Copy, PartialEq)]
enum UpdateAction {
    Check,
    Download,
    Apply,
    SetCheck(bool),
}

/// 내려받은 업데이트 자산을 두는 폴더.
fn update_download_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "trustanc", "neural-linker")
        .map(|d| d.cache_dir().join("updates"))
        .unwrap_or_else(|| std::env::temp_dir().join("neural-linker-updates"))
}

/// 파이프라인 노드를 복제하는 op 묶음과 새 id. 복제 집합 안쪽 링크도 함께 복사한다.
pub fn duplicate_pnode_ops(
    pl: &nl_core::Pipeline,
    pid: nl_core::PipelineId,
    ids: &[PNodeId],
) -> (Vec<Op>, Vec<PNodeId>) {
    let offset = crate::pcanvas::duplicate_offset();
    let mut map: BTreeMap<PNodeId, PNodeId> = BTreeMap::new();
    let mut ops = Vec::new();
    let mut new_ids = Vec::new();
    for id in ids {
        let Some(src) = pl.nodes.get(id) else { continue };
        let mut node = src.clone();
        node.id = PNodeId::new();
        node.pos = [src.pos[0] + offset.x, src.pos[1] + offset.y];
        map.insert(*id, node.id);
        new_ids.push(node.id);
        ops.push(Op::UpsertPNode { pipeline: pid, node });
    }
    for l in pl.links.values() {
        if let (Some(&from), Some(&to)) = (map.get(&l.from), map.get(&l.to)) {
            ops.push(Op::UpsertLink { pipeline: pid, link: Link { id: LinkId::new(), from, to } });
        }
    }
    (ops, new_ids)
}

/// 배포 아카이브(tar.gz)를 임시 폴더에 풀어 실행 파일을 띄운다. 돌려주는 값은 실행한 경로.
fn extract_and_run(archive: &Path) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("nl-app-try-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = std::fs::File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let gz = flate2::read::GzDecoder::new(file);
    tar::Archive::new(gz).unpack(&dir).map_err(|e| format!("압축을 풀지 못했습니다: {e}"))?;
    // `<slug>/<slug>` 규칙 (nl-bundle templates).
    let exe = find_executable(&dir).ok_or_else(|| "아카이브에서 실행 파일을 찾지 못했습니다".to_string())?;
    std::process::Command::new(&exe)
        .current_dir(exe.parent().unwrap_or(&dir))
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    Ok(exe)
}

/// 풀린 폴더에서 확장자 없는 실행 파일 하나를 찾는다.
fn find_executable(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_executable(&p) {
                return Some(f);
            }
        } else if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.extension().is_none()
        && std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.extension().map(|e| e.eq_ignore_ascii_case("exe")).unwrap_or(false)
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

    /// 텍스트 칸을 편집하는 중에도 저장·뷰 전환은 살아 있고, egui 가 쓰는 조합은 넘긴다.
    #[test]
    fn typing_keeps_save_and_view_but_yields_editing_keys() {
        let all = Keys {
            del: true,
            undo: true,
            redo: true,
            save: true,
            save_as: true,
            open: true,
            new: true,
            fit: true,
            select_all: true,
            duplicate: true,
            escape: true,
            outline: true,
            inspector: true,
            dock: true,
            view: Some(1),
            digit: Some(3),
        };
        let k = all.while_typing();
        // 글자를 넣는 동작이 아닌 것은 그대로 산다.
        assert!(k.save && k.save_as && k.open && k.new);
        assert!(k.outline && k.inspector && k.dock);
        assert_eq!(k.view, Some(1));
        // `TextEdit` 이 스스로 쓰는 것은 넘긴다.
        assert!(!k.del && !k.undo && !k.redo && !k.select_all && !k.escape);
        // 캔버스 전용 동작도 편집 중에는 뜻이 없다.
        assert!(!k.fit && !k.duplicate);
        assert_eq!(k.digit, None, "숫자키는 글자로 들어가야 한다");
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
    fn canvas_views_are_the_two_node_editors() {
        let c: Vec<View> = View::ALL.iter().copied().filter(|v| v.uses_canvas()).collect();
        assert_eq!(c, vec![View::Model, View::Pipeline]);
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
    fn pipeline_node_edits_undo_in_one_step() {
        let mut doc = doc_with_sample();
        let pid = *doc.project.pipelines.keys().next().unwrap();
        let before = doc.project.clone();
        let node = PNode::new(nl_core::PNodeKind::Sink { sink: nl_core::Sink::Log }, [10.0, 20.0]);
        let id = node.id;
        doc.apply_local(vec![Op::UpsertPNode { pipeline: pid, node }]);
        assert!(doc.project.pipelines[&pid].nodes.contains_key(&id));
        doc.undo();
        assert_eq!(doc.project, before);
        doc.redo();
        assert!(doc.project.pipelines[&pid].nodes.contains_key(&id));
    }

    #[test]
    fn deleting_a_pipeline_node_restores_its_links_on_undo() {
        let mut doc = doc_with_sample();
        let pid = *doc.project.pipelines.keys().next().unwrap();
        let pl = doc.project.pipelines[&pid].clone();
        // 링크가 둘 붙어 있는 가운데 노드를 고른다.
        let victim = pl
            .nodes
            .keys()
            .copied()
            .find(|id| pl.links.values().filter(|l| l.from == *id || l.to == *id).count() == 2)
            .expect("가운데 노드");
        let before = doc.project.clone();
        doc.apply_local(vec![Op::DeletePNode { pipeline: pid, id: victim }]);
        assert!(doc.project.pipelines[&pid].links.len() < pl.links.len());
        doc.undo();
        assert_eq!(doc.project, before);
    }

    #[test]
    fn duplicating_pipeline_nodes_copies_inner_links_only() {
        let doc = doc_with_sample();
        let pid = *doc.project.pipelines.keys().next().unwrap();
        let pl = doc.project.pipelines[&pid].clone();
        // Manual → 모델 → 로그 사슬에서 앞의 둘만 복제한다.
        let chain: Vec<PNodeId> = {
            let first = pl.nodes.values().find(|n| n.kind.is_source() && n.name == "입력").unwrap().id;
            let second = pl.links.values().find(|l| l.from == first).unwrap().to;
            vec![first, second]
        };
        let (ops, ids) = duplicate_pnode_ops(&pl, pid, &chain);
        assert_eq!(ids.len(), 2);
        assert_eq!(ops.iter().filter(|o| matches!(o, Op::UpsertPNode { .. })).count(), 2);
        assert_eq!(
            ops.iter().filter(|o| matches!(o, Op::UpsertLink { .. })).count(),
            1,
            "복제 집합 밖으로 나가는 링크는 복사하지 않는다"
        );
        // 새 노드는 원본과 다른 id 이고 위치가 어긋나 있다.
        let mut p = doc.project.clone();
        apply_ops(&mut p, &ops);
        for (old, new) in chain.iter().zip(&ids) {
            assert_ne!(old, new);
            assert_ne!(p.pipelines[&pid].nodes[new].pos, p.pipelines[&pid].nodes[old].pos);
        }
    }

    #[test]
    fn widget_edits_go_through_ops_and_undo() {
        let mut doc = doc_with_sample();
        let id = *doc.project.gui.widgets.keys().next().unwrap();
        let before = doc.project.clone();
        let mut w = doc.project.gui.widgets[&id].clone();
        w.rect = [1.0, 2.0, 30.0, 40.0];
        doc.apply_local(vec![Op::UpsertWidget { widget: w }]);
        assert_eq!(doc.project.gui.widgets[&id].rect, [1.0, 2.0, 30.0, 40.0]);
        doc.undo();
        assert_eq!(doc.project, before);
        // 삭제도 되돌아온다.
        doc.apply_local(vec![Op::DeleteWidget { id }]);
        assert!(!doc.project.gui.widgets.contains_key(&id));
        doc.undo();
        assert_eq!(doc.project, before);
    }

    #[test]
    fn build_spec_lives_in_settings_and_round_trips() {
        let mut doc = doc_with_sample();
        let before = doc.project.clone();
        let mut settings = doc.project.settings.clone();
        let spec = settings.build.clone().expect("샘플에 빌드 설정이 있다");
        settings.build = Some(BuildSpec { app_version: "9.9.9".into(), ..spec });
        doc.apply_local(vec![Op::SetSettings { settings }]);
        assert_eq!(doc.project.settings.build.as_ref().unwrap().app_version, "9.9.9");

        // 파일로 나갔다 들어와도 그대로.
        let json = nl_core::ProjectFile::new(doc.project.clone()).to_json();
        let back = nl_core::ProjectFile::from_json(&json).unwrap();
        assert_eq!(back.project.settings.build, doc.project.settings.build);

        doc.undo();
        assert_eq!(doc.project, before);
    }

    /// 옛 문서(빌드 설정이 없던 시절)도 그대로 열려야 한다.
    #[test]
    fn documents_without_a_build_spec_still_load() {
        let json = r#"{"format_version":1,"project":{"id":"00000000-0000-0000-0000-000000000001",
            "name":"x","created":"2026-01-01T00:00:00Z","settings":{"default_device":{"type":"Cpu"}}}}"#;
        let f = nl_core::ProjectFile::from_json(json).unwrap();
        assert_eq!(f.project.settings.build, None);
        assert_eq!(f.project.settings.default_device, DevicePref::Cpu);
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
