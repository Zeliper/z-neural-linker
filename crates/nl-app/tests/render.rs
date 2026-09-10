//! 헤드리스 렌더 스모크 테스트 (trust-pms `views_render_headless` 계승).
//!
//! GUI 를 띄우지 않고 모든 뷰를 실제 앱 경로로 그려 egui id 충돌·레이아웃 assert·패닉을 잡는다.
//! 문서는 샘플 프로젝트와 빈 프로젝트 둘 다 지나가게 해서 "0으로 나누기·빈 목록" 분기도 밟는다.

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use nl_app::app::{DocState, NlApp, View};
use nl_app::canvas::Selection;
use nl_app::{project, sample};
use nl_core::{LayerKind, Node, Op, Project};

/// 실제 앱을 eframe 하네스로 띄운다 (스토리지 없음 = 첫 실행과 같은 상태).
fn harness<'a>(project: Project) -> Harness<'a, NlApp> {
    let mut h = Harness::builder()
        .with_size([1480.0, 900.0])
        .build_eframe(|cc| {
            let mut app = NlApp::new(cc);
            app.doc = DocState::new(project);
            // 문서를 갈아끼웠으니 선택도 새 문서 기준으로.
            let first = app.doc.project.models.keys().next().copied();
            app.canvas.set_selection(match first {
                Some(id) => Selection::Model(id),
                None => Selection::Project,
            });
            app
        });
    h.run();
    h
}

/// 프로젝트 하나로 모든 뷰 · 도크 탭 · 인스펙터 분기를 훑는다.
fn sweep(project: Project, label: &str) {
    let mut h = harness(project);
    let ids: Vec<Selection> = {
        let p = &h.state().doc.project;
        let mut v = vec![Selection::None, Selection::Project];
        for (mid, m) in &p.models {
            v.push(Selection::Model(*mid));
            if let Some(nid) = m.graph.nodes.keys().next() {
                v.push(Selection::Node(*mid, *nid));
            }
            if let Some(eid) = m.graph.edges.keys().next() {
                v.push(Selection::Edge(*mid, *eid));
            }
        }
        v.extend(p.datasets.keys().map(|id| Selection::Dataset(*id)));
        v.extend(p.payloads.keys().map(|id| Selection::Payload(*id)));
        v.extend(p.runs.keys().map(|id| Selection::Run(*id)));
        v
    };

    for view in View::ALL {
        for sel in &ids {
            h.state_mut().view = view;
            h.state_mut().canvas.set_selection(*sel);
            h.run();
            // 뷰 바는 어느 뷰에서나 일곱 개 라벨을 그린다 — 접근성 트리에 없으면 화면이 비었다는 뜻.
            assert!(
                h.query_all_by_label(view.label()).next().is_some(),
                "{label}/{}: 뷰 바가 그려지지 않았다 ({sel:?})",
                view.label()
            );
        }
    }

    // 도크 세 탭 (문제 목록은 클릭 대상이 있어야 하므로 열어 둔 채로 한 번 더).
    h.state_mut().show_dock = true;
    for view in View::ALL {
        h.state_mut().view = view;
        h.run();
    }
    // 패널을 모두 접은 상태도 레이아웃이 무너지지 않아야 한다.
    h.state_mut().show_outline = false;
    h.state_mut().show_inspector = false;
    h.state_mut().show_dock = false;
    for view in View::ALL {
        h.state_mut().view = view;
        h.run();
    }
}

#[test]
fn every_view_renders_with_the_xor_sample() {
    sweep(sample::xor_project(), "XOR 샘플");
}

#[test]
fn every_view_renders_with_the_cnn_sample() {
    sweep(sample::cnn_project(), "CNN 샘플");
}

#[test]
fn every_view_renders_with_an_empty_project() {
    sweep(Project::new("빈 프로젝트"), "빈 문서");
}

#[test]
fn every_view_renders_with_a_broken_graph() {
    // 형상 오류·순환·미연결이 섞인 문서 — 캔버스의 빨간 테두리·문제 도크 경로를 밟는다.
    let mut p = sample::xor_project();
    let mid = *p.models.keys().next().unwrap();
    let g = &mut p.models.get_mut(&mid).unwrap().graph;
    // 4차원 입력을 요구하는 Conv2d 를 벡터 뒤에 붙인다 → ShapeMismatch.
    let conv = g.add_node(Node::new(
        LayerKind::Conv2d { out_channels: 4, kernel: [3, 3], stride: [1, 1], padding: [0, 0], bias: true },
        [200.0, 400.0],
    ));
    let input = g.input_nodes()[0];
    g.add_edge(input, nl_core::Port::new(conv, 0)).unwrap();
    // 아무 데도 연결되지 않은 노드 → MissingInput.
    g.add_node(Node::new(LayerKind::Flatten, [600.0, 400.0]));
    sweep(p, "깨진 그래프");
}

#[test]
fn the_smallest_window_still_lays_out() {
    // 좁은 창에서도 툴바·뷰 바가 패널을 밀어내지 않아야 한다.
    let mut h = Harness::builder()
        .with_size([960.0, 600.0])
        .build_eframe(|cc| NlApp::new(cc));
    for view in View::ALL {
        h.state_mut().view = view;
        h.run();
    }
}

#[test]
fn saving_and_reopening_keeps_the_document() {
    let dir = std::env::temp_dir().join(format!("nl-app-render-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("round.nlproj");

    let mut h = harness(sample::cnn_project());
    // 문서를 한 번 고친 뒤 저장한다.
    let mid = *h.state().doc.project.models.keys().next().unwrap();
    let node = Node::new(LayerKind::Dropout { p: 0.25 }, [900.0, 300.0]);
    let node_id = node.id;
    h.state_mut().doc.apply_local(vec![Op::UpsertNode { model: mid, node }]);
    let expected = h.state().doc.project.clone();
    project::save(&path, &expected).unwrap();

    let loaded = project::load(&path).unwrap();
    assert_eq!(loaded.project, expected);
    assert!(loaded.project.models[&mid].graph.nodes.contains_key(&node_id));

    // 다시 그 문서로 앱을 띄워도 모든 뷰가 그려진다.
    sweep(loaded.project, "다시 연 문서");
    let _ = std::fs::remove_dir_all(&dir);
}
