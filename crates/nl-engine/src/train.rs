//! 학습 루프. 별도 스레드에서 돌고 이벤트 채널로 진행 상황을 보낸다. UI 스레드는 이벤트만 받는다.
//!
//! 옵티마이저(SGD/Adam/AdamW)는 burn 의 `Module` 파생에 기대지 않고 파라미터 텐서를 직접 갱신한다 —
//! 그래야 런타임에 정의된 그래프를 그대로 학습할 수 있다.

use crate::data::{self, Sample};
use crate::device::{self, dispatch_autodiff};
use crate::exec::{DynTensor, Model};
use crate::tensor::HostTensor;
use crate::weights;
use anyhow::{bail, Context, Result};
use burn::tensor::backend::AutodiffBackend;
use burn::tensor::{activation, ElementConversion, Int, Shape, Tensor, TensorData};
use crossbeam_channel::{Receiver, Sender};
use nl_core::dataset::Split;
use nl_core::{
    DatasetSpec, EpochMetrics, Loss, LrSchedule, Metric, ModelDef, Optimizer, RunId, RunRecord, RunStatus, TrainConfig,
};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub enum TrainEvent {
    Started {
        device: String,
        batches_per_epoch: usize,
        params: usize,
    },
    Step {
        epoch: usize,
        step: usize,
        loss: f64,
    },
    Epoch(EpochMetrics),
    Checkpoint {
        path: PathBuf,
    },
    /// 진단 메시지 (도크 로그 탭).
    Log(String),
    /// 마지막 이벤트. `run` 은 최종 상태(Finished/Stopped)·지표·체크포인트 경로를 담는다.
    Finished {
        run: RunRecord,
    },
    /// 마지막 이벤트.
    Failed {
        run: RunRecord,
        error: String,
    },
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

/// 이벤트 채널 용량.
///
/// `Step` 은 스텝마다 나가므로 무한 채널이면 UI 가 늦게 비울 때 메모리가 계속 는다.
/// 유계로 두고 **`Step` 만** 넘칠 때 버린다 (최신 것이 다음에 다시 온다).
/// 나머지 이벤트(Started·Epoch·Checkpoint·Log·Finished·Failed)는 버리지 않는다.
const EVENT_CAPACITY: usize = 1024;

/// 소비자가 아예 읽지 않을 때 중요한 이벤트를 기다리는 한계.
/// 이걸 넘기면 포기하고 로그를 남긴다 — 학습 스레드가 영영 멈춰 있는 편이 더 나쁘다.
const CRITICAL_SEND_LIMIT: Duration = Duration::from_secs(30);

/// `Step` 이벤트의 최소 간격. `NL_STEP_EVENT_MS` 로 바꿀 수 있다 (0 = 매 스텝).
fn step_event_gap() -> Duration {
    let ms = std::env::var("NL_STEP_EVENT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(50);
    Duration::from_millis(ms)
}

/// 이벤트 송신을 한 곳에 모은다. `Step` 은 솎아 내고 버릴 수 있지만 나머지는 그렇지 않다.
struct Emitter {
    tx: Sender<TrainEvent>,
    gap: Duration,
    last_step: Option<Instant>,
    dropped: usize,
    throttled: usize,
}

impl Emitter {
    fn new(tx: Sender<TrainEvent>) -> Self {
        Self {
            tx,
            gap: step_event_gap(),
            last_step: None,
            dropped: 0,
            throttled: 0,
        }
    }

    /// 지금 `Step` 을 내보낼 때가 되었는가. 손실 readback 도 이 주기에 맞춘다.
    fn step_due(&self) -> bool {
        self.gap.is_zero() || self.last_step.is_none_or(|t| t.elapsed() >= self.gap)
    }

    fn step(&mut self, ev: TrainEvent) {
        self.last_step = Some(Instant::now());
        if self.tx.try_send(ev).is_err() {
            self.dropped += 1;
        }
    }

    fn skip_step(&mut self) {
        self.throttled += 1;
    }

    /// 진단 메시지. 버리지 않는다.
    fn log(&self, msg: String) {
        self.critical(TrainEvent::Log(msg), None);
    }

    /// 버리지 않는다. 소비자가 멈춰 있으면 기다리되, 중지 요청이나 한계 시간에는 그만둔다.
    fn critical(&self, ev: TrainEvent, ctl: Option<&TrainControl>) {
        let started = Instant::now();
        let mut pending = ev;
        loop {
            match self.tx.send_timeout(pending, Duration::from_millis(200)) {
                Ok(()) => return,
                Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => return,
                Err(crossbeam_channel::SendTimeoutError::Timeout(back)) => {
                    if started.elapsed() >= CRITICAL_SEND_LIMIT || ctl.is_some_and(|c| c.should_stop()) {
                        log::error!("이벤트 수신자가 비우지 않아 이벤트를 보내지 못했습니다: {back:?}");
                        return;
                    }
                    pending = back;
                }
            }
        }
    }
}

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
        let ctl = TrainControl {
            state: state.clone(),
            stop: stop.clone(),
            done: done.clone(),
        };
        (
            Self {
                run_id,
                events,
                state,
                stop,
                done,
            },
            ctl,
        )
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
    let (tx, rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
    let (handle, ctl) = TrainHandle::new(req.run_id, rx);
    std::thread::Builder::new().name("nl-train".into()).spawn(move || {
        let mut run = RunRecord {
            id: req.run_id,
            model: req.model.id,
            dataset: Some(req.dataset.id),
            config: req.model.train.clone(),
            started: chrono::Utc::now(),
            finished: None,
            status: RunStatus::Running,
            device_name: String::new(),
            epochs: vec![],
            checkpoint: None,
            best_checkpoint: None,
            error: None,
            note: String::new(),
        };
        // 백엔드(특히 GPU 드라이버)가 패닉하면 채널이 조용히 끊긴다. 그러면 UI 가 영원히 기다리므로
        // 패닉을 잡아 `Failed` 이벤트로 바꾼다. 다른 스레드에서 나는 패닉까지 잡을 수는 없다.
        let mut emit = Emitter::new(tx);
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_training(&req, &mut emit, &ctl, &mut run)
        })) {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!(
                "학습 스레드가 패닉했습니다: {}",
                panic_message(e.as_ref())
            )),
        };
        run.finished = Some(chrono::Utc::now());
        match result {
            Ok(()) => {
                if run.status == RunStatus::Running {
                    run.status = RunStatus::Finished;
                }
                // run.json 은 최종 상태까지 담아 다시 쓴다 (실패해도 이벤트는 보낸다).
                if let Err(e) = write_run_json(&req.run_dir, &run) {
                    emit.critical(TrainEvent::Log(format!("run.json 쓰기 실패: {e:#}")), Some(&ctl));
                }
                emit.critical(TrainEvent::Finished { run }, Some(&ctl));
            }
            Err(e) => {
                run.status = RunStatus::Failed;
                let msg = format!("{e:#}");
                run.error = Some(msg.clone());
                let _ = write_run_json(&req.run_dir, &run);
                emit.critical(TrainEvent::Failed { run, error: msg }, Some(&ctl));
            }
        }
        ctl.mark_done();
    })?;
    Ok(handle)
}

fn panic_message(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 오류".to_string()
    }
}

/// 체크포인트(safetensors)에서 파라미터 이름·형상 목록 (모델 관리 뷰).
pub fn checkpoint_summary(path: &std::path::Path) -> anyhow::Result<Vec<(String, Vec<usize>)>> {
    weights::summary(path)
}

fn write_run_json(run_dir: &Path, run: &RunRecord) -> Result<()> {
    std::fs::create_dir_all(run_dir).with_context(|| format!("폴더 생성 실패: {}", crate::paths::short(run_dir)))?;
    let json = serde_json::to_string_pretty(run).context("RunRecord 직렬화 실패")?;
    let tmp = run_dir.join("run.json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("run.json 쓰기 실패: {}", crate::paths::short(&tmp)))?;
    std::fs::rename(&tmp, run_dir.join("run.json")).context("run.json 이동 실패")?;
    Ok(())
}

fn run_training(req: &TrainRequest, emit: &mut Emitter, ctl: &TrainControl, run: &mut RunRecord) -> Result<()> {
    let (info, handle) = device::resolve_entry(req.model.train.device);
    run.device_name = info.name.clone();
    let gpu = handle.is_gpu();
    dispatch_autodiff!(handle, train_on, req, emit, ctl, run, &info.name, gpu)
}

// ───────────────────────────── 학습 본체 ─────────────────────────────

/// 데이터셋을 장치에 통째로 올려 두는 상한.
///
/// 에포크마다 셔플된 사본을 한 벌 더 만들기 때문에 실제 점유는 이 값의 두 배까지 간다 —
/// 그래서 상한(512 MiB) 의 절반을 데이터셋 기준으로 잡는다. 넘으면 배치마다 호스트에서 올린다.
const RESIDENT_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// CPU(ndarray)에서 상주 경로가 이득이 되기 시작하는 샘플 크기.
///
/// ndarray 는 호스트 `Vec` 을 복사 없이 텐서로 삼기 때문에, 표본이 작으면 배치를 그때그때 만드는 쪽이
/// `select`/`narrow` 로 잘라내는 쪽보다 싸다. 교대 측정(CPU 시간 중앙값, 각 5 라운드):
///
/// | 작업 | 샘플 | 상주 | 호스트 |
/// |---|---|---|---|
/// | XOR 1000×200 | 12 B | 10.20 s | 8.72 s |
/// | 8×8 CNN 1000×8 | 260 B | 23.18 s | 25.84 s |
///
/// 교차점을 정확히 재지는 않았으므로 이득이 확인된 쪽(260 B)에 맞춰 보수적으로 잡는다.
/// GPU 는 배치마다 호스트→장치 전송이 들어가므로 크기와 무관하게 상주가 유리하다.
const RESIDENT_MIN_SAMPLE_BYTES: usize = 256;

fn train_on<B: AutodiffBackend>(
    device: &B::Device,
    req: &TrainRequest,
    emit: &mut Emitter,
    ctl: &TrainControl,
    run: &mut RunRecord,
    device_name: &str,
    gpu: bool,
) -> Result<()> {
    let cfg = req.model.train.clone();
    let mut model = Model::<B>::new(&req.model, device, cfg.seed)?;

    let in_shapes = model.input_sample_shapes();
    let outs = model.output_nodes().to_vec();
    if outs.len() > 1 {
        let first = model.graph().nodes[&outs[0]].display_name();
        emit.log(format!(
            "Output 레이어가 {} 개입니다 — 손실과 지표는 첫 번째 Output '{first}' 만 씁니다. 나머지는 추론에서만 쓰입니다.",
            outs.len()
        ));
    }
    if in_shapes.len() > 1 {
        let names: Vec<String> = model
            .input_nodes()
            .iter()
            .map(|id| model.graph().nodes[id].display_name())
            .collect();
        let counts: Vec<usize> = in_shapes.iter().map(|s| s.iter().product()).collect();
        emit.log(format!(
            "Input 레이어가 {} 개입니다 — 샘플을 {names:?} 순서로 {counts:?} 개씩 잘라 넣습니다.",
            in_shapes.len()
        ));
    }

    // 이어서 학습 — 가중치만 복원한다. 옵티마이저 모멘트·스텝 수는 새로 시작한다.
    if let Some(p) = &req.resume_from {
        let loaded = weights::load_for(p, Some(req.model.id))?;
        model
            .load_host_params(&loaded)
            .with_context(|| format!("체크포인트 적용 실패: {}", crate::paths::short(p)))?;
        emit.log(format!(
            "체크포인트에서 이어서 학습: {} (가중치만 복원 — 옵티마이저 상태와 워밍업은 처음부터입니다)",
            crate::paths::short(p)
        ));
    }
    model.require_grad_all();

    // 데이터. 이미지 리사이즈 힌트는 Input 이 하나일 때만 의미가 있다.
    let hint = if in_shapes.len() == 1 {
        Some(in_shapes[0].clone())
    } else {
        None
    };
    let (mut train_set, info) = data::load_all(&req.dataset, &req.base_dir, hint.as_deref())?;
    if train_set.is_empty() {
        bail!("데이터셋이 비어 있습니다");
    }
    check_dataset_inputs(&info.input_shape, &in_shapes)?;
    if let Some(w) = info.empty_class_warning() {
        emit.log(w);
    }
    let out_width: usize = model
        .output_sample_shapes()
        .first()
        .map(|s| s.iter().product())
        .unwrap_or(0);
    check_target_range(
        &train_set,
        &info,
        cfg.loss,
        out_width,
        &model.graph().nodes[&outs[0]].display_name(),
    )?;

    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    if req.dataset.shuffle {
        train_set.shuffle(&mut rng);
    }

    let mut val_set: Vec<Sample> = Vec::new();
    match &req.dataset.split {
        Split::Separate { validation } => {
            let (v, _) = data::load_source(validation, &req.base_dir, hint.as_deref(), None)?;
            val_set = v;
        }
        Split::Ratio => {
            let ratio = cfg.val_split.clamp(0.0, 0.9);
            let n_val = ((train_set.len() as f64) * ratio).round() as usize;
            if n_val > 0 && n_val < train_set.len() {
                val_set = train_set.split_off(train_set.len() - n_val);
            }
        }
    }

    // 데이터셋을 장치에 상주시킨다 — 매 스텝 호스트에서 올리는 비용을 없앤다.
    let resident_train = resident(&train_set, device, emit, gpu, "학습");
    let resident_val = resident(&val_set, device, emit, gpu, "검증");

    let batch_size = cfg.batch_size.max(1);
    let batches = train_set.len().div_ceil(batch_size);
    emit.critical(
        TrainEvent::Started {
            device: device_name.to_string(),
            batches_per_epoch: batches,
            params: model.trainable_count(),
        },
        Some(ctl),
    );

    let mut opt = Opt::<B>::new(cfg.optimizer);
    let mut lrc = LrController::new(&cfg);
    let mut global_step = 0usize;
    let mut stopped = false;
    let mut best_val: Option<f64> = None;
    let mut since_improve = 0usize;
    let mut skipped_steps = 0usize;
    let mut best_path: Option<PathBuf> = None;
    let mut best_epoch = 0usize;
    let mut early_stopped = false;

    'epochs: for epoch in 1..=cfg.epochs.max(1) {
        let t0 = std::time::Instant::now();
        let mut order: Vec<usize> = (0..train_set.len()).collect();
        if req.dataset.shuffle {
            let mut ep_rng = ChaCha8Rng::seed_from_u64(cfg.seed.wrapping_add(epoch as u64));
            order.shuffle(&mut ep_rng);
        }

        let epoch_set = match &resident_train {
            Some(d) => Some(d.permuted(&order, device)?),
            None => None,
        };

        let epoch_lr = lrc.epoch_lr(epoch);
        let mut last_lr = epoch_lr;
        // 손실 합은 장치에 쌓아 두고 에포크 끝에 한 번만 읽는다 — 스텝마다 읽으면 GPU 에서
        // 그 동기화가 스텝 비용을 지배한다.
        let mut epoch_loss_dev: Option<Tensor<B::InnerBackend, 1>> = None;
        let mut seen = 0usize;
        let total_steps = order.len().div_ceil(batch_size);

        for (step, chunk) in order.chunks(batch_size).enumerate() {
            if ctl.wait_if_paused() || ctl.should_stop() {
                stopped = true;
                break 'epochs;
            }
            last_lr = lrc.step_lr(epoch_lr, global_step);
            opt.set_lr(last_lr);
            global_step += 1;

            let (x, y) = match &epoch_set {
                Some(d) => d.batch(step * batch_size, chunk.len())?,
                None => stack_generic::<B>(&train_set, chunk, device)?,
            };
            let out = forward_first::<B>(&mut model, x, &in_shapes, true)?;
            let loss = device_loss::<B>(&out, &y, cfg.loss)?;

            // 값은 이벤트를 내보낼 때만 읽는다 (발산 검사도 그때 함께). 마지막 스텝은 항상 읽는다.
            let last_step = step + 1 == total_steps;
            if emit.step_due() || last_step {
                let value: f64 = loss.clone().into_scalar().elem::<f64>();
                if !value.is_finite() {
                    bail!("손실이 발산했습니다 (epoch {epoch}, step {step}) — 학습률을 낮춰 보세요");
                }
                emit.step(TrainEvent::Step {
                    epoch,
                    step,
                    loss: value,
                });
            } else {
                emit.skip_step();
            }

            let weighted = loss.clone().detach().inner().mul_scalar(chunk.len() as f64);
            epoch_loss_dev = Some(match epoch_loss_dev {
                Some(acc) => acc + weighted,
                None => weighted,
            });

            let grads = loss.backward();
            if opt.step(&mut model, grads, cfg.grad_clip)? == StepOutcome::Skipped {
                skipped_steps += 1;
                if skipped_steps <= 3 {
                    emit.log(format!(
                        "epoch {epoch} step {step}: 그래디언트 노름이 유한하지 않아 이 스텝을 건너뜁니다"
                    ));
                }
                continue;
            }

            seen += chunk.len();
        }

        let train_loss = match (&epoch_loss_dev, seen) {
            (Some(acc), n) if n > 0 => acc.clone().into_scalar().elem::<f64>() / n as f64,
            _ => 0.0,
        };
        let (val_loss, val_metric) = if val_set.is_empty() {
            (None, None)
        } else {
            let (l, m) = evaluate::<B>(
                &mut model,
                &val_set,
                resident_val.as_ref(),
                &in_shapes,
                batch_size,
                device,
                cfg.loss,
                cfg.metric,
            )?;
            (Some(l), m)
        };
        let em = EpochMetrics {
            epoch,
            train_loss,
            val_loss,
            val_metric,
            seconds: t0.elapsed().as_secs_f64(),
            lr: Some(last_lr),
        };
        run.epochs.push(em);
        emit.critical(TrainEvent::Epoch(em), Some(ctl));
        lrc.on_epoch_end(val_loss);

        if cfg.checkpoint_every > 0 && epoch % cfg.checkpoint_every == 0 {
            let path = req.run_dir.join(format!("epoch-{epoch:04}.safetensors"));
            weights::save(&path, req.model.id, &model.host_params())?;
            emit.critical(TrainEvent::Checkpoint { path }, Some(ctl));
        }

        // 검증 손실이 갱신되면 그 시점의 가중치를 따로 남긴다.
        //
        // 조기 종료는 정의상 "patience 에포크만큼 나빠진 뒤" 멈추므로, 마지막 가중치는 언제나
        // 최적점보다 열화된 쪽이다. 조기 종료를 켠 사용자가 원한 것과 정반대라 best 를 보관한다.
        if let Some(v) = val_loss {
            let improved = best_val.is_none_or(|b| v + 1e-12 < b);
            if improved {
                best_val = Some(v);
                since_improve = 0;
                let path = req.run_dir.join("best.safetensors");
                weights::save(&path, req.model.id, &model.host_params())?;
                emit.critical(TrainEvent::Checkpoint { path: path.clone() }, Some(ctl));
                best_path = Some(path);
                best_epoch = epoch;
            } else {
                since_improve += 1;
            }

            // 조기 종료 — 중지(Stopped)가 아니라 정상 종료(Finished)다.
            if cfg.early_stop_patience > 0 && since_improve >= cfg.early_stop_patience {
                emit.log(format!(
                    "조기 종료: 검증 손실이 {since_improve} 에포크 동안 나아지지 않았습니다 \
                     (최저 {:.6}, 에포크 {best_epoch}). 그 에포크의 가중치를 best.safetensors 로 남겼습니다",
                    best_val.unwrap_or(v)
                ));
                early_stopped = true;
                break;
            }
        }
    }

    // 중지되었어도 마지막 가중치는 남긴다.
    let final_path = req.run_dir.join("final.safetensors");
    weights::save(&final_path, req.model.id, &model.host_params())?;
    emit.critical(
        TrainEvent::Checkpoint {
            path: final_path.clone(),
        },
        Some(ctl),
    );

    run.best_checkpoint = best_path.as_ref().map(|p| relative_to(&req.base_dir, p));
    // 조기 종료로 끝났으면 결과물은 마지막이 아니라 최적 가중치여야 한다.
    run.checkpoint = match (&best_path, early_stopped) {
        (Some(p), true) => Some(relative_to(&req.base_dir, p)),
        _ => Some(relative_to(&req.base_dir, &final_path)),
    };
    if skipped_steps > 0 {
        emit.log(format!("그래디언트가 유한하지 않아 건너뛴 스텝: {skipped_steps} 개"));
    }
    if emit.throttled > 0 || emit.dropped > 0 {
        emit.log(format!(
            "진행 이벤트 {} 개는 주기에 맞춰 솎아 냈고 {} 개는 수신자가 밀려 버렸습니다 \
             (학습 자체에는 영향이 없습니다)",
            emit.throttled, emit.dropped
        ));
    }
    run.status = if stopped {
        RunStatus::Stopped
    } else {
        RunStatus::Finished
    };
    Ok(())
}

/// 배치 입력을 Input 노드별로 나눠 순전파하고 **첫 Output** 만 돌려준다.
fn forward_first<B: AutodiffBackend>(
    model: &mut Model<B>,
    x: DynTensor<B>,
    in_shapes: &[Vec<usize>],
    train: bool,
) -> Result<DynTensor<B>> {
    let inputs = split_inputs(x, in_shapes)?;
    model
        .forward(inputs, train)?
        .into_iter()
        .next()
        .context("출력이 없습니다")
}

/// 분류 타깃이 출력 폭 안에 있는지 **학습 시작 전에** 확인한다.
///
/// `device_loss` 의 `gather` 는 범위를 넘으면 ndarray 에서 패닉하고 wgpu 에서는 조용히 엉뚱한 값을
/// 읽는다. 두 경우 다 원인을 알려 주지 못하므로, 한 번만 도는 호스트 쪽 검사로 미리 막는다.
fn check_target_range(
    set: &[Sample],
    info: &nl_core::dataset::DatasetInfo,
    loss: Loss,
    out_width: usize,
    out_name: &str,
) -> Result<()> {
    if loss != Loss::CrossEntropy || out_width == 0 {
        return Ok(());
    }
    let target_width: usize = set.first().map(|s| s.target.data.len()).unwrap_or(0);
    if target_width != 1 {
        // one-hot 타깃. `gather` 를 타지는 않지만 폭이 출력과 다르면 손실 자체를 계산할 수 없다.
        // 학습을 시작하고 첫 배치에서 터지는 대신 여기서 막는다.
        if target_width != out_width {
            bail!(
                "타깃이 one-hot {target_width} 폭인데 Output '{out_name}' 은 {out_width} 유닛입니다 —                  CrossEntropy 는 둘이 같아야 합니다 (클래스 인덱스 타깃이면 폭이 1 이어야 합니다)"
            );
        }
        return Ok(());
    }
    let mut max = 0.0f32;
    for s in set {
        if let Some(&v) = s.target.data.first() {
            if v < 0.0 || v.fract() != 0.0 {
                bail!("CrossEntropy 타깃 {v} 가 음이 아닌 정수가 아닙니다 — 클래스 인덱스여야 합니다");
            }
            max = max.max(v);
        }
    }
    let needed = max as usize + 1;
    if needed > out_width {
        let classes = if info.classes.is_empty() {
            format!("{needed} 개")
        } else {
            format!("{} 개 ({:?})", info.classes.len(), info.classes)
        };
        bail!(
            "데이터의 클래스가 {classes} 인데 Output '{out_name}' 은 {out_width} 유닛뿐입니다 — \
             가장 큰 클래스 인덱스가 {} 라 최소 {needed} 유닛이 필요합니다",
            max as usize
        );
    }
    Ok(())
}

/// 데이터셋 형상이 모델 Input 과 맞는지 본다.
///
/// - Input 이 하나면 형상이 **정확히** 같아야 한다.
/// - 여럿이면 샘플의 평탄한 원소 수가 Input 원소 수의 합과 같아야 하고, `Graph::input_nodes()`
///   순서대로 잘라 넣는다 (CSV 라면 `input_cols` 를 그 개수만큼 앞에서부터 묶는다).
fn check_dataset_inputs(ds: &[usize], model_inputs: &[Vec<usize>]) -> Result<()> {
    if model_inputs.len() == 1 {
        if ds != model_inputs[0].as_slice() {
            bail!(
                "데이터 입력 형상 {ds:?} 이 모델 Input 형상 {:?} 과 다릅니다",
                model_inputs[0]
            );
        }
        return Ok(());
    }
    let counts: Vec<usize> = model_inputs.iter().map(|s| s.iter().product()).collect();
    let want: usize = counts.iter().sum();
    let got: usize = ds.iter().product();
    if got != want {
        bail!(
            "Input 레이어가 {} 개인 모델은 샘플당 원소 {want} 개가 필요합니다 (노드 순서대로 {counts:?} 개씩). \
             데이터는 {ds:?} = {got} 개를 줍니다",
            model_inputs.len()
        );
    }
    Ok(())
}

/// 배치 입력 텐서를 `Graph::input_nodes()` 순서의 Input 텐서들로 나눈다.
/// Input 이 하나면 그대로 쓴다 (형상은 이미 맞다).
fn split_inputs<B: burn::tensor::backend::Backend>(
    x: DynTensor<B>,
    shapes: &[Vec<usize>],
) -> Result<Vec<DynTensor<B>>> {
    if shapes.len() <= 1 {
        return Ok(vec![x]);
    }
    let dims = x.dims();
    let batch = *dims.first().context("배치 차원이 없습니다")?;
    let total: usize = dims[1..].iter().product();
    let flat = x.reshape(&[batch, total])?;

    let mut out = Vec::with_capacity(shapes.len());
    let mut offset = 0usize;
    for s in shapes {
        let count: usize = s.iter().product();
        if offset + count > total {
            bail!("입력을 {shapes:?} 로 나눌 수 없습니다 — 샘플 원소 수가 {total} 개뿐입니다");
        }
        let piece = flat.clone().narrow_dim(1, offset, count)?;
        let mut target = vec![batch];
        target.extend(s.iter().copied());
        out.push(piece.reshape(&target)?);
        offset += count;
    }
    Ok(out)
}

// ───────────────────────────── 장치 상주 데이터 ─────────────────────────────

/// 데이터셋 전체를 장치에 올려 둔 것. 배치는 여기서 잘라 쓴다.
///
/// 에포크마다 `permuted` 로 셔플 순서를 한 번에 적용해 두면, 배치는 `narrow` 로 자르기만 하면 된다
/// (게더 커널이 에포크당 한 번, 스텝당 0 번).
struct DeviceSet<B: burn::tensor::backend::Backend> {
    x: DynTensor<B>,
    y: DynTensor<B>,
}

impl<B: burn::tensor::backend::Backend> DeviceSet<B> {
    fn upload(set: &[Sample], device: &B::Device) -> Result<Self> {
        let all: Vec<usize> = (0..set.len()).collect();
        let (x, y) = stack_generic::<B>(set, &all, device)?;
        Ok(Self { x, y })
    }

    /// `order` 순서로 행을 재배열한 사본. 순서가 원래대로면 복사하지 않는다.
    fn permuted(&self, order: &[usize], device: &B::Device) -> Result<Self> {
        if order.iter().enumerate().all(|(i, &v)| i == v) {
            return Ok(Self {
                x: self.x.clone(),
                y: self.y.clone(),
            });
        }
        let data = TensorData::new(
            order.iter().map(|&i| i as i64).collect::<Vec<i64>>(),
            Shape::from(vec![order.len()]),
        );
        let idx = Tensor::<B, 1, Int>::from_data(data, device);
        Ok(Self {
            x: self.x.clone().select_rows(&idx).detach(),
            y: self.y.clone().select_rows(&idx).detach(),
        })
    }

    /// `[start, start+len)` 행.
    ///
    /// **`detach` 가 핵심이다.** 떼어내지 않으면 배치가 autodiff 그래프에서 데이터셋 전체 텐서와
    /// 이어진 채 남아, 역전파가 스텝마다 데이터셋 크기의 버퍼를 다룬다 —
    /// 8×8 CNN 2000×20 에서 CPU 시간 252 s 대 129 s 로 두 배 가까이 벌어졌다.
    fn batch(&self, start: usize, len: usize) -> Result<(DynTensor<B>, DynTensor<B>)> {
        Ok((
            self.x.clone().narrow_dim(0, start, len)?.detach(),
            self.y.clone().narrow_dim(0, start, len)?.detach(),
        ))
    }
}

/// 샘플 하나가 차지하는 바이트 (f32 기준).
fn sample_bytes(set: &[Sample]) -> usize {
    set.first()
        .map_or(0, |s| (s.input.data.len() + s.target.data.len()) * 4)
}

fn dataset_bytes(set: &[Sample]) -> usize {
    sample_bytes(set) * set.len()
}

/// 상주 경로를 쓸지 정하고, 쓸 만하면 장치에 올린다. `None` 이면 배치마다 호스트에서 올린다.
fn resident<B: burn::tensor::backend::Backend>(
    set: &[Sample],
    device: &B::Device,
    emit: &Emitter,
    gpu: bool,
    what: &str,
) -> Option<DeviceSet<B>> {
    if set.is_empty() {
        return None;
    }
    // 문제 진단용 탈출구 — 드라이버가 select/narrow 에서 말썽이면 호스트 경로로 되돌린다.
    if std::env::var("NL_NO_RESIDENT").as_deref() == Ok("1") {
        emit.log(format!("NL_NO_RESIDENT=1 — {what} 데이터를 배치마다 올립니다"));
        return None;
    }
    let sample_bytes = sample_bytes(set);
    if !gpu && sample_bytes < RESIDENT_MIN_SAMPLE_BYTES {
        // 작은 표본은 호스트에서 배치를 만드는 쪽이 더 빠르다 (상수 설명 참고).
        return None;
    }
    let bytes = dataset_bytes(set);
    if bytes > RESIDENT_LIMIT_BYTES {
        emit.log(format!(
            "{what} 데이터가 {:.0} MiB 라 장치에 상주시키지 않고 배치마다 올립니다",
            bytes as f64 / (1024.0 * 1024.0)
        ));
        return None;
    }
    match DeviceSet::upload(set, device) {
        Ok(d) => Some(d),
        Err(e) => {
            emit.log(format!("{what} 데이터 상주 실패, 배치마다 올립니다: {e:#}"));
            None
        }
    }
}

// ───────────────────────────── 학습률 스케줄 ─────────────────────────────

/// 스케줄 · 워밍업 · 정체 감쇠를 한 곳에서 계산한다.
struct LrController {
    base: f64,
    schedule: LrSchedule,
    warmup_steps: usize,
    total_epochs: usize,
    plateau_scale: f64,
    plateau_best: Option<f64>,
    plateau_wait: usize,
}

impl LrController {
    fn new(cfg: &TrainConfig) -> Self {
        Self {
            base: cfg.optimizer.lr(),
            schedule: cfg.schedule,
            warmup_steps: cfg.warmup_steps,
            total_epochs: cfg.epochs.max(1),
            plateau_scale: 1.0,
            plateau_best: None,
            plateau_wait: 0,
        }
    }

    /// 에포크(1 기반) 시작 학습률.
    fn epoch_lr(&self, epoch: usize) -> f64 {
        match self.schedule {
            LrSchedule::None => self.base,
            LrSchedule::Step { every, gamma } => {
                let every = every.max(1);
                self.base * gamma.powi(((epoch - 1) / every) as i32)
            }
            LrSchedule::Cosine { min_lr } => {
                let span = self.total_epochs.saturating_sub(1).max(1) as f64;
                let t = ((epoch - 1) as f64 / span).clamp(0.0, 1.0);
                min_lr + (self.base - min_lr) * 0.5 * (1.0 + (std::f64::consts::PI * t).cos())
            }
            LrSchedule::Plateau { .. } => self.base * self.plateau_scale,
        }
    }

    /// 워밍업까지 반영한 스텝 학습률 (`global_step` 은 0 기반).
    fn step_lr(&self, epoch_lr: f64, global_step: usize) -> f64 {
        if self.warmup_steps == 0 || global_step >= self.warmup_steps {
            return epoch_lr;
        }
        epoch_lr * (global_step + 1) as f64 / self.warmup_steps as f64
    }

    /// 에포크가 끝날 때 정체 스케줄의 상태를 갱신한다.
    fn on_epoch_end(&mut self, val_loss: Option<f64>) {
        let LrSchedule::Plateau { patience, factor } = self.schedule else {
            return;
        };
        let Some(v) = val_loss else {
            return;
        };
        match self.plateau_best {
            Some(best) if v + 1e-12 >= best => {
                self.plateau_wait += 1;
                if self.plateau_wait >= patience.max(1) {
                    self.plateau_scale *= factor;
                    self.plateau_wait = 0;
                }
            }
            _ => {
                self.plateau_best = Some(v);
                self.plateau_wait = 0;
            }
        }
    }
}

/// `base_dir` 기준 상대 경로. 만들 수 없으면 절대 경로 문자열.
fn relative_to(base_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(base_dir) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// 샘플 묶음을 장치 텐서 한 쌍으로 (배치 차원 포함). 상주 경로를 못 쓸 때의 폴백이다.
fn stack_generic<B: burn::tensor::backend::Backend>(
    set: &[Sample],
    idx: &[usize],
    device: &B::Device,
) -> Result<(DynTensor<B>, DynTensor<B>)> {
    let first = set.get(idx[0]).context("배치 인덱스가 범위를 벗어났습니다")?;
    let in_shape = first.input.shape.clone();
    let tg_shape = first.target.shape.clone();
    let (in_n, tg_n) = (first.input.data.len(), first.target.data.len());

    let mut xs = Vec::with_capacity(idx.len() * in_n);
    let mut ys = Vec::with_capacity(idx.len() * tg_n);
    for &i in idx {
        let s = &set[i];
        if s.input.shape != in_shape || s.target.shape != tg_shape {
            bail!(
                "샘플 형상이 서로 다릅니다: {:?}/{:?} vs {:?}/{:?}",
                in_shape,
                tg_shape,
                s.input.shape,
                s.target.shape
            );
        }
        xs.extend_from_slice(&s.input.data);
        ys.extend_from_slice(&s.target.data);
    }
    let mut xshape = vec![idx.len()];
    xshape.extend(in_shape);
    let mut yshape = vec![idx.len()];
    yshape.extend(tg_shape);
    Ok((
        DynTensor::from_host(&HostTensor::new(xshape, xs), device)?,
        DynTensor::from_host(&HostTensor::new(yshape, ys), device)?,
    ))
}

// ───────────────────────────── 손실 ─────────────────────────────

/// 장치에서 계산하는 손실 (역전파 대상).
fn device_loss<B: burn::tensor::backend::Backend>(
    out: &DynTensor<B>,
    target: &DynTensor<B>,
    loss: Loss,
) -> Result<Tensor<B, 1>> {
    match loss {
        Loss::Mse => {
            check_same_shape(out, target, "MSE")?;
            Ok(out.clone().try_sub(target.clone())?.square().mean_all())
        }
        Loss::Mae => {
            check_same_shape(out, target, "MAE")?;
            Ok(out.clone().try_sub(target.clone())?.abs().mean_all())
        }
        Loss::BceWithLogits => {
            check_same_shape(out, target, "BCE")?;
            // max(x,0) - x·z + ln(1 + e^-|x|) — 수치적으로 안정한 형태.
            let x = out.clone();
            let a = x.clone().clamp_min(0.0);
            let b = x.clone().try_mul(target.clone())?;
            let c = x.abs().negate().exp().log1p();
            Ok(a.try_sub(b)?.try_add(c)?.mean_all())
        }
        Loss::CrossEntropy => {
            let logits = out
                .clone()
                .into_r2()
                .context("CrossEntropy 는 [B, C] 출력이 필요합니다")?;
            let classes = logits.dims()[1];
            // 클래스가 하나면 log_softmax 가 항상 0 이라 손실도 그래디언트도 0 이다.
            // 학습이 아무 일도 하지 않고 "성공" 으로 끝나므로 여기서 막는다.
            if classes < 2 {
                bail!(
                    "CrossEntropy 는 출력 클래스가 2 개 이상이어야 합니다 (지금 {classes}). \
                     이진 분류라면 BCE (logits) 를 쓰거나 Output 을 2 유닛으로 하십시오"
                );
            }
            let logp = activation::log_softmax(logits, 1);
            let td = target.dims();
            if td.len() != 2 {
                bail!("CrossEntropy 타깃은 [B, 1] 또는 [B, C] 여야 합니다 (지금 {td:?})");
            }
            // 폭 1 을 먼저 본다 — 클래스 수가 1 일 때의 모호함을 없앤다(위에서 이미 막았지만 의도를 분명히).
            if td[1] == 1 {
                let idx = target.clone().into_r2()?.int();
                Ok(logp.gather(1, idx).mean().neg())
            } else if td[1] == classes {
                let t2 = target.clone().into_r2()?;
                Ok((logp * t2).sum_dim(1).mean().neg())
            } else {
                bail!(
                    "CrossEntropy 타깃 마지막 차원 {} 이 클래스 수 {} 도 1 도 아닙니다",
                    td[1],
                    classes
                )
            }
        }
    }
}

fn check_same_shape<B: burn::tensor::backend::Backend>(a: &DynTensor<B>, b: &DynTensor<B>, what: &str) -> Result<()> {
    if a.dims() != b.dims() {
        bail!(
            "{what} 는 출력과 타깃 형상이 같아야 합니다: {:?} vs {:?}",
            a.dims(),
            b.dims()
        );
    }
    Ok(())
}

/// 검증: 손실과 지표를 호스트에서 계산한다 (autodiff 그래프를 남기지 않는다).
#[allow(clippy::too_many_arguments)]
fn evaluate<B: AutodiffBackend>(
    model: &mut Model<B>,
    set: &[Sample],
    resident: Option<&DeviceSet<B>>,
    in_shapes: &[Vec<usize>],
    batch_size: usize,
    device: &B::Device,
    loss: Loss,
    metric: Metric,
) -> Result<(f64, Option<f64>)> {
    let mut loss_sum = 0.0f64;
    let mut hit = 0.0f64;
    let mut n = 0usize;
    let order: Vec<usize> = (0..set.len()).collect();
    for (step, chunk) in order.chunks(batch_size.max(1)).enumerate() {
        let (x, y) = match resident {
            Some(d) => d.batch(step * batch_size.max(1), chunk.len())?,
            None => stack_generic::<B>(set, chunk, device)?,
        };
        // 손실 정의는 학습과 **같은 함수** 하나뿐이다. 검증만 따로 구현하면 한쪽에만 검사가 들어가
        // 서로 다르게 동작한다(실제로 B2 가 그렇게 생겼다). `detach` 로 autodiff 그래프는 남기지 않는다.
        let out = forward_first::<B>(model, x, in_shapes, false)?.detach();
        let l = device_loss::<B>(&out, &y, loss)?.into_scalar().elem::<f64>();
        loss_sum += l * chunk.len() as f64;
        if metric != Metric::None {
            hit += host_metric(&out.to_host(), &y.to_host(), metric)? * chunk.len() as f64;
        }
        n += chunk.len();
    }
    if n == 0 {
        return Ok((0.0, None));
    }
    let m = if metric == Metric::None {
        None
    } else {
        Some(hit / n as f64)
    };
    Ok((loss_sum / n as f64, m))
}

fn host_metric(out: &HostTensor, target: &HostTensor, metric: Metric) -> Result<f64> {
    match metric {
        Metric::None => Ok(0.0),
        Metric::Mae => {
            if out.data.len() != target.data.len() {
                bail!("MAE 지표는 출력과 타깃 원소 수가 같아야 합니다");
            }
            let s: f64 = out
                .data
                .iter()
                .zip(&target.data)
                .map(|(o, t)| (*o as f64 - *t as f64).abs())
                .sum();
            Ok(s / out.data.len() as f64)
        }
        Metric::Accuracy => {
            let (b, c) = rows_cols(out)?;
            let tc = target.data.len() / b.max(1);
            let mut hit = 0usize;
            for i in 0..b {
                // 출력이 한 유닛이면 argmax 가 언제나 0 이라 "라벨 0 비율"이 나온다.
                // 이진 분류(BCE)의 표준 구성이므로 임계 판정으로 처리한다.
                let (pred, truth) = if c == 1 {
                    ((out.data[i] >= 0.0) as usize, (target.data[i] >= 0.5) as usize)
                } else {
                    let t = if tc == 1 {
                        target.data[i] as usize
                    } else {
                        argmax(&target.data[i * tc..(i + 1) * tc])
                    };
                    (argmax(&out.data[i * c..(i + 1) * c]), t)
                };
                if pred == truth {
                    hit += 1;
                }
            }
            Ok(hit as f64 / b as f64)
        }
    }
}

fn rows_cols(t: &HostTensor) -> Result<(usize, usize)> {
    if t.shape.len() < 2 {
        bail!("[B, C] 형상이 필요합니다 (지금 {:?})", t.shape);
    }
    let b = t.shape[0];
    let c: usize = t.shape[1..].iter().product();
    Ok((b, c))
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .fold(
            (0usize, f32::NEG_INFINITY),
            |m, (i, &x)| if x > m.1 { (i, x) } else { m },
        )
        .0
}

// ───────────────────────────── 옵티마이저 ─────────────────────────────

struct OptState<B: AutodiffBackend> {
    /// SGD 모멘텀 버퍼 또는 Adam 1차 모멘트.
    m: DynTensor<B::InnerBackend>,
    /// Adam 2차 모멘트.
    v: Option<DynTensor<B::InnerBackend>>,
}

/// 한 스텝의 결과. 그래디언트 노름이 유한하지 않으면 파라미터를 건드리지 않고 건너뛴다.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StepOutcome {
    Applied,
    Skipped,
}

/// 파라미터 텐서를 직접 갱신하는 옵티마이저. `Gradients` 에서 텐서별 grad 를 꺼내 쓴다.
struct Opt<B: AutodiffBackend> {
    kind: Optimizer,
    state: BTreeMap<String, OptState<B>>,
    t: u64,
}

impl<B: AutodiffBackend> Opt<B> {
    fn new(kind: Optimizer) -> Self {
        Self {
            kind,
            state: BTreeMap::new(),
            t: 0,
        }
    }

    /// 스케줄이 정한 학습률로 바꾼다 (모멘트 상태는 그대로).
    fn set_lr(&mut self, lr: f64) {
        self.kind.set_lr(lr);
    }

    fn step(&mut self, model: &mut Model<B>, mut grads: B::Gradients, grad_clip: f64) -> Result<StepOutcome> {
        self.t += 1;

        // 1) 그래디언트 수집.
        let mut collected: Vec<(String, DynTensor<B::InnerBackend>)> = Vec::new();
        for name in model.trainable_names() {
            let Some(p) = model.param(&name) else { continue };
            if let Some(g) = p.grad_remove(&mut grads) {
                collected.push((name, g));
            }
        }
        if collected.is_empty() {
            bail!("그래디언트를 얻지 못했습니다 — 학습 대상 파라미터가 손실과 연결되어 있는지 확인하세요");
        }

        // 2) 전역 노름 클리핑.
        if grad_clip > 0.0 {
            // 파라미터마다 스칼라를 읽으면 동기화가 파라미터 수만큼 걸린다.
            // 제곱합을 장치 텐서로 더한 뒤 **한 번만** 읽는다.
            let mut acc: Option<Tensor<B::InnerBackend, 1>> = None;
            for (_, g) in &collected {
                let s = g.sum_squares_tensor();
                acc = Some(match acc {
                    Some(a) => a + s,
                    None => s,
                });
            }
            let total: f64 = acc.map_or(0.0, |a| a.into_scalar().elem::<f64>()).sqrt();
            if !total.is_finite() {
                // 클리핑이 있는 이유가 바로 이 경우다. 그대로 넣으면 파라미터가 NaN 으로 오염된다.
                return Ok(StepOutcome::Skipped);
            }
            if total > grad_clip {
                let scale = grad_clip / total;
                for (_, g) in collected.iter_mut() {
                    *g = g.clone().mul_scalar(scale);
                }
            }
        }

        // 3) 파라미터 갱신.
        for (name, g) in collected {
            let p = model.param(&name).context("파라미터가 사라졌습니다")?.clone().inner();
            let updated = self.update(&name, p, g)?;
            model.set_param(&name, DynTensor::from_inner(updated).require_grad());
        }
        Ok(StepOutcome::Applied)
    }

    fn update(
        &mut self,
        name: &str,
        p: DynTensor<B::InnerBackend>,
        g: DynTensor<B::InnerBackend>,
    ) -> Result<DynTensor<B::InnerBackend>> {
        match self.kind {
            Optimizer::Sgd { lr, momentum } => {
                if momentum <= 0.0 {
                    return p.try_sub(g.mul_scalar(lr));
                }
                let st = self.state.entry(name.to_string()).or_insert_with(|| OptState {
                    m: g.zeros_like(),
                    v: None,
                });
                let buf = st.m.clone().mul_scalar(momentum).try_add(g)?;
                st.m = buf.clone();
                p.try_sub(buf.mul_scalar(lr))
            }
            Optimizer::Adam { lr, beta1, beta2, eps } => self.adam(name, p, g, lr, beta1, beta2, eps, 0.0),
            Optimizer::AdamW {
                lr,
                beta1,
                beta2,
                eps,
                weight_decay,
            } => self.adam(name, p, g, lr, beta1, beta2, eps, weight_decay),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn adam(
        &mut self,
        name: &str,
        p: DynTensor<B::InnerBackend>,
        g: DynTensor<B::InnerBackend>,
        lr: f64,
        beta1: f64,
        beta2: f64,
        eps: f64,
        weight_decay: f64,
    ) -> Result<DynTensor<B::InnerBackend>> {
        let st = self.state.entry(name.to_string()).or_insert_with(|| OptState {
            m: g.zeros_like(),
            v: Some(g.zeros_like()),
        });
        if st.v.is_none() {
            st.v = Some(g.zeros_like());
        }

        let m =
            st.m.clone()
                .mul_scalar(beta1)
                .try_add(g.clone().mul_scalar(1.0 - beta1))?;
        let v =
            st.v.clone()
                .expect("바로 위에서 채웠다")
                .mul_scalar(beta2)
                .try_add(g.square().mul_scalar(1.0 - beta2))?;
        st.m = m.clone();
        st.v = Some(v.clone());

        let bc1 = 1.0 - beta1.powi(self.t as i32);
        let bc2 = 1.0 - beta2.powi(self.t as i32);
        let m_hat = m.mul_scalar(1.0 / bc1);
        let v_hat = v.mul_scalar(1.0 / bc2);

        // AdamW: 가중치 감쇠를 그래디언트가 아니라 파라미터에 직접 (decoupled).
        let p = if weight_decay > 0.0 {
            p.mul_scalar(1.0 - lr * weight_decay)
        } else {
            p
        };
        let step = m_hat.try_div(v_hat.sqrt().add_scalar(eps))?.mul_scalar(lr);
        p.try_sub(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::CpuB;
    use burn::backend::ndarray::NdArrayDevice;

    fn t(shape: &[usize], data: Vec<f32>) -> DynTensor<CpuB> {
        DynTensor::from_host(&HostTensor::new(shape.to_vec(), data), &NdArrayDevice::Cpu).unwrap()
    }

    #[test]
    fn split_inputs_cuts_in_node_order() {
        // 배치 2, 샘플당 5 원소 → Input 형상 [2] 와 [1, 3] 으로 나뉜다.
        let x = t(&[2, 5], vec![1., 2., 3., 4., 5., 6., 7., 8., 9., 10.]);
        let shapes = vec![vec![2], vec![1, 3]];
        let parts = split_inputs::<CpuB>(x, &shapes).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].dims(), vec![2, 2]);
        assert_eq!(parts[1].dims(), vec![2, 1, 3]);
        assert_eq!(parts[0].to_host().data, vec![1., 2., 6., 7.]);
        assert_eq!(parts[1].to_host().data, vec![3., 4., 5., 8., 9., 10.]);
    }

    #[test]
    fn split_inputs_is_identity_for_a_single_input() {
        let x = t(&[2, 3], vec![1., 2., 3., 4., 5., 6.]);
        let parts = split_inputs::<CpuB>(x, &[vec![3]]).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].dims(), vec![2, 3]);
    }

    #[test]
    fn dataset_input_rules() {
        // Input 하나면 형상이 정확히 같아야 한다.
        assert!(check_dataset_inputs(&[1, 8, 8], &[vec![1, 8, 8]]).is_ok());
        assert!(
            check_dataset_inputs(&[64], &[vec![1, 8, 8]]).is_err(),
            "원소 수만 같아도 거절해야 한다"
        );
        // 여럿이면 원소 수 합이 맞으면 된다 (형상은 자유).
        assert!(check_dataset_inputs(&[5], &[vec![2], vec![3]]).is_ok());
        assert!(
            check_dataset_inputs(&[2, 3], &[vec![2], vec![4]]).is_ok(),
            "6 = 2 + 4 이므로 통과"
        );
        assert!(check_dataset_inputs(&[2, 3], &[vec![2], vec![5]]).is_err(), "6 ≠ 2 + 5");
        let e = check_dataset_inputs(&[4], &[vec![2], vec![3]]).unwrap_err().to_string();
        assert!(e.contains("[2, 3]"), "오류에 노드별 원소 수가 없습니다: {e}");
    }

    fn cfg_with(schedule: LrSchedule, lr: f64, epochs: usize, warmup: usize) -> TrainConfig {
        TrainConfig {
            optimizer: Optimizer::Sgd { lr, momentum: 0.0 },
            epochs,
            schedule,
            warmup_steps: warmup,
            ..TrainConfig::default()
        }
    }

    #[test]
    fn step_schedule_halves_every_n_epochs() {
        let c = LrController::new(&cfg_with(LrSchedule::Step { every: 2, gamma: 0.5 }, 1.0, 6, 0));
        let got: Vec<f64> = (1..=6).map(|e| c.epoch_lr(e)).collect();
        assert_eq!(got, vec![1.0, 1.0, 0.5, 0.5, 0.25, 0.25]);
    }

    #[test]
    fn cosine_schedule_goes_from_base_to_min() {
        let c = LrController::new(&cfg_with(LrSchedule::Cosine { min_lr: 0.1 }, 1.0, 5, 0));
        assert!((c.epoch_lr(1) - 1.0).abs() < 1e-12);
        assert!((c.epoch_lr(5) - 0.1).abs() < 1e-12);
        let mid = c.epoch_lr(3);
        assert!(mid < 1.0 && mid > 0.1, "중간값이 범위 밖: {mid}");
    }

    #[test]
    fn plateau_schedule_decays_after_patience() {
        let mut c = LrController::new(&cfg_with(
            LrSchedule::Plateau {
                patience: 2,
                factor: 0.5,
            },
            1.0,
            10,
            0,
        ));
        assert_eq!(c.epoch_lr(1), 1.0);
        c.on_epoch_end(Some(1.0)); // 첫 기록
        c.on_epoch_end(Some(1.0)); // 정체 1
        assert_eq!(c.epoch_lr(3), 1.0);
        c.on_epoch_end(Some(1.0)); // 정체 2 → 감쇠
        assert_eq!(c.epoch_lr(4), 0.5);
        c.on_epoch_end(Some(0.1)); // 개선 → 대기 초기화
        assert_eq!(c.epoch_lr(5), 0.5);
    }

    #[test]
    fn warmup_ramps_the_first_steps_only() {
        let c = LrController::new(&cfg_with(LrSchedule::None, 1.0, 10, 4));
        assert_eq!(c.step_lr(1.0, 0), 0.25);
        assert_eq!(c.step_lr(1.0, 1), 0.5);
        assert_eq!(c.step_lr(1.0, 3), 1.0);
        assert_eq!(c.step_lr(1.0, 4), 1.0);
        // 워밍업을 끄면 항상 그대로.
        let off = LrController::new(&cfg_with(LrSchedule::None, 1.0, 10, 0));
        assert_eq!(off.step_lr(0.3, 0), 0.3);
    }

    #[test]
    fn device_set_batches_match_host_stacking() {
        let set: Vec<Sample> = (0..6)
            .map(|i| Sample {
                input: HostTensor::new(vec![2], vec![i as f32, i as f32 + 0.5]),
                target: HostTensor::new(vec![1], vec![(i % 2) as f32]),
            })
            .collect();
        let dev = NdArrayDevice::Cpu;
        let resident = DeviceSet::<CpuB>::upload(&set, &dev).unwrap();

        // 원래 순서와 뒤섞은 순서 모두 호스트 경로와 같은 배치를 내야 한다.
        for order in [vec![0, 1, 2, 3, 4, 5], vec![4, 1, 5, 0, 3, 2]] {
            let epoch = resident.permuted(&order, &dev).unwrap();
            for start in [0usize, 2, 4] {
                let (rx, ry) = epoch.batch(start, 2).unwrap();
                let (hx, hy) = stack_generic::<CpuB>(&set, &order[start..start + 2], &dev).unwrap();
                assert_eq!(rx.to_host(), hx.to_host(), "order {order:?} start {start}");
                assert_eq!(ry.to_host(), hy.to_host());
            }
        }
    }

    #[test]
    fn dataset_bytes_counts_inputs_and_targets() {
        let set: Vec<Sample> = (0..10)
            .map(|_| Sample {
                input: HostTensor::new(vec![3], vec![0.0; 3]),
                target: HostTensor::new(vec![1], vec![0.0]),
            })
            .collect();
        assert_eq!(dataset_bytes(&set), (3 + 1) * 4 * 10);
        assert_eq!(dataset_bytes(&[]), 0);
    }
}
