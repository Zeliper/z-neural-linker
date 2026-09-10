//! 학습 루프. 별도 스레드에서 돌고 이벤트 채널로 진행 상황을 보낸다. UI 스레드는 이벤트만 받는다.

use crate::tensor::HostTensor;
use crossbeam_channel::Receiver;
use nl_core::{DatasetSpec, EpochMetrics, ModelDef, RunId, RunRecord};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum TrainEvent {
    Started { device: String, batches_per_epoch: usize, params: usize },
    Step { epoch: usize, step: usize, loss: f64 },
    Epoch(EpochMetrics),
    Checkpoint { path: PathBuf },
    /// 진단 메시지 (도크 로그 탭).
    Log(String),
    /// 마지막 이벤트. `run` 은 최종 상태(Finished/Stopped)·지표·체크포인트 경로를 담는다.
    Finished { run: RunRecord },
    /// 마지막 이벤트.
    Failed { run: RunRecord, error: String },
}

#[derive(Clone, Debug)]
pub struct TrainRequest {
    pub run_id: RunId,
    pub model: ModelDef,
    pub dataset: DatasetSpec,
    /// 데이터셋의 상대 경로 기준 (프로젝트 파일 폴더).
    pub base_dir: PathBuf,
    /// 체크포인트·run.json 을 쓰는 폴더 (없으면 만든다).
    pub run_dir: PathBuf,
    /// 이 가중치에서 이어서 학습 (없으면 새로 초기화).
    pub resume_from: Option<PathBuf>,
}

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;

#[derive(Clone)]
pub struct TrainHandle {
    pub run_id: RunId,
    pub events: Receiver<TrainEvent>,
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl TrainHandle {
    pub(crate) fn new(run_id: RunId, events: Receiver<TrainEvent>) -> (Self, TrainControl) {
        let state = Arc::new(AtomicU8::new(RUNNING));
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let ctl = TrainControl { state: state.clone(), stop: stop.clone(), done: done.clone() };
        (Self { run_id, events, state, stop, done }, ctl)
    }
    pub fn pause(&self) {
        self.state.store(PAUSED, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.state.store(RUNNING, Ordering::SeqCst);
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.store(RUNNING, Ordering::SeqCst);
    }
    pub fn is_paused(&self) -> bool {
        self.state.load(Ordering::SeqCst) == PAUSED
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// 학습 스레드가 보는 쪽.
pub(crate) struct TrainControl {
    state: Arc<AtomicU8>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl TrainControl {
    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
    /// 일시정지면 풀릴 때까지 블록. 중지 요청이 오면 true.
    pub fn wait_if_paused(&self) -> bool {
        while self.state.load(Ordering::SeqCst) == PAUSED && !self.should_stop() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.should_stop()
    }
    pub fn mark_done(&self) {
        self.done.store(true, Ordering::SeqCst);
    }
}

/// 학습을 시작한다. 즉시 돌아오며, 실패도 `TrainEvent::Failed` 로 온다(스레드 생성 실패만 Err).
pub fn start(req: TrainRequest) -> anyhow::Result<TrainHandle> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (handle, ctl) = TrainHandle::new(req.run_id, rx);
    std::thread::Builder::new().name("nl-train".into()).spawn(move || {
        let run = RunRecord {
            id: req.run_id,
            model: req.model.id,
            dataset: Some(req.dataset.id),
            config: req.model.train.clone(),
            started: chrono::Utc::now(),
            finished: Some(chrono::Utc::now()),
            status: nl_core::RunStatus::Failed,
            device_name: String::new(),
            epochs: vec![],
            checkpoint: None,
            error: Some("학습 엔진 미구현".into()),
            note: String::new(),
        };
        let _ = tx.send(TrainEvent::Failed { run, error: "학습 엔진 미구현".into() });
        ctl.mark_done();
    })?;
    Ok(handle)
}

/// 체크포인트(safetensors)에서 파라미터 이름·형상 목록 (모델 관리 뷰).
pub fn checkpoint_summary(_path: &std::path::Path) -> anyhow::Result<Vec<(String, Vec<usize>)>> {
    anyhow::bail!("미구현")
}

#[allow(dead_code)]
fn _unused(_: HostTensor) {}
