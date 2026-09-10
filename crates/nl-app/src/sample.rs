//! 새 프로젝트 기본 템플릿과 "샘플 열기" 메뉴가 쓰는 예제 프로젝트.
//!
//! id 는 `from_u128` 으로 고정한다 — 캔버스 배치·테스트·스크린샷이 실행마다 흔들리지 않는다.

use nl_core::dataset::{DataSource, DatasetSpec, SyntheticKind};
use nl_core::payload::PayloadSpec;
use nl_core::train::{Loss, Metric, Optimizer, TrainConfig};
use nl_core::{
    Act, DatasetId, Edge, EdgeId, LayerKind, ModelDef, ModelId, Node, NodeId, PayloadId, Port, Project, ProjectId,
};

/// 한 줄로 이어지는 그래프를 만든다. `(kind, 표시 이름)` 목록을 받아 왼쪽에서 오른쪽으로 배치하고 순서대로 잇는다.
fn chain(model: &mut ModelDef, base: u128, layers: &[(LayerKind, &str)]) {
    let mut prev: Option<NodeId> = None;
    for (i, (kind, name)) in layers.iter().enumerate() {
        let id = NodeId::from_u128(base + i as u128);
        let node = Node {
            id,
            name: (*name).to_string(),
            kind: kind.clone(),
            pos: [80.0 + i as f32 * 260.0, 160.0],
        };
        model.graph.nodes.insert(id, node);
        if let Some(from) = prev {
            let eid = EdgeId::from_u128(base + 1000 + i as u128);
            model.graph.edges.insert(eid, Edge { id: eid, from, to: Port::new(id, 0) });
        }
        prev = Some(id);
    }
}

/// "새로 만들기" 가 주는 빈 뼈대: 모델 하나에 Input → Output 만.
/// (레이어는 캔버스 빈 곳 우클릭 → 팔레트로 사이에 끼워 넣는다)
pub fn new_project() -> Project {
    let mut p = Project::new("새 프로젝트");
    let mut m = ModelDef::new("모델");
    chain(&mut m, 0x1_0000, &[(LayerKind::Input { shape: vec![4] }, "입력"), (LayerKind::Output, "출력")]);
    // 사이에 레이어를 넣을 자리를 비워 둔다.
    if let Some(n) = m.graph.nodes.get_mut(&NodeId::from_u128(0x1_0001)) {
        n.pos = [700.0, 160.0];
    }
    p.models.insert(m.id, m);
    p
}

/// XOR 분류 샘플: 합성 데이터셋 + 작은 MLP + tabular 페이로드.
pub fn xor_project() -> Project {
    let mut p = Project::new("XOR 샘플");
    p.id = ProjectId::from_u128(0x2_0000);
    p.description = "2 입력 XOR 을 작은 MLP 로 분류한다. 학습 뷰에서 바로 시작할 수 있다.".into();

    let mut payload = PayloadSpec::tabular("XOR 표", 2, 2);
    payload.id = PayloadId::from_u128(0x2_0001);

    let mut dataset = DatasetSpec::new("XOR 합성 1000", DataSource::Synthetic { kind: SyntheticKind::Xor, samples: 1000 });
    dataset.id = DatasetId::from_u128(0x2_0002);
    dataset.payload = Some(payload.id);

    let mut m = ModelDef::new("XOR MLP");
    m.id = ModelId::from_u128(0x2_0003);
    m.description = "Input[2] → Linear 16 → ReLU → Linear 2 → Output".into();
    m.payload = Some(payload.id);
    chain(
        &mut m,
        0x2_1000,
        &[
            (LayerKind::Input { shape: vec![2] }, "입력"),
            (LayerKind::Linear { out_features: 16, bias: true }, "은닉"),
            (LayerKind::Activation { act: Act::Relu }, ""),
            (LayerKind::Linear { out_features: 2, bias: true }, "분류"),
            (LayerKind::Output, "출력"),
        ],
    );
    m.train = TrainConfig {
        dataset: Some(dataset.id),
        optimizer: Optimizer::default_adam(),
        loss: Loss::CrossEntropy,
        metric: Metric::Accuracy,
        epochs: 200,
        batch_size: 16,
        ..TrainConfig::default()
    };

    p.payloads.insert(payload.id, payload);
    p.datasets.insert(dataset.id, dataset);
    p.models.insert(m.id, m);
    p
}

/// 이미지 CNN 샘플: 8×8 사분면 합성 데이터 + 작은 합성곱 망.
pub fn cnn_project() -> Project {
    let mut p = Project::new("사분면 CNN 샘플");
    p.id = ProjectId::from_u128(0x3_0000);
    p.description = "8×8 흑백 이미지에서 밝은 사각형이 어느 사분면에 있는지 맞춘다.".into();

    let mut payload = PayloadSpec::image_classifier(
        "사분면 이미지",
        8,
        8,
        vec!["좌상".into(), "우상".into(), "좌하".into(), "우하".into()],
    );
    payload.id = PayloadId::from_u128(0x3_0001);
    // 합성 데이터는 이미 1채널이다 — 인코더도 흑백으로 맞춘다.
    if let Some(f) = payload.inputs.first_mut() {
        f.kind = nl_core::payload::FieldKind::Image { width: 8, height: 8, channels: 1 };
    }

    let mut dataset =
        DatasetSpec::new("사분면 합성 2000", DataSource::Synthetic { kind: SyntheticKind::Quadrants, samples: 2000 });
    dataset.id = DatasetId::from_u128(0x3_0002);
    dataset.payload = Some(payload.id);

    let mut m = ModelDef::new("사분면 CNN");
    m.id = ModelId::from_u128(0x3_0003);
    m.description = "Input[1,8,8] → Conv2d 8 → ReLU → MaxPool → Flatten → Linear 4 → Output".into();
    m.payload = Some(payload.id);
    chain(
        &mut m,
        0x3_1000,
        &[
            (LayerKind::Input { shape: vec![1, 8, 8] }, "입력"),
            (LayerKind::Conv2d { out_channels: 8, kernel: [3, 3], stride: [1, 1], padding: [1, 1], bias: true }, "특징"),
            (LayerKind::Activation { act: Act::Relu }, ""),
            (LayerKind::MaxPool2d { kernel: [2, 2], stride: [2, 2] }, ""),
            (LayerKind::Flatten, ""),
            (LayerKind::Linear { out_features: 4, bias: true }, "분류"),
            (LayerKind::Output, "출력"),
        ],
    );
    m.train = TrainConfig {
        dataset: Some(dataset.id),
        optimizer: Optimizer::default_adam(),
        loss: Loss::CrossEntropy,
        metric: Metric::Accuracy,
        epochs: 30,
        batch_size: 32,
        ..TrainConfig::default()
    };

    p.payloads.insert(payload.id, payload);
    p.datasets.insert(dataset.id, dataset);
    p.models.insert(m.id, m);
    p
}

/// 샘플 프로젝트를 만드는 함수.
pub type SampleFactory = fn() -> Project;

/// "샘플 열기" 메뉴 항목: (이름, 만드는 함수).
pub const SAMPLES: [(&str, SampleFactory); 2] =
    [("XOR 분류 (MLP)", xor_project), ("사분면 분류 (CNN)", cnn_project)];

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::{shape, validate, Severity};

    #[test]
    fn samples_infer_cleanly_and_validate_without_errors() {
        for (name, make) in SAMPLES {
            let p = make();
            for m in p.models.values() {
                let rep = shape::infer(&m.graph);
                assert!(rep.is_ok(), "{name}/{}: {:?}", m.name, rep.errors);
            }
            let issues = validate(&p);
            let errors: Vec<_> = issues.iter().filter(|i| i.severity == Severity::Error).collect();
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }

    #[test]
    fn xor_output_is_two_classes() {
        let p = xor_project();
        let m = p.models.values().next().unwrap();
        let rep = shape::infer(&m.graph);
        let out = m.graph.output_nodes()[0];
        assert_eq!(rep.shape(out).unwrap().sample(), vec![2]);
        assert_eq!(m.train.epochs, 200);
        assert_eq!(m.train.batch_size, 16);
    }

    #[test]
    fn cnn_output_is_four_classes() {
        let p = cnn_project();
        let m = p.models.values().next().unwrap();
        let rep = shape::infer(&m.graph);
        let out = m.graph.output_nodes()[0];
        assert_eq!(rep.shape(out).unwrap().sample(), vec![4]);
    }

    #[test]
    fn new_project_is_a_connected_skeleton() {
        let p = new_project();
        let m = p.models.values().next().unwrap();
        assert_eq!(m.graph.nodes.len(), 2);
        assert_eq!(m.graph.edges.len(), 1);
        assert!(shape::infer(&m.graph).is_ok());
    }
}
