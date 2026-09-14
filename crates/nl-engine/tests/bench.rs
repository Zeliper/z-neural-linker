//! 학습 말고 다른 곳의 성능 기준값. 학습 벤치는 `tests/engine.rs` 의 `bench_*` 에 있다.
//!
//! 전부 `NL_BENCH=1` 일 때만 돈다. `scripts/bench.sh` 가 릴리스 빌드로 여러 번 돌려
//! 중앙값을 모으고 직전 결과와 비교한다.

use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::model::{Act, LayerKind, ModelDef, Node, Port};
use nl_core::{DatasetSpec, DevicePref, Loss, Optimizer, RunId, RunStatus};
use nl_engine::onnx::{self, ExportOptions};
use nl_engine::{TrainEvent, TrainRequest};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

// ───────────────────────────── 도우미 ─────────────────────────────

fn enabled() -> bool {
    if std::env::var("NL_BENCH").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("NL_BENCH=1 이 아니어서 건너뜁니다");
    false
}

/// 벤치 결과를 **기계가 읽는 한 줄**로. `scripts/bench.sh` 가 이 줄만 골라 모은다.
fn bench_json(name: &str, unit: &str, value: f64) {
    println!("BENCHJSON {{\"name\":\"{name}\",\"unit\":\"{unit}\",\"value\":{value:.4}}}");
}

/// 값 여러 개의 중앙값. 한 프로세스 안에서 반복할 수 있는 항목에 쓴다.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("NaN"));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("nl-bench-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&p).expect("임시 폴더");
    p
}

fn chain(kinds: Vec<LayerKind>) -> ModelDef {
    let mut def = ModelDef::new("b");
    let g = &mut def.graph;
    let mut prev = None;
    for k in kinds {
        let id = g.add_node(Node::new(k, [0.0, 0.0]));
        if let Some(p) = prev {
            g.add_edge(p, Port::new(id, 0)).expect("엣지");
        }
        prev = Some(id);
    }
    let out = g.add_node(Node::new(LayerKind::Output, [0.0, 0.0]));
    g.add_edge(prev.expect("빈 그래프"), Port::new(out, 0)).expect("엣지");
    def
}

fn linear(out_features: usize) -> LayerKind {
    LayerKind::Linear {
        out_features,
        bias: true,
    }
}

/// 짧게 학습해 체크포인트를 만든다. 내보내기 시간을 재려면 진짜 가중치가 있어야 한다.
fn train_briefly(mut def: ModelDef, kind: SyntheticKind, dir: &Path) -> (ModelDef, PathBuf) {
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 1;
    def.train.batch_size = 64;
    def.train.val_split = 0.0;
    def.train.device = DevicePref::Cpu;
    def.train.seed = 7;

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def.clone(),
        dataset: DatasetSpec::new("syn", DataSource::Synthetic { kind, samples: 256 }),
        base_dir: dir.to_path_buf(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let handle = nl_engine::start(req).expect("학습 시작");
    let mut record = None;
    while let Ok(ev) = handle.events.recv() {
        match ev {
            TrainEvent::Finished { run } => {
                record = Some(run);
                break;
            }
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }
    let run = record.expect("학습 기록");
    assert_eq!(run.status, RunStatus::Finished);
    let ckpt = run.checkpoint.clone().expect("체크포인트");
    (def, dir.join(ckpt))
}

// ───────────────────────────── ONNX 내보내기 ─────────────────────────────

/// 내보내기는 사용자가 버튼을 누르고 기다리는 시간이다. 회귀하면 바로 체감된다.
///
/// 학습은 재지 않는다 — 가중치를 만들어 두고 `export` 호출만 여러 번 재 중앙값을 쓴다.
#[test]
fn bench_onnx_export() {
    if !enabled() {
        return;
    }
    const ROUNDS: usize = 9;

    // (이름, 모델, 데이터셋 종류)
    let dir = temp_dir("onnx-mlp");
    let mlp = chain(vec![
        LayerKind::Input { shape: vec![2] },
        linear(64),
        LayerKind::Activation { act: Act::Relu },
        linear(64),
        LayerKind::Activation { act: Act::Relu },
        linear(2),
    ]);
    let (mlp, mlp_ckpt) = train_briefly(mlp, SyntheticKind::Xor, &dir);

    let cnn_dir = temp_dir("onnx-cnn");
    let cnn = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 16,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [1, 1],
            bias: true,
        },
        LayerKind::BatchNorm {
            eps: 1e-5,
            momentum: 0.1,
        },
        LayerKind::Activation { act: Act::Relu },
        LayerKind::MaxPool2d {
            kernel: [2, 2],
            stride: [2, 2],
        },
        LayerKind::Flatten,
        linear(4),
    ]);
    let (cnn, cnn_ckpt) = train_briefly(cnn, SyntheticKind::Quadrants, &cnn_dir);

    for (name, def, ckpt, out_dir) in [
        ("onnx_export_mlp", &mlp, &mlp_ckpt, &dir),
        ("onnx_export_cnn", &cnn, &cnn_ckpt, &cnn_dir),
    ] {
        let out = out_dir.join("m.onnx");
        let mut times = Vec::with_capacity(ROUNDS);
        let mut report = None;
        for _ in 0..ROUNDS {
            let t0 = Instant::now();
            let r = onnx::export(def, ckpt, &out, ExportOptions::default()).expect("내보내기");
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
            report = Some(r);
        }
        let r = report.expect("보고서");
        let ms = median(times);
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        println!(
            "BENCH {name}: {ms:.2} ms (노드 {}, 가중치 {}, {bytes} 바이트)",
            r.nodes, r.initializers
        );
        bench_json(name, "ms", ms);
    }

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&cnn_dir).ok();
}

// ───────────────────────────── 장치 검사 ─────────────────────────────

/// `probe` 는 **UI 가 기다리는 시간**이다. 첫 호출에서 GPU 셰이더를 컴파일하므로
/// 여기가 길어지면 앱이 멈춘 것처럼 보인다.
///
/// 결과는 프로세스 수명 동안 캐시되므로 **한 번만** 잴 수 있다. `scripts/bench.sh` 가
/// 프로세스를 여러 번 띄워 중앙값을 만든다.
#[test]
fn bench_device_probe() {
    if !enabled() {
        return;
    }

    let t0 = Instant::now();
    let cpu = nl_engine::probe(DevicePref::Cpu);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    assert!(cpu.is_ok(), "CPU 검사가 실패했다: {cpu:?}");
    println!("BENCH probe_cpu: {ms:.2} ms");
    bench_json("probe_cpu", "ms", ms);

    // GPU 는 셰이더 컴파일이 섞여 수십 초가 걸릴 수 있다. 켤 때만 잰다.
    if std::env::var("NL_TEST_GPU").as_deref() == Ok("1") {
        let t0 = Instant::now();
        let picked = nl_engine::resolve(DevicePref::Auto);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("BENCH resolve_auto_cold: {ms:.2} ms ({})", picked.info.name);
        bench_json("resolve_auto_cold", "ms", ms);
    }
}
