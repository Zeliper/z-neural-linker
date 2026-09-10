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
use burn::tensor::{activation, ElementConversion, Tensor};
use crossbeam_channel::{Receiver, Sender};
use nl_core::dataset::Split;
use nl_core::{DatasetSpec, EpochMetrics, Loss, Metric, ModelDef, Optimizer, RunId, RunRecord, RunStatus};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
            error: None,
            note: String::new(),
        };
        // 백엔드(특히 GPU 드라이버)가 패닉하면 채널이 조용히 끊긴다. 그러면 UI 가 영원히 기다리므로
        // 패닉을 잡아 `Failed` 이벤트로 바꾼다. 다른 스레드에서 나는 패닉까지 잡을 수는 없다.
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_training(&req, &tx, &ctl, &mut run)
        })) {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("학습 스레드가 패닉했습니다: {}", panic_message(e.as_ref()))),
        };
        run.finished = Some(chrono::Utc::now());
        match result {
            Ok(()) => {
                if run.status == RunStatus::Running {
                    run.status = RunStatus::Finished;
                }
                // run.json 은 최종 상태까지 담아 다시 쓴다 (실패해도 이벤트는 보낸다).
                if let Err(e) = write_run_json(&req.run_dir, &run) {
                    let _ = tx.send(TrainEvent::Log(format!("run.json 쓰기 실패: {e:#}")));
                }
                let _ = tx.send(TrainEvent::Finished { run });
            }
            Err(e) => {
                run.status = RunStatus::Failed;
                let msg = format!("{e:#}");
                run.error = Some(msg.clone());
                let _ = write_run_json(&req.run_dir, &run);
                let _ = tx.send(TrainEvent::Failed { run, error: msg });
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
    std::fs::create_dir_all(run_dir).with_context(|| format!("폴더 생성 실패: {}", run_dir.display()))?;
    let json = serde_json::to_string_pretty(run).context("RunRecord 직렬화 실패")?;
    let tmp = run_dir.join("run.json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("run.json 쓰기 실패: {}", tmp.display()))?;
    std::fs::rename(&tmp, run_dir.join("run.json")).context("run.json 이동 실패")?;
    Ok(())
}

fn run_training(req: &TrainRequest, tx: &Sender<TrainEvent>, ctl: &TrainControl, run: &mut RunRecord) -> Result<()> {
    let (info, handle) = device::resolve_entry(req.model.train.device);
    run.device_name = info.name.clone();
    dispatch_autodiff!(handle, train_on, req, tx, ctl, run, &info.name)
}

// ───────────────────────────── 학습 본체 ─────────────────────────────

fn train_on<B: AutodiffBackend>(
    device: &B::Device,
    req: &TrainRequest,
    tx: &Sender<TrainEvent>,
    ctl: &TrainControl,
    run: &mut RunRecord,
    device_name: &str,
) -> Result<()> {
    let cfg = req.model.train.clone();
    let mut model = Model::<B>::new(&req.model, device, cfg.seed)?;

    if model.input_nodes().len() != 1 {
        bail!("학습은 Input 레이어가 하나인 모델만 지원합니다 (지금 {} 개)", model.input_nodes().len());
    }
    if model.output_nodes().len() != 1 {
        bail!("학습은 Output 레이어가 하나인 모델만 지원합니다 (지금 {} 개)", model.output_nodes().len());
    }
    let in_shape = model.input_sample_shapes().first().cloned().unwrap_or_default();

    // 이어서 학습.
    if let Some(p) = &req.resume_from {
        let loaded = weights::load(p)?;
        model.load_host_params(&loaded).with_context(|| format!("체크포인트 적용 실패: {}", p.display()))?;
        let _ = tx.send(TrainEvent::Log(format!("체크포인트에서 이어서 학습: {}", p.display())));
    }
    model.require_grad_all();

    // 데이터.
    let (mut train_set, info) = data::load_all(&req.dataset, &req.base_dir, Some(&in_shape))?;
    if train_set.is_empty() {
        bail!("데이터셋이 비어 있습니다");
    }
    if info.input_shape != in_shape {
        bail!(
            "데이터 입력 형상 {:?} 이 모델 Input 형상 {:?} 과 다릅니다",
            info.input_shape,
            in_shape
        );
    }

    let mut rng = ChaCha8Rng::seed_from_u64(cfg.seed);
    if req.dataset.shuffle {
        train_set.shuffle(&mut rng);
    }

    let mut val_set: Vec<Sample> = Vec::new();
    match &req.dataset.split {
        Split::Separate { validation } => {
            let (v, _) = data::load_source(validation, &req.base_dir, Some(&in_shape), None)?;
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

    let batch_size = cfg.batch_size.max(1);
    let batches = train_set.len().div_ceil(batch_size);
    let _ = tx.send(TrainEvent::Started {
        device: device_name.to_string(),
        batches_per_epoch: batches,
        params: model.trainable_count(),
    });

    let mut opt = Opt::<B>::new(cfg.optimizer);
    let mut stopped = false;

    'epochs: for epoch in 1..=cfg.epochs.max(1) {
        let t0 = std::time::Instant::now();
        let mut order: Vec<usize> = (0..train_set.len()).collect();
        if req.dataset.shuffle {
            let mut ep_rng = ChaCha8Rng::seed_from_u64(cfg.seed.wrapping_add(epoch as u64));
            order.shuffle(&mut ep_rng);
        }

        let mut epoch_loss = 0.0f64;
        let mut seen = 0usize;
        for (step, chunk) in order.chunks(batch_size).enumerate() {
            if ctl.wait_if_paused() || ctl.should_stop() {
                stopped = true;
                break 'epochs;
            }
            let (x, y) = stack::<B>(&train_set, chunk, device)?;
            let out = model.forward(vec![x], true)?.pop().context("출력이 없습니다")?;
            let loss = device_loss::<B>(&out, &y, cfg.loss)?;
            let value: f64 = loss.clone().into_scalar().elem::<f64>();
            if !value.is_finite() {
                bail!("손실이 발산했습니다 (epoch {epoch}, step {step}) — 학습률을 낮춰 보세요");
            }

            let grads = loss.backward();
            opt.step(&mut model, grads, cfg.grad_clip)?;

            epoch_loss += value * chunk.len() as f64;
            seen += chunk.len();
            let _ = tx.send(TrainEvent::Step { epoch, step, loss: value });
        }

        let train_loss = if seen > 0 { epoch_loss / seen as f64 } else { 0.0 };
        let (val_loss, val_metric) = if val_set.is_empty() {
            (None, None)
        } else {
            let (l, m) = evaluate::<B>(&mut model, &val_set, batch_size, device, cfg.loss, cfg.metric)?;
            (Some(l), m)
        };
        let em = EpochMetrics { epoch, train_loss, val_loss, val_metric, seconds: t0.elapsed().as_secs_f64() };
        run.epochs.push(em);
        let _ = tx.send(TrainEvent::Epoch(em));

        if cfg.checkpoint_every > 0 && epoch % cfg.checkpoint_every == 0 {
            let path = req.run_dir.join(format!("epoch-{epoch:04}.safetensors"));
            weights::save(&path, req.model.id, &model.host_params())?;
            let _ = tx.send(TrainEvent::Checkpoint { path });
        }
    }

    // 중지되었어도 마지막 가중치는 남긴다.
    let final_path = req.run_dir.join("final.safetensors");
    weights::save(&final_path, req.model.id, &model.host_params())?;
    let _ = tx.send(TrainEvent::Checkpoint { path: final_path.clone() });
    run.checkpoint = Some(relative_to(&req.base_dir, &final_path));
    run.status = if stopped { RunStatus::Stopped } else { RunStatus::Finished };
    Ok(())
}

/// `base_dir` 기준 상대 경로. 만들 수 없으면 절대 경로 문자열.
fn relative_to(base_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(base_dir) {
        Ok(p) => p.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// 샘플 묶음을 장치 텐서 한 쌍으로 (배치 차원 포함).
fn stack<B: AutodiffBackend>(
    set: &[Sample],
    idx: &[usize],
    device: &B::Device,
) -> Result<(DynTensor<B>, DynTensor<B>)> {
    stack_generic::<B>(set, idx, device)
}

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
            bail!("샘플 형상이 서로 다릅니다: {:?}/{:?} vs {:?}/{:?}", in_shape, tg_shape, s.input.shape, s.target.shape);
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
            let logits = out.clone().into_r2().context("CrossEntropy 는 [B, C] 출력이 필요합니다")?;
            let classes = logits.dims()[1];
            let logp = activation::log_softmax(logits, 1);
            let td = target.dims();
            if td.len() != 2 {
                bail!("CrossEntropy 타깃은 [B, 1] 또는 [B, C] 여야 합니다 (지금 {td:?})");
            }
            if td[1] == classes {
                // one-hot
                let t2 = target.clone().into_r2()?;
                Ok((logp * t2).sum_dim(1).mean().neg())
            } else if td[1] == 1 {
                // 클래스 인덱스
                let idx = target.clone().into_r2()?.int();
                Ok(logp.gather(1, idx).mean().neg())
            } else {
                bail!("CrossEntropy 타깃 마지막 차원 {} 이 클래스 수 {} 도 1 도 아닙니다", td[1], classes)
            }
        }
    }
}

fn check_same_shape<B: burn::tensor::backend::Backend>(
    a: &DynTensor<B>,
    b: &DynTensor<B>,
    what: &str,
) -> Result<()> {
    if a.dims() != b.dims() {
        bail!("{what} 는 출력과 타깃 형상이 같아야 합니다: {:?} vs {:?}", a.dims(), b.dims());
    }
    Ok(())
}

/// 검증: 손실과 지표를 호스트에서 계산한다 (autodiff 그래프를 남기지 않는다).
fn evaluate<B: AutodiffBackend>(
    model: &mut Model<B>,
    set: &[Sample],
    batch_size: usize,
    device: &B::Device,
    loss: Loss,
    metric: Metric,
) -> Result<(f64, Option<f64>)> {
    let mut loss_sum = 0.0f64;
    let mut hit = 0.0f64;
    let mut n = 0usize;
    let idx: Vec<usize> = (0..set.len()).collect();
    for chunk in idx.chunks(batch_size.max(1)) {
        let (x, y) = stack::<B>(set, chunk, device)?;
        let out = model.forward(vec![x], false)?.pop().context("출력이 없습니다")?;
        let oh = out.to_host();
        let th = y.to_host();
        loss_sum += host_loss(&oh, &th, loss)? * chunk.len() as f64;
        if metric != Metric::None {
            hit += host_metric(&oh, &th, metric)? * chunk.len() as f64;
        }
        n += chunk.len();
    }
    if n == 0 {
        return Ok((0.0, None));
    }
    let m = if metric == Metric::None { None } else { Some(hit / n as f64) };
    Ok((loss_sum / n as f64, m))
}

/// 호스트 손실 — 장치 손실과 같은 정의. 검증에만 쓴다.
pub(crate) fn host_loss(out: &HostTensor, target: &HostTensor, loss: Loss) -> Result<f64> {
    match loss {
        Loss::Mse | Loss::Mae | Loss::BceWithLogits => {
            if out.data.len() != target.data.len() {
                bail!("출력 {} 개와 타깃 {} 개의 수가 다릅니다", out.data.len(), target.data.len());
            }
            let mut sum = 0.0f64;
            for (o, t) in out.data.iter().zip(&target.data) {
                let (o, t) = (*o as f64, *t as f64);
                sum += match loss {
                    Loss::Mse => (o - t) * (o - t),
                    Loss::Mae => (o - t).abs(),
                    _ => o.max(0.0) - o * t + (1.0 + (-o.abs()).exp()).ln(),
                };
            }
            Ok(sum / out.data.len() as f64)
        }
        Loss::CrossEntropy => {
            let (b, c) = rows_cols(out)?;
            let tc = target.data.len() / b.max(1);
            let mut sum = 0.0f64;
            for i in 0..b {
                let row = &out.data[i * c..(i + 1) * c];
                let logp = log_softmax_row(row);
                if tc == c {
                    let row_t = &target.data[i * c..(i + 1) * c];
                    sum -= row_t.iter().zip(&logp).map(|(t, lp)| *t as f64 * lp).sum::<f64>();
                } else if tc == 1 {
                    let k = target.data[i] as usize;
                    if k >= c {
                        bail!("클래스 인덱스 {k} 가 클래스 수 {c} 를 넘습니다");
                    }
                    sum -= logp[k];
                } else {
                    bail!("CrossEntropy 타깃 폭 {tc} 이 클래스 수 {c} 도 1 도 아닙니다");
                }
            }
            Ok(sum / b as f64)
        }
    }
}

fn host_metric(out: &HostTensor, target: &HostTensor, metric: Metric) -> Result<f64> {
    match metric {
        Metric::None => Ok(0.0),
        Metric::Mae => {
            if out.data.len() != target.data.len() {
                bail!("MAE 지표는 출력과 타깃 원소 수가 같아야 합니다");
            }
            let s: f64 = out.data.iter().zip(&target.data).map(|(o, t)| (*o as f64 - *t as f64).abs()).sum();
            Ok(s / out.data.len() as f64)
        }
        Metric::Accuracy => {
            let (b, c) = rows_cols(out)?;
            let tc = target.data.len() / b.max(1);
            let mut hit = 0usize;
            for i in 0..b {
                let pred = argmax(&out.data[i * c..(i + 1) * c]);
                let truth = if tc == 1 {
                    target.data[i] as usize
                } else {
                    argmax(&target.data[i * tc..(i + 1) * tc])
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
    v.iter().enumerate().fold((0usize, f32::NEG_INFINITY), |m, (i, &x)| if x > m.1 { (i, x) } else { m }).0
}

fn log_softmax_row(row: &[f32]) -> Vec<f64> {
    let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max) as f64;
    let sum: f64 = row.iter().map(|v| (*v as f64 - max).exp()).sum();
    let lse = max + sum.ln();
    row.iter().map(|v| *v as f64 - lse).collect()
}

// ───────────────────────────── 옵티마이저 ─────────────────────────────

struct OptState<B: AutodiffBackend> {
    /// SGD 모멘텀 버퍼 또는 Adam 1차 모멘트.
    m: DynTensor<B::InnerBackend>,
    /// Adam 2차 모멘트.
    v: Option<DynTensor<B::InnerBackend>>,
}

/// 파라미터 텐서를 직접 갱신하는 옵티마이저. `Gradients` 에서 텐서별 grad 를 꺼내 쓴다.
struct Opt<B: AutodiffBackend> {
    kind: Optimizer,
    state: BTreeMap<String, OptState<B>>,
    t: u64,
}

impl<B: AutodiffBackend> Opt<B> {
    fn new(kind: Optimizer) -> Self {
        Self { kind, state: BTreeMap::new(), t: 0 }
    }

    fn step(&mut self, model: &mut Model<B>, mut grads: B::Gradients, grad_clip: f64) -> Result<()> {
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
            let total: f64 = collected.iter().map(|(_, g)| g.sum_squares()).sum::<f64>().sqrt();
            if total.is_finite() && total > grad_clip {
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
        Ok(())
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
                let st = self
                    .state
                    .entry(name.to_string())
                    .or_insert_with(|| OptState { m: g.zeros_like(), v: None });
                let buf = st.m.clone().mul_scalar(momentum).try_add(g)?;
                st.m = buf.clone();
                p.try_sub(buf.mul_scalar(lr))
            }
            Optimizer::Adam { lr, beta1, beta2, eps } => self.adam(name, p, g, lr, beta1, beta2, eps, 0.0),
            Optimizer::AdamW { lr, beta1, beta2, eps, weight_decay } => {
                self.adam(name, p, g, lr, beta1, beta2, eps, weight_decay)
            }
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
        let st = self
            .state
            .entry(name.to_string())
            .or_insert_with(|| OptState { m: g.zeros_like(), v: Some(g.zeros_like()) });
        if st.v.is_none() {
            st.v = Some(g.zeros_like());
        }

        let m = st.m.clone().mul_scalar(beta1).try_add(g.clone().mul_scalar(1.0 - beta1))?;
        let v = st
            .v
            .clone()
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
        let p = if weight_decay > 0.0 { p.mul_scalar(1.0 - lr * weight_decay) } else { p };
        let step = m_hat.try_div(v_hat.sqrt().add_scalar(eps))?.mul_scalar(lr);
        p.try_sub(step)
    }
}
