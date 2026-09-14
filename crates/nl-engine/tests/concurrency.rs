//! 동시 학습 가드. **별도 시험 바이너리인 것이 중요하다.**
//!
//! 가드는 프로세스 전역 카운터를 본다. `tests/engine.rs` 안에 두면 같은 프로세스의 다른 시험들이
//! `allow_concurrent: true` 로 학습을 돌리는 바람에 카운터가 흔들려 무엇을 재는지 알 수 없게 된다.
//! cargo 는 시험 바이너리를 하나씩 돌리므로, 파일을 나누면 이 프로세스에는 우리 학습만 있다.
//!
//! 그래서 시험도 **하나**다. 이 파일 안에서 둘로 나누면 둘이 서로를 밟는다.

use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::model::{Act, LayerKind, ModelDef, Node, Port};
use nl_core::{DatasetSpec, DevicePref, Loss, Optimizer, RunId};
use nl_engine::{HostTensor, Session, TrainEvent, TrainHandle, TrainRequest};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn temp_dir(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("nl-conc-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&p).expect("임시 폴더");
    p
}

fn mlp(input: usize, hidden: usize, output: usize) -> ModelDef {
    let mut def = ModelDef::new("t");
    let g = &mut def.graph;
    let mut prev = None;
    for k in [
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
        LayerKind::Output,
    ] {
        let id = g.add_node(Node::new(k, [0.0, 0.0]));
        if let Some(p) = prev {
            g.add_edge(p, Port::new(id, 0)).expect("엣지");
        }
        prev = Some(id);
    }
    def.train.loss = Loss::CrossEntropy;
    def.train.optimizer = Optimizer::Adam {
        lr: 1e-2,
        beta1: 0.9,
        beta2: 0.999,
        eps: 1e-8,
    };
    def.train.batch_size = 16;
    def.train.val_split = 0.0;
    def.train.device = DevicePref::Cpu;
    def
}

fn spawn(def: ModelDef, dir: &Path, allow_concurrent: bool) -> anyhow::Result<TrainHandle> {
    let run_id = RunId::new();
    nl_engine::start(TrainRequest {
        run_id,
        model: def,
        dataset: DatasetSpec::new(
            "syn",
            DataSource::Synthetic {
                kind: SyntheticKind::Xor,
                samples: 256,
            },
        ),
        base_dir: dir.to_path_buf(),
        run_dir: dir.join(format!("run-{}", run_id.short())),
        resume_from: None,
        allow_concurrent,
    })
}

/// 우리가 멈출 때까지 안 끝나는 학습.
fn long_training(dir: &Path, allow_concurrent: bool) -> anyhow::Result<TrainHandle> {
    let mut def = mlp(2, 8, 2);
    def.train.epochs = 100_000;
    spawn(def, dir, allow_concurrent)
}

fn drain(handle: TrainHandle) {
    while let Ok(ev) = handle.events.recv() {
        if matches!(ev, TrainEvent::Finished { .. } | TrainEvent::Failed { .. }) {
            break;
        }
    }
}

/// `active_count()` 가 `want` 가 될 때까지 기다린다. 스레드가 자리를 잡거나 놓는 데 잠깐 걸린다.
fn wait_for(want: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if nl_engine::active_count() == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "active_count 가 {want} 가 되지 않았다 (지금 {})",
        nl_engine::active_count()
    );
}

#[test]
fn the_concurrency_guard_blocks_a_second_training_but_never_inference() {
    let dir = temp_dir("guard");
    assert_eq!(nl_engine::active_count(), 0, "시작 전에는 도는 학습이 없다");

    // ── 1. 첫 학습이 자리를 잡는다 ──
    let first = long_training(&dir, false).expect("첫 학습은 된다");
    wait_for(1);

    // ── 2. 두 번째는 거부된다 ──
    // `TrainHandle` 은 Debug 가 없어 `expect_err` 를 못 쓴다.
    let e = match long_training(&dir, false) {
        Ok(h) => {
            h.stop();
            panic!("두 번째 학습이 거부되지 않았다");
        }
        Err(e) => format!("{e:#}"),
    };
    assert!(e.contains("이미 진행 중"), "{e}");
    assert!(e.contains("allow_concurrent"), "해결 방법을 알려 줘야 한다: {e}");
    assert_eq!(nl_engine::active_count(), 1, "거부된 학습이 자리를 차지하면 안 된다");

    // ── 3. 학습 중에도 추론은 열린다 ──
    // 가드는 학습끼리만 막는다. CPU 에서는 안전하고 GPU 에서도 느려질 뿐이다.
    let def = mlp(2, 8, 2);
    let mut s = Session::load(&def, None, DevicePref::Cpu).expect("학습 중에도 세션이 열려야 한다");
    let y = s.run(&[HostTensor::new(vec![1, 2], vec![0.3, 0.7])]).expect("추론");
    assert_eq!(y[0].shape, vec![1, 2]);
    assert_eq!(nl_engine::active_count(), 1, "추론이 학습 자리를 건드리면 안 된다");

    // ── 4. 명시적으로 허용하면 둘이 함께 돈다 ──
    let mut short = mlp(2, 8, 2);
    short.train.epochs = 1;
    short.train.batch_size = 64;
    let second = spawn(short, &dir, true).expect("허용하면 시작된다");
    wait_for(2);
    drain(second);
    wait_for(1);

    // ── 5. 첫 학습이 끝나면 자리가 빈다 ──
    first.stop();
    drain(first);
    wait_for(0);

    // ── 6. 그 뒤에는 평소대로 다시 시작된다 ──
    // 한 번 막힌 뒤로 영영 못 돌게 되면 사용자는 앱을 다시 켜는 수밖에 없다.
    let again = long_training(&dir, false).expect("끝난 뒤에는 다시 시작된다");
    wait_for(1);
    again.stop();
    drain(again);
    wait_for(0);

    std::fs::remove_dir_all(&dir).ok();
}
