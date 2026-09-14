//! ONNX 내보내기 왕복 검증.
//!
//! 우리가 쓴 `.onnx` 를 **다른 구현**(`tract-onnx`)으로 읽어 같은 입력에 같은 값이 나오는지 본다.
//! 우리 코드끼리 비교하면 규약을 잘못 이해한 경우를 못 잡는다 — 바깥 구현이어야 의미가 있다.
//!
//! `tract-onnx` 는 `[dev-dependencies]` 라 배포 바이너리에는 들어가지 않는다(넣으면 +34 MiB).

use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::model::{Act, Graph, LayerKind, ModelDef, Node, Port};
use nl_core::{DatasetSpec, DevicePref, Loss, Metric, NodeId, Optimizer, RunId, RunRecord, RunStatus};
use nl_engine::onnx::{self, Batch, ExportOptions};
use nl_engine::{HostTensor, Session, TrainEvent, TrainRequest};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tract_onnx::prelude::*;

// ───────────────────────────── 도우미 ─────────────────────────────

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("nl-onnx-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&p).expect("임시 폴더");
    p
}

fn add(g: &mut Graph, k: LayerKind) -> NodeId {
    g.add_node(Node::new(k, [0.0, 0.0]))
}

fn link(g: &mut Graph, a: NodeId, b: NodeId) {
    g.add_edge(a, Port::new(b, 0)).expect("엣지");
}

fn linear(out_features: usize) -> LayerKind {
    LayerKind::Linear {
        out_features,
        bias: true,
    }
}

/// 일자형 그래프 + Output.
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

fn adam(lr: f64) -> Optimizer {
    Optimizer::Adam {
        lr,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    }
}

/// 학습을 끝까지 돌리고 기록을 돌려준다.
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
    panic!("Finished/Failed 없이 채널이 끊겼습니다");
}

/// 학습해서 체크포인트 경로를 돌려준다.
fn train_and_checkpoint(mut def: ModelDef, ds: DatasetSpec, dir: &Path, epochs: usize) -> (ModelDef, PathBuf) {
    def.train.optimizer = adam(5e-3);
    def.train.epochs = epochs;
    def.train.batch_size = 32;
    def.train.seed = 7;
    def.train.device = DevicePref::Cpu;
    let run = train_to_end(def.clone(), ds, dir);
    assert_eq!(run.status, RunStatus::Finished);
    let ckpt = run
        .best_checkpoint
        .clone()
        .or_else(|| run.checkpoint.clone())
        .expect("체크포인트 경로");
    (def, dir.join(ckpt))
}

/// 내보낸 ONNX 를 tract 로 읽어 한 번 돌린다.
fn tract_run(onnx: &Path, input: &HostTensor) -> Vec<f32> {
    let shape: Vec<usize> = input.shape.clone();
    let model = tract_onnx::onnx()
        .model_for_path(onnx)
        .expect("tract 가 ONNX 를 읽지 못했습니다")
        .with_input_fact(0, f32::fact(shape.as_slice()).into())
        .expect("입력 형상 지정")
        .into_optimized()
        .expect("tract 최적화")
        .into_runnable()
        .expect("tract 실행 계획");

    let t = Tensor::from_shape(&shape, &input.data).expect("입력 텐서");
    let out = model.run(tvec!(t.into())).expect("tract 실행");
    out[0]
        .to_plain_array_view::<f32>()
        .expect("f32 출력")
        .iter()
        .copied()
        .collect()
}

/// 우리 Session 과 tract 의 결과가 `tol` 안에서 같은지 본다.
fn assert_same(what: &str, ours: &[f32], theirs: &[f32], tol: f32) {
    assert_eq!(ours.len(), theirs.len(), "{what}: 출력 길이가 다릅니다");
    let mut worst = 0.0f32;
    let mut at = 0usize;
    for (i, (a, b)) in ours.iter().zip(theirs).enumerate() {
        let d = (a - b).abs();
        if d > worst {
            worst = d;
            at = i;
        }
    }
    assert!(
        worst <= tol,
        "{what}: {at} 번째에서 {worst} 만큼 다릅니다 (우리 {}, tract {}), 허용 {tol}",
        ours[at],
        theirs[at]
    );
}

/// 학습 → 내보내기 → tract 로 읽어 우리 Session 과 비교, 한 묶음.
fn round_trip(tag: &str, def: ModelDef, ds: DatasetSpec, epochs: usize, input: HostTensor) -> onnx::ExportReport {
    let dir = temp_dir(tag);
    let (def, ckpt) = train_and_checkpoint(def, ds, &dir, epochs);
    let out = dir.join("model.onnx");
    let report = onnx::export(&def, &ckpt, &out, ExportOptions::default()).expect("ONNX 내보내기");
    assert!(out.exists(), "{tag}: ONNX 파일이 없습니다");
    assert!(report.unsupported.is_empty());
    assert!(report.nodes > 0 && report.initializers > 0);

    let mut s = Session::load(&def, Some(&ckpt), DevicePref::Cpu).expect("세션");
    let ours = s.run(std::slice::from_ref(&input)).expect("우리 추론");
    let theirs = tract_run(&out, &input);
    assert_same(tag, &ours[0].data, &theirs, 1e-4);

    std::fs::remove_dir_all(&dir).ok();
    report
}

fn synthetic(kind: SyntheticKind, samples: usize) -> DatasetSpec {
    DatasetSpec::new("syn", DataSource::Synthetic { kind, samples })
}

// ───────────────────────────── 왕복 테스트 ─────────────────────────────

#[test]
fn xor_mlp_round_trips_through_tract() {
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![2] },
        linear(16),
        LayerKind::Activation { act: Act::Relu },
        LayerKind::Dropout { p: 0.25 }, // 추론에서 항등 — 내보내기에서 사라져야 한다
        linear(2),
    ]);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;

    let x = HostTensor::new(vec![3, 2], vec![0.1, 0.9, 0.9, 0.1, 0.8, 0.8]);
    let report = round_trip("xor", def, synthetic(SyntheticKind::Xor, 256), 6, x);

    // Dropout 은 노드를 만들지 않는다: MatMul·Add·Relu·MatMul·Add·Identity = 6.
    assert_eq!(report.nodes, 6, "Dropout 이 노드를 남겼습니다");
}

#[test]
fn quadrants_cnn_round_trips_through_tract() {
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 6,
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
        linear(4),
    ]);
    def.train.loss = Loss::CrossEntropy;
    def.train.metric = Metric::Accuracy;

    // BatchNorm 이 있으므로 running 통계가 실제로 실려야 값이 맞는다.
    let data: Vec<f32> = (0..128).map(|i| (i % 9) as f32 * 0.11).collect();
    let x = HostTensor::new(vec![2, 1, 8, 8], data);
    round_trip("cnn", def, synthetic(SyntheticKind::Quadrants, 256), 4, x);
}

#[test]
fn global_avg_pool_and_reshape_round_trip() {
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![1, 8, 8] },
        LayerKind::Conv2d {
            out_channels: 5,
            kernel: [3, 3],
            stride: [1, 1],
            padding: [1, 1],
            bias: false, // 편향 없는 Conv 도 확인한다
        },
        LayerKind::Activation { act: Act::Silu },
        LayerKind::GlobalAvgPool, // ONNX 는 [N,C,1,1], 우리는 [N,C]
        LayerKind::Reshape { shape: vec![5, 1] },
        LayerKind::Flatten,
        linear(4),
    ]);
    def.train.loss = Loss::CrossEntropy;

    let data: Vec<f32> = (0..64).map(|i| ((i * 7) % 13) as f32 * 0.07).collect();
    let x = HostTensor::new(vec![1, 1, 8, 8], data);
    round_trip("gap", def, synthetic(SyntheticKind::Quadrants, 192), 3, x);
}

#[test]
fn residual_and_concat_round_trip() {
    // Input → Linear(8) → [Linear(8) → GELU → Linear(8)] → Add(우회) → Concat(우회) → Linear(2)
    // 잔차 덧셈, 이어붙이기, erf 전개 GELU 를 한 그래프에서 본다.
    let mut def = ModelDef::new("잔차");
    let g = &mut def.graph;
    let input = add(g, LayerKind::Input { shape: vec![2] }); // Spirals 는 2 입력이다
    let stem = add(g, linear(8));
    link(g, input, stem);

    let h1 = add(g, linear(8));
    link(g, stem, h1);
    let act = add(g, LayerKind::Activation { act: Act::Gelu });
    link(g, h1, act);
    let h2 = add(g, linear(8));
    link(g, act, h2);

    let sum = add(g, LayerKind::Add);
    g.add_edge(stem, Port::new(sum, 0)).expect("잔차 우회");
    g.add_edge(h2, Port::new(sum, 1)).expect("본줄기");

    let cat = add(g, LayerKind::Concat { dim: 0 });
    g.add_edge(sum, Port::new(cat, 0)).expect("합");
    g.add_edge(stem, Port::new(cat, 1)).expect("우회");

    let head = add(g, linear(2));
    link(g, cat, head);
    let out = add(g, LayerKind::Output);
    link(g, head, out);

    def.train.loss = Loss::CrossEntropy;

    let x = HostTensor::new(vec![3, 2], vec![0.2, -0.4, 0.9, 0.1, -1.1, 0.3]);
    round_trip("residual", def, synthetic(SyntheticKind::Spirals, 256), 4, x);
}

#[test]
fn embedding_and_layer_norm_round_trip() {
    // Input[6] → Embedding(12, 8) → LayerNorm → Flatten → Linear(2)
    // 정수 인덱스 Cast + Gather, opset 17 LayerNormalization 을 본다.
    let dir = temp_dir("embed");
    let ds = memory_sequence_csv(&dir, 128, 6, 12);
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![6] },
        LayerKind::Embedding { vocab: 12, dim: 8 },
        LayerKind::LayerNorm { eps: 1e-5 },
        LayerKind::Flatten,
        linear(2),
    ]);
    def.train.loss = Loss::CrossEntropy;
    def.train.val_split = 0.0;

    let (def, ckpt) = train_and_checkpoint(def, ds, &dir, 3);
    let out = dir.join("model.onnx");
    onnx::export(&def, &ckpt, &out, ExportOptions::default()).expect("내보내기");

    let x = HostTensor::new(
        vec![2, 6],
        vec![0.0, 3.0, 7.0, 11.0, 1.0, 5.0, 2.0, 2.0, 9.0, 4.0, 0.0, 8.0],
    );
    let mut s = Session::load(&def, Some(&ckpt), DevicePref::Cpu).expect("세션");
    let ours = s.run(std::slice::from_ref(&x)).expect("우리 추론");
    assert_same("embedding", &ours[0].data, &tract_run(&out, &x), 1e-4);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_activation_matches_tract() {
    // 활성화 8종을 한 번씩 통과시켜 본다. Gelu(erf 전개)와 Silu(분해)가 특히 중요하다.
    for act in Act::ALL {
        let dir = temp_dir("act");
        let mut def = chain(vec![
            LayerKind::Input { shape: vec![2] },
            linear(6),
            LayerKind::Activation { act },
            linear(2),
        ]);
        def.train.loss = Loss::CrossEntropy;
        // LogSoftmax 는 CrossEntropy 와 함께 쓰지 말라고 되어 있어 중간에만 둔다 — 여기서는 중간이다.
        let (def, ckpt) = train_and_checkpoint(def, synthetic(SyntheticKind::Xor, 128), &dir, 2);
        let out = dir.join("model.onnx");
        onnx::export(&def, &ckpt, &out, ExportOptions::default()).expect("내보내기");

        let x = HostTensor::new(vec![3, 2], vec![0.7, -0.3, -1.2, 0.4, 0.0, 1.5]);
        let mut s = Session::load(&def, Some(&ckpt), DevicePref::Cpu).expect("세션");
        let ours = s.run(std::slice::from_ref(&x)).expect("우리 추론");
        assert_same(&format!("{act:?}"), &ours[0].data, &tract_run(&out, &x), 1e-4);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn dynamic_batch_really_accepts_different_batch_sizes() {
    let dir = temp_dir("batch");
    let mut def = chain(vec![
        LayerKind::Input { shape: vec![2] },
        linear(8),
        LayerKind::Activation { act: Act::Tanh },
        linear(2),
    ]);
    def.train.loss = Loss::CrossEntropy;
    let (def, ckpt) = train_and_checkpoint(def, synthetic(SyntheticKind::Xor, 128), &dir, 2);
    let out = dir.join("model.onnx");
    onnx::export(&def, &ckpt, &out, ExportOptions::default()).expect("내보내기");

    let mut s = Session::load(&def, Some(&ckpt), DevicePref::Cpu).expect("세션");
    for batch in [1usize, 5] {
        let data: Vec<f32> = (0..batch * 2).map(|i| i as f32 * 0.3 - 0.5).collect();
        let x = HostTensor::new(vec![batch, 2], data);
        let ours = s.run(std::slice::from_ref(&x)).expect("우리 추론");
        assert_same(&format!("배치 {batch}"), &ours[0].data, &tract_run(&out, &x), 1e-4);
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fixed_batch_is_written_as_a_concrete_dimension() {
    let dir = temp_dir("fixed");
    let mut def = chain(vec![LayerKind::Input { shape: vec![2] }, linear(2)]);
    def.train.loss = Loss::CrossEntropy;
    let (def, ckpt) = train_and_checkpoint(def, synthetic(SyntheticKind::Xor, 64), &dir, 1);
    let out = dir.join("fixed.onnx");
    onnx::export(
        &def,
        &ckpt,
        &out,
        ExportOptions {
            opset: onnx::DEFAULT_OPSET,
            batch: Batch::Fixed(4),
        },
    )
    .expect("내보내기");

    // 배치를 4 로 박았으니 tract 가 형상 지정 없이도 읽고 돌려야 한다.
    let model = tract_onnx::onnx()
        .model_for_path(&out)
        .expect("읽기")
        .into_optimized()
        .expect("최적화")
        .into_runnable()
        .expect("실행 계획");
    let t = Tensor::from_shape(&[4usize, 2], &[0.1f32; 8]).expect("입력");
    let y = model.run(tvec!(t.into())).expect("실행");
    assert_eq!(y[0].shape(), &[4, 2]);

    let mut s = Session::load(&def, Some(&ckpt), DevicePref::Cpu).expect("세션");
    let ours = s.run(&[HostTensor::new(vec![4, 2], vec![0.1; 8])]).expect("우리 추론");
    let theirs: Vec<f32> = y[0].to_plain_array_view::<f32>().unwrap().iter().copied().collect();
    assert_same("고정 배치", &ours[0].data, &theirs, 1e-4);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exported_file_declares_opset_17_and_our_producer() {
    let dir = temp_dir("meta");
    let mut def = chain(vec![LayerKind::Input { shape: vec![2] }, linear(2)]);
    def.train.loss = Loss::CrossEntropy;
    let (def, ckpt) = train_and_checkpoint(def, synthetic(SyntheticKind::Xor, 64), &dir, 1);
    let out = dir.join("meta.onnx");
    onnx::export(&def, &ckpt, &out, ExportOptions::default()).expect("내보내기");

    // tract 의 protobuf 타입으로 다시 읽어 머리말을 확인한다.
    let proto = tract_onnx::onnx().proto_model_for_path(&out).expect("protobuf 읽기");
    assert_eq!(proto.producer_name, "neural-linker");
    let ai_onnx = proto
        .opset_import
        .iter()
        .find(|o| o.domain.is_empty())
        .expect("기본 도메인 opset");
    assert_eq!(ai_onnx.version, 17);
    assert_eq!(proto.ir_version, 8);
    std::fs::remove_dir_all(&dir).ok();
}

/// `[t0..t{len-1}, y]` CSV. 첫 열이 곧 정답이라 금방 학습된다.
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
