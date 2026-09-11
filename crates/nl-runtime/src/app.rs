//! 배포 런타임의 eframe 앱: 상단 제어 바 + 번들 GUI 레이아웃 + 파이프라인 실행기 연결.

use nl_bundle::Bundle;
use nl_core::gui::{Binding, BuiltinAction};
use nl_core::{BundleManifest, DevicePref, GuiLayout, PNodeId, Pipeline, Project, WidgetId, WidgetKind};
use nl_engine::{DeviceInfo, Value};
use nl_gui::{GuiEvent, GuiState, RenderMode};
use crate::update::UpdateUi;
use nl_io::runner::RunnerInput;
use nl_io::{Runner, RunnerEvent, RunnerHandle};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 로그 창에 남기는 줄 수.
const LOG_LINES: usize = 200;
/// 플롯 히스토리 기본 길이 (`WidgetKind::Plot` 이 아닌 위젯에서 온 값).
const DEFAULT_POINTS: usize = 300;
/// 실행 중일 때 최소 다시 그리기 간격.
const REPAINT: Duration = Duration::from_millis(33);

// ───────────────────────────── 임시 작업 폴더 ─────────────────────────────

/// 번들 가중치를 풀어 두는 임시 폴더. 살아 있는 동안만 존재하고 `Drop` 에서 지워진다.
pub struct WorkDir(PathBuf);

impl WorkDir {
    pub fn create() -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("nl-runtime-{}", std::process::id()));
        // 같은 pid 가 재사용된 경우를 대비해 비우고 시작한다.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            log::warn!("임시 폴더를 지우지 못했습니다 ({}): {e}", self.0.display());
        }
    }
}

/// 번들의 가중치·에셋을 작업 폴더에 푼다. 모델이 프로젝트 상대 경로(`ModelDef::weights`)를 가리키면
/// 그 경로에도 같은 파일을 놓아 `Runner` 가 어느 규칙으로 찾아도 맞도록 한다.
pub fn prepare_workspace(bundle: &Bundle, dir: &Path) -> anyhow::Result<()> {
    bundle.materialize_weights(&dir.join("weights"))?;
    bundle.materialize_assets(&dir.join("assets"))?;
    for bm in &bundle.manifest.models {
        let Some(bytes) = bundle.weights.get(&bm.weights_file) else { continue };
        let Some(model) = bundle.project.models.get(&bm.model) else { continue };
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
) -> anyhow::Result<RunnerHandle> {
    // 배포 런타임은 사용자가 만든 앱이므로 마우스/키보드 싱크를 실제로 동작시킨다(빌더의 시험 실행은 기본 비무장).
    let mut runner = Runner::new(project.clone(), pipeline.clone(), base_dir.to_path_buf(), device);
    runner.arm_input = true;
    runner.start()
}

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
        RunnerEvent::ValuePreview { node, width, height, .. } => {
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
}

impl RuntimeApp {
    /// 번들과 이미 준비된 작업 폴더로 앱을 만든다. `manifest.autostart` 면 곧바로 파이프라인을 시작한다.
    pub fn new(bundle: &Bundle, base_dir: PathBuf, device: DevicePref) -> Self {
        let pipeline = entry_pipeline(&bundle.project, &bundle.manifest);
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
        match spawn_runner(&self.project, &pipeline, &self.base_dir, self.device) {
            Ok(handle) => {
                self.runner = Some(handle);
                self.errors = 0;
                self.log(format!("파이프라인 시작: {name} ({})", self.device.label()));
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

    /// `Binding::PipelineOutput{node}` 로 묶인 위젯에도 값을 반영한다.
    fn apply_node_value(&mut self, node: PNodeId, value: &Value) {
        let targets: Vec<WidgetId> = self
            .layout
            .widgets
            .values()
            .filter(|w| matches!(&w.binding, Some(Binding::PipelineOutput { node: n }) if *n == node))
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
            .show(ui, |ui| nl_gui::render_layout(ui, &self.layout, &mut self.gui, RenderMode::Run))
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

        egui::Window::new("업데이트").open(&mut open).default_size([420.0, 240.0]).show(ctx, |ui| {
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
                    let progress = nl_update::Progress { received: *received, total: *total };
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    toggle_logs = ui.button("⚙").on_hover_text("로그 창").clicked();
                    if let Some(badge) = &badge {
                        toggle_update =
                            ui.button(badge).on_hover_text("업데이트 창을 엽니다").clicked();
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
        egui::Window::new("로그").open(&mut open).default_size([520.0, 280.0]).show(ctx, |ui| {
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
        let sink = pipeline.add_node(PNode::new(
            PNodeKind::Sink { sink: Sink::Log },
            [0.0, 0.0],
        ));
        let pid = pipeline.id;
        project.pipelines.insert(pid, pipeline);

        let mut layout = GuiLayout::default();
        layout.window.title = "데모 앱".into();
        layout.add(Widget::new(WidgetKind::Label { text: "데모 라벨".into() }, [10.0, 10.0, 160.0, 20.0]));
        let mut btn = Widget::new(WidgetKind::Button { text: "시작".into() }, [10.0, 40.0, 100.0, 28.0]);
        btn.binding = Some(Binding::Action { action: BuiltinAction::StartPipeline });
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
        h.state_mut().update_ui_mut().unwrap().inject(Event::Available(available));
        h.state_mut().update_ui_mut().unwrap().show = true;
        h.run_steps(2);
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 새 버전 0.2.0"));

        // 진행률 → 진행 막대.
        h.state_mut().update_ui_mut().unwrap().inject(Event::Progress(Progress { received: 64, total: Some(128) }));
        h.run_steps(2);
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 내려받는 중"));

        // 다 받음 → "지금 적용" 버튼. 실제로 누르지는 않는다.
        h.state_mut().update_ui_mut().unwrap().inject(Event::Downloaded(dir.path().join("app")));
        h.run_steps(2);
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 적용 준비됨"));

        // 실패 → 배지는 사라지고 창에 오류가 남는다.
        h.state_mut().update_ui_mut().unwrap().inject(Event::Failed("연결 실패".into()));
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
        defs.font_data.insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            defs.families.entry(fam).or_default().push("cjk".into());
        }
        Some(defs)
    }

    /// 건너뛴 이유를 알린다. `NL_SNAPSHOT_REQUIRED=1` 이면 건너뛰지 않고 실패시킨다 —
    /// CI 는 이 변수를 켜 두어야 렌더 백엔드나 글꼴이 빠진 채 조용히 초록불이 뜨지 않는다.
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
            Err(_) => skip(
                "wgpu 어댑터가 없습니다. Linux 라면 소프트웨어 래스터라이저(mesa 의 lavapipe)를 깔면 돕니다",
            ),
        }
    }

    /// 실행 중 + 통계 + 업데이트 배지가 한 화면에 나온 상태를 굳힌다.
    #[test]
    fn runtime_app_snapshot() {
        if !renderer_ready() {
            return;
        }
        let Some(fonts) = snapshot_fonts() else {
            skip(&format!(
                "골든을 만든 글꼴({SNAPSHOT_FONT})이 없습니다. \
                 Fedora `google-noto-sans-cjk-fonts`, Debian/Ubuntu `fonts-noto-cjk` 를 깔면 돕니다"
            ));
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = demo_bundle(true);
        bundle.manifest.update_url = Some("https://updates.example/demo/latest.json".into());

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
        h.state_mut().update_ui_mut().unwrap().inject(nl_update::Event::Available(nl_update::Available {
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
        assert_eq!(h.state().update_ui().unwrap().badge().as_deref(), Some("⬆ 새 버전 0.2.0"));

        let options = egui_kittest::SnapshotOptions::new().threshold(0.7).max_failed_pixels(64);
        h.try_snapshot_options("runtime-app", &options).unwrap();
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
        let node = bundle.project.pipelines.values().next().unwrap().nodes.keys().copied().next().unwrap();
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
        bundle.manifest.models =
            vec![nl_core::bundle::BundledModel { model, weights_file: "m.safetensors".into() }];
        bundle.weights.insert("m.safetensors".into(), vec![1, 2, 3]);

        prepare_workspace(&bundle, dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("weights/m.safetensors")).unwrap(), vec![1, 2, 3]);
        assert_eq!(std::fs::read(dir.path().join("runs/abc/model.safetensors")).unwrap(), vec![1, 2, 3]);
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
