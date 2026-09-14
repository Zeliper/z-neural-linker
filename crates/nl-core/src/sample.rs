//! 예제 프로젝트. 빌더의 "샘플 열기" 와 명령줄 `nl sample` 이 **같은 것**을 만들도록 여기 한 곳에 둔다.
//!
//! id 는 `from_u128` 으로 고정한다 — 캔버스 배치·테스트·스크린샷이 실행마다 흔들리지 않는다.
//! (`Project::created` 만은 만든 시각이라 호출마다 달라진다.)
//!
//! 순수 데이터라 nl-core 밖 의존이 없다. GUI 도 엔진도 필요 없다.

use crate::dataset::{DataSource, DatasetSpec, SyntheticKind};
use crate::gui::{Binding, BuiltinAction};
use crate::payload::{FieldKind, PayloadSpec};
use crate::pipeline::{PNode, PNodeKind, Sink, Source};
use crate::train::{Loss, Metric, Optimizer, TrainConfig};
use crate::{
    Act, BuildSpec, BuildTarget, DatasetId, DevicePref, Edge, EdgeId, LayerKind, Link, LinkId, ModelDef, ModelId,
    Node, NodeId, PNodeId, PayloadId, Pipeline, PipelineId, Port, Project, ProjectId, Widget, WidgetId, WidgetKind,
};

/// 추론 API 파이프라인이 여는 주소. 배포 앱을 바깥 프로그램이 호출하는 입구다.
pub const API_BIND: &str = "127.0.0.1:8799";
/// 사분면 CNN 샘플의 추론 API 주소.
///
/// XOR 과 **다른 포트**를 쓴다 — 두 샘플로 만든 앱을 같은 컴퓨터에서 동시에 띄울 수 있어야 한다.
pub const API_BIND_CNN: &str = "127.0.0.1:8800";
/// 추론 API 가 받는 경로.
pub const API_PATH: &str = "/infer";

/// 파이프라인 캔버스의 열 간격. 빌더의 `pcanvas::chain_pos` 와 같은 값이어야 노드가 같은 자리에 놓인다.
const NODE_GAP: f32 = 250.0;

fn chain_pos(index: usize, row: f32) -> [f32; 2] {
    [80.0 + index as f32 * NODE_GAP, row]
}

/// 이 플랫폼에 맞는 빌드 대상.
fn host_target() -> Option<BuildTarget> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(BuildTarget::LinuxX64)
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(BuildTarget::WindowsX64)
    } else {
        None
    }
}

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

/// 고정 id 로 위젯 하나.
fn with_id(id: WidgetId, kind: WidgetKind, rect: [f32; 4], binding: Option<Binding>) -> Widget {
    let mut w = Widget::new(kind, rect);
    w.id = id;
    w.binding = binding;
    w
}

/// 고정 id 로 파이프라인 노드 하나 (열 `col`, 행 `row`).
fn pnode(id: u128, kind: PNodeKind, name: &str, col: usize, row: f32) -> PNode {
    let mut n = PNode::new(kind, chain_pos(col, row));
    n.id = PNodeId::from_u128(id);
    n.name = name.to_string();
    n
}

fn link(pl: &mut Pipeline, id: u128, from: PNodeId, to: PNodeId) {
    let l = Link { id: LinkId::from_u128(id), from, to };
    pl.links.insert(l.id, l);
}

/// `HttpServer → Model → HttpReply` 세 노드짜리 추론 API 파이프라인.
///
/// 배포 앱을 GUI 없이 헤드리스로 띄워도 이것만으로 쓸모가 있다. `base` 는 노드 id 묶음의 시작값이라
/// 샘플마다 달라야 한다(같은 프로젝트 안에서 id 가 겹치지 않게).
///
/// **주의**: 들어오는 값은 HTTP 본문에서 온 텍스트/JSON 이다. 그래서 이 파이프라인이 실제로 답하려면
/// 모델의 입력 페이로드가 숫자 계열(`Vector`·`Tensor`·`Scalar`)이어야 한다. `Image` 필드는 `Value::Image`
/// 나 3차원 텐서를 요구하므로, 이미지 모델의 API 는 지금 구조로는 답하지 못하고 시간 초과로 끝난다.
pub fn api_pipeline(base: u128, bind: &str, model: ModelId, payload: PayloadId) -> Pipeline {
    let mut pl = Pipeline::new("추론 API");
    pl.id = PipelineId::from_u128(base);
    pl.tick_hz = 60.0;
    let server = pnode(
        base + 1,
        PNodeKind::Source {
            // 샘플은 루프백(127.0.0.1)에만 묶으므로 토큰 없이도 열린다. 바깥에서 닿는 주소로 바꾸려면
            // 토큰을 함께 넣어야 한다 — 그러지 않으면 실행기가 거부한다.
            source: Source::HttpServer { bind: bind.into(), path: API_PATH.into(), token: None },
        },
        "요청",
        0,
        120.0,
    );
    let infer = pnode(base + 2, PNodeKind::Model { model, payload: Some(payload) }, "추론", 1, 120.0);
    let reply = pnode(base + 3, PNodeKind::Sink { sink: Sink::HttpReply { server: server.id } }, "응답", 2, 120.0);
    for n in [server, infer, reply] {
        pl.nodes.insert(n.id, n);
    }
    link(&mut pl, base + 0x100, PNodeId::from_u128(base + 1), PNodeId::from_u128(base + 2));
    link(&mut pl, base + 0x101, PNodeId::from_u128(base + 2), PNodeId::from_u128(base + 3));
    pl
}

// ───────────────────────────── XOR (MLP) ─────────────────────────────

/// XOR 분류 샘플: 합성 데이터셋 + 작은 MLP + tabular 페이로드 + GUI + 파이프라인 둘 + 빌드 설정.
///
/// 입력이 숫자 벡터라 추론 API 가 HTTP 로 그대로 동작한다 — `nl sample → train → build → 배포판 실행` 이
/// 사람 손 없이 끝까지 이어진다.
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
        // 50 에포크면 CPU 에서 몇 초 안에 끝난다 — 샘플은 "열자마자 끝까지" 돌아야 한다.
        epochs: 50,
        batch_size: 16,
        ..TrainConfig::default()
    };

    // ── GUI: 제목 · 결과 표시 · 시작/정지 ──
    let value_widget = WidgetId::from_u128(0x2_2001);
    p.gui.window.title = "XOR 분류기".into();
    p.gui.window.width = 420.0;
    p.gui.window.height = 240.0;
    p.gui.add(with_id(
        WidgetId::from_u128(0x2_2000),
        WidgetKind::Label { text: "XOR 분류기".into() },
        [24.0, 20.0, 240.0, 28.0],
        None,
    ));
    p.gui.add(with_id(value_widget, WidgetKind::Value { prefix: "출력 ".into() }, [24.0, 64.0, 240.0, 48.0], None));
    p.gui.add(with_id(
        WidgetId::from_u128(0x2_2002),
        WidgetKind::Button { text: "시작".into() },
        [24.0, 136.0, 110.0, 32.0],
        Some(Binding::Action { action: BuiltinAction::StartPipeline }),
    ));
    p.gui.add(with_id(
        WidgetId::from_u128(0x2_2003),
        WidgetKind::Button { text: "정지".into() },
        [152.0, 136.0, 110.0, 32.0],
        Some(Binding::Action { action: BuiltinAction::StopPipeline }),
    ));

    // ── 파이프라인 ①: Manual → 모델 → 로그, 그리고 타이머 → 위젯 ──
    let mut pl = Pipeline::new("XOR 시험");
    pl.id = PipelineId::from_u128(0x2_3000);
    pl.tick_hz = 20.0;
    let manual = pnode(0x2_3001, PNodeKind::Source { source: Source::Manual }, "입력", 0, 120.0);
    let infer = pnode(0x2_3002, PNodeKind::Model { model: m.id, payload: Some(payload.id) }, "추론", 1, 120.0);
    let logsink = pnode(0x2_3003, PNodeKind::Sink { sink: Sink::Log }, "로그", 2, 120.0);
    let timer = pnode(0x2_3004, PNodeKind::Source { source: Source::Timer { interval_ms: 500 } }, "박자", 0, 300.0);
    let widget_sink = pnode(
        0x2_3005,
        PNodeKind::Sink { sink: Sink::GuiWidget { widget: value_widget } },
        "화면 표시",
        1,
        300.0,
    );
    for n in [manual, infer, logsink, timer, widget_sink] {
        pl.nodes.insert(n.id, n);
    }
    link(&mut pl, 0x2_3100, PNodeId::from_u128(0x2_3001), PNodeId::from_u128(0x2_3002));
    link(&mut pl, 0x2_3101, PNodeId::from_u128(0x2_3002), PNodeId::from_u128(0x2_3003));
    link(&mut pl, 0x2_3102, PNodeId::from_u128(0x2_3004), PNodeId::from_u128(0x2_3005));

    // ── 파이프라인 ②: HTTP 요청 → 모델 → HTTP 응답 ──
    let api = api_pipeline(0x2_4000, API_BIND, m.id, payload.id);

    // ── 빌드 설정: 호스트 대상 하나만 미리 골라 둔다 ──
    p.settings.build = Some(BuildSpec {
        app_name: "XOR 분류기".into(),
        app_version: "0.1.0".into(),
        targets: host_target().into_iter().collect(),
        entry_pipeline: Some(pl.id),
        autostart: true,
        default_device: DevicePref::Cpu,
        models: vec![m.id],
        ..BuildSpec::default()
    });

    p.pipelines.insert(api.id, api);
    p.pipelines.insert(pl.id, pl);
    p.payloads.insert(payload.id, payload);
    p.datasets.insert(dataset.id, dataset);
    p.models.insert(m.id, m);
    p
}

// ───────────────────────────── 사분면 (CNN) ─────────────────────────────

/// 이미지 CNN 샘플: 8×8 사분면 합성 데이터 + 작은 합성곱 망.
///
/// 추론 API 파이프라인이 함께 들어가지만, 입력이 `Image` 필드라 **HTTP 본문(텍스트·JSON)으로는 답하지
/// 못한다** — 요청은 시간 초과로 끝난다. 캔버스에서 구조를 보여 주는 데 뜻이 있고, 실제로 쓰려면
/// 이미지를 실어 보낼 방법이 필요하다 ([`api_pipeline`] 의 주의 참고).
pub fn quadrants_cnn_project() -> Project {
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
        f.kind = FieldKind::Image { width: 8, height: 8, channels: 1 };
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
            (
                LayerKind::Conv2d { out_channels: 8, kernel: [3, 3], stride: [1, 1], padding: [1, 1], bias: true },
                "특징",
            ),
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

    let api = api_pipeline(0x3_4000, API_BIND_CNN, m.id, payload.id);
    p.pipelines.insert(api.id, api);
    p.payloads.insert(payload.id, payload);
    p.datasets.insert(dataset.id, dataset);
    p.models.insert(m.id, m);
    p
}

/// 샘플 프로젝트를 만드는 함수.
pub type SampleFactory = fn() -> Project;

/// "샘플 열기" 메뉴 항목: (이름, 만드는 함수).
pub const SAMPLES: [(&str, SampleFactory); 2] =
    [("XOR 분류 (MLP)", xor_project), ("사분면 분류 (CNN)", quadrants_cnn_project)];

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{shape, validate, Severity};

    #[test]
    fn samples_infer_cleanly_and_validate_without_errors() {
        for (name, make) in SAMPLES {
            let p = make();
            for m in p.models.values() {
                let rep = shape::infer(&m.graph);
                assert!(rep.is_ok(), "{name}/{}: {:?}", m.name, rep.errors);
            }
            let errors: Vec<_> =
                validate(&p).into_iter().filter(|i| i.severity == Severity::Error).collect();
            assert!(errors.is_empty(), "{name}: {errors:?}");
        }
    }

    /// 같은 함수를 두 번 부르면 `created` 만 빼고 완전히 같아야 한다 (id 를 고정해 뒀다).
    #[test]
    fn samples_are_reproducible() {
        for (name, make) in SAMPLES {
            let a = make();
            let mut b = make();
            b.created = a.created;
            assert_eq!(a, b, "{name}");
        }
    }

    /// 빌더의 `sample.rs` 와 맞춰 둔 값들. 어느 한쪽이 바뀌면 여기서 걸린다.
    #[test]
    fn xor_matches_the_builder_sample() {
        let p = xor_project();
        let m = p.models.values().next().unwrap();
        assert_eq!(p.id, ProjectId::from_u128(0x2_0000));
        assert_eq!(p.name, "XOR 샘플");
        assert_eq!(m.id, ModelId::from_u128(0x2_0003));
        assert_eq!(m.name, "XOR MLP");
        assert_eq!(m.graph.nodes.len(), 5);
        assert_eq!(m.graph.edges.len(), 4);
        assert_eq!(m.train.epochs, 50);
        assert_eq!(m.train.batch_size, 16);
        assert_eq!(m.payload, Some(PayloadId::from_u128(0x2_0001)));
        assert_eq!(p.datasets.len(), 1);
        assert_eq!(p.payloads.len(), 1);

        let rep = shape::infer(&m.graph);
        assert_eq!(rep.shape(m.graph.output_nodes()[0]).unwrap().sample(), vec![2]);

        // GUI: 위젯 4개 + 창 설정.
        assert_eq!(p.gui.widgets.len(), 4);
        assert_eq!(p.gui.window.title, "XOR 분류기");
        assert_eq!(p.gui.window.width, 420.0);
        assert_eq!(p.gui.window.height, 240.0);

        // 시험 파이프라인.
        let test_pl = &p.pipelines[&PipelineId::from_u128(0x2_3000)];
        assert_eq!(test_pl.name, "XOR 시험");
        assert_eq!(test_pl.nodes.len(), 5);
        assert_eq!(test_pl.links.len(), 3);
        assert_eq!(test_pl.tick_hz, 20.0);

        // 빌드 설정.
        let build = p.settings.build.as_ref().expect("빌드 설정이 없다");
        assert_eq!(build.app_name, "XOR 분류기");
        assert_eq!(build.entry_pipeline, Some(test_pl.id));
        assert_eq!(build.default_device, DevicePref::Cpu);
        assert_eq!(build.models, vec![m.id]);
        assert!(!build.arm_input, "배포 앱 입력 무장은 기본 꺼짐");
    }

    #[test]
    fn cnn_matches_the_builder_sample() {
        let p = quadrants_cnn_project();
        let m = p.models.values().next().unwrap();
        assert_eq!(p.id, ProjectId::from_u128(0x3_0000));
        assert_eq!(p.name, "사분면 CNN 샘플");
        assert_eq!(m.id, ModelId::from_u128(0x3_0003));
        assert_eq!(m.name, "사분면 CNN");
        assert_eq!(m.graph.nodes.len(), 7);
        assert_eq!(m.graph.edges.len(), 6);
        assert_eq!(m.train.epochs, 30);
        assert_eq!(m.train.batch_size, 32);
        assert_eq!(m.payload, Some(PayloadId::from_u128(0x3_0001)));

        let rep = shape::infer(&m.graph);
        assert_eq!(rep.shape(m.graph.output_nodes()[0]).unwrap().sample(), vec![4]);

        // 합성 데이터가 1채널이라 인코더도 1채널이어야 한다.
        let pay = &p.payloads[&PayloadId::from_u128(0x3_0001)];
        assert_eq!(pay.name, "사분면 이미지");
        match &pay.inputs[0].kind {
            FieldKind::Image { width, height, channels } => {
                assert_eq!((*width, *height, *channels), (8, 8, 1));
            }
            other => panic!("이미지 필드가 아니다: {other:?}"),
        }
        match &pay.outputs[0].kind {
            FieldKind::ClassLabel { labels } => assert_eq!(labels.len(), 4),
            other => panic!("클래스 라벨이 아니다: {other:?}"),
        }
    }

    #[test]
    fn both_samples_carry_a_working_api_pipeline_shape() {
        for (name, make) in SAMPLES {
            let p = make();
            let api = p.pipelines.values().find(|x| x.name == "추론 API").expect(name);
            assert_eq!(api.nodes.len(), 3, "{name}");
            assert_eq!(api.links.len(), 2, "{name}");
            assert_eq!(api.tick_hz, 60.0, "{name}");

            let server = api
                .nodes
                .values()
                .find(|n| matches!(n.kind, PNodeKind::Source { source: Source::HttpServer { .. } }))
                .unwrap_or_else(|| panic!("{name}: HTTP 서버 노드가 없다"));
            match &server.kind {
                PNodeKind::Source { source: Source::HttpServer { bind, path, token } } => {
                    assert!([API_BIND, API_BIND_CNN].contains(&bind.as_str()), "{name}: 모르는 주소 {bind}");
                    assert_eq!(path, API_PATH);
                    assert!(token.is_none(), "샘플은 루프백이라 토큰 없이 연다");
                    assert!(crate::pipeline::is_loopback_bind(bind), "샘플 주소가 루프백이 아니다");
                }
                _ => unreachable!(),
            }
            // 응답 싱크가 그 서버를 가리킨다 — validate 가 보는 조건이다.
            let reply = api
                .nodes
                .values()
                .find(|n| matches!(n.kind, PNodeKind::Sink { sink: Sink::HttpReply { .. } }))
                .unwrap_or_else(|| panic!("{name}: HTTP 응답 노드가 없다"));
            match &reply.kind {
                PNodeKind::Sink { sink: Sink::HttpReply { server: target } } => assert_eq!(*target, server.id),
                _ => unreachable!(),
            }
        }
    }

    /// 두 샘플로 만든 앱을 동시에 띄울 수 있어야 한다 — 추론 API 주소가 겹치면 안 된다.
    #[test]
    fn the_two_samples_listen_on_different_ports() {
        assert_ne!(API_BIND, API_BIND_CNN);
        let bind_of = |p: &Project| -> String {
            let api = p.pipelines.values().find(|x| x.name == "추론 API").expect("추론 API");
            api.nodes
                .values()
                .find_map(|n| match &n.kind {
                    PNodeKind::Source { source: Source::HttpServer { bind, .. } } => Some(bind.clone()),
                    _ => None,
                })
                .expect("HTTP 서버 노드")
        };
        assert_eq!(bind_of(&xor_project()), API_BIND);
        assert_eq!(bind_of(&quadrants_cnn_project()), API_BIND_CNN);
    }

    /// 두 샘플의 노드 id 묶음이 겹치지 않아야 한다 (한 프로젝트에 둘을 합쳐도 안전하게).
    #[test]
    fn sample_id_ranges_do_not_collide() {
        let (a, b) = (xor_project(), quadrants_cnn_project());
        for k in a.pipelines.keys() {
            assert!(!b.pipelines.contains_key(k), "파이프라인 id 충돌: {k:?}");
        }
        for k in a.models.keys() {
            assert!(!b.models.contains_key(k), "모델 id 충돌: {k:?}");
        }
        for k in a.payloads.keys() {
            assert!(!b.payloads.contains_key(k), "페이로드 id 충돌: {k:?}");
        }
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
