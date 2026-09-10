//! 파이프라인 실행기. 틱 루프가 별도 스레드에서 소스 → 모델/로직 → 싱크 순으로 값을 흘린다.
//! 빌더의 "시험 실행"과 배포 런타임이 같은 Runner 를 쓴다.

use crossbeam_channel::{Receiver, Sender};
use nl_core::{DevicePref, PNodeId, Pipeline, Project, WidgetId};
use nl_engine::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum RunnerEvent {
    Started,
    /// 노드가 값을 냈다 (GUI 위젯 바인딩·디버그 표시).
    Value { node: PNodeId, value: Value },
    /// `Sink::GuiWidget` 로 위젯에 표시할 값.
    Widget { widget: WidgetId, value: Value },
    Log(String),
    Error { node: Option<PNodeId>, message: String },
    Stopped,
}

/// GUI → 파이프라인 입력 (`Source::GuiEvent`, `Source::Manual`).
#[derive(Clone, Debug)]
pub enum RunnerInput {
    Widget { widget: WidgetId, value: Value },
    Manual { node: PNodeId, value: Value },
}

pub struct Runner {
    pub project: Project,
    pub pipeline: Pipeline,
    /// 가중치 상대 경로 기준.
    pub base_dir: PathBuf,
    pub device: DevicePref,
}

#[derive(Clone)]
pub struct RunnerHandle {
    pub events: Receiver<RunnerEvent>,
    pub inputs: Sender<RunnerInput>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl RunnerHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

impl Runner {
    /// 즉시 돌아온다. 준비 실패(모델 로드 등)도 `RunnerEvent::Error` + `Stopped` 로 온다.
    pub fn start(self) -> anyhow::Result<RunnerHandle> {
        let (etx, erx) = crossbeam_channel::unbounded();
        let (itx, _irx) = crossbeam_channel::unbounded::<RunnerInput>();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        std::thread::Builder::new().name("nl-runner".into()).spawn(move || {
            let _ = etx.send(RunnerEvent::Error { node: None, message: "파이프라인 실행기 미구현".into() });
            let _ = etx.send(RunnerEvent::Stopped);
            d2.store(true, Ordering::SeqCst);
        })?;
        Ok(RunnerHandle { events: erx, inputs: itx, stop, done })
    }
}
