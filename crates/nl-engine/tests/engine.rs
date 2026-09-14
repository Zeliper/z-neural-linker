//! nl-engine 통합 테스트. 공개 API 만 쓴다.
//!
//! GPU 경로는 `NL_TEST_GPU=1` 일 때만 돈다: `NL_TEST_GPU=1 cargo test -p nl-engine gpu`.

use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::model::{Act, Graph, LayerKind, ModelDef, Node, Port};
use nl_core::shape;
use nl_core::templates;
use nl_core::{DatasetSpec, DevicePref, Loss, Metric, Optimizer, RunId, RunRecord, RunStatus};
use nl_engine::{HostTensor, Session, TrainEvent, TrainRequest};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// ───────────────────────────── 도우미 ─────────────────────────────

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("nl-engine-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&p).expect("임시 폴더 생성");
    p
}

/// 학습·추론 테스트가 쓸 장치.
///
/// 기본은 CPU 다. `NL_TEST_DEVICE=gpu` 를 주면 `Auto` 로 바꿔 **상주 배치 경로**(GPU 는 크기와
/// 무관하게 데이터셋을 장치에 올린다)를 타게 한다. 그래야 B1~B7 수정이 GPU 경로에서도 유효한지
/// 확인할 수 있다: `NL_TEST_GPU=1 NL_TEST_DEVICE=gpu cargo test -p nl-engine`.
fn test_device() -> DevicePref {
    match std::env::var("NL_TEST_DEVICE").as_deref() {
        Ok("gpu") => DevicePref::Auto,
        _ => DevicePref::Cpu,
    }
}

fn add(g: &mut Graph, k: LayerKind) -> nl_core::NodeId {
    g.add_node(Node::new(k, [0.0, 0.0]))
}

/// 이름 붙은 노드 — `input_nodes()`/`output_nodes()` 순서는 이름 순이다.
fn named(g: &mut Graph, k: LayerKind, name: &str) -> nl_core::NodeId {
    let mut n = Node::new(k, [0.0, 0.0]);
    n.name = name.to_string();
    g.add_node(n)
}

/// 학습을 끝까지 돌리고 로그까지 모아 돌려준다.
fn train_collecting_logs(def: ModelDef, ds: DatasetSpec, dir: &Path) -> (RunRecord, Vec<String>) {
    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: ds,
        base_dir: dir.to_path_buf(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let handle = nl_engine::start(req).expect("학습 스레드 시작");
    let mut logs = Vec::new();
    while let Ok(ev) = handle.events.recv() {
        match ev {
            TrainEvent::Log(m) => logs.push(m),
            TrainEvent::Finished { run } => return (run, logs),
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error} (로그: {logs:?})"),
            _ => {}
        }
    }
    panic!("Finished/Failed 없이 이벤트 채널이 끊겼습니다");
}

/// 4 열 CSV 데이터셋을 만든다: y = 3·c0 − 2·c2 + 0.5.
fn four_column_csv(dir: &Path, rows: usize) -> DatasetSpec {
    use std::fmt::Write as _;
    let mut text = String::from("a0,a1,b0,b1,y\n");
    for i in 0..rows {
        let f = |k: usize| ((i * 7 + k * 13) % 21) as f32 / 10.0 - 1.0;
        let (c0, c1, c2, c3) = (f(0), f(1), f(2), f(3));
        let y = 3.0 * c0 - 2.0 * c2 + 0.5;
        let _ = writeln!(text, "{c0},{c1},{c2},{c3},{y}");
    }
    let path = dir.join("four.csv");
    std::fs::write(&path, text).unwrap();
    DatasetSpec::new(
        "4열",
        DataSource::Csv {
            path: "four.csv".into(),
            input_cols: vec!["a0".into(), "a1".into(), "b0".into(), "b1".into()],
            target_cols: vec!["y".into()],
            header: true,
        },
    )
}

fn link(g: &mut Graph, a: nl_core::NodeId, b: nl_core::NodeId) {
    g.add_edge(a, Port::new(b, 0)).expect("엣지 추가");
}

/// 일자형 그래프. 마지막 종류가 Output 이 아니면 Output 을 붙인다.
fn chain(kinds: Vec<LayerKind>) -> ModelDef {
    let mut def = ModelDef::new("t");
    let g = &mut def.graph;
    let mut prev = None;
    for k in kinds {
        let id = add(g, k);
        if let Some(p) = prev {
            link(g, p, id);
        }
        prev = Some(id);
    }
    let out = add(g, LayerKind::Output);
    link(g, prev.expect("빈 그래프"), out);
    def
}

fn mlp(input: usize, hidden: usize, output: usize) -> ModelDef {
    chain(vec![
        LayerKind::Input { shape: vec![input] },
        LayerKind::Linear {
            out_features: hidden,
            bias: true,
        },
        LayerKind::Activation { act: Act::Relu },
        LayerKind::Linear {
            out_features: output,
            bias: true,
        },
    ])
}

fn synthetic(kind: SyntheticKind, samples: usize) -> DatasetSpec {
    DatasetSpec::new("syn", DataSource::Synthetic { kind, samples })
}

fn train_to_end(def: ModelDef, ds: DatasetSpec, dir: &Path) -> RunRecord {
    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: ds,
        base_dir: dir.to_path_buf(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let handle = nl_engine::start(req).expect("학습 스레드 시작");
    while let Ok(ev) = handle.events.recv() {
        match ev {
            TrainEvent::Finished { run } => return run,
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }
    panic!("Finished/Failed 이벤트 없이 이벤트 채널이 끊겼습니다 (학습 스레드 비정상 종료)");
}

/// 출력 노드의 샘플 형상(배치 제외)을 `shape::infer` 로 구한다.
fn inferred_output_shape(def: &ModelDef) -> Vec<usize> {
    let rep = shape::infer(&def.graph);
    assert!(rep.errors.is_empty(), "형상 추론 오류: {:?}", rep.errors);
    let out = def.graph.output_nodes()[0];
    rep.shape(out).expect("출력 형상").sample()
}

fn run_once(def: &ModelDef, input: HostTensor) -> Vec<HostTensor> {
    let mut s = Session::load(def, None, test_device()).expect("세션 생성");
    s.run(&[input]).expect("추론")
}

// ───────────────────────────── (a) 형상 일치 ─────────────────────────────

#[test]
fn mlp_output_shape_matches_shape_infer() {
    let def = mlp(4, 8, 3);
    let want = inferred_output_shape(&def);
    assert_eq!(want, vec![3]);
    let out = run_once(&def, HostTensor::new(vec![5, 4], vec![0.1; 20]));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].shape, vec![5, 3]);
}

#[test]
fn cnn_output_shape_matches_shape_infer() {
    let def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 4,
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
        LayerKind::AvgPool2d {
            kernel: [2, 2],
            stride: [2, 2],
        },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 3,
            bias: true,
        },
    ]);
    let want = inferred_output_shape(&def);
    assert_eq!(want, vec![3]);
    let out = run_once(&def, HostTensor::new(vec![2, 1, 8, 8], vec![0.3; 128]));
    assert_eq!(out[0].shape, vec![2, 3]);
}

#[test]
fn residual_add_and_concat_output_shape_matches_shape_infer() {
    let mut def = ModelDef::new("res");
    let g = &mut def.graph;
    let i = add(g, LayerKind::Input { shape: vec![8] });
    let l = add(
        g,
        LayerKind::Linear {
            out_features: 8,
            bias: true,
        },
    );
    let s = add(g, LayerKind::Add);
    let c = add(g, LayerKind::Concat { dim: 0 });
    let n = add(g, LayerKind::LayerNorm { eps: 1e-5 });
    let o = add(g, LayerKind::Output);
    link(g, i, l);
    g.add_edge(l, Port::new(s, 0)).unwrap();
    g.add_edge(i, Port::new(s, 1)).unwrap();
    g.add_edge(s, Port::new(c, 0)).unwrap();
    g.add_edge(i, Port::new(c, 1)).unwrap();
    link(g, c, n);
    link(g, n, o);

    let want = inferred_output_shape(&def);
    assert_eq!(want, vec![16]);
    let out = run_once(&def, HostTensor::new(vec![3, 8], vec![0.5; 24]));
    assert_eq!(out[0].shape, vec![3, 16]);
}

#[test]
fn embedding_output_shape_matches_shape_infer() {
    let def = chain(vec![
        LayerKind::Input { shape: vec![4] },
        LayerKind::Embedding { vocab: 10, dim: 6 },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 3,
            bias: true,
        },
    ]);
    let want = inferred_output_shape(&def);
    assert_eq!(want, vec![3]);
    let idx = HostTensor::new(vec![2, 4], vec![0.0, 1.0, 2.0, 3.0, 9.0, 8.0, 7.0, 6.0]);
    let out = run_once(&def, idx);
    assert_eq!(out[0].shape, vec![2, 3]);
}

#[test]
fn mul_dropout_and_reshape_run_and_match_shapes() {
    let mut def = ModelDef::new("mix");
    let g = &mut def.graph;
    let i = add(g, LayerKind::Input { shape: vec![4] });
    let d = add(g, LayerKind::Dropout { p: 0.5 });
    let m = add(g, LayerKind::Mul);
    let r = add(g, LayerKind::Reshape { shape: vec![1, 2, 2] });
    let gap = add(g, LayerKind::GlobalAvgPool);
    let o = add(g, LayerKind::Output);
    link(g, i, d);
    g.add_edge(d, Port::new(m, 0)).unwrap();
    g.add_edge(i, Port::new(m, 1)).unwrap();
    link(g, m, r);
    link(g, r, gap);
    link(g, gap, o);

    assert_eq!(inferred_output_shape(&def), vec![1]);
    let out = run_once(&def, HostTensor::new(vec![2, 4], vec![1.0; 8]));
    assert_eq!(out[0].shape, vec![2, 1]);
    // 추론 모드에서는 Dropout 이 항등이라 결과는 1.0 이어야 한다.
    for v in &out[0].data {
        assert!((v - 1.0).abs() < 1e-5, "Dropout 이 추론에서 항등이 아님: {v}");
    }
}

// ───────────────────────────── (b, d, e) XOR 학습 ─────────────────────────────

struct XorRun {
    dir: PathBuf,
    def: ModelDef,
    run: RunRecord,
}

fn xor_run() -> &'static XorRun {
    static ONCE: OnceLock<XorRun> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = temp_dir("xor");
        let mut def = mlp(2, 16, 2);
        def.train.loss = Loss::CrossEntropy;
        def.train.metric = Metric::Accuracy;
        def.train.optimizer = Optimizer::Adam {
            lr: 1e-2,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        };
        def.train.epochs = 40;
        def.train.batch_size = 64;
        def.train.device = test_device();
        def.train.seed = 7;
        let run = train_to_end(def.clone(), synthetic(SyntheticKind::Xor, 1024), &dir);
        XorRun { dir, def, run }
    })
}

#[test]
fn xor_mlp_reaches_high_accuracy_on_cpu() {
    let r = xor_run();
    assert_eq!(r.run.status, RunStatus::Finished);
    let last = r.run.last().expect("에포크 기록");
    let acc = last.val_metric.expect("정확도");
    assert!(
        acc >= 0.95,
        "XOR 정확도가 낮습니다: {acc} (train_loss {})",
        last.train_loss
    );
    // 총 스텝 수가 수백 단위인지 (기대치 확인용).
    assert!(r.run.epochs.len() == 40);
}

#[test]
fn checkpoint_round_trip_gives_identical_outputs() {
    let r = xor_run();
    let ckpt = r.dir.join(r.run.checkpoint.as_ref().expect("체크포인트 경로"));
    assert!(ckpt.exists(), "체크포인트 파일이 없습니다: {}", ckpt.display());

    // 시드를 다르게 해도 같은 가중치를 얹으면 결과가 같아야 한다.
    let mut a = r.def.clone();
    a.train.seed = 1;
    let mut b = r.def.clone();
    b.train.seed = 999;

    let x = HostTensor::new(vec![4, 2], vec![0.5, 0.5, -0.5, 0.5, 0.5, -0.5, -0.5, -0.5]);
    let mut sa = Session::load(&a, Some(&ckpt), test_device()).unwrap();
    let mut sb = Session::load(&b, Some(&ckpt), test_device()).unwrap();
    let inputs = [x];
    let oa = sa.run(&inputs).unwrap();
    let ob = sb.run(&inputs).unwrap();
    assert_eq!(oa, ob);

    // 이름·형상 요약도 읽혀야 한다.
    let sum = nl_engine::checkpoint_summary(&ckpt).unwrap();
    assert!(
        sum.iter().any(|(n, s)| n.ends_with(".weight") && s.len() == 2),
        "요약: {sum:?}"
    );
}

#[test]
fn session_infers_with_trained_checkpoint() {
    let r = xor_run();
    let ckpt = r.dir.join(r.run.checkpoint.as_ref().unwrap());
    let mut s = Session::load(&r.def, Some(&ckpt), test_device()).unwrap();
    assert!(!s.device_name().is_empty());
    assert_eq!(s.input_sample_shapes(), &[vec![2]]);
    assert_eq!(s.output_sample_shapes(), &[vec![2]]);

    // XOR 의 네 모서리: (+,+)=0, (−,+)=1, (+,−)=1, (−,−)=0.
    let x = HostTensor::new(vec![4, 2], vec![0.8, 0.8, -0.8, 0.8, 0.8, -0.8, -0.8, -0.8]);
    let out = s.run(&[x]).unwrap();
    assert_eq!(out[0].shape, vec![4, 2]);
    assert_eq!(out[0].argmax_last(), vec![0, 1, 1, 0]);
}

// ───────────────────────────── (c) 회귀 수렴 ─────────────────────────────

#[test]
fn linear_regression_converges_below_mse_005() {
    let dir = temp_dir("linreg");
    let mut def = mlp(2, 32, 1);
    def.train.loss = Loss::Mse;
    def.train.metric = Metric::Mae;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 80;
    def.train.batch_size = 64;
    def.train.device = test_device();
    let run = train_to_end(def, synthetic(SyntheticKind::LinearRegression, 2048), &dir);
    let last = run.last().unwrap();
    let val = last.val_loss.expect("검증 손실");
    assert!(
        val < 0.05,
        "MSE 가 수렴하지 않았습니다: val {val}, train {}",
        last.train_loss
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── (g) 옵티마이저 ─────────────────────────────

#[test]
fn every_optimizer_reduces_the_loss() {
    let opts = [
        Optimizer::Sgd {
            lr: 5e-2,
            momentum: 0.9,
        },
        Optimizer::Adam {
            lr: 1e-2,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        },
        Optimizer::AdamW {
            lr: 1e-2,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
            weight_decay: 1e-2,
        },
    ];
    for opt in opts {
        let dir = temp_dir("opt");
        let mut def = mlp(2, 16, 2);
        def.train.loss = Loss::CrossEntropy;
        def.train.metric = Metric::Accuracy;
        def.train.optimizer = opt;
        def.train.epochs = 8;
        def.train.batch_size = 64;
        def.train.device = test_device();
        def.train.grad_clip = 1.0;
        let run = train_to_end(def, synthetic(SyntheticKind::Xor, 1024), &dir);
        let first = run.epochs.first().unwrap().train_loss;
        let last = run.epochs.last().unwrap().train_loss;
        assert!(
            last < first,
            "{} 이 손실을 줄이지 못했습니다: {first} → {last}",
            opt.label()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

// ───────────────────────────── (h) 일시정지·중지 ─────────────────────────────

#[test]
fn pause_and_stop_control_the_run() {
    let dir = temp_dir("stop");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.epochs = 500; // 중지가 없으면 한참 돈다
    def.train.batch_size = 32;
    def.train.device = test_device();

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: synthetic(SyntheticKind::Xor, 2048),
        base_dir: dir.clone(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let h = nl_engine::start(req).unwrap();

    // Started 와 Step 몇 개를 받을 때까지 기다린다.
    let mut steps = 0;
    while steps < 5 {
        match h
            .events
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("이벤트")
        {
            TrainEvent::Step { .. } => steps += 1,
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }

    // 일시정지: 진행 중인 배치 하나만 더 오고 멈춰야 한다.
    h.pause();
    assert!(h.is_paused());
    std::thread::sleep(std::time::Duration::from_millis(200));
    while h.events.try_recv().is_ok() {} // 밀린 이벤트를 비운다
    std::thread::sleep(std::time::Duration::from_millis(400));
    let leaked = std::iter::from_fn(|| h.events.try_recv().ok()).count();
    assert!(leaked <= 1, "일시정지 뒤에도 이벤트가 {leaked} 개 왔습니다");

    h.resume();
    h.stop();

    let mut run = None;
    while let Ok(ev) = h.events.recv_timeout(std::time::Duration::from_secs(60)) {
        match ev {
            TrainEvent::Finished { run: r } => {
                run = Some(r);
                break;
            }
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }
    let run = run.expect("Finished 이벤트");
    assert_eq!(run.status, RunStatus::Stopped, "중지했는데 상태가 {:?}", run.status);
    assert!(run.checkpoint.is_some(), "중지해도 마지막 가중치는 저장되어야 합니다");
    assert!(run.epochs.len() < 500);
    assert!(h.is_done());
    assert!(dir.join("run/run.json").exists(), "run.json 이 없습니다");
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 분할 · 이어서 학습 ─────────────────────────────

#[test]
fn separate_validation_source_is_used() {
    let dir = temp_dir("split");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.epochs = 5;
    def.train.batch_size = 64;
    def.train.device = test_device();
    def.train.val_split = 0.0; // 비율 분할은 끄고 별도 소스만 쓴다

    let mut ds = synthetic(SyntheticKind::Xor, 512);
    ds.split = nl_core::Split::Separate {
        validation: Box::new(DataSource::Synthetic {
            kind: SyntheticKind::Xor,
            samples: 128,
        }),
    };
    let run = train_to_end(def, ds, &dir);
    assert_eq!(run.status, RunStatus::Finished);
    let last = run.last().unwrap();
    assert!(last.val_loss.is_some(), "별도 검증 소스가 쓰이지 않았습니다");
    assert!(last.val_metric.is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resume_from_checkpoint_starts_from_a_lower_loss() {
    let dir = temp_dir("resume");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 15;
    def.train.batch_size = 64;
    def.train.device = test_device();
    def.train.seed = 3;

    let first = train_to_end(def.clone(), synthetic(SyntheticKind::Xor, 1024), &dir);
    let ckpt = dir.join(first.checkpoint.as_ref().unwrap());
    assert!(ckpt.exists());

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: synthetic(SyntheticKind::Xor, 1024),
        base_dir: dir.clone(),
        run_dir: dir.join("run2"),
        resume_from: Some(ckpt),
    };
    let h = nl_engine::start(req).unwrap();
    let mut second = None;
    while let Ok(ev) = h.events.recv() {
        match ev {
            TrainEvent::Finished { run } => {
                second = Some(run);
                break;
            }
            TrainEvent::Failed { error, .. } => panic!("이어서 학습 실패: {error}"),
            _ => {}
        }
    }
    let second = second.unwrap();
    let a = first.epochs.first().unwrap().train_loss;
    let b = second.epochs.first().unwrap().train_loss;
    assert!(
        b < a,
        "이어서 학습한 첫 에포크 손실이 더 낮아야 합니다: 처음 {a}, 이어서 {b}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 다입력 · 다출력 ─────────────────────────────

#[test]
fn two_input_model_trains_with_columns_split_in_node_order() {
    let dir = temp_dir("multi-in");
    let ds = four_column_csv(&dir, 600);

    // Input "a"[2] + Input "b"[2] → Concat → MLP → 회귀 출력.
    let mut def = ModelDef::new("2입력");
    let g = &mut def.graph;
    let a = named(g, LayerKind::Input { shape: vec![2] }, "a");
    let b = named(g, LayerKind::Input { shape: vec![2] }, "b");
    let cat = add(g, LayerKind::Concat { dim: 0 });
    let l1 = add(
        g,
        LayerKind::Linear {
            out_features: 32,
            bias: true,
        },
    );
    let act = add(g, LayerKind::Activation { act: Act::Relu });
    let l2 = add(
        g,
        LayerKind::Linear {
            out_features: 1,
            bias: true,
        },
    );
    let o = add(g, LayerKind::Output);
    g.add_edge(a, Port::new(cat, 0)).unwrap();
    g.add_edge(b, Port::new(cat, 1)).unwrap();
    link(g, cat, l1);
    link(g, l1, act);
    link(g, act, l2);
    link(g, l2, o);

    assert_eq!(def.graph.input_nodes(), vec![a, b], "input_nodes 는 이름 순이어야 한다");
    def.train.loss = Loss::Mse;
    def.train.metric = Metric::Mae;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 60;
    def.train.batch_size = 32;
    def.train.device = test_device();

    let (run, logs) = train_collecting_logs(def, ds, &dir);
    assert_eq!(run.status, RunStatus::Finished);
    assert!(
        logs.iter().any(|m| m.contains("Input 레이어가 2 개")),
        "다입력 안내 로그가 없습니다: {logs:?}"
    );
    let last = run.last().unwrap();
    let val = last.val_loss.expect("검증 손실");
    assert!(
        val < 0.05,
        "2입력 회귀가 수렴하지 않았습니다: {val} (train {})",
        last.train_loss
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mismatched_column_count_is_rejected_with_a_clear_message() {
    let dir = temp_dir("multi-in-bad");
    let ds = four_column_csv(&dir, 20);

    // Input 이 [2] + [3] = 5 개를 원하는데 CSV 는 4 열만 준다.
    let mut def = ModelDef::new("어긋남");
    let g = &mut def.graph;
    let a = named(g, LayerKind::Input { shape: vec![2] }, "a");
    let b = named(g, LayerKind::Input { shape: vec![3] }, "b");
    let cat = add(g, LayerKind::Concat { dim: 0 });
    let l = add(
        g,
        LayerKind::Linear {
            out_features: 1,
            bias: true,
        },
    );
    let o = add(g, LayerKind::Output);
    g.add_edge(a, Port::new(cat, 0)).unwrap();
    g.add_edge(b, Port::new(cat, 1)).unwrap();
    link(g, cat, l);
    link(g, l, o);
    def.train.loss = Loss::Mse;
    def.train.epochs = 1;
    def.train.device = test_device();

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: ds,
        base_dir: dir.clone(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let h = nl_engine::start(req).unwrap();
    let mut err = None;
    while let Ok(ev) = h.events.recv() {
        if let TrainEvent::Failed { error, .. } = ev {
            err = Some(error);
            break;
        }
    }
    let err = err.expect("Failed 이벤트가 와야 합니다");
    assert!(err.contains("5 개"), "필요 원소 수가 없습니다: {err}");
    assert!(err.contains("[2, 3]"), "노드별 원소 수가 없습니다: {err}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn two_output_model_trains_on_the_first_output() {
    let dir = temp_dir("multi-out");

    // 공통 몸통 → Output "a"(분류, 손실 대상) + Output "b"(보조 회귀, 추론 전용).
    let mut def = ModelDef::new("2출력");
    let g = &mut def.graph;
    let i = add(g, LayerKind::Input { shape: vec![2] });
    let l1 = add(
        g,
        LayerKind::Linear {
            out_features: 16,
            bias: true,
        },
    );
    let act = add(g, LayerKind::Activation { act: Act::Relu });
    let head_a = add(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    );
    let head_b = add(
        g,
        LayerKind::Linear {
            out_features: 1,
            bias: true,
        },
    );
    let oa = named(g, LayerKind::Output, "a");
    let ob = named(g, LayerKind::Output, "b");
    link(g, i, l1);
    link(g, l1, act);
    link(g, act, head_a);
    link(g, act, head_b);
    link(g, head_a, oa);
    link(g, head_b, ob);
    assert_eq!(
        def.graph.output_nodes(),
        vec![oa, ob],
        "output_nodes 는 이름 순이어야 한다"
    );

    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 40;
    def.train.batch_size = 64;
    def.train.device = test_device();
    def.train.seed = 7;

    let (run, logs) = train_collecting_logs(def.clone(), synthetic(SyntheticKind::Xor, 1024), &dir);
    assert_eq!(run.status, RunStatus::Finished);
    assert!(
        logs.iter()
            .any(|m| m.contains("Output 레이어가 2 개") && m.contains("'a'")),
        "다출력 안내 로그가 없습니다: {logs:?}"
    );
    let acc = run.last().unwrap().val_metric.expect("정확도");
    assert!(acc >= 0.95, "첫 Output 으로 학습되지 않았습니다: {acc}");

    // 추론은 두 출력을 모두 돌려준다.
    let ckpt = dir.join(run.checkpoint.as_ref().unwrap());
    let mut s = Session::load(&def, Some(&ckpt), test_device()).unwrap();
    let out = s
        .run(&[HostTensor::new(vec![2, 2], vec![0.8, 0.8, -0.8, 0.8])])
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].shape, vec![2, 2]);
    assert_eq!(out[1].shape, vec![2, 1]);
    assert_eq!(out[0].argmax_last(), vec![0, 1]);
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 스케줄 · 조기 종료 ─────────────────────────────

#[test]
fn step_schedule_is_reported_per_epoch() {
    let dir = temp_dir("sched-step");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = Optimizer::Sgd { lr: 0.1, momentum: 0.0 };
    def.train.schedule = nl_core::LrSchedule::Step { every: 2, gamma: 0.5 };
    def.train.epochs = 6;
    def.train.batch_size = 128;
    def.train.device = test_device();

    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 256), &dir);
    let lrs: Vec<f64> = run
        .epochs
        .iter()
        .map(|e| e.lr.expect("lr 이 기록되어야 합니다"))
        .collect();
    assert_eq!(lrs, vec![0.1, 0.1, 0.05, 0.05, 0.025, 0.025]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cosine_schedule_and_warmup_shape_the_learning_rate() {
    let dir = temp_dir("sched-cos");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = Optimizer::Sgd { lr: 0.2, momentum: 0.0 };
    def.train.schedule = nl_core::LrSchedule::Cosine { min_lr: 0.02 };
    def.train.epochs = 5;
    def.train.batch_size = 256; // 에포크당 1 스텝
    def.train.warmup_steps = 2;
    def.train.device = test_device();

    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 256), &dir);
    let lrs: Vec<f64> = run.epochs.iter().map(|e| e.lr.unwrap()).collect();
    // 1 스텝째는 워밍업 절반, 2 스텝째부터 스케줄 그대로.
    assert!((lrs[0] - 0.1).abs() < 1e-9, "워밍업이 적용되지 않았습니다: {lrs:?}");
    assert!(lrs[1] < 0.2 && lrs[1] > 0.02);
    assert!((lrs[4] - 0.02).abs() < 1e-9, "마지막이 min_lr 이 아닙니다: {lrs:?}");
    assert!(
        lrs[1] > lrs[2] && lrs[2] > lrs[3],
        "코사인이 단조 감소해야 합니다: {lrs:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn early_stopping_ends_the_run_as_finished() {
    let dir = temp_dir("early");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    // 학습률 0 → 검증 손실이 절대 나아지지 않는다.
    def.train.optimizer = Optimizer::Sgd { lr: 0.0, momentum: 0.0 };
    def.train.epochs = 50;
    def.train.batch_size = 64;
    def.train.val_split = 0.25;
    def.train.early_stop_patience = 2;
    def.train.device = test_device();

    let (run, logs) = train_collecting_logs(def, synthetic(SyntheticKind::Xor, 256), &dir);
    assert_eq!(run.status, RunStatus::Finished, "조기 종료는 정상 종료여야 합니다");
    assert_eq!(run.epochs.len(), 3, "patience 2 면 3 에포크에서 멈춰야 합니다");
    assert!(
        logs.iter().any(|m| m.contains("조기 종료")),
        "조기 종료 로그가 없습니다: {logs:?}"
    );
    assert!(run.checkpoint.is_some());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn resume_restores_weights_only_and_says_so() {
    let dir = temp_dir("resume-log");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 10;
    def.train.batch_size = 64;
    def.train.device = test_device();
    def.train.seed = 3;

    let first = train_to_end(def.clone(), synthetic(SyntheticKind::Xor, 512), &dir);
    let ckpt = dir.join(first.checkpoint.as_ref().unwrap());

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: synthetic(SyntheticKind::Xor, 512),
        base_dir: dir.clone(),
        run_dir: dir.join("run2"),
        resume_from: Some(ckpt),
    };
    let h = nl_engine::start(req).unwrap();
    let mut logs = Vec::new();
    let mut second = None;
    while let Ok(ev) = h.events.recv() {
        match ev {
            TrainEvent::Log(m) => logs.push(m),
            TrainEvent::Finished { run } => {
                second = Some(run);
                break;
            }
            TrainEvent::Failed { error, .. } => panic!("이어서 학습 실패: {error}"),
            _ => {}
        }
    }
    let second = second.unwrap();
    assert!(
        logs.iter()
            .any(|m| m.contains("가중치만 복원") && m.contains("옵티마이저")),
        "이어서 학습 로그가 계약을 밝히지 않습니다: {logs:?}"
    );
    assert!(
        second.epochs[0].train_loss < first.epochs[0].train_loss,
        "가중치가 이어지지 않았습니다: {} → {}",
        first.epochs[0].train_loss,
        second.epochs[0].train_loss
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 샘플이 이미지 크기면 데이터셋이 장치에 상주한 채 배치가 잘린다 — 그 경로를 실제로 태운다.
#[test]
fn image_sized_samples_train_through_the_resident_path() {
    let dir = temp_dir("resident-cnn");
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 4,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [1, 1],
            bias: true,
        },
        LayerKind::Activation { act: Act::Relu },
        LayerKind::MaxPool2d {
            kernel: [2, 2],
            stride: [2, 2],
        },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 4,
            bias: true,
        },
    ]);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 5e-3,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 8;
    def.train.batch_size = 32;
    def.train.device = test_device();
    def.train.seed = 5;

    let run = train_to_end(def, synthetic(SyntheticKind::Quadrants, 512), &dir);
    assert_eq!(run.status, RunStatus::Finished);
    let first = run.epochs.first().unwrap().train_loss;
    let last = run.epochs.last().unwrap();
    assert!(
        last.train_loss < first,
        "학습이 진행되지 않았습니다: {first} → {}",
        last.train_loss
    );
    let acc = last.val_metric.expect("정확도");
    assert!(acc > 0.5, "사분면 분류가 무작위 수준입니다: {acc}");
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 빈 클래스 · 텍스트 입력 ─────────────────────────────

#[test]
fn training_warns_about_classes_with_no_samples() {
    let dir = temp_dir("empty-class");
    // 라벨 0, 1, 3 만 쓴다 → 클래스 2 는 비어 있다.
    let mut text = String::from("x0,x1,y\n");
    for i in 0..120 {
        let label = [0, 1, 3][i % 3];
        let f = ((i * 7) % 19) as f32 / 10.0 - 1.0;
        text.push_str(&format!("{f},{},{label}\n", f * 0.5));
    }
    std::fs::write(dir.join("gap.csv"), text).unwrap();
    let ds = DatasetSpec::new(
        "간격",
        DataSource::Csv {
            path: "gap.csv".into(),
            input_cols: vec!["x0".into(), "x1".into()],
            target_cols: vec!["y".into()],
            header: true,
        },
    );

    let mut def = mlp(2, 8, 4);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.epochs = 3;
    def.train.batch_size = 32;
    def.train.device = test_device();

    let (run, logs) = train_collecting_logs(def, ds, &dir);
    assert_eq!(run.status, RunStatus::Finished);
    let warning = logs
        .iter()
        .find(|m| m.contains("샘플이 하나도 없는 클래스"))
        .unwrap_or_else(|| panic!("빈 클래스 경고가 없습니다: {logs:?}"));
    assert!(warning.contains('2'), "어느 클래스가 비었는지 없습니다: {warning}");

    // 스캔 결과도 같은 것을 알려야 한다.
    let info = nl_engine::scan(
        &nl_core::DatasetSpec::new(
            "간격",
            DataSource::Csv {
                path: "gap.csv".into(),
                input_cols: vec!["x0".into(), "x1".into()],
                target_cols: vec!["y".into()],
                header: true,
            },
        ),
        &dir,
    )
    .unwrap();
    assert_eq!(info.classes.len(), 4);
    assert_eq!(info.empty_classes, vec![2]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tokenized_text_feeds_an_embedding_model() {
    use nl_core::payload::{Field, FieldKind, Transform};

    const VOCAB: &str = "abcdefghijklmnopqrstuvwxyz ";
    const LEN: usize = 8;

    let mut field = Field::new("문장", FieldKind::Text);
    field.encode = vec![Transform::Tokenize {
        vocab: VOCAB.into(),
        max_len: LEN,
    }];
    assert_eq!(field.tensor_shape(), Some(vec![LEN]));

    // 텍스트 → [1, 8] 정수 텐서.
    let encoded = nl_engine::encode(&field, &nl_engine::Value::Text("hello you".into())).unwrap();
    assert_eq!(encoded.shape, vec![1, LEN]);
    assert!(encoded
        .data
        .iter()
        .all(|v| *v >= 0.0 && (*v as usize) <= VOCAB.chars().count()));

    // 그 텐서를 그대로 먹는 Embedding 모델.
    let def = chain(vec![
        LayerKind::Input { shape: vec![LEN] },
        LayerKind::Embedding {
            vocab: VOCAB.chars().count() + 1,
            dim: 6,
        },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 3,
            bias: true,
        },
    ]);
    assert_eq!(inferred_output_shape(&def), vec![3]);

    let mut session = Session::load(&def, None, test_device()).unwrap();
    let out = session.run(&[encoded]).unwrap();
    assert_eq!(out[0].shape, vec![1, 3]);
    assert!(out[0].data.iter().all(|v| v.is_finite()));
}

#[test]
fn resolve_cached_is_safe_to_call_every_frame() {
    // 목록만 준비되면(백그라운드 작업이 한 번 돌면) 비차단 조회가 값을 준다.
    let _ = nl_engine::enumerate();
    let cached = nl_engine::resolve_cached(DevicePref::Cpu).expect("CPU 는 항상 있다");
    assert_eq!(cached.pref, DevicePref::Cpu);
    // 몇 번을 불러도 같은 답.
    for _ in 0..100 {
        assert_eq!(nl_engine::resolve_cached(DevicePref::Cpu), Some(cached.clone()));
    }
}

// ───────────────────────────── 리뷰 회귀 (B1~B7) ─────────────────────────────

/// 학습을 돌리되 실패를 오류 문자열로 받는다.
fn train_expect_failure(def: ModelDef, ds: DatasetSpec, dir: &Path) -> String {
    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: ds,
        base_dir: dir.to_path_buf(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let h = nl_engine::start(req).expect("학습 스레드 시작");
    while let Ok(ev) = h.events.recv() {
        match ev {
            TrainEvent::Failed { error, .. } => return error,
            TrainEvent::Finished { run } => panic!("실패해야 하는데 {:?} 로 끝났습니다", run.status),
            _ => {}
        }
    }
    panic!("Failed 이벤트가 오지 않았습니다");
}

/// B1: 출력 1 유닛 + BCE 에서 Accuracy 가 "라벨 0 비율" 이 아니라 실제 정답률이어야 한다.
#[test]
fn b1_accuracy_is_real_for_single_unit_binary_output() {
    let dir = temp_dir("b1");
    let mut def = mlp(2, 16, 1);
    def.train.loss = Loss::BceWithLogits;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 60;
    def.train.batch_size = 64;
    def.train.val_split = 0.3;
    def.train.device = test_device();
    def.train.seed = 7;

    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 600), &dir);
    let last = run.last().unwrap();
    let acc = last.val_metric.expect("정확도");
    // 고치기 전에는 검증셋의 라벨 0 비율(약 0.48)에 고정되어 있었다.
    assert!(
        acc > 0.9,
        "단일 출력 정확도가 낮습니다: {acc} (손실 {})",
        last.val_loss.unwrap_or(f64::NAN)
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// B2: 클래스 수보다 출력이 적으면 학습 시작 전에 이유를 담아 막는다.
#[test]
fn b2_too_few_output_units_is_reported_before_training() {
    let dir = temp_dir("b2");
    let mut def = mlp(2, 8, 2); // Quadrants 는 클래스 4 개인데 출력 2 유닛
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 1;
    def.train.device = test_device();

    // 입력 형상을 Quadrants 에 맞춘다.
    let mut d2 = ModelDef::new("b2");
    let g = &mut d2.graph;
    let i = add(g, LayerKind::Input { shape: vec![1, 8, 8] });
    let f = add(g, LayerKind::Flatten);
    let l = add(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    );
    let o = add(g, LayerKind::Output);
    link(g, i, f);
    link(g, f, l);
    link(g, l, o);
    d2.train = def.train.clone();

    let e = train_expect_failure(d2, synthetic(SyntheticKind::Quadrants, 200), &dir);
    assert!(e.contains("클래스"), "클래스 수를 짚어 주지 않습니다: {e}");
    assert!(
        e.contains("2 유닛") || e.contains("Output"),
        "출력 폭을 짚어 주지 않습니다: {e}"
    );
    assert!(
        !e.contains("index out of bounds"),
        "백엔드 패닉이 그대로 새어 나옵니다: {e}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 추론 실패 메시지를 전체 체인으로 받는다 (`Session` 은 Debug 가 아니라 unwrap_err 를 못 쓴다).
fn run_err(s: &mut Session, values: &[f32]) -> String {
    let input = HostTensor::new(vec![1, values.len()], values.to_vec());
    match s.run(&[input]) {
        Ok(_) => panic!("{values:?} 를 받아들이면 안 됩니다"),
        Err(e) => format!("{e:#}"),
    }
}

/// B3: Embedding 인덱스가 범위 밖이거나 정수가 아니면 레이어 맥락과 함께 알린다.
#[test]
fn b3_embedding_rejects_out_of_range_and_fractional_indices() {
    let def = chain(vec![
        LayerKind::Input { shape: vec![2] },
        LayerKind::Embedding { vocab: 4, dim: 3 },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    ]);
    let mut s = Session::load(&def, None, test_device()).unwrap();

    // 범위 밖.
    let e = run_err(&mut s, &[0.0, 9.0]);
    assert!(e.contains("vocab") && e.contains("Embedding"), "{e}");
    // 소수 — 조용히 절단되면 안 된다.
    let e = run_err(&mut s, &[0.0, 1.5]);
    assert!(e.contains("정수"), "{e}");
    // 음수.
    let e = run_err(&mut s, &[-1.0, 1.0]);
    assert!(e.contains("범위"), "{e}");
    // 올바른 인덱스는 통과.
    assert!(s.run(&[HostTensor::new(vec![1, 2], vec![0.0, 3.0])]).is_ok());
}

/// B4: 출력 1 유닛 + CrossEntropy 는 손실이 항상 0 이라 학습이 무효다 — 어느 경로로든 막아야 한다.
#[test]
fn b4_single_class_cross_entropy_is_rejected() {
    // (a) 리뷰의 재현 구성. 이제는 클래스 수 대조(B2)가 먼저 잡고 더 구체적으로 알려 준다.
    let dir = temp_dir("b4a");
    let mut def = mlp(2, 8, 1);
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 3;
    def.train.device = test_device();
    let e = train_expect_failure(def, synthetic(SyntheticKind::Xor, 200), &dir);
    assert!(e.contains("1 유닛"), "출력 폭을 짚어 주지 않습니다: {e}");
    assert!(e.contains("클래스"), "{e}");
    std::fs::remove_dir_all(&dir).ok();

    // (b) 타깃이 전부 0 이라 클래스 수 대조를 통과하는 경우 — 손실 쪽 방어선이 잡아야 한다.
    let dir = temp_dir("b4b");
    let mut text = String::from("x0,x1,y\n");
    for i in 0..80 {
        let f = ((i * 7) % 19) as f32 / 10.0 - 1.0;
        text.push_str(&format!("{f},{},0\n", f * 0.5));
    }
    std::fs::write(dir.join("zeros.csv"), text).unwrap();
    let ds = DatasetSpec::new(
        "전부0",
        DataSource::Csv {
            path: "zeros.csv".into(),
            input_cols: vec!["x0".into(), "x1".into()],
            target_cols: vec!["y".into()],
            header: true,
        },
    );
    let mut def = mlp(2, 8, 1);
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 3;
    def.train.device = test_device();
    let e = train_expect_failure(def, ds, &dir);
    assert!(e.contains("2 개 이상"), "{e}");
    assert!(e.contains("BCE"), "대안을 알려 주지 않습니다: {e}");
    std::fs::remove_dir_all(&dir).ok();
}

/// B5: 랭크 6 이상은 편집기 단계(`shape::infer`·`validate`)에서 이미 걸려야 한다.
#[test]
fn b5_rank_above_the_limit_is_caught_by_validation_not_at_train_time() {
    let def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Reshape {
            shape: vec![1, 2, 2, 4, 4],
        }, // 배치 포함 랭크 6
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    ]);
    let rep = shape::infer(&def.graph);
    assert!(!rep.errors.is_empty(), "형상 추론이 랭크 상한을 놓쳤습니다");
    let issues = nl_core::validate(&{
        let mut p = nl_core::model::Project::new("t");
        p.models.insert(def.id, def.clone());
        p
    });
    assert!(!issues.is_empty(), "검증기가 랭크 상한을 놓쳤습니다");

    // 엔진도 같은 이유로 거절한다 (계약이 한 곳에서 맞물린다).
    assert!(Session::load(&def, None, test_device()).is_err());
}

/// B7: 조기 종료는 최적 에포크의 가중치를 남기고 `checkpoint` 가 그것을 가리켜야 한다.
#[test]
fn b7_early_stopping_keeps_the_best_weights() {
    let dir = temp_dir("b7");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    // 학습률 0 → 첫 에포크가 최적이고 그 뒤로는 개선이 없다.
    def.train.optimizer = Optimizer::Sgd { lr: 0.0, momentum: 0.0 };
    def.train.epochs = 50;
    def.train.batch_size = 64;
    def.train.val_split = 0.25;
    def.train.early_stop_patience = 2;
    def.train.device = test_device();

    let (run, logs) = train_collecting_logs(def, synthetic(SyntheticKind::Xor, 256), &dir);
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.epochs.len(), 3);

    let best = run
        .best_checkpoint
        .as_ref()
        .expect("best_checkpoint 가 기록되어야 합니다");
    assert!(best.ends_with("best.safetensors"), "{best}");
    assert!(dir.join(best).exists(), "best 파일이 없습니다");
    assert_eq!(
        run.checkpoint.as_deref(),
        Some(best.as_str()),
        "조기 종료면 checkpoint 가 best 를 가리켜야 합니다"
    );
    assert!(logs.iter().any(|m| m.contains("best.safetensors")), "{logs:?}");

    // 조기 종료가 아니면 final 을 가리킨다.
    let dir2 = temp_dir("b7-normal");
    let mut d2 = mlp(2, 8, 2);
    d2.train.loss = Loss::CrossEntropy;
    d2.train.epochs = 3;
    d2.train.batch_size = 64;
    d2.train.device = test_device();
    let run2 = train_to_end(d2, synthetic(SyntheticKind::Xor, 256), &dir2);
    assert!(run2.checkpoint.as_deref().unwrap().ends_with("final.safetensors"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

/// S4: 다른 모델의 체크포인트를 얹으면 이유를 밝힌다.
#[test]
fn s4_checkpoint_from_another_model_is_refused() {
    let r = xor_run();
    let ckpt = r.dir.join(r.run.checkpoint.as_ref().unwrap());
    // 같은 구조지만 id 가 다른 모델.
    let other = mlp(2, 16, 2);
    let e = match Session::load(&other, Some(&ckpt), test_device()) {
        Ok(_) => panic!("다른 모델의 체크포인트를 받아들였습니다"),
        Err(e) => format!("{e:#}"),
    };
    assert!(e.contains("다른 모델"), "{e}");
}

// ───────────────────────────── 리뷰 "개선" 항목 ─────────────────────────────

/// 그래프 오류 메시지는 출력에 기여하는 노드를 먼저 지목해야 한다.
#[test]
fn graph_errors_point_at_the_node_that_blocks_the_output() {
    let mut def = ModelDef::new("오류");
    let g = &mut def.graph;
    // 실제 원인: Conv2d 가 [4] 벡터를 받는다.
    let i = add(g, LayerKind::Input { shape: vec![4] });
    let conv = named(
        g,
        LayerKind::Conv2d {
            out_channels: 4,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [0, 0],
            bias: true,
        },
        "진짜원인",
    );
    let o = add(g, LayerKind::Output);
    link(g, i, conv);
    link(g, conv, o);
    // 캔버스에 떠 있는, 출력과 이어지지 않은 노드. id 순으로는 이쪽이 먼저 걸릴 수 있다.
    named(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
        "떠있는노드",
    );

    let e = match Session::load(&def, None, test_device()) {
        Ok(_) => panic!("오류 그래프를 받아들였습니다"),
        Err(e) => format!("{e:#}"),
    };
    let real = e.find("진짜원인").expect("진짜 원인을 지목하지 않습니다");
    if let Some(stray) = e.find("떠있는노드") {
        assert!(real < stray, "떠 있는 노드가 먼저 나옵니다: {e}");
    }
    assert!(
        e.contains("이어지지 않은"),
        "떠 있는 노드가 있다는 사실을 알려야 합니다: {e}"
    );
}

/// `Step` 은 솎아 내도 되지만 `Epoch`·`Finished` 는 한 개도 빠지면 안 된다.
#[test]
fn epoch_and_finished_events_are_never_dropped() {
    let dir = temp_dir("events");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.epochs = 12;
    def.train.batch_size = 8; // 에포크당 스텝을 많이 만들어 Step 이 솎이게 한다
    def.train.device = test_device();

    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: synthetic(SyntheticKind::Xor, 512),
        base_dir: dir.clone(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let h = nl_engine::start(req).unwrap();

    let mut epochs = 0usize;
    let mut steps = 0usize;
    let mut finished = None;
    while let Ok(ev) = h.events.recv() {
        match ev {
            TrainEvent::Epoch(_) => epochs += 1,
            TrainEvent::Step { .. } => steps += 1,
            TrainEvent::Finished { run } => {
                finished = Some(run);
                break;
            }
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }
    let run = finished.expect("Finished 가 와야 합니다");
    assert_eq!(epochs, 12, "Epoch 이 빠졌습니다");
    assert_eq!(run.epochs.len(), 12);
    // 총 스텝은 12 × 64 = 768 인데 이벤트는 주기에 맞춰 훨씬 적게 온다.
    assert!(steps <= 768, "Step 이 총 스텝보다 많습니다: {steps}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `NL_STEP_EVENT_MS=0` 이면 모든 스텝이 이벤트로 나온다 (주기 옵션 확인).
#[test]
fn step_event_interval_can_be_disabled() {
    let dir = temp_dir("events-all");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 2;
    def.train.batch_size = 64;
    def.train.val_split = 0.0;
    def.train.device = test_device();

    // SAFETY: 이 테스트만 이 변수를 쓰고, 학습 스레드가 뜨기 전에 설정한다.
    unsafe { std::env::set_var("NL_STEP_EVENT_MS", "0") };
    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: synthetic(SyntheticKind::Xor, 256),
        base_dir: dir.clone(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let h = nl_engine::start(req).unwrap();
    let mut steps = 0usize;
    while let Ok(ev) = h.events.recv() {
        match ev {
            TrainEvent::Step { .. } => steps += 1,
            TrainEvent::Finished { .. } => break,
            TrainEvent::Failed { error, .. } => panic!("학습 실패: {error}"),
            _ => {}
        }
    }
    unsafe { std::env::remove_var("NL_STEP_EVENT_MS") };
    // 256 샘플 / 배치 64 = 4 스텝 × 2 에포크.
    assert_eq!(steps, 8, "주기를 0 으로 두면 모든 스텝이 나와야 합니다");
    std::fs::remove_dir_all(&dir).ok();
}

/// 검증 손실이 학습 손실과 같은 구현에서 나온다 — 같은 데이터면 같은 값이어야 한다.
#[test]
fn validation_loss_uses_the_same_definition_as_training() {
    let dir = temp_dir("loss-one");
    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::None;
    // 학습률 0 → 가중치가 변하지 않으므로 학습 손실과 검증 손실이 같은 모델에서 나온다.
    def.train.optimizer = Optimizer::Sgd { lr: 0.0, momentum: 0.0 };
    def.train.epochs = 1;
    def.train.batch_size = 512;
    def.train.val_split = 0.5;
    def.train.device = test_device();
    def.train.seed = 11;

    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 512), &dir);
    let e = run.last().unwrap();
    let v = e.val_loss.expect("검증 손실");
    // XOR 은 두 분할의 분포가 같으므로 두 손실이 크게 벌어질 이유가 없다.
    assert!((e.train_loss - v).abs() < 0.15, "학습 {} vs 검증 {v}", e.train_loss);
    std::fs::remove_dir_all(&dir).ok();
}

/// B2 의 남은 틈: one-hot 타깃 폭이 출력과 다르면 학습 시작 전에 막는다.
#[test]
fn b2_one_hot_target_width_is_checked_before_training() {
    let dir = temp_dir("b2-onehot");
    // 타깃 3 열 one-hot, 출력은 2 유닛.
    let mut text = String::from("x0,x1,t0,t1,t2\n");
    for i in 0..60 {
        let f = ((i * 7) % 19) as f32 / 10.0 - 1.0;
        let k = i % 3;
        let (a, b, c) = ((k == 0) as u8, (k == 1) as u8, (k == 2) as u8);
        text.push_str(&format!("{f},{},{a},{b},{c}\n", f * 0.5));
    }
    std::fs::write(dir.join("onehot.csv"), text).unwrap();
    let ds = DatasetSpec::new(
        "원핫",
        DataSource::Csv {
            path: "onehot.csv".into(),
            input_cols: vec!["x0".into(), "x1".into()],
            target_cols: vec!["t0".into(), "t1".into(), "t2".into()],
            header: true,
        },
    );

    let mut def = mlp(2, 8, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 1;
    def.train.device = test_device();

    let e = train_expect_failure(def, ds, &dir);
    assert!(e.contains("one-hot"), "one-hot 폭 문제를 짚어 주지 않습니다: {e}");
    assert!(e.contains("3") && e.contains("2"), "폭을 알려 주지 않습니다: {e}");
    assert!(!e.contains("index out of bounds"), "백엔드 오류가 새어 나옵니다: {e}");

    // 폭이 맞으면 통과한다.
    let dir2 = temp_dir("b2-onehot-ok");
    std::fs::copy(dir.join("onehot.csv"), dir2.join("onehot.csv")).unwrap();
    let ds2 = DatasetSpec::new(
        "원핫",
        DataSource::Csv {
            path: "onehot.csv".into(),
            input_cols: vec!["x0".into(), "x1".into()],
            target_cols: vec!["t0".into(), "t1".into(), "t2".into()],
            header: true,
        },
    );
    let mut ok = mlp(2, 8, 3);
    ok.train.loss = Loss::CrossEntropy;
    ok.train.epochs = 2;
    ok.train.device = test_device();
    let run = train_to_end(ok, ds2, &dir2);
    assert_eq!(run.status, RunStatus::Finished);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

// ───────────────────────────── 순환 · 어텐션 (M3) ─────────────────────────────

/// 토큰 `L` 개짜리 시퀀스 CSV. 라벨 = **첫 토큰**(0/1), 나머지는 2..vocab 의 잡음.
///
/// 마지막까지 첫 원소를 기억해야 풀리므로 순환·어텐션이 실제로 동작하는지 본다.
fn memory_sequence_csv(dir: &Path, rows: usize, len: usize, vocab: usize) -> DatasetSpec {
    use std::fmt::Write as _;
    let cols: Vec<String> = (0..len).map(|i| format!("t{i}")).collect();
    let mut text = cols.join(",");
    let _ = writeln!(text, ",y");
    for i in 0..rows {
        let label = i % 2;
        let mut row = vec![label.to_string()];
        for k in 1..len {
            row.push((2 + (i * 7 + k * 13) % (vocab - 2)).to_string());
        }
        let _ = writeln!(text, "{},{label}", row.join(","));
    }
    std::fs::write(dir.join("seq.csv"), text).unwrap();
    DatasetSpec::new(
        "시퀀스",
        DataSource::Csv {
            path: "seq.csv".into(),
            input_cols: cols,
            target_cols: vec!["y".into()],
            header: true,
        },
    )
}

/// `Input[L] → Embedding → (순환/어텐션) → … → Linear(2)` 모델.
fn sequence_model(len: usize, vocab: usize, dim: usize, middle: Vec<LayerKind>) -> ModelDef {
    let mut kinds = vec![
        LayerKind::Input { shape: vec![len] },
        LayerKind::Embedding { vocab, dim },
    ];
    kinds.extend(middle);
    kinds.push(LayerKind::Linear {
        out_features: 2,
        bias: true,
    });
    chain(kinds)
}

#[test]
fn recurrent_and_attention_output_shapes_match_shape_infer() {
    let (len, vocab, dim) = (5usize, 8usize, 6usize);
    let cases: Vec<(&str, Vec<LayerKind>, Vec<usize>)> = vec![
        (
            "LSTM 마지막만",
            vec![LayerKind::Lstm {
                hidden: 4,
                bidirectional: false,
                return_sequence: false,
            }],
            vec![1, 2],
        ),
        (
            "LSTM 시퀀스 → Flatten",
            vec![
                LayerKind::Lstm {
                    hidden: 4,
                    bidirectional: false,
                    return_sequence: true,
                },
                LayerKind::Flatten,
            ],
            vec![1, 2],
        ),
        (
            "양방향 GRU 마지막만",
            vec![LayerKind::Gru {
                hidden: 3,
                bidirectional: true,
                return_sequence: false,
            }],
            vec![1, 2],
        ),
        (
            "양방향 LSTM 시퀀스 → Flatten",
            vec![
                LayerKind::Lstm {
                    hidden: 3,
                    bidirectional: true,
                    return_sequence: true,
                },
                LayerKind::Flatten,
            ],
            vec![1, 2],
        ),
        (
            "어텐션 → Flatten",
            vec![
                LayerKind::MultiHeadAttention { heads: 3, dropout: 0.0 },
                LayerKind::Flatten,
            ],
            vec![1, 2],
        ),
    ];

    for (name, middle, want) in cases {
        let def = sequence_model(len, vocab, dim, middle);
        assert_eq!(inferred_output_shape(&def), vec![2], "{name}");
        let idx = HostTensor::new(vec![1, len], (0..len).map(|i| (i % vocab) as f32).collect());
        let out = run_once(&def, idx);
        assert_eq!(out[0].shape, want, "{name}");
        assert!(out[0].data.iter().all(|v| v.is_finite()), "{name} 에 NaN/Inf");
    }
}

#[test]
fn recurrent_middle_shapes_are_what_shape_infer_says() {
    // 중간 레이어 출력 형상을 직접 확인한다 (Flatten 뒤로 숨지 않게).
    let mut def = ModelDef::new("중간");
    let g = &mut def.graph;
    let i = add(g, LayerKind::Input { shape: vec![5, 6] });
    let seq = add(
        g,
        LayerKind::Lstm {
            hidden: 4,
            bidirectional: true,
            return_sequence: true,
        },
    );
    let o = add(g, LayerKind::Output);
    link(g, i, seq);
    link(g, seq, o);
    assert_eq!(inferred_output_shape(&def), vec![5, 8], "양방향이면 hidden 이 두 배");

    let mut s = Session::load(&def, None, test_device()).unwrap();
    let out = s.run(&[HostTensor::new(vec![2, 5, 6], vec![0.1; 60])]).unwrap();
    assert_eq!(out[0].shape, vec![2, 5, 8]);

    // 어텐션은 형상을 그대로 둔다.
    let att = chain(vec![
        LayerKind::Input { shape: vec![5, 6] },
        LayerKind::MultiHeadAttention { heads: 2, dropout: 0.0 },
    ]);
    assert_eq!(inferred_output_shape(&att), vec![5, 6]);
    let mut s = Session::load(&att, None, test_device()).unwrap();
    assert_eq!(
        s.run(&[HostTensor::new(vec![2, 5, 6], vec![0.1; 60])]).unwrap()[0].shape,
        vec![2, 5, 6]
    );
}

#[test]
fn lstm_learns_to_remember_the_first_token() {
    let dir = temp_dir("lstm-train");
    let (len, vocab) = (6usize, 8usize);
    let ds = memory_sequence_csv(&dir, 600, len, vocab);
    let mut def = sequence_model(
        len,
        vocab,
        8,
        vec![LayerKind::Lstm {
            hidden: 16,
            bidirectional: false,
            return_sequence: false,
        }],
    );
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 25;
    def.train.batch_size = 32;
    def.train.device = test_device();
    def.train.seed = 7;

    let run = train_to_end(def, ds, &dir);
    assert_eq!(run.status, RunStatus::Finished);
    let last = run.last().unwrap();
    let acc = last.val_metric.expect("정확도");
    assert!(
        acc > 0.9,
        "LSTM 이 첫 토큰을 기억하지 못했습니다: {acc} (손실 {})",
        last.train_loss
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn gru_and_attention_also_learn_the_task() {
    for (name, middle) in [
        (
            "GRU",
            vec![LayerKind::Gru {
                hidden: 16,
                bidirectional: false,
                return_sequence: false,
            }],
        ),
        (
            "어텐션",
            vec![
                LayerKind::MultiHeadAttention { heads: 2, dropout: 0.0 },
                LayerKind::Flatten,
            ],
        ),
    ] {
        let dir = temp_dir("seq-train");
        let (len, vocab) = (6usize, 8usize);
        let ds = memory_sequence_csv(&dir, 600, len, vocab);
        let mut def = sequence_model(len, vocab, 8, middle);
        def.train.loss = Loss::CrossEntropy;
        def.train.metric = Metric::Accuracy;
        def.train.optimizer = Optimizer::Adam {
            lr: 1e-2,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        };
        def.train.epochs = 25;
        def.train.batch_size = 32;
        def.train.device = test_device();
        def.train.seed = 7;

        let run = train_to_end(def, ds, &dir);
        let acc = run.last().unwrap().val_metric.expect("정확도");
        assert!(acc > 0.9, "{name} 이 과제를 풀지 못했습니다: {acc}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn sequence_layer_weights_survive_a_safetensors_round_trip() {
    let dir = temp_dir("seq-ckpt");
    let (len, vocab) = (4usize, 6usize);
    let ds = memory_sequence_csv(&dir, 128, len, vocab);
    let mut def = sequence_model(
        len,
        vocab,
        6,
        vec![
            LayerKind::Lstm {
                hidden: 5,
                bidirectional: true,
                return_sequence: true,
            },
            LayerKind::MultiHeadAttention { heads: 2, dropout: 0.0 },
            LayerKind::Flatten,
        ],
    );
    def.train.loss = Loss::CrossEntropy;
    def.train.epochs = 2;
    def.train.batch_size = 32;
    def.train.device = test_device();

    let run = train_to_end(def.clone(), ds, &dir);
    let ckpt = dir.join(run.checkpoint.as_ref().unwrap());

    // 순환·어텐션 파라미터가 빠짐없이 들어 있어야 한다.
    let names: Vec<String> = nl_engine::checkpoint_summary(&ckpt)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for part in [
        "weight_ih",
        "weight_hh",
        "bias_ih",
        "bias_hh",
        "weight_ih_reverse",
        "weight_hh_reverse",
        "q_weight",
        "k_bias",
        "out_weight",
    ] {
        assert!(
            names.iter().any(|n| n.ends_with(part)),
            "{part} 가 체크포인트에 없습니다: {names:?}"
        );
    }

    // 시드를 달리해도 같은 가중치를 얹으면 결과가 같아야 한다.
    let mut a = def.clone();
    a.train.seed = 1;
    let mut b = def;
    b.train.seed = 999;
    let x = HostTensor::new(vec![2, len], vec![0.0, 3.0, 1.0, 5.0, 2.0, 4.0, 0.0, 1.0]);
    let mut sa = Session::load(&a, Some(&ckpt), test_device()).unwrap();
    let mut sb = Session::load(&b, Some(&ckpt), test_device()).unwrap();
    let inputs = [x];
    assert_eq!(sa.run(&inputs).unwrap(), sb.run(&inputs).unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 성능 측정 (NL_BENCH=1) ─────────────────────────────

/// XOR 1000 샘플 × 200 에포크 CPU 소요 시간. 배치 업로드 경로를 바꿀 때 전후 비교용.
#[test]
fn bench_xor_1000_samples_200_epochs() {
    if std::env::var("NL_BENCH").as_deref() != Ok("1") {
        eprintln!("NL_BENCH=1 이 아니어서 건너뜁니다");
        return;
    }
    let dir = temp_dir("bench");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::None;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 200;
    def.train.batch_size = 32;
    def.train.val_split = 0.0;
    def.train.device = test_device();
    def.train.seed = 7;

    let t0 = std::time::Instant::now();
    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 1000), &dir);
    let elapsed = t0.elapsed();
    assert_eq!(run.status, RunStatus::Finished);
    let last = run.last().unwrap();
    println!(
        "BENCH xor 1000×200: {:.3}초 (에포크당 {:.1} ms, 최종 train_loss {:.4})",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / 200.0,
        last.train_loss
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// 순환 레이어의 시퀀스 길이별 비용. 시간축을 한 스텝씩 도는 구조라 길이에 선형으로 는다.
///
/// `NL_BENCH=1`, 장치는 `NL_TEST_DEVICE=gpu` 로 바꿀 수 있다.
#[test]
fn bench_lstm_sequence_lengths() {
    if std::env::var("NL_BENCH").as_deref() != Ok("1") {
        eprintln!("NL_BENCH=1 이 아니어서 건너뜁니다");
        return;
    }
    for len in [32usize, 128, 512] {
        let dir = temp_dir("bench-lstm");
        let rows = 256;
        let ds = memory_sequence_csv(&dir, rows, len, 16);
        let mut def = sequence_model(
            len,
            16,
            32,
            vec![LayerKind::Lstm {
                hidden: 64,
                bidirectional: false,
                return_sequence: false,
            }],
        );
        def.train.loss = Loss::CrossEntropy;
        def.train.metric = Metric::None;
        def.train.optimizer = Optimizer::Adam {
            lr: 1e-3,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        };
        def.train.epochs = 2;
        def.train.batch_size = 32;
        def.train.val_split = 0.0;
        def.train.device = test_device();

        let run = train_to_end(def, ds, &dir);
        assert_eq!(run.status, RunStatus::Finished);
        // 첫 에포크는 셰이더 컴파일·할당이 섞이므로 마지막 에포크로 잰다.
        let last = run.epochs.last().unwrap();
        let steps = rows.div_ceil(32);
        println!(
            "BENCH lstm len={len} hidden=64 batch=32: 스텝당 {:.1} ms (에포크 {:.3}초, {steps} 스텝)",
            last.seconds * 1000.0 / steps as f64,
            last.seconds
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// 샘플당 데이터가 큰 경우(8×8 이미지 + CNN). 배치 업로드 비용이 드러나는 쪽.
#[test]
fn bench_quadrants_cnn() {
    if std::env::var("NL_BENCH").as_deref() != Ok("1") {
        eprintln!("NL_BENCH=1 이 아니어서 건너뜁니다");
        return;
    }
    let dir = temp_dir("bench-cnn");
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 8,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [1, 1],
            bias: true,
        },
        LayerKind::Activation { act: Act::Relu },
        LayerKind::MaxPool2d {
            kernel: [2, 2],
            stride: [2, 2],
        },
        LayerKind::Flatten,
        LayerKind::Linear {
            out_features: 4,
            bias: true,
        },
    ]);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::None;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-3,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 8;
    def.train.batch_size = 32;
    def.train.val_split = 0.0;
    def.train.device = test_device();

    let t0 = std::time::Instant::now();
    let run = train_to_end(def, synthetic(SyntheticKind::Quadrants, 1000), &dir);
    let elapsed = t0.elapsed();
    assert_eq!(run.status, RunStatus::Finished);
    println!(
        "BENCH quadrants-cnn 1000×8: {:.3}초 (에포크당 {:.1} ms)",
        elapsed.as_secs_f64(),
        elapsed.as_secs_f64() * 1000.0 / 8.0
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ───────────────────────────── 데이터 · 장치 ─────────────────────────────

#[test]
fn scan_reports_synthetic_shapes() {
    let info = nl_engine::scan(&synthetic(SyntheticKind::Quadrants, 64), Path::new(".")).unwrap();
    assert_eq!(info.samples, 64);
    assert_eq!(info.input_shape, vec![1, 8, 8]);
    assert_eq!(info.classes.len(), 4);

    let p = nl_engine::preview(&synthetic(SyntheticKind::Spirals, 64), Path::new("."), 3).unwrap();
    assert_eq!(p.len(), 3);
    assert_eq!(p[0].input.shape, vec![2]);
}

#[test]
fn device_list_starts_with_cpu() {
    let list = nl_engine::enumerate();
    assert_eq!(list[0].kind, nl_engine::DeviceKind::Cpu);
    // Auto 는 GPU 를 실제로 돌려 보므로(= 느리다) 여기서 부르지 않는다 — gpu_auto_* 테스트가 맡는다.
    let r = nl_engine::resolve(DevicePref::Cpu);
    assert_eq!(r.pref, DevicePref::Cpu);
    assert!(!r.info.name.is_empty());
    println!("장치 목록: {list:#?}");
}

// ───────────────────────────── GPU (NL_TEST_GPU=1) ─────────────────────────────

/// `Auto` 는 실제로 동작하는 장치를 골라야 한다. 드라이버가 깨진 GPU 는 건너뛴다.
#[test]
fn gpu_auto_picks_a_device_that_actually_works() {
    if std::env::var("NL_TEST_GPU").as_deref() != Ok("1") {
        eprintln!("NL_TEST_GPU=1 이 아니어서 건너뜁니다");
        return;
    }
    let list = nl_engine::enumerate();
    let chosen = nl_engine::resolve(DevicePref::Auto);
    println!("Auto 가 고른 장치: {} ({:?})", chosen.info.name, chosen.pref);
    for d in &list {
        println!("  {}", nl_engine::describe(d.pref));
    }

    // 고른 장치는 검사를 통과했거나(=GPU) CPU 폴백이어야 한다.
    match chosen.pref {
        DevicePref::Cpu => {
            // GPU 가 하나도 쓸 만하지 않았다는 뜻 — 전부 실패로 기록되어 있어야 한다.
            for d in list.iter().skip(1) {
                if matches!(
                    d.kind,
                    nl_engine::DeviceKind::DiscreteGpu | nl_engine::DeviceKind::IntegratedGpu
                ) {
                    assert!(
                        matches!(nl_engine::probe_cached(d.pref), Some(Err(_))),
                        "CPU 로 떨어졌는데 {} 가 실패로 기록되지 않았습니다",
                        d.name
                    );
                }
            }
        }
        p => {
            assert!(nl_engine::probe(p).is_ok(), "Auto 가 검사에 실패한 장치를 골랐습니다");
            let info = list.iter().find(|d| d.pref == p).unwrap();
            assert_ne!(
                info.kind,
                nl_engine::DeviceKind::OtherGpu,
                "소프트웨어 래스터라이저는 후보가 아닙니다"
            );
        }
    }

    // 검사에 실패한 GPU 는 절대 고르지 않는다.
    for d in list.iter().skip(1) {
        if matches!(nl_engine::probe_cached(d.pref), Some(Err(_))) {
            assert_ne!(chosen.pref, d.pref, "실패한 장치를 골랐습니다: {}", d.name);
        }
    }

    // 고른 장치로 실제 학습이 되어야 한다.
    let dir = temp_dir("gpu-auto");
    let mut def = mlp(2, 16, 2);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.epochs = 10;
    def.train.batch_size = 64;
    def.train.device = DevicePref::Auto;
    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 512), &dir);
    assert_eq!(run.status, RunStatus::Finished);
    println!("Auto 학습 장치 = {}", run.device_name);
    std::fs::remove_dir_all(&dir).ok();
}

/// wgpu 어댑터를 이산 GPU 부터 차례로 시도한다. 하나라도 학습에 성공하면 통과이고,
/// 실패한 어댑터는 이유와 함께 출력한다 (드라이버 문제를 숨기지 않으려고).
#[test]
fn gpu_xor_trains_on_wgpu() {
    if std::env::var("NL_TEST_GPU").as_deref() != Ok("1") {
        eprintln!("NL_TEST_GPU=1 이 아니어서 건너뜁니다");
        return;
    }
    let mut gpus: Vec<_> = nl_engine::enumerate().into_iter().skip(1).collect();
    assert!(!gpus.is_empty(), "NL_TEST_GPU=1 인데 wgpu 어댑터가 없습니다");
    gpus.sort_by_key(|d| match d.kind {
        nl_engine::DeviceKind::DiscreteGpu => 0,
        nl_engine::DeviceKind::IntegratedGpu => 1,
        _ => 2,
    });
    println!("wgpu 어댑터 목록: {gpus:#?}");

    let mut failures = Vec::new();
    for target in &gpus {
        println!("→ 시도: {} ({:?}, {})", target.name, target.pref, target.backend);
        let dir = temp_dir("gpu-xor");
        let mut def = mlp(2, 16, 2);
        def.train.loss = Loss::CrossEntropy;
        def.train.metric = Metric::Accuracy;
        def.train.optimizer = Optimizer::Adam {
            lr: 1e-2,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        };
        def.train.epochs = 40;
        def.train.batch_size = 64;
        def.train.device = target.pref;
        def.train.seed = 7;

        match try_train(def.clone(), synthetic(SyntheticKind::Xor, 1024), &dir) {
            Err(e) => {
                println!("  실패: {e}");
                failures.push(format!("{}: {e}", target.name));
                std::fs::remove_dir_all(&dir).ok();
            }
            Ok(run) => {
                assert_eq!(run.status, RunStatus::Finished);
                assert!(!run.device_name.is_empty());
                let acc = run.last().unwrap().val_metric.expect("정확도");
                println!("  GPU 장치 = {}, 정확도 = {acc}", run.device_name);
                assert!(acc >= 0.95, "GPU XOR 정확도가 낮습니다: {acc}");

                // GPU 로 학습한 가중치를 GPU 세션에서 다시 쓴다.
                let ckpt = dir.join(run.checkpoint.as_ref().unwrap());
                let mut s = Session::load(&def, Some(&ckpt), target.pref).unwrap();
                let x = HostTensor::new(vec![4, 2], vec![0.8, 0.8, -0.8, 0.8, 0.8, -0.8, -0.8, -0.8]);
                let out = s.run(&[x]).unwrap();
                assert_eq!(out[0].argmax_last(), vec![0, 1, 1, 0]);
                std::fs::remove_dir_all(&dir).ok();
                if !failures.is_empty() {
                    println!("참고 — 실패한 어댑터: {failures:#?}");
                }
                return;
            }
        }
    }
    panic!("모든 wgpu 어댑터에서 학습에 실패했습니다: {failures:#?}");
}

/// 학습을 끝까지 돌리되 실패를 패닉이 아니라 `Err` 로 돌려준다 (GPU 어댑터 시도용).
fn try_train(def: ModelDef, ds: DatasetSpec, dir: &Path) -> Result<RunRecord, String> {
    let req = TrainRequest {
        run_id: RunId::new(),
        model: def,
        dataset: ds,
        base_dir: dir.to_path_buf(),
        run_dir: dir.join("run"),
        resume_from: None,
    };
    let handle = nl_engine::start(req).map_err(|e| e.to_string())?;
    while let Ok(ev) = handle.events.recv() {
        match ev {
            TrainEvent::Finished { run } => return Ok(run),
            TrainEvent::Failed { error, .. } => return Err(error),
            _ => {}
        }
    }
    Err("Finished/Failed 없이 이벤트 채널이 끊겼습니다 (백엔드가 학습 스레드 밖에서 패닉)".into())
}

// ───────────────────────────── 레이어 템플릿 (nl-core::templates) ─────────────────────────────

/// 템플릿 블록을 `prev` 뒤에 붙이고 블록 출력 노드를 돌려준다.
///
/// 열린 슬롯은 **전부** `prev` 에 잇는다 — 잔차 우회로가 본줄기와 같은 상류를 봐야 형상이 맞는다.
/// 앱 팔레트가 하게 될 일과 같은 순서다 (`instantiate` → `open_inputs` → 노드·엣지 삽입 → 연결).
fn splice(g: &mut Graph, prev: nl_core::NodeId, params: templates::TemplateParams) -> nl_core::NodeId {
    let (nodes, edges) = templates::instantiate(params.name(), [0.0, 0.0], &params).expect("템플릿 생성");
    let open = templates::open_inputs(&nodes, &edges);
    let out = nodes.last().expect("빈 템플릿").id;
    for n in nodes {
        g.add_node(n);
    }
    for e in edges {
        g.edges.insert(e.id, e);
    }
    for p in open {
        assert!(g.add_edge(prev, p).is_some(), "열린 슬롯 {p:?} 연결 실패");
    }
    out
}

fn adam(lr: f64) -> Optimizer {
    Optimizer::Adam {
        lr,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    }
}

/// 손실이 실제로 내려갔는지 — 순전파만이 아니라 **역전파가 블록을 통과했다는** 증거다.
fn assert_loss_improved(run: &RunRecord, what: &str) {
    assert_eq!(run.status, RunStatus::Finished, "{what}: 학습이 끝나지 않았습니다");
    let first = run.epochs.first().expect("에포크 기록").train_loss;
    let last = run.epochs.last().expect("에포크 기록").train_loss;
    assert!(last.is_finite(), "{what}: 손실이 유한하지 않습니다 ({last})");
    assert!(last < first, "{what}: 손실이 줄지 않았습니다 ({first} → {last})");
}

#[test]
fn residual_block_template_trains() {
    let dir = temp_dir("tpl-residual");
    // Input[2] → Linear(16) → [잔차 블록] → Linear(2) → Output
    let mut def = ModelDef::new("잔차");
    let g = &mut def.graph;
    let input = add(g, LayerKind::Input { shape: vec![2] });
    let up = add(
        g,
        LayerKind::Linear {
            out_features: 16,
            bias: true,
        },
    );
    link(g, input, up);
    let block = splice(g, up, templates::TemplateParams::ResidualBlock { width: 16 });
    let head = add(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    );
    link(g, block, head);
    let out = add(g, LayerKind::Output);
    link(g, head, out);

    assert_eq!(inferred_output_shape(&def), vec![2]);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = adam(1e-2);
    def.train.epochs = 12;
    def.train.batch_size = 64;
    def.train.seed = 7;
    def.train.device = test_device();

    let run = train_to_end(def, synthetic(SyntheticKind::Xor, 512), &dir);
    assert_loss_improved(&run, "잔차 블록");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn transformer_block_template_trains_on_a_sequence_task() {
    let dir = temp_dir("tpl-transformer");
    let (len, vocab, d_model) = (8usize, 16usize, 16usize);
    // Input[L] → Embedding → [트랜스포머 블록] → Flatten → Linear(2) → Output
    let mut def = ModelDef::new("트랜스포머");
    let g = &mut def.graph;
    let input = add(g, LayerKind::Input { shape: vec![len] });
    let emb = add(g, LayerKind::Embedding { vocab, dim: d_model });
    link(g, input, emb);
    let block = splice(
        g,
        emb,
        templates::TemplateParams::TransformerBlock {
            d_model,
            heads: 4,
            ff_mult: 2,
        },
    );
    let flat = add(g, LayerKind::Flatten);
    link(g, block, flat);
    let head = add(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    );
    link(g, flat, head);
    let out = add(g, LayerKind::Output);
    link(g, head, out);

    // 블록이 [L, D] 를 보존하므로 Flatten 은 L*D 다.
    assert_eq!(inferred_output_shape(&def), vec![2]);
    let rep = shape::infer(&def.graph);
    assert_eq!(rep.shape(block).expect("블록 출력 형상").sample(), vec![len, d_model]);

    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::None;
    def.train.optimizer = adam(3e-3);
    def.train.epochs = 6;
    def.train.batch_size = 16;
    def.train.val_split = 0.0;
    def.train.seed = 3;
    def.train.device = test_device();

    let ds = memory_sequence_csv(&dir, 128, len, vocab);
    let run = train_to_end(def, ds, &dir);
    assert_loss_improved(&run, "트랜스포머 블록");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn conv_block_template_trains_on_images() {
    let dir = temp_dir("tpl-conv");
    // Input[1,8,8] → [합성곱 블록] → Flatten → Linear(4) → Output
    let mut def = ModelDef::new("합성곱");
    let g = &mut def.graph;
    let input = add(g, LayerKind::Input { shape: vec![1, 8, 8] });
    let block = splice(g, input, templates::TemplateParams::ConvBlock { channels: 8 });
    let flat = add(g, LayerKind::Flatten);
    link(g, block, flat);
    let head = add(
        g,
        LayerKind::Linear {
            out_features: 4,
            bias: true,
        },
    );
    link(g, flat, head);
    let out = add(g, LayerKind::Output);
    link(g, head, out);

    // 합성곱은 8×8 을 유지하고 풀링이 4×4 로 줄인다 → 8 채널.
    let rep = shape::infer(&def.graph);
    assert_eq!(rep.shape(block).expect("블록 출력 형상").sample(), vec![8, 4, 4]);
    assert_eq!(inferred_output_shape(&def), vec![4]);

    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;
    def.train.optimizer = adam(3e-3);
    def.train.epochs = 6;
    def.train.batch_size = 32;
    def.train.seed = 11;
    def.train.device = test_device();

    let run = train_to_end(def, synthetic(SyntheticKind::Quadrants, 256), &dir);
    assert_loss_improved(&run, "합성곱 블록");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stacked_template_blocks_keep_the_shape_and_save_weights() {
    let dir = temp_dir("tpl-stack");
    // 잔차 블록 두 개를 겹쳐 쌓아도 폭이 그대로고, 체크포인트가 남는지까지 본다.
    let mut def = ModelDef::new("2단 잔차");
    let g = &mut def.graph;
    let input = add(g, LayerKind::Input { shape: vec![2] });
    let mut prev = add(
        g,
        LayerKind::Linear {
            out_features: 12,
            bias: true,
        },
    );
    link(g, input, prev);
    for _ in 0..2 {
        prev = splice(g, prev, templates::TemplateParams::ResidualBlock { width: 12 });
    }
    let head = add(
        g,
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
    );
    link(g, prev, head);
    let out = add(g, LayerKind::Output);
    link(g, head, out);

    assert_eq!(inferred_output_shape(&def), vec![2]);
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = adam(1e-2);
    def.train.epochs = 4;
    def.train.batch_size = 64;
    def.train.seed = 5;
    def.train.device = test_device();

    let run = train_to_end(def.clone(), synthetic(SyntheticKind::Xor, 256), &dir);
    assert_eq!(run.status, RunStatus::Finished);
    let ckpt = run.best_checkpoint.clone().or_else(|| run.checkpoint.clone());
    let ckpt = dir.join(ckpt.expect("체크포인트 경로"));
    assert!(ckpt.exists(), "체크포인트가 저장되지 않았습니다: {}", ckpt.display());

    // 저장된 가중치로 추론까지 돌아야 파라미터 이름이 맞는 것이다.
    let mut s = Session::load(&def, Some(&ckpt), test_device()).expect("세션 생성");
    let y = s
        .run(&[HostTensor::new(vec![2, 2], vec![0.1, 0.9, 0.9, 0.1])])
        .expect("추론");
    assert_eq!(y[0].shape, vec![2, 2]);
    std::fs::remove_dir_all(&dir).ok();
}
