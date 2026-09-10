//! 배포 런타임의 eframe 앱: 상단 제어 바 + 번들 GUI 레이아웃 + 파이프라인 실행기 연결.

use nl_bundle::Bundle;
use nl_core::gui::{Binding, BuiltinAction};
use nl_core::{BundleManifest, DevicePref, GuiLayout, PNodeId, Pipeline, Project, WidgetId, WidgetKind};
use nl_engine::{DeviceInfo, Value};
use nl_gui::{GuiEvent, GuiState, RenderMode};
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
    let runner = Runner {
        project: project.clone(),
        pipeline: pipeline.clone(),
        base_dir: base_dir.to_path_buf(),
        device,
    };
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
        };
        app.log(format!("{} {}", app.manifest.app_name, app.manifest.app_version));
        if app.manifest.autostart {
            app.start();
        }
        app
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
                _ => {}
            }
            match ev {
                // 값 이벤트는 초당 수십 번 오므로 로그를 채우지 않는다.
                RunnerEvent::Value { .. } | RunnerEvent::Widget { .. } => {}
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
        egui::Panel::top("nl_runtime_bar").show(ui, |ui| self.top_bar(ui));
        let events = egui::CentralPanel::default()
            .show(ui, |ui| nl_gui::render_layout(ui, &self.layout, &mut self.gui, RenderMode::Run))
            .inner;
        self.handle_gui_events(events, &ctx);
        self.log_window(&ctx);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let running = self.is_running();
        let status = if running {
            "실행 중".to_string()
        } else if self.error_count() > 0 {
            format!("정지 · 오류 {}건", self.error_count())
        } else {
            "정지".to_string()
        };

        let mut start = false;
        let mut stop = false;
        let mut toggle_logs = false;
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
