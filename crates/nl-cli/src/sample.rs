//! 샘플 프로젝트. `crates/nl-app/src/sample.rs` 의 `xor_project` 와 **같은 구조**를 만든다.
//!
//! nl-app 에 의존하지 않으려고 복제했다 — 명령줄 도구가 GUI 크레이트를 끌어올 이유가 없다.
//! 여기서는 `nl-core` 타입만 쓴다. 나중에 이 함수를 그대로 `nl-core` 로 옮기면 양쪽이 하나를 공유하게 된다.
//!
//! 두 쪽이 어긋나면 `matches_the_app_sample_structure` 테스트가 걸린다. nl-app 쪽을 고칠 때는
//! 여기도 같이 고쳐야 한다.
//!
//! nl-app 에 없는 것이 하나 있다: **"추론 API" 파이프라인**. 배포 앱을 HTTP 로 호출할 수 있게 해 주며,
//! `nl sample → nl train → nl build → 배포판 실행` 이 사람 손 없이 끝까지 이어지게 하는 조각이다.

use nl_core::dataset::{DataSource, DatasetSpec, SyntheticKind};
use nl_core::gui::{Binding, BuiltinAction};
use nl_core::payload::PayloadSpec;
use nl_core::pipeline::{PNode, PNodeKind, Sink, Source};
use nl_core::train::{Loss, Metric, Optimizer, TrainConfig};
use nl_core::{
    Act, BuildSpec, BuildTarget, DatasetId, DevicePref, Edge, EdgeId, LayerKind, Link, LinkId, ModelDef, ModelId,
    Node, NodeId, PNodeId, PayloadId, Pipeline, PipelineId, Port, Project, ProjectId, Widget, WidgetId, WidgetKind,
};

/// 추론 API 파이프라인이 여는 주소. 배포 앱을 바깥 프로그램이 호출하는 입구다.
pub const API_BIND: &str = "127.0.0.1:8799";
/// 추론 API 가 받는 경로.
pub const API_PATH: &str = "/infer";

/// 파이프라인 캔버스의 열 간격. `nl_app::pcanvas::chain_pos` 와 같은 값이어야 노드가 같은 자리에 놓인다.
const NODE_GAP: f32 = 250.0;

fn chain_pos(index: usize, row: f32) -> [f32; 2] {
    [80.0 + index as f32 * NODE_GAP, row]
}

/// 이 플랫폼에 맞는 빌드 대상. `nl_app::tools::host_target` 과 같은 규칙.
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

/// XOR 분류 샘플: 합성 데이터셋 + 작은 MLP + tabular 페이로드 + GUI + 파이프라인 둘 + 빌드 설정.
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
    // 배포 앱을 바깥 프로그램이 호출할 수 있게 해 준다. GUI 없이 헤드리스로 띄워도 이것만으로 쓸모가 있다.
    let api = api_pipeline(m.id, payload.id);

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

/// `HttpServer → Model → HttpReply` 세 노드짜리 추론 API 파이프라인.
///
/// 이 조각은 nl-app 의 `sample.rs` 에도 그대로 들어가야 한다 (보고서에 같은 코드를 실어 두었다).
pub fn api_pipeline(model: ModelId, payload: PayloadId) -> Pipeline {
    let mut pl = Pipeline::new("추론 API");
    pl.id = PipelineId::from_u128(0x2_4000);
    pl.tick_hz = 60.0;
    let server = pnode(
        0x2_4001,
        PNodeKind::Source { source: Source::HttpServer { bind: API_BIND.into(), path: API_PATH.into() } },
        "요청",
        0,
        120.0,
    );
    let infer = pnode(0x2_4002, PNodeKind::Model { model, payload: Some(payload) }, "추론", 1, 120.0);
    let reply = pnode(
        0x2_4003,
        PNodeKind::Sink { sink: Sink::HttpReply { server: server.id } },
        "응답",
        2,
        120.0,
    );
    for n in [server, infer, reply] {
        pl.nodes.insert(n.id, n);
    }
    link(&mut pl, 0x2_4100, PNodeId::from_u128(0x2_4001), PNodeId::from_u128(0x2_4002));
    link(&mut pl, 0x2_4101, PNodeId::from_u128(0x2_4002), PNodeId::from_u128(0x2_4003));
    pl
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

    /// nl-app 의 `sample.rs` 와 맞춰 둔 값들. 어느 한쪽이 바뀌면 여기서 걸린다.
    #[test]
    fn matches_the_app_sample_structure() {
        let p = xor_project();
        let m = p.models.values().next().unwrap();
        assert_eq!(p.id, ProjectId::from_u128(0x2_0000));
        assert_eq!(p.name, "XOR 샘플");
        assert_eq!(m.id, ModelId::from_u128(0x2_0003));
        assert_eq!(m.graph.nodes.len(), 5);
        assert_eq!(m.graph.edges.len(), 4);
        assert_eq!(m.train.epochs, 50, "nl-app 과 같은 에포크 수");
        assert_eq!(m.train.batch_size, 16);
        assert_eq!(p.datasets.len(), 1);
        assert_eq!(p.payloads.len(), 1);
        assert_eq!(m.payload, Some(PayloadId::from_u128(0x2_0001)));

        // GUI: 위젯 4개 + 창 설정.
        assert_eq!(p.gui.widgets.len(), 4);
        assert_eq!(p.gui.window.title, "XOR 분류기");
        assert_eq!(p.gui.window.width, 420.0);
        assert_eq!(p.gui.window.height, 240.0);

        // 시험 파이프라인은 nl-app 과 같은 id·노드 수.
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

    /// nl-app 에는 아직 없는, nl-cli 가 더한 파이프라인.
    #[test]
    fn the_api_pipeline_wires_http_to_the_model() {
        let p = xor_project();
        let api = &p.pipelines[&PipelineId::from_u128(0x2_4000)];
        assert_eq!(api.name, "추론 API");
        assert_eq!(api.nodes.len(), 3);
        assert_eq!(api.links.len(), 2);

        let server = &api.nodes[&PNodeId::from_u128(0x2_4001)];
        match &server.kind {
            PNodeKind::Source { source: Source::HttpServer { bind, path } } => {
                assert_eq!(bind, API_BIND);
                assert_eq!(path, API_PATH);
            }
            other => panic!("HTTP 서버가 아니다: {other:?}"),
        }
        // 응답 싱크가 그 서버를 가리킨다 — validate 가 보는 조건이다.
        let reply = &api.nodes[&PNodeId::from_u128(0x2_4003)];
        match &reply.kind {
            PNodeKind::Sink { sink: Sink::HttpReply { server: target } } => assert_eq!(*target, server.id),
            other => panic!("HTTP 응답이 아니다: {other:?}"),
        }
        // 파이프라인이 둘이다 (시험 + API).
        assert_eq!(p.pipelines.len(), 2);
    }
}
