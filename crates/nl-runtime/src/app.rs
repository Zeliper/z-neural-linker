//! 배포 런타임의 eframe 앱: 상단 제어 바 + 번들 GUI 레이아웃 + 파이프라인 실행기 연결.

use crate::update::UpdateUi;
use nl_bundle::Bundle;
use nl_core::gui::{Binding, BuiltinAction};
use nl_core::{BundleManifest, DevicePref, GuiLayout, ModelId, PNodeId, Pipeline, Project, WidgetId, WidgetKind};
use nl_engine::{DeviceInfo, Value};
use nl_gui::{GuiEvent, GuiState, RenderMode};
use nl_io::runner::RunnerInput;
use nl_io::{Runner, RunnerEvent, RunnerHandle};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 로그 창에 남기는 줄 수.
const LOG_LINES: usize = 200;
/// 플롯 히스토리 기본 길이 (`WidgetKind::Plot` 이 아닌 위젯에서 온 값).
const DEFAULT_POINTS: usize = 300;
/// 실행 중일 때 최소 다시 그리기 간격.
const REPAINT: Duration = Duration::from_millis(33);

// ───────────────────────────── 작업 폴더 ─────────────────────────────

/// 번들 해시를 적어 두는 파일. 다음 실행이 "같은 번들인가" 를 이것으로 판단한다.
const BUNDLE_MARK: &str = ".bundle-sha256";
/// 사용자가 직접 놓는 파일을 두는 곳. 번들을 갱신해도 **여기는 건드리지 않는다.**
pub const LOCAL_DIR: &str = "local";

/// 번들 가중치·에셋을 푸는 작업 폴더.
///
/// 두 모드가 있다.
/// - [`WorkDir::create`] — 무작위 이름 임시 폴더. 살아 있는 동안만 있고 `Drop` 에서 지워진다. **기본값.**
/// - [`WorkDir::fixed`] — 사용자가 정한 경로. 서비스로 상시 운영할 때 쓴다. 지워지지 않는다.
///
/// 임시 폴더의 이름이 무작위인 이유는 예전에 `temp_dir()/nl-runtime-<pid>` 를 쓰다 겪은 문제 때문이다.
/// 경로가 완전히 예측 가능해서 sticky 비트가 걸린 `/tmp` 에서 다른 로컬 사용자가 미리 그 폴더를
/// 만들어 둘 수 있었고, 그러면 가중치가 **공격자 소유 폴더**에 풀렸다.
///
/// 고정 폴더는 그 위험을 사용자가 경로 선택으로 진다 — 그래서 홈 아래를 쓰라고 안내한다.
pub enum WorkDir {
    /// 끝나면 지워지는 임시 폴더.
    Temp(tempfile::TempDir),
    /// 사용자가 정한 폴더. 그대로 남는다.
    Fixed(PathBuf),
}

impl WorkDir {
    pub fn create() -> std::io::Result<Self> {
        // tempdir 은 O_EXCL 로 만든다 — 선점된 폴더를 물려받는 일이 없다.
        let dir = tempfile::Builder::new().prefix("nl-runtime-").tempdir()?;
        set_owner_only(dir.path())?;
        Ok(Self::Temp(dir))
    }

    /// 정해진 경로를 작업 폴더로 쓴다. 없으면 0700 으로 만든다.
    ///
    /// 이미 있으면 **소유권을 확인하지 않는다** — 사용자가 고른 경로라 그 판단을 존중한다.
    /// 대신 권한을 0700 으로 다시 조여 다른 사용자가 읽지 못하게 한다.
    pub fn fixed(path: &Path) -> std::io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
        set_owner_only(path)?;
        std::fs::create_dir_all(path.join(LOCAL_DIR))?;
        Ok(Self::Fixed(path.to_path_buf()))
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Temp(d) => d.path(),
            Self::Fixed(p) => p,
        }
    }

    /// 다음 실행에서 다시 쓸 수 있는 폴더인가. 로그 문구를 가르는 데 쓴다.
    pub fn is_persistent(&self) -> bool {
        matches!(self, Self::Fixed(_))
    }
}

/// 번들 내용의 지문. 같은 내용이면 같은 값이다.
///
/// zip 바이트를 그대로 해싱하지 않는 이유는 압축 시각·순서 같은 것이 섞여 들어가 **같은 번들인데
/// 값이 달라질 수 있어서**다. 매니페스트·프로젝트·가중치·에셋의 내용만 정해진 순서로 넣는다.
pub fn bundle_fingerprint(bundle: &Bundle) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(nl_bundle::sha256_hex(
        serde_json::to_string(&bundle.manifest).unwrap_or_default().as_bytes(),
    ));
    parts.push(nl_bundle::sha256_hex(
        serde_json::to_string(&bundle.project).unwrap_or_default().as_bytes(),
    ));
    // BTreeMap 이라 순회 순서가 이름 순으로 정해져 있다 — 같은 번들이면 같은 순서다.
    for (name, bytes) in bundle.weights.iter().chain(bundle.assets.iter()) {
        parts.push(format!("{name}:{}", nl_bundle::sha256_hex(bytes)));
    }
    nl_bundle::sha256_hex(parts.join("\n").as_bytes())
}

/// 번들을 작업 폴더에 준비한다. 이미 같은 번들이 풀려 있으면 다시 풀지 않는다.
///
/// 돌려주는 값은 "실제로 풀었는가" 다. 임시 폴더는 언제나 `true`(비어 있으니까).
///
/// 판단 기준은 번들 zip 의 sha256 이다. 파일 목록을 비교하지 않는 이유는 사용자가
/// `local/` 에 무엇을 두든 그것이 판단을 흔들면 안 되기 때문이다.
pub fn sync_workspace(bundle: &Bundle, zip_sha256: &str, dir: &Path) -> anyhow::Result<bool> {
    let mark = dir.join(BUNDLE_MARK);
    let same = std::fs::read_to_string(&mark).is_ok_and(|s| s.trim() == zip_sha256);
    if same && dir.join("weights").is_dir() {
        return Ok(false);
    }

    // 갱신할 때는 번들이 소유한 폴더만 비운다. `local/` 은 사용자 것이라 그대로 둔다.
    for owned in ["weights", "assets"] {
        let p = dir.join(owned);
        if p.exists() {
            std::fs::remove_dir_all(&p).map_err(|e| anyhow::anyhow!("{} 를 비우지 못했습니다: {e}", p.display()))?;
        }
    }
    let _ = std::fs::remove_file(&mark);

    prepare_workspace(bundle, dir)?;
    std::fs::create_dir_all(dir.join(LOCAL_DIR))?;
    std::fs::write(&mark, format!("{zip_sha256}\n"))
        .map_err(|e| anyhow::anyhow!("{} 를 쓰지 못했습니다: {e}", mark.display()))?;
    Ok(true)
}

/// 소유자만 드나들 수 있게 한다. 번들 가중치가 같은 호스트의 다른 사용자에게 읽히지 않도록.
#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// 번들의 가중치·에셋을 작업 폴더에 푼다. 모델이 프로젝트 상대 경로(`ModelDef::weights`)를 가리키면
/// 그 경로에도 같은 파일을 놓아 `Runner` 가 어느 규칙으로 찾아도 맞도록 한다.
pub fn prepare_workspace(bundle: &Bundle, dir: &Path) -> anyhow::Result<()> {
    bundle.materialize_weights(&dir.join("weights"))?;
    bundle.materialize_assets(&dir.join("assets"))?;
    for bm in &bundle.manifest.models {
        let Some(bytes) = bundle.weights.get(&bm.weights_file) else {
            continue;
        };
        let Some(model) = bundle.project.models.get(&bm.model) else {
            continue;
        };
        let Some(rel) = model.weights.as_deref() else { continue };
        if rel == format!("weights/{}", bm.weights_file) {
            continue;
        }
        let Some(target) = join_inside(dir, rel) else {
            log::warn!("모델 가중치 경로가 작업 폴더 밖을 가리킵니다: {rel}");
            continue;
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)?;
    }
    Ok(())
}

/// `..` 이나 절대 경로로 밖을 가리키지 않는 경우에만 합친 경로를 돌려준다.
fn join_inside(base: &Path, rel: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (out != base).then_some(out)
}

// ───────────────────────────── 파이프라인 ─────────────────────────────

/// 파이프라인의 모델 노드 → 그 노드가 돌리는 모델.
///
/// `Binding::ModelOutput` 은 모델을 가리키는데 실행기는 노드 단위로 값을 낸다. 그 사이를 잇는 표다.
fn model_nodes_of(pipeline: Option<&Pipeline>) -> BTreeMap<PNodeId, ModelId> {
    let Some(pipeline) = pipeline else {
        return BTreeMap::new();
    };
    pipeline
        .nodes
        .values()
        .filter_map(|n| match &n.kind {
            nl_core::PNodeKind::Model { model, .. } => Some((n.id, *model)),
            _ => None,
        })
        .collect()
}

/// 매니페스트의 진입 파이프라인. 지정이 없고 파이프라인이 하나뿐이면 그것을 쓴다.
pub fn entry_pipeline(project: &Project, manifest: &BundleManifest) -> Option<Pipeline> {
    if let Some(id) = manifest.entry_pipeline {
        return project.pipelines.get(&id).cloned();
    }
    if project.pipelines.len() == 1 {
        return project.pipelines.values().next().cloned();
    }
    None
}

/// **`Runner` 를 만드는 유일한 자리.** `nl-io` 가 `Runner::new(project, pipeline, base_dir, device)` 와
/// `arm_input` 필드를 추가하면 이 함수 한 곳만 고치면 된다.
pub fn spawn_runner(
    project: &Project,
    pipeline: &Pipeline,
    base_dir: &Path,
    device: DevicePref,
    arm_input: bool,
) -> anyhow::Result<RunnerHandle> {
    // 마우스/키보드 싱크는 **번들이 명시적으로 허용했을 때만** 실제 입력을 보낸다
    // (`BundleManifest::arm_input`, 기본 꺼짐). 받은 사람이 모르는 사이 커서가 움직이면 안 된다.
    let mut runner = Runner::new(project.clone(), pipeline.clone(), base_dir.to_path_buf(), device);
    runner.arm_input = arm_input;
    runner.start()
}

/// 상단 바 배지. 이 앱이 실제 마우스·키보드 입력을 보낼 수 있다는 표시.
pub const ARM_INPUT_BADGE: &str = "⚠ 입력 무장";
/// 무장 상태를 처음 알릴 때 쓰는 문장. 헤드리스와 GUI 가 같은 말을 쓴다.
pub const ARM_INPUT_NOTICE: &str = "이 앱은 마우스·키보드를 실제로 조작합니다 (빌드할 때 입력 무장을 켰습니다).";

/// 헤드리스·GUI 로그가 같은 문장을 쓰도록 이벤트를 한 줄로 만든다.
pub fn describe_event(ev: &RunnerEvent) -> String {
    match ev {
        RunnerEvent::Started => "파이프라인 시작".into(),
        RunnerEvent::Stopped => "파이프라인 정지".into(),
        RunnerEvent::Log(s) => s.clone(),
        RunnerEvent::Error { node: Some(n), message } => format!("오류 [{}]: {message}", n.short()),
        RunnerEvent::Error { node: None, message } => format!("오류: {message}"),
        RunnerEvent::Value { node, value } => format!("값 [{}] {}", node.short(), nl_gui::format_value(Some(value))),
        RunnerEvent::Widget { widget, value } => {
            format!("위젯 [{}] {}", widget.short(), nl_gui::format_value(Some(value)))
        }
        RunnerEvent::ValuePreview {
            node, width, height, ..
        } => {
            format!("미리보기 [{}] {width}×{height}", node.short())
        }
        RunnerEvent::Stats { tick, tick_ms, hz } => format!("틱 {tick} · {hz:.1}Hz · 틱당 {tick_ms:.1}ms"),
    }
}

// ───────────────────────────── 앱 ─────────────────────────────

pub struct RuntimeApp {
    project: Project,
    manifest: BundleManifest,
    layout: GuiLayout,
    pipeline: Option<Pipeline>,
    gui: GuiState,
    device: DevicePref,
    devices: Vec<DeviceInfo>,
    runner: Option<RunnerHandle>,
    logs: VecDeque<String>,
    errors: usize,
    show_logs: bool,
    base_dir: PathBuf,
    /// 마지막 `RunnerEvent::Stats` 의 (실제 Hz, 틱당 ms). 상단 바에 띄운다.
    stats: Option<(f32, f32)>,
    /// 자동 업데이트. 번들에 주소가 없거나 `--no-update` 면 `None`.
    update: Option<UpdateUi>,
    /// 입력 무장 안내를 이미 로그에 남겼는가 (시작/정지를 반복해도 한 번만).
    warned_arm_input: bool,
    /// 파이프라인의 모델 노드 → 그 노드가 돌리는 모델. `Binding::ModelOutput` 위젯에 값을 넘길 때 쓴다.
    ///
    /// `PipelineOutput` 은 노드를 직접 가리키지만 `ModelOutput` 은 모델을 가리킨다. 그 모델을 돌리는
    /// 노드가 값을 내는 순간이 곧 "마지막 추론 값" 이라, 노드에서 모델로 한 번 옮겨야 한다.
    model_nodes: BTreeMap<PNodeId, ModelId>,
}

impl RuntimeApp {
    /// 번들과 이미 준비된 작업 폴더로 앱을 만든다. `manifest.autostart` 면 곧바로 파이프라인을 시작한다.
    pub fn new(bundle: &Bundle, base_dir: PathBuf, device: DevicePref) -> Self {
        let pipeline = entry_pipeline(&bundle.project, &bundle.manifest);
        let model_nodes = model_nodes_of(pipeline.as_ref());
        let mut app = Self {
            project: bundle.project.clone(),
            manifest: bundle.manifest.clone(),
            layout: bundle.project.gui.clone(),
            pipeline,
            gui: GuiState::default(),
            device,
            devices: nl_engine::enumerate(),
            runner: None,
            logs: VecDeque::new(),
            errors: 0,
            show_logs: false,
            base_dir,
            stats: None,
            update: None,
            warned_arm_input: false,
            model_nodes,
        };
        app.log(format!("{} {}", app.manifest.app_name, app.manifest.app_version));
        if app.manifest.autostart {
            app.start();
        }
        app
    }

    /// 자동 업데이트를 켠다. 번들 매니페스트에 주소가 있어야 실제로 붙는다.
    pub fn with_updates(mut self, enabled: bool) -> Self {
        if !enabled {
            log::info!("자동 업데이트를 껐습니다 (--no-update)");
            return self;
        }
        match UpdateUi::new(&self.manifest) {
            Some(mut ui) => {
                let url = ui.manifest_url().to_string();
                ui.start_check();
                self.update = Some(ui);
                self.log(format!("업데이트를 확인합니다: {url}"));
            }
            None => log::info!("번들에 업데이트 주소가 없어 자동 업데이트를 쓰지 않습니다"),
        }
        self
    }

    /// 이미 만들어 둔 업데이트 상태를 붙인다. 확인을 시작하지 않으므로 테스트가 네트워크 없이 쓴다.
    #[cfg(test)]
    pub fn with_update_ui(mut self, ui: UpdateUi) -> Self {
        self.update = Some(ui);
        self
    }

    #[cfg(test)]
    pub fn update_ui(&self) -> Option<&UpdateUi> {
        self.update.as_ref()
    }

    #[cfg(test)]
    pub fn update_ui_mut(&mut self) -> Option<&mut UpdateUi> {
        self.update.as_mut()
    }

    /// 상단 바 통계를 고정값으로 박는다. 스냅샷이 실제 틱 속도에 흔들리지 않게 한다.
    #[cfg(test)]
    pub fn inject_stats(&mut self, hz: f32, tick_ms: f32) {
        self.stats = Some((hz, tick_ms));
    }

    /// 창을 여는 쪽에서 한 번 부른다 (폰트·테마). 테스트는 부르지 않아도 된다.
    pub fn install_style(&self, ctx: &egui::Context) {
        ctx.set_fonts(nl_gui::font_definitions());
        if self.layout.window.dark {
            ctx.set_visuals(nl_gui::dark_visuals());
        }
    }

    pub fn is_running(&self) -> bool {
        self.runner.is_some()
    }

    pub fn error_count(&self) -> usize {
        self.errors
    }

    pub fn log_lines(&self) -> Vec<&str> {
        self.logs.iter().map(String::as_str).collect()
    }

    fn log(&mut self, msg: impl Into<String>) {
        let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg.into());
        log::info!("{line}");
        self.logs.push_back(line);
        while self.logs.len() > LOG_LINES {
            self.logs.pop_front();
        }
    }

    pub fn start(&mut self) {
        if self.runner.is_some() {
            return;
        }
        let Some(pipeline) = self.pipeline.clone() else {
            self.errors += 1;
            self.log("실행할 파이프라인이 없습니다 (매니페스트의 entry_pipeline 확인)");
            return;
        };
        let name = pipeline.name.clone();
        let arm = self.manifest.arm_input;
        match spawn_runner(&self.project, &pipeline, &self.base_dir, self.device, arm) {
            Ok(handle) => {
                self.runner = Some(handle);
                self.errors = 0;
                self.log(format!("파이프라인 시작: {name} ({})", self.device.label()));
                // 무장은 처음 시작할 때 한 번만 알린다 (시작/정지를 반복해도 로그가 불어나지 않게).
                if arm && !self.warned_arm_input {
                    self.warned_arm_input = true;
                    self.log(ARM_INPUT_NOTICE);
                }
            }
            Err(e) => {
                self.errors += 1;
                self.log(format!("파이프라인을 시작하지 못했습니다: {e}"));
            }
        }
    }

    pub fn stop(&mut self) {
        let Some(handle) = self.runner.clone() else { return };
        handle.stop();
        self.log("정지를 요청했습니다");
    }

    /// 파이프라인 이벤트를 이번 프레임에 도착한 만큼 모두 소비한다.
    fn poll_runner(&mut self, ctx: &egui::Context) {
        let Some(handle) = self.runner.clone() else { return };
        let mut stopped = self.drain(&handle);
        if handle.is_done() {
            // 스레드가 끝난 뒤에 큐에 남은 이벤트까지 비운다.
            self.drain(&handle);
            stopped = true;
        }
        if stopped {
            self.runner = None;
            self.stats = None;
        } else {
            ctx.request_repaint_after(REPAINT);
        }
    }

    /// 큐를 비우고 `Stopped` 를 봤는지 돌려준다.
    fn drain(&mut self, handle: &RunnerHandle) -> bool {
        let mut stopped = false;
        while let Ok(ev) = handle.events.try_recv() {
            match &ev {
                RunnerEvent::Stopped => stopped = true,
                RunnerEvent::Error { .. } => self.errors += 1,
                RunnerEvent::Widget { widget, value } => {
                    let points = self.max_points(*widget);
                    self.gui.push_value(*widget, value.clone(), points);
                }
                RunnerEvent::Value { node, value } => self.apply_node_value(*node, value),
                // 이미지 원본은 `Value` 로 오지 않는다(드롭 정책) — 이 축소판이 유일한 통로다.
                RunnerEvent::ValuePreview {
                    node,
                    width,
                    height,
                    rgba,
                } => {
                    let preview = Value::Image {
                        width: *width,
                        height: *height,
                        rgba: rgba.clone(),
                    };
                    self.apply_node_value(*node, &preview);
                }
                RunnerEvent::Stats { hz, tick_ms, .. } => self.stats = Some((*hz, *tick_ms)),
                _ => {}
            }
            match ev {
                // 값·미리보기는 초당 수십 번, 통계는 초당 한 번 온다 — 200줄짜리 로그를 채우지 않는다.
                // 통계는 로그 대신 상단 바 상태에 띄운다.
                RunnerEvent::Value { .. }
                | RunnerEvent::Widget { .. }
                | RunnerEvent::ValuePreview { .. }
                | RunnerEvent::Stats { .. } => {}
                other => {
                    let line = describe_event(&other);
                    self.log(line);
                }
            }
        }
        stopped
    }

    /// 이 노드의 값을 받을 위젯들에 넘긴다. 빌더 미리보기(`nl_app::session`)와 같은 규칙이다.
    ///
    /// `PipelineOutput` 은 노드를 직접 가리키고, `ModelOutput` 은 모델을 가리킨다. 뒤쪽은 그 모델을
    /// 돌리는 노드가 값을 낼 때가 곧 마지막 추론 값이라 여기서 함께 채운다.
    fn apply_node_value(&mut self, node: PNodeId, value: &Value) {
        let model = self.model_nodes.get(&node).copied();
        let targets: Vec<WidgetId> = self
            .layout
            .widgets
            .values()
            .filter(|w| match &w.binding {
                Some(Binding::PipelineOutput { node: n }) => *n == node,
                // 필드 이름은 아직 쓰지 않는다 — 모델 출력이 하나면 그것이 곧 그 필드다.
                // 다출력을 지원할 때 `Value::Json` 에서 필드를 꺼내는 자리가 여기다.
                Some(Binding::ModelOutput { model: m, .. }) => model == Some(*m),
                _ => false,
            })
            .map(|w| w.id)
            .collect();
        for id in targets {
            let points = self.max_points(id);
            self.gui.push_value(id, value.clone(), points);
        }
    }

    fn max_points(&self, widget: WidgetId) -> usize {
        match self.layout.widgets.get(&widget).map(|w| &w.kind) {
            Some(WidgetKind::Plot { max_points }) => (*max_points).max(1),
            _ => DEFAULT_POINTS,
        }
    }

    /// 위젯 이벤트를 바인딩에 따라 파이프라인 입력이나 내장 동작으로 보낸다.
    fn handle_gui_events(&mut self, events: Vec<GuiEvent>, ctx: &egui::Context) {
        for ev in events {
            let (id, value) = match ev {
                GuiEvent::Clicked(id) => (id, Value::Number(1.0)),
                GuiEvent::Changed(id, v) => (id, v),
                // Design 모드 전용 이벤트는 런타임에서 나오지 않는다.
                GuiEvent::Selected(_) | GuiEvent::Moved(..) => continue,
            };
            let binding = self.layout.widgets.get(&id).and_then(|w| w.binding.clone());
            match binding {
                Some(Binding::PipelineInput { .. }) => self.send_input(id, value),
                Some(Binding::Action { action }) => match action {
                    BuiltinAction::StartPipeline => self.start(),
                    BuiltinAction::StopPipeline => self.stop(),
                    BuiltinAction::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                },
                _ => {}
            }
        }
    }

    fn send_input(&mut self, widget: WidgetId, value: Value) {
        let Some(handle) = self.runner.clone() else {
            self.log("파이프라인이 멈춰 있어 입력을 보내지 않았습니다");
            return;
        };
        if handle.inputs.send(RunnerInput::Widget { widget, value }).is_err() {
            self.errors += 1;
            self.log("파이프라인 입력 채널이 닫혔습니다");
        }
    }

    // ── 그리기 ──

    /// eframe 과 테스트가 함께 쓰는 그리기 진입점.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll_runner(&ctx);
        self.poll_update(&ctx);
        egui::Panel::top("nl_runtime_bar").show(ui, |ui| self.top_bar(ui));
        let events = egui::CentralPanel::default()
            .show(ui, |ui| {
                nl_gui::render_layout(ui, &self.layout, &mut self.gui, RenderMode::Run)
            })
            .inner;
        self.handle_gui_events(events, &ctx);
        self.log_window(&ctx);
        self.update_window(&ctx);
    }

    /// 업데이트 이벤트를 소비하고, 자동 다운로드 조건을 보고, 적용이 끝났으면 앱을 닫는다.
    fn poll_update(&mut self, ctx: &egui::Context) {
        let running = self.is_running();
        let Some(update) = &mut self.update else { return };

        let events = update.poll();
        update.maybe_auto_download(running);
        let busy = update.is_busy();

        let mut close = false;
        let mut lines = Vec::new();
        for ev in &events {
            if matches!(ev, nl_update::Event::Applied(_)) {
                close = true;
            }
            // 적용이 끝나면 옛 내려받기를 치운다 — 방금 쓴 것만 남긴다.
            if let nl_update::Event::Downloaded(path) = ev {
                update.prune(path);
            }
            if let Some(line) = crate::update::describe(ev) {
                lines.push(line);
            }
        }
        for line in lines {
            self.log(line);
        }
        if busy {
            ctx.request_repaint_after(REPAINT);
        }
        if close {
            // 새 프로세스가 이미 예약돼 있다. 옛 프로세스는 비켜 준다.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// 업데이트 창. 릴리스 노트·진행률·"지금 적용" 버튼.
    fn update_window(&mut self, ctx: &egui::Context) {
        let Some(update) = &self.update else { return };
        if !update.show {
            return;
        }
        let state = update.state().clone();
        let mut open = true;
        let mut download = false;
        let mut apply = false;
        let mut recheck = false;

        egui::Window::new("업데이트")
            .open(&mut open)
            .default_size([420.0, 240.0])
            .show(ctx, |ui| {
                match &state {
                    nl_update::State::Available(a) => {
                        ui.heading(format!("새 버전 {}", a.version));
                        ui.label(format!("지금 버전 {}", self.manifest.app_version));
                        if !a.notes.is_empty() {
                            ui.separator();
                            egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                                ui.label(&a.notes);
                            });
                        }
                        ui.separator();
                        download = ui.button("내려받기").clicked();
                    }
                    nl_update::State::Downloading { received, total } => {
                        ui.label("내려받는 중…");
                        let progress = nl_update::Progress {
                            received: *received,
                            total: *total,
                        };
                        match progress.fraction() {
                            Some(f) => {
                                ui.add(egui::ProgressBar::new(f).show_percentage());
                            }
                            None => {
                                ui.label(format!("{received} 바이트"));
                            }
                        }
                    }
                    nl_update::State::Downloaded { .. } => {
                        ui.label("내려받았습니다.");
                        ui.label("적용하면 앱이 종료되고 새 버전이 다시 시작됩니다.");
                        ui.separator();
                        apply = ui.button("지금 적용").clicked();
                    }
                    nl_update::State::Applying => {
                        ui.spinner();
                        ui.label("적용 중…");
                    }
                    nl_update::State::Applied(a) => {
                        ui.label(a.message());
                    }
                    nl_update::State::Failed(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                        ui.separator();
                        recheck = ui.button("다시 확인").clicked();
                    }
                    nl_update::State::Disabled(why) => {
                        // 보통은 여기까지 오지 않는다 — 공개키가 없으면 UpdateUi 가 아예 만들어지지 않는다.
                        ui.colored_label(ui.visuals().warn_fg_color, "자동 업데이트를 쓸 수 없습니다");
                        ui.label(why);
                    }
                    nl_update::State::Idle | nl_update::State::Checking | nl_update::State::UpToDate => {
                        ui.label(state.message());
                    }
                }
            });

        if let Some(update) = &mut self.update {
            update.show = open;
            if download {
                update.start_download();
            }
            if apply {
                update.apply();
            }
            if recheck {
                update.recheck();
            }
        }
        if apply {
            self.log("업데이트를 적용합니다");
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.is_running();
        let armed = self.manifest.arm_input;
        let status = if running {
            match self.stats {
                Some((hz, tick_ms)) => format!("실행 중 · {hz:.0}Hz · 틱당 {tick_ms:.1}ms"),
                None => "실행 중".to_string(),
            }
        } else if self.error_count() > 0 {
            format!("정지 · 오류 {}건", self.error_count())
        } else {
            "정지".to_string()
        };

        let mut start = false;
        let mut stop = false;
        let mut toggle_logs = false;
        let mut toggle_update = false;
        let badge = self.update.as_ref().and_then(|u| u.badge());
        let mut device = self.device;
        {
            let devices = &self.devices;
            ui.horizontal(|ui| {
                if running {
                    stop = ui.button("■ 정지").clicked();
                } else {
                    start = ui.button("▶ 시작").clicked();
                }
                ui.separator();
                egui::ComboBox::from_id_salt("nl_runtime_device")
                    .selected_text(device_label(devices, device))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut device, DevicePref::Auto, "자동");
                        for d in devices {
                            ui.selectable_value(&mut device, d.pref, &d.name);
                        }
                    });
                ui.separator();
                ui.label(&status);
                if armed {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(0xE0, 0xA0, 0x30), ARM_INPUT_BADGE)
                        .on_hover_text(ARM_INPUT_NOTICE);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    toggle_logs = ui.button("⚙").on_hover_text("로그 창").clicked();
                    if let Some(badge) = &badge {
                        toggle_update = ui.button(badge).on_hover_text("업데이트 창을 엽니다").clicked();
                    }
                });
            });
        }

        if start {
            self.start();
        }
        if stop {
            self.stop();
        }
        if device != self.device {
            self.device = device;
            let label = device.label();
            self.log(format!("장치를 {label} 로 바꿨습니다 (다음 시작부터 적용)"));
        }
        if toggle_logs {
            self.show_logs = !self.show_logs;
        }
        if toggle_update {
            if let Some(update) = &mut self.update {
                update.show = !update.show;
            }
        }
    }

    fn log_window(&mut self, ctx: &egui::Context) {
        if !self.show_logs {
            return;
        }
        let mut open = true;
        egui::Window::new("로그")
            .open(&mut open)
            .default_size([520.0, 280.0])
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                    for line in self.log_lines() {
                        ui.label(line);
                    }
                });
            });
        self.show_logs = open;
    }
}

fn device_label(devices: &[DeviceInfo], pref: DevicePref) -> String {
    match devices.iter().find(|d| d.pref == pref) {
        Some(d) => d.name.clone(),
        None => pref.label(),
    }
}

impl eframe::App for RuntimeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use nl_core::gui::Widget;
    use nl_core::{PNode, PNodeKind, Sink};

    fn demo_bundle(autostart: bool) -> Bundle {
        let mut project = Project::new("데모");
        let mut pipeline = Pipeline::new("주 파이프라인");
        let sink = pipeline.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [0.0, 0.0]));
        let pid = pipeline.id;
        project.pipelines.insert(pid, pipeline);

        let mut layout = GuiLayout::default();
        layout.window.title = "데모 앱".into();
        layout.add(Widget::new(
            WidgetKind::Label {
                text: "데모 라벨".into(),
            },
            [10.0, 10.0, 160.0, 20.0],
        ));
        let mut btn = Widget::new(WidgetKind::Button { text: "시작".into() }, [10.0, 40.0, 100.0, 28.0]);
        btn.binding = Some(Binding::Action {
            action: BuiltinAction::StartPipeline,
        });
        layout.add(btn);
        let mut out = Widget::new(WidgetKind::Value { prefix: "값: ".into() }, [10.0, 80.0, 200.0, 30.0]);
        out.binding = Some(Binding::PipelineOutput { node: sink });
        layout.add(out);
        project.gui = layout;

        let mut manifest = BundleManifest::new("데모 앱", "0.1.0");
        manifest.entry_pipeline = Some(pid);
        manifest.autostart = autostart;
        Bundle::new(manifest, project)
    }

    fn harness(autostart: bool) -> (tempfile::TempDir, Harness<'static, RuntimeApp>) {
        let dir = tempfile::tempdir().unwrap();
        let bundle = demo_bundle(autostart);
        let app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu);
        let h = Harness::builder()
            .with_size(egui::vec2(800.0, 500.0))
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(60)
            .build_ui_state(|ui, app: &mut RuntimeApp| app.draw(ui), app);
        (dir, h)
    }

    /// `Binding::ModelOutput` 위젯이 든 번들. 모델 노드 하나와 그 모델을 가리키는 값 위젯·이미지 위젯.
    fn model_output_bundle() -> (Bundle, PNodeId, ModelId, WidgetId, WidgetId) {
        let mut project = Project::new("데모");
        let model = ModelId::from_u128(77);
        project.models.insert(model, nl_core::ModelDef::new("분류기"));

        let mut pipeline = Pipeline::new("주 파이프라인");
        let node = pipeline.add_node(PNode::new(PNodeKind::Model { model, payload: None }, [0.0, 0.0]));
        // 같은 파이프라인의 다른 노드. 모델이 아니므로 ModelOutput 위젯을 건드리면 안 된다.
        let other = pipeline.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [0.0, 60.0]));
        let pid = pipeline.id;
        project.pipelines.insert(pid, pipeline);

        let mut layout = GuiLayout::default();
        layout.window.title = "모델 출력".into();
        let mut value = Widget::new(
            WidgetKind::Value {
                prefix: "예측: ".into(),
            },
            [10.0, 10.0, 260.0, 30.0],
        );
        value.binding = Some(Binding::ModelOutput {
            model,
            field: "라벨".into(),
        });
        let value_id = layout.add(value);
        let mut image = Widget::new(WidgetKind::Image, [10.0, 50.0, 120.0, 120.0]);
        image.binding = Some(Binding::ModelOutput {
            model,
            field: "미리보기".into(),
        });
        let image_id = layout.add(image);
        // 다른 노드에 묶인 위젯 — 모델 값에 반응하면 안 된다.
        let mut unrelated = Widget::new(
            WidgetKind::Value {
                prefix: "로그: ".into(),
            },
            [10.0, 180.0, 260.0, 30.0],
        );
        unrelated.binding = Some(Binding::PipelineOutput { node: other });
        layout.add(unrelated);
        project.gui = layout;

        let mut manifest = BundleManifest::new("모델 출력", "0.1.0");
        manifest.entry_pipeline = Some(pid);
        manifest.autostart = false;
        (Bundle::new(manifest, project), node, model, value_id, image_id)
    }

    fn model_output_app(bundle: &Bundle) -> (tempfile::TempDir, RuntimeApp) {
        let dir = tempfile::tempdir().unwrap();
        let app = RuntimeApp::new(bundle, dir.path().to_path_buf(), DevicePref::Cpu);
        (dir, app)
    }

    /// 모델 노드가 값을 내면 그 모델을 가리키는 `ModelOutput` 위젯에 값이 들어간다.
    #[test]
    fn a_model_node_value_reaches_widgets_bound_to_that_model() {
        let (bundle, node, model, value_id, _image_id) = model_output_bundle();
        let (_dir, mut app) = model_output_app(&bundle);
        assert_eq!(app.model_nodes.get(&node), Some(&model), "모델 노드 표가 비었습니다");

        app.apply_node_value(node, &Value::Number(0.75));
        assert_eq!(app.gui.values.get(&value_id), Some(&Value::Number(0.75)));
    }

    /// 모델이 아닌 노드의 값은 `ModelOutput` 위젯을 건드리지 않는다.
    #[test]
    fn a_value_from_another_node_does_not_touch_model_output_widgets() {
        let (bundle, _node, _model, value_id, _image_id) = model_output_bundle();
        let (_dir, mut app) = model_output_app(&bundle);

        let other = PNodeId::from_u128(4242);
        app.apply_node_value(other, &Value::Number(9.0));
        assert!(!app.gui.values.contains_key(&value_id), "엉뚱한 노드 값이 들어갔습니다");
    }

    /// 다른 모델을 가리키는 위젯에는 들어가지 않는다.
    #[test]
    fn only_the_matching_model_receives_the_value() {
        let (mut bundle, node, _model, _value_id, _image_id) = model_output_bundle();
        // 위젯의 바인딩을 다른 모델로 바꾼다.
        let other_model = ModelId::from_u128(99);
        for w in bundle.project.gui.widgets.values_mut() {
            if let Some(Binding::ModelOutput { model, .. }) = &mut w.binding {
                *model = other_model;
            }
        }
        let ids: Vec<WidgetId> = bundle.project.gui.widgets.keys().copied().collect();
        let (_dir, mut app) = model_output_app(&bundle);

        app.apply_node_value(node, &Value::Number(1.0));
        for id in ids {
            assert!(!app.gui.values.contains_key(&id), "{id:?} 에 값이 들어갔습니다");
        }
    }

    /// 이미지 축소판(`ValuePreview`)도 같은 배선을 탄다 — 이미지 원본은 `Value` 로 오지 않는다.
    #[test]
    fn a_value_preview_reaches_model_output_widgets_as_an_image() {
        let (bundle, node, _model, value_id, image_id) = model_output_bundle();
        let (_dir, mut app) = model_output_app(&bundle);

        let rgba = vec![0u8; 2 * 2 * 4];
        app.apply_node_value(
            node,
            &Value::Image {
                width: 2,
                height: 2,
                rgba: rgba.clone(),
            },
        );
        assert!(
            matches!(
                app.gui.values.get(&image_id),
                Some(Value::Image {
                    width: 2,
                    height: 2,
                    ..
                })
            ),
            "이미지 위젯에 축소판이 들어가지 않았습니다"
        );
        // 같은 모델을 가리키므로 값 위젯에도 들어간다 (렌더러가 "이미지 2×2" 로 적는다).
        assert!(app.gui.values.contains_key(&value_id));
    }

    /// 모델 출력이 든 화면이 패닉 없이 그려진다.
    #[test]
    fn a_bundle_with_model_output_widgets_renders() {
        let (bundle, node, _model, _value_id, _image_id) = model_output_bundle();
        let dir = tempfile::tempdir().unwrap();
        let mut app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu);
        app.apply_node_value(node, &Value::Number(0.5));

        let mut h = Harness::builder()
            .with_size(egui::vec2(400.0, 300.0))
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(60)
            .build_ui_state(|ui, app: &mut RuntimeApp| app.draw(ui), app);
        h.run_steps(3);
        assert!(h.state().log_lines().iter().any(|l| l.contains("모델 출력")));
    }

    #[test]
    fn renders_without_panic_and_shows_bundle_gui() {
        let (_dir, mut h) = harness(true);
        h.run_steps(3);
        let app = h.state();
        // 스텁 Runner 는 즉시 Stopped 를 보내므로 실행 중이 아니어야 하고 로그가 남아야 한다.
        assert!(!app.log_lines().is_empty());
        assert!(
            app.log_lines().iter().any(|l| l.contains("파이프라인 시작")),
            "자동 시작 로그가 없습니다: {:?}",
            app.log_lines()
        );
    }

    /// 업데이트 상태를 주입하고 한 프레임 그린다 — 배지와 창이 패닉 없이 뜨는지 본다.
    #[test]
    fn update_badge_and_window_render() {
        use crate::update::UpdateUi;
        use nl_update::{Asset, AssetKind, Available, Event, Progress};

        let dir = tempfile::tempdir().unwrap();
        let mut bundle = demo_bundle(false);
        bundle.manifest.update_url = Some("https://updates.example/demo/latest.json".into());
        bundle.manifest.update_public_key = Some("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".into());

        let ui = UpdateUi::new(&bundle.manifest).expect("주소가 있으면 만들어진다");
        let app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu).with_update_ui(ui);
        let mut h = Harness::builder()
            .with_size(egui::vec2(900.0, 560.0))
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(60)
            .build_ui_state(|ui, app: &mut RuntimeApp| app.draw(ui), app);

        // 아직 알릴 것이 없다.
        h.run_steps(2);
        assert!(h.state().update_ui().unwrap().badge().is_none());

        let available = Available {
            version: semver::Version::new(0, 2, 0),
            notes: "고친 것이 많습니다".into(),
            asset: Asset {
                url: "https://updates.example/demo/app".into(),
                sha256: "ab".into(),
                kind: AssetKind::Binary,
                size: 128,
            },
            target: nl_update::target_key(),
        };

        // 새 버전 → 배지가 뜨고 창이 열린다.
        h.state_mut()
            .update_ui_mut()
            .unwrap()
            .inject(Event::Available(available));
        h.state_mut().update_ui_mut().unwrap().show = true;
        h.run_steps(2);
        assert_eq!(
            h.state().update_ui().unwrap().badge().as_deref(),
            Some("⬆ 새 버전 0.2.0")
        );

        // 진행률 → 진행 막대.
        h.state_mut().update_ui_mut().unwrap().inject(Event::Progress(Progress {
            received: 64,
            total: Some(128),
        }));
        h.run_steps(2);
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 내려받는 중"));

        // 다 받음 → "지금 적용" 버튼. 실제로 누르지는 않는다.
        h.state_mut()
            .update_ui_mut()
            .unwrap()
            .inject(Event::Downloaded(dir.path().join("app")));
        h.run_steps(2);
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 적용 준비됨"));

        // 실패 → 배지는 사라지고 창에 오류가 남는다.
        h.state_mut()
            .update_ui_mut()
            .unwrap()
            .inject(Event::Failed("연결 실패".into()));
        h.run_steps(2);
        assert!(h.state().update_ui().unwrap().badge().is_none());
        assert!(
            h.state().log_lines().iter().any(|l| l.contains("연결 실패")),
            "실패를 로그에 남겨야 합니다: {:?}",
            h.state().log_lines()
        );
    }

    // ── 골든 이미지 스냅샷 ──────────────────────────────────────
    //
    // `egui_kittest` 가 wgpu 로 오프스크린 렌더해 `tests/snapshots/` 의 PNG 와 견준다.
    // 갱신: `UPDATE_SNAPSHOTS=1 cargo test -p nl-runtime`
    //
    // 런타임 상단 바는 한국어라 CJK 글꼴 없이 찍으면 두부 글자만 남아 사람이 검토할 수 없다.
    // 그래서 골든을 만든 것과 **같은 글꼴**(Noto Sans CJK Regular)이 있을 때만 비교하고,
    // 없으면 건너뛴다 — 다른 글꼴로 찍혀 영문 모를 불일치가 나는 것보다 낫다.
    // (nl-gui 쪽 스냅샷은 라벨이 ASCII 라 이런 제약이 없다.)

    /// 골든을 만든 글꼴. 배포판마다 경로가 달라 후보를 훑는다.
    const SNAPSHOT_FONT: &str = "Noto Sans CJK Regular";

    /// 골든과 같은 CJK 글꼴을 얹는다. 없으면 `None`.
    fn snapshot_fonts() -> Option<egui::FontDefinitions> {
        let candidates = [
            "/usr/share/fonts/google-noto-sans-cjk-fonts/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ];
        let path = candidates.iter().find(|p| std::path::Path::new(p).is_file())?;
        let bytes = std::fs::read(path).ok()?;
        let mut defs = egui::FontDefinitions::default();
        defs.font_data
            .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            defs.families.entry(fam).or_default().push("cjk".into());
        }
        Some(defs)
    }

    /// 갖출 수 있는 전제가 빠졌다 — `NL_SNAPSHOT_REQUIRED=1` 이면 실패시킨다.
    /// CI 는 이 변수를 켜 두어야 렌더 백엔드가 빠진 채 조용히 초록불이 뜨지 않는다.
    /// (cargo 는 통과한 테스트의 출력을 삼키므로 `eprintln!` 만으로는 눈에 띄지 않는다.)
    fn skip(reason: &str) -> bool {
        let message = format!("스냅샷 건너뜀: {reason}");
        if std::env::var("NL_SNAPSHOT_REQUIRED").is_ok_and(|v| v != "0") {
            panic!("{message} (NL_SNAPSHOT_REQUIRED 가 켜져 있어 실패로 처리한다)");
        }
        eprintln!("{message}");
        eprintln!("  건너뜀을 실패로 보려면 NL_SNAPSHOT_REQUIRED=1 로 돌린다.");
        false
    }

    /// 기계마다 다를 수밖에 없는 전제가 빠졌다 — 알리기만 하고 실패시키지 않는다.
    /// 골든을 만든 글꼴이 그렇다. 배포판마다 판본이 달라 CI 에 강제할 수 없고,
    /// 다른 글꼴로 찍으면 영문 모를 불일치가 난다.
    fn skip_optional(reason: &str) -> bool {
        eprintln!("스냅샷 건너뜀(선택 전제): {reason}");
        false
    }

    /// 렌더 백엔드가 없으면 조용히 통과하지 않도록 이유를 찍고 건너뛴다.
    fn renderer_ready() -> bool {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let probed = std::panic::catch_unwind(|| {
            let mut h = Harness::builder().with_size(egui::vec2(32.0, 32.0)).build_ui(|ui| {
                ui.label("probe");
            });
            h.run_steps(1);
            h.render().is_ok()
        });
        std::panic::set_hook(hook);
        match probed {
            Ok(true) => true,
            Ok(false) => skip("wgpu 렌더가 이미지를 내지 못했습니다 (어댑터는 있으나 렌더 실패)"),
            Err(_) => {
                skip("wgpu 어댑터가 없습니다. Linux 라면 소프트웨어 래스터라이저(mesa 의 lavapipe)를 깔면 돕니다")
            }
        }
    }

    /// 실행 중 + 통계 + 업데이트 배지가 한 화면에 나온 상태를 굳힌다.
    #[test]
    fn runtime_app_snapshot() {
        if !renderer_ready() {
            return;
        }
        let Some(fonts) = snapshot_fonts() else {
            skip_optional(&format!(
                "골든을 만든 글꼴({SNAPSHOT_FONT})이 없습니다. \
                 Fedora `google-noto-sans-cjk-fonts`, Debian/Ubuntu `fonts-noto-cjk` 를 깔면 돕니다"
            ));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = demo_bundle(true);
        bundle.manifest.update_url = Some("https://updates.example/demo/latest.json".into());
        bundle.manifest.update_public_key = Some("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".into());

        let ui = crate::update::UpdateUi::new(&bundle.manifest).expect("주소가 있으면 만들어진다");
        let app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu).with_update_ui(ui);

        let mut h = Harness::builder()
            .with_size(egui::vec2(720.0, 260.0))
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(60)
            .build_ui_state(|ui, app: &mut RuntimeApp| app.draw(ui), app);
        h.ctx.set_fonts(fonts);
        h.run_steps(2);

        // 배지가 뜨도록 새 버전을 알리고, 상단 바 숫자는 고정값으로 박는다.
        h.state_mut()
            .update_ui_mut()
            .unwrap()
            .inject(nl_update::Event::Available(nl_update::Available {
                version: semver::Version::new(0, 2, 0),
                notes: "fixes".into(),
                asset: nl_update::Asset {
                    url: "https://updates.example/demo/app".into(),
                    sha256: "ab".into(),
                    kind: nl_update::AssetKind::Binary,
                    size: 128,
                },
                target: nl_update::target_key(),
            }));
        h.run_steps(2);
        h.state_mut().inject_stats(30.0, 0.4);
        h.run_steps(1);

        assert!(h.state().is_running(), "실행 중 상태여야 상단 바에 통계가 뜬다");
        assert_eq!(
            h.state().update_ui().unwrap().badge().as_deref(),
            Some("⬆ 새 버전 0.2.0")
        );

        let options = egui_kittest::SnapshotOptions::new()
            .threshold(0.7)
            .max_failed_pixels(64);
        h.try_snapshot_options("runtime-app", &options).unwrap();
    }

    /// `ModelOutput` 위젯에 값이 들어간 화면을 굳힌다. 배선이 끊기면 값 칸이 비어 골든과 어긋난다.
    #[test]
    fn model_output_snapshot() {
        if !renderer_ready() {
            return;
        }
        let Some(fonts) = snapshot_fonts() else {
            skip_optional(&format!(
                "골든을 만든 글꼴({SNAPSHOT_FONT})이 없습니다. \
                 Fedora `google-noto-sans-cjk-fonts`, Debian/Ubuntu `fonts-noto-cjk` 를 깔면 돕니다"
            ));
            return;
        };
        let (bundle, node, _model, _value_id, _image_id) = model_output_bundle();
        let dir = tempfile::tempdir().unwrap();
        let mut app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu);

        // 모델 노드가 낸 값. 배선이 맞으면 "예측: 0.750" 이 보이고, 끊기면 빈 칸이 된다.
        //
        // 숫자 하나만 넣는다. 두 위젯이 같은 모델을 가리키므로 값은 **둘 다** 받는다 —
        // `field` 로 갈라 보내는 것은 모델이 출력을 여럿 낼 때의 일이고 아직 하지 않는다.
        // 이미지 경로는 `a_value_preview_reaches_model_output_widgets_as_an_image` 가 따로 본다.
        app.apply_node_value(node, &Value::Number(0.75));

        let mut h = Harness::builder()
            .with_size(egui::vec2(420.0, 300.0))
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(60)
            .build_ui_state(|ui, app: &mut RuntimeApp| app.draw(ui), app);
        h.ctx.set_fonts(fonts);
        h.run_steps(3);

        let options = egui_kittest::SnapshotOptions::new()
            .threshold(0.7)
            .max_failed_pixels(64);
        h.try_snapshot_options("model-output", &options).unwrap();
    }

    #[test]
    fn no_update_url_means_no_update_ui() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = demo_bundle(false);
        let app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu).with_updates(true);
        assert!(app.update_ui().is_none(), "번들에 주소가 없으면 붙지 않는다");
    }

    #[test]
    fn updates_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = demo_bundle(false);
        bundle.manifest.update_url = Some("https://updates.example/demo/latest.json".into());
        bundle.manifest.update_public_key = Some("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".into());
        let app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu).with_updates(false);
        assert!(app.update_ui().is_none(), "--no-update 면 확인조차 하지 않는다");
    }

    #[test]
    fn without_autostart_the_pipeline_stays_stopped() {
        let (_dir, mut h) = harness(false);
        h.run_steps(2);
        assert!(!h.state().is_running());
        assert!(h.state().log_lines().iter().all(|l| !l.contains("파이프라인 시작")));
    }

    #[test]
    fn node_value_reaches_bound_widget() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = demo_bundle(false);
        let node = bundle
            .project
            .pipelines
            .values()
            .next()
            .unwrap()
            .nodes
            .keys()
            .copied()
            .next()
            .unwrap();
        let widget = bundle
            .project
            .gui
            .widgets
            .values()
            .find(|w| matches!(w.binding, Some(Binding::PipelineOutput { .. })))
            .unwrap()
            .id;
        let mut app = RuntimeApp::new(&bundle, dir.path().to_path_buf(), DevicePref::Cpu);
        app.apply_node_value(node, &Value::Number(4.5));
        assert_eq!(app.gui.values.get(&widget), Some(&Value::Number(4.5)));
    }

    #[test]
    fn workspace_materializes_weights() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = demo_bundle(false);
        let model = bundle.project.add_model("분류기");
        bundle.project.models.get_mut(&model).unwrap().weights = Some("runs/abc/model.safetensors".into());
        bundle.manifest.models = vec![nl_core::bundle::BundledModel {
            model,
            weights_file: "m.safetensors".into(),
        }];
        bundle.weights.insert("m.safetensors".into(), vec![1, 2, 3]);

        prepare_workspace(&bundle, dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("weights/m.safetensors")).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            std::fs::read(dir.path().join("runs/abc/model.safetensors")).unwrap(),
            vec![1, 2, 3]
        );
    }

    // ── 고정 작업 폴더 ──

    fn bundle_with_asset(name: &str, bytes: &[u8]) -> Bundle {
        let mut b = demo_bundle(false);
        b.assets.insert(name.to_string(), bytes.to_vec());
        b
    }

    /// 고정 폴더는 `local/` 을 만들고 0700 으로 잠근다.
    #[test]
    fn a_fixed_work_dir_is_created_owner_only_with_a_local_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        let work = WorkDir::fixed(&dir).unwrap();

        assert_eq!(work.path(), dir.as_path());
        assert!(work.is_persistent());
        assert!(dir.join(LOCAL_DIR).is_dir(), "local/ 이 없습니다");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    /// 고정 폴더는 Drop 에서 지워지지 않는다 — 임시 폴더와 갈리는 지점이다.
    #[test]
    fn a_fixed_work_dir_survives_drop_but_a_temp_one_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        drop(WorkDir::fixed(&dir).unwrap());
        assert!(dir.is_dir(), "고정 폴더가 사라졌습니다");

        let temp = WorkDir::create().unwrap();
        let temp_path = temp.path().to_path_buf();
        assert!(!temp.is_persistent());
        drop(temp);
        assert!(!temp_path.exists(), "임시 폴더가 남았습니다");
    }

    /// 같은 번들이면 다시 풀지 않는다. 서비스 재시작이 빨라야 한다.
    #[test]
    fn the_same_bundle_is_not_extracted_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        let bundle = bundle_with_asset("표.csv", b"a,b\n1,2\n");
        let fp = bundle_fingerprint(&bundle);

        let work = WorkDir::fixed(&dir).unwrap();
        assert!(sync_workspace(&bundle, &fp, work.path()).unwrap(), "처음에는 푼다");
        let asset = dir.join("assets/표.csv");
        assert_eq!(std::fs::read(&asset).unwrap(), b"a,b\n1,2\n");

        // 두 번째는 그대로 쓴다. 파일을 건드렸는지 보려고 내용을 바꿔 둔다.
        std::fs::write(&asset, "손댐".as_bytes()).unwrap();
        assert!(
            !sync_workspace(&bundle, &fp, work.path()).unwrap(),
            "같은 번들은 다시 풀지 않는다"
        );
        assert_eq!(
            std::fs::read(&asset).unwrap(),
            "손댐".as_bytes(),
            "다시 풀어 덮어썼습니다"
        );
    }

    /// 번들이 바뀌면 다시 푼다. 옛 에셋은 남지 않는다.
    #[test]
    fn a_changed_bundle_is_re_extracted_and_stale_files_go_away() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        let work = WorkDir::fixed(&dir).unwrap();

        let old = bundle_with_asset("옛.csv", b"1");
        sync_workspace(&old, &bundle_fingerprint(&old), work.path()).unwrap();
        assert!(dir.join("assets/옛.csv").is_file());

        let new = bundle_with_asset("새.csv", b"2");
        let fp_new = bundle_fingerprint(&new);
        assert_ne!(fp_new, bundle_fingerprint(&old), "내용이 다르면 지문도 달라야 합니다");
        assert!(sync_workspace(&new, &fp_new, work.path()).unwrap(), "바뀌면 다시 푼다");

        assert!(dir.join("assets/새.csv").is_file());
        assert!(!dir.join("assets/옛.csv").exists(), "옛 에셋이 남았습니다");
    }

    /// 번들을 갱신해도 `local/` 은 건드리지 않는다. 인증서가 여기에 있다.
    #[test]
    fn refreshing_the_bundle_never_touches_the_local_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        let work = WorkDir::fixed(&dir).unwrap();

        let old = bundle_with_asset("옛.csv", b"1");
        sync_workspace(&old, &bundle_fingerprint(&old), work.path()).unwrap();

        // 사용자가 인증서를 놓는다.
        let cert = dir.join(LOCAL_DIR).join("server.crt");
        std::fs::write(&cert, b"-----BEGIN CERTIFICATE-----").unwrap();

        let new = bundle_with_asset("새.csv", b"2");
        sync_workspace(&new, &bundle_fingerprint(&new), work.path()).unwrap();

        assert_eq!(
            std::fs::read(&cert).unwrap(),
            b"-----BEGIN CERTIFICATE-----",
            "번들 갱신이 local/ 을 건드렸습니다"
        );
    }

    /// 지문은 내용만 본다 — 같은 내용이면 같고, 한 바이트만 달라도 다르다.
    ///
    /// `demo_bundle()` 을 두 번 부르면 프로젝트 id 와 생성 시각이 달라 서로 다른 번들이 된다.
    /// 그래서 같은 번들을 복제해 비교한다 — 배포에서는 `.nlapp` 하나를 읽으므로 매 실행이 같다.
    #[test]
    fn the_fingerprint_follows_content_only() {
        let a = bundle_with_asset("x", b"same");
        assert_eq!(
            bundle_fingerprint(&a),
            bundle_fingerprint(&a.clone()),
            "같은 번들은 같은 지문"
        );

        // 에셋 내용이 다르면 다르다.
        let mut c = a.clone();
        c.assets.insert("x".into(), b"other".to_vec());
        assert_ne!(bundle_fingerprint(&a), bundle_fingerprint(&c));

        // 에셋 이름이 달라도 다르다.
        let mut d = a.clone();
        d.assets.clear();
        d.assets.insert("y".into(), b"same".to_vec());
        assert_ne!(bundle_fingerprint(&a), bundle_fingerprint(&d));

        // 매니페스트가 달라도 다르다.
        let mut e = a.clone();
        e.manifest.app_version = "9.9.9".into();
        assert_ne!(bundle_fingerprint(&a), bundle_fingerprint(&e));

        // 가중치가 달라도 다르다.
        let mut f = a.clone();
        f.weights.insert("w.safetensors".into(), b"weights".to_vec());
        assert_ne!(bundle_fingerprint(&a), bundle_fingerprint(&f));
    }

    /// 표시가 깨져 있으면 다시 푼다 — 반쯤 풀린 폴더를 그대로 쓰지 않는다.
    #[test]
    fn a_missing_or_wrong_marker_forces_a_re_extract() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("작업");
        let work = WorkDir::fixed(&dir).unwrap();
        let bundle = bundle_with_asset("x", b"1");
        let fp = bundle_fingerprint(&bundle);

        sync_workspace(&bundle, &fp, work.path()).unwrap();
        assert!(!sync_workspace(&bundle, &fp, work.path()).unwrap());

        // 표시를 지우면 다시 푼다.
        std::fs::remove_file(dir.join(".bundle-sha256")).unwrap();
        assert!(
            sync_workspace(&bundle, &fp, work.path()).unwrap(),
            "표시가 없으면 다시 푼다"
        );

        // weights 폴더가 사라져도 다시 푼다.
        std::fs::remove_dir_all(dir.join("weights")).unwrap();
        assert!(
            sync_workspace(&bundle, &fp, work.path()).unwrap(),
            "내용이 없으면 다시 푼다"
        );
    }

    /// M13: 작업 폴더는 이름을 예측할 수 없고 소유자만 드나들 수 있어야 한다.
    #[test]
    fn work_dir_is_unpredictable_and_owner_only() {
        let a = WorkDir::create().unwrap();
        let b = WorkDir::create().unwrap();
        assert_ne!(a.path(), b.path(), "두 번 만들면 다른 경로여야 합니다");

        // 옛 구현이 쓰던 pid 기반 이름이 아니다.
        let predictable = std::env::temp_dir().join(format!("nl-runtime-{}", std::process::id()));
        assert_ne!(a.path(), predictable.as_path());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(a.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{:o}", mode);
        }
    }

    #[test]
    fn work_dir_is_removed_on_drop() {
        let work = WorkDir::create().unwrap();
        let path = work.path().to_path_buf();
        assert!(path.is_dir());
        drop(work);
        assert!(!path.exists());
    }

    #[test]
    fn join_inside_rejects_escapes() {
        let base = Path::new("/tmp/x");
        assert_eq!(join_inside(base, "a/b.bin"), Some(PathBuf::from("/tmp/x/a/b.bin")));
        assert_eq!(join_inside(base, "../out"), None);
        assert_eq!(join_inside(base, "/etc/passwd"), None);
    }
}
