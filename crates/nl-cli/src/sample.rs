//! 샘플 프로젝트. `nl-app` 의 `sample.rs` 와 **같은 구조**를 만든다.
//!
//! nl-app 에 의존하지 않으려고 일부러 복제했다 (GUI 크레이트를 명령줄 도구가 끌어올 이유가 없다).
//! 둘이 어긋나지 않는지는 `xor_project_matches_the_app_sample` 테스트가 구조 수준에서 지킨다.
//! 나중에 `nl-core` 로 옮기면 양쪽이 이 함수 하나를 쓰면 된다.

use nl_core::dataset::{DataSource, DatasetSpec, SyntheticKind};
use nl_core::payload::PayloadSpec;
use nl_core::train::{Loss, Metric, Optimizer, TrainConfig};
use nl_core::{
    Act, DatasetId, Edge, EdgeId, LayerKind, ModelDef, ModelId, Node, NodeId, PayloadId, Port, Project, ProjectId,
};

/// 한 줄로 이어지는 그래프. `(kind, 표시 이름)` 을 왼쪽에서 오른쪽으로 배치하고 순서대로 잇는다.
fn chain(model: &mut ModelDef, base: u128, layers: &[(LayerKind, &str)]) {
    let mut prev: Option<NodeId> = None;
    for (i, (kind, name)) in layers.iter().enumerate() {
        let id = NodeId::from_u128(base + i as u128);
        model.graph.nodes.insert(
            id,
            Node { id, name: (*name).to_string(), kind: kind.clone(), pos: [80.0 + i as f32 * 260.0, 160.0] },
        );
        if let Some(from) = prev {
            let eid = EdgeId::from_u128(base + 1000 + i as u128);
            model.graph.edges.insert(eid, Edge { id: eid, from, to: Port::new(id, 0) });
        }
        prev = Some(id);
    }
}

/// XOR 분류 샘플: 합성 데이터셋 + 작은 MLP + tabular 페이로드.
///
/// id 는 `from_u128` 으로 고정한다 — 캔버스 배치·테스트·스크린샷이 실행마다 흔들리지 않는다.
/// (`created` 만은 만든 시각이라 호출마다 달라진다.)
pub fn xor_project() -> Project {
    let mut p = Project::new("XOR 샘플");
    p.id = ProjectId::from_u128(0x2_0000);
    p.description = "2 입력 XOR 을 작은 MLP 로 분류한다. 학습 뷰에서 바로 시작할 수 있다.".into();

    let mut payload = PayloadSpec::tabular("XOR 표", 2, 2);
    payload.id = PayloadId::from_u128(0x2_0001);

    let mut dataset =
        DatasetSpec::new("XOR 합성 1000", DataSource::Synthetic { kind: SyntheticKind::Xor, samples: 1000 });
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

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::{shape, validate, Severity};

    #[test]
    fn xor_project_infers_and_validates_cleanly() {
        let p = xor_project();
        let m = p.models.values().next().unwrap();
        let rep = shape::infer(&m.graph);
        assert!(rep.is_ok(), "{:?}", rep.errors);
        let out = m.graph.output_nodes()[0];
        assert_eq!(rep.shape(out).unwrap().sample(), vec![2], "출력이 2 클래스여야 한다");
        let errors: Vec<_> = validate(&p).into_iter().filter(|i| i.severity == Severity::Error).collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// 같은 함수를 두 번 부르면 `created` 만 빼고 완전히 같아야 한다 (id 를 고정해 뒀다).
    #[test]
    fn xor_project_is_reproducible() {
        let a = xor_project();
        let mut b = xor_project();
        b.created = a.created;
        assert_eq!(a, b);
    }

    #[test]
    fn xor_project_matches_the_app_sample_structure() {
        let p = xor_project();
        let m = p.models.values().next().unwrap();
        // nl-app 의 sample.rs 와 맞춰 둔 값들. 어느 한쪽이 바뀌면 여기서 걸린다.
        assert_eq!(p.id, ProjectId::from_u128(0x2_0000));
        assert_eq!(m.id, ModelId::from_u128(0x2_0003));
        assert_eq!(m.graph.nodes.len(), 5);
        assert_eq!(m.graph.edges.len(), 4);
        assert_eq!(m.train.epochs, 200);
        assert_eq!(m.train.batch_size, 16);
        assert_eq!(p.datasets.len(), 1);
        assert_eq!(p.payloads.len(), 1);
        assert_eq!(m.payload, Some(PayloadId::from_u128(0x2_0001)));
    }
}
