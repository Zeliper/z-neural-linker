//! 빌더 안에서 파이프라인을 돌리는 세션 (시험 실행 · GUI 미리보기).
//!
//! 배선은 배포 런타임(`nl-runtime::app`)과 같다: 이벤트를 받아 위젯 값에 반영하고,
//! 위젯 이벤트는 바인딩을 보고 `RunnerInput` 으로 되돌린다. 다른 점은 **입력 무장이 기본 꺼짐**이라는 것뿐이다.

use crate::pcanvas::LiveView;
use nl_core::gui::Binding;
use nl_core::{DevicePref, GuiLayout, PNodeId, Pipeline, PipelineId, Project, WidgetId, WidgetKind};
use nl_engine::Value;
use nl_gui::GuiState;
use nl_io::runner::RunnerInput;
use nl_io::{Runner, RunnerEvent, RunnerHandle};
use std::path::PathBuf;
use std::time::Duration;

/// `WidgetKind::Plot` 이 아닌 위젯에서 온 값의 히스토리 길이.
const DEFAULT_POINTS: usize = 300;
/// 정지 요청 뒤 스레드가 끝나기를 기다리는 최대 시간.
const STOP_GRACE: Duration = Duration::from_millis(1500);

pub struct RunnerSession {
    pub handle: RunnerHandle,
    pub pipeline: PipelineId,
    /// 캔버스에 보여 줄 노드별 마지막 값·오류.
    pub live: LiveView,
    pub started_at: f64,
    /// `Stopped` 이벤트를 본 뒤.
    pub finished: bool,
    /// 저장 안 된 프로젝트를 위해 만든 임시 작업 폴더 (있으면 끝날 때 지운다).
    pub temp_dir: Option<PathBuf>,
    pub errors: usize,
}

#[derive(Default)]
pub struct RunnerPoll {
    /// 도크 로그에 남길 줄.
    pub logs: Vec<String>,
    /// 다시 그려야 한다.
    pub changed: bool,
    /// 이번 폴에서 멈췄다.
    pub stopped: bool,
}

impl RunnerSession {
    /// 파이프라인을 시작한다. `base_dir` 은 모델 가중치 상대 경로의 기준이다.
    pub fn start(
        project: &Project,
        pipeline: &Pipeline,
        base_dir: PathBuf,
        device: DevicePref,
        arm_input: bool,
        temp_dir: Option<PathBuf>,
        now: f64,
    ) -> anyhow::Result<Self> {
        let mut runner = Runner::new(project.clone(), pipeline.clone(), base_dir, device);
        runner.arm_input = arm_input;
        let handle = runner.start()?;
        Ok(Self {
            handle,
            pipeline: pipeline.id,
            live: LiveView { running: true, ..LiveView::default() },
            started_at: now,
            finished: false,
            temp_dir,
            errors: 0,
        })
    }

    pub fn is_running(&self) -> bool {
        !self.finished
    }

    pub fn stop(&self) {
        self.handle.stop();
    }

    /// 정지를 요청하고 스레드가 끝나기를 잠깐 기다린다 (앱 종료·문서 교체 때).
    pub fn stop_and_wait(&self) {
        self.handle.stop();
        self.handle.wait_done(STOP_GRACE);
    }

    /// 수동 입력·위젯 입력을 실행기에 보낸다. 채널이 닫혔으면 `false`.
    pub fn send(&self, input: RunnerInput) -> bool {
        self.handle.inputs.send(input).is_ok()
    }

    /// 이번 프레임에 도착한 이벤트를 모두 소비한다.
    pub fn poll(&mut self, gui: &mut GuiState, layout: &GuiLayout) -> RunnerPoll {
        let mut out = RunnerPoll::default();
        while let Ok(ev) = self.handle.events.try_recv() {
            out.changed = true;
            match ev {
                RunnerEvent::Started => out.logs.push("파이프라인 시작".into()),
                RunnerEvent::Stopped => {
                    self.finished = true;
                    self.live.running = false;
                    out.stopped = true;
                    out.logs.push("파이프라인 정지".into());
                }
                RunnerEvent::Log(s) => out.logs.push(s),
                RunnerEvent::Error { node, message } => {
                    self.errors += 1;
                    match node {
                        Some(n) => {
                            self.live.errors.insert(n, message.clone());
                            out.logs.push(format!("오류 [{}]: {message}", n.short()));
                        }
                        None => out.logs.push(format!("오류: {message}")),
                    }
                }
                RunnerEvent::Value { node, value } => {
                    self.live.values.insert(node, nl_gui::format_value(Some(&value)));
                    self.live.errors.remove(&node);
                    push_to_bound_widgets(gui, layout, node, &value);
                }
                RunnerEvent::Widget { widget, value } => {
                    let points = max_points(layout, widget);
                    gui.push_value(widget, value, points);
                }
            }
        }
        if self.handle.is_done() && !self.finished {
            self.finished = true;
            self.live.running = false;
            out.stopped = true;
            out.changed = true;
        }
        out
    }

    /// 위젯 이벤트를 바인딩에 따라 실행기로 보낸다. 내장 동작은 호출자가 처리하도록 돌려준다.
    pub fn route_widget_event(&self, layout: &GuiLayout, id: WidgetId, value: Value) -> Option<Binding> {
        let binding = layout.widgets.get(&id).and_then(|w| w.binding.clone())?;
        if matches!(binding, Binding::PipelineInput { .. }) {
            self.send(RunnerInput::Widget { widget: id, value });
        }
        Some(binding)
    }
}

impl Drop for RunnerSession {
    fn drop(&mut self) {
        // 세션이 사라지는데 스레드가 남아 실제 입력을 보내면 안 된다.
        self.handle.stop();
        self.handle.wait_done(STOP_GRACE);
        if let Some(dir) = &self.temp_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// `Binding::PipelineOutput{node}` 로 묶인 위젯에도 노드 값을 반영한다 (런타임과 같은 규칙).
fn push_to_bound_widgets(gui: &mut GuiState, layout: &GuiLayout, node: PNodeId, value: &Value) {
    let targets: Vec<WidgetId> = layout
        .widgets
        .values()
        .filter(|w| matches!(&w.binding, Some(Binding::PipelineOutput { node: n }) if *n == node))
        .map(|w| w.id)
        .collect();
    for id in targets {
        let points = max_points(layout, id);
        gui.push_value(id, value.clone(), points);
    }
}

fn max_points(layout: &GuiLayout, widget: WidgetId) -> usize {
    match layout.widgets.get(&widget).map(|w| &w.kind) {
        Some(WidgetKind::Plot { max_points }) => (*max_points).max(1),
        _ => DEFAULT_POINTS,
    }
}

/// 저장되지 않은 프로젝트를 시험 실행할 때 쓰는 임시 폴더.
pub fn temp_run_dir() -> PathBuf {
    std::env::temp_dir().join(format!("nl-app-run-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::gui::BuiltinAction;
    use nl_core::{Widget, WidgetKind};

    fn layout_with(binding: Option<Binding>, kind: WidgetKind) -> (GuiLayout, WidgetId) {
        let mut l = GuiLayout::default();
        let mut w = Widget::new(kind, [0.0, 0.0, 100.0, 20.0]);
        w.binding = binding;
        let id = w.id;
        l.add(w);
        (l, id)
    }

    #[test]
    fn plot_widgets_keep_their_own_history_length() {
        let (l, id) = layout_with(None, WidgetKind::Plot { max_points: 7 });
        assert_eq!(max_points(&l, id), 7);
        let (l2, id2) = layout_with(None, WidgetKind::Label { text: "x".into() });
        assert_eq!(max_points(&l2, id2), DEFAULT_POINTS);
        // 없는 위젯도 기본값으로 떨어진다.
        assert_eq!(max_points(&l2, WidgetId::from_u128(9)), DEFAULT_POINTS);
    }

    #[test]
    fn node_values_reach_widgets_bound_to_that_node() {
        let node = PNodeId::from_u128(3);
        let (l, id) = layout_with(
            Some(Binding::PipelineOutput { node }),
            WidgetKind::Value { prefix: String::new() },
        );
        let mut gui = GuiState::default();
        push_to_bound_widgets(&mut gui, &l, node, &Value::Number(1.5));
        assert!(matches!(gui.values.get(&id), Some(Value::Number(n)) if (*n - 1.5).abs() < 1e-9));
        // 다른 노드의 값은 오지 않는다.
        push_to_bound_widgets(&mut gui, &l, PNodeId::from_u128(4), &Value::Number(9.0));
        assert!(matches!(gui.values.get(&id), Some(Value::Number(n)) if (*n - 1.5).abs() < 1e-9));
    }

    #[test]
    fn a_plot_binding_accumulates_history() {
        let node = PNodeId::from_u128(3);
        let (l, id) = layout_with(Some(Binding::PipelineOutput { node }), WidgetKind::Plot { max_points: 2 });
        let mut gui = GuiState::default();
        for v in [1.0, 2.0, 3.0] {
            push_to_bound_widgets(&mut gui, &l, node, &Value::Number(v));
        }
        assert_eq!(gui.history.get(&id).map(Vec::len), Some(2), "max_points 만큼만 남는다");
    }

    #[test]
    fn builtin_action_bindings_are_returned_not_sent() {
        // route_widget_event 는 실행기를 필요로 하므로 바인딩 판정만 따로 확인한다.
        let (l, id) = layout_with(
            Some(Binding::Action { action: BuiltinAction::StopPipeline }),
            WidgetKind::Button { text: "정지".into() },
        );
        let binding = l.widgets.get(&id).and_then(|w| w.binding.clone());
        assert!(matches!(binding, Some(Binding::Action { action: BuiltinAction::StopPipeline })));
    }

    #[test]
    fn temp_run_dir_is_process_specific() {
        let d = temp_run_dir();
        assert!(d.to_string_lossy().contains(&std::process::id().to_string()));
    }
}
