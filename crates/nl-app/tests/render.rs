//! 헤드리스 렌더 스모크 테스트 (trust-pms `views_render_headless` 계승).
//!
//! GUI 를 띄우지 않고 모든 뷰를 실제 앱 경로로 그려 egui id 충돌·레이아웃 assert·패닉을 잡는다.
//! 문서는 샘플 프로젝트와 빈 프로젝트 둘 다 지나가게 해서 "0으로 나누기·빈 목록" 분기도 밟는다.

use eframe::egui;
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
            // 네트워크·드라이버를 두드리는 백그라운드 확인은 계속 다시 그리기를 요청해 하네스가 멎지 않는다.
            app.disable_update_check();
            app.disable_device_probe();
            app.doc = DocState::new(project);
            // 문서를 갈아끼웠으니 선택도 새 문서 기준으로.
            let first = app.doc.project.models.keys().next().copied();
            app.sel.set(match first {
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
        for (pid, pl) in &p.pipelines {
            v.push(Selection::Pipeline(*pid));
            if let Some(nid) = pl.nodes.keys().next() {
                v.push(Selection::PNode(*pid, *nid));
            }
            if let Some(lid) = pl.links.keys().next() {
                v.push(Selection::Link(*pid, *lid));
            }
            // 종류별 노드를 하나씩 — 인스펙터의 모든 분기를 지난다.
            for n in pl.nodes.values() {
                v.push(Selection::PNode(*pid, n.id));
            }
        }
        v.extend(p.gui.widgets.keys().map(|id| Selection::Widget(*id)));
        v
    };

    for view in View::ALL {
        for sel in &ids {
            h.state_mut().view = view;
            h.state_mut().sel.set(*sel);
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

/// 파이프라인 노드의 모든 변형이 인스펙터에서 패닉 없이 그려지는지.
/// (`Source` 8종 · `Logic` 5종 · `Sink` 7종 + 모델)
#[test]
fn every_pipeline_node_kind_renders_in_the_inspector() {
    use nl_app::pcanvas::{logic_palette, sink_palette, source_palette};
    use nl_core::pipeline::{PNode, PNodeKind};

    let mut p = sample::xor_project();
    let mid = *p.models.keys().next().unwrap();
    let pid = *p.pipelines.keys().next().unwrap();
    let pl = p.pipelines.get_mut(&pid).unwrap();
    let mut kinds: Vec<PNodeKind> = source_palette().into_iter().map(|s| PNodeKind::Source { source: s }).collect();
    kinds.extend(logic_palette().into_iter().map(|l| PNodeKind::Logic { logic: l }));
    kinds.extend(sink_palette().into_iter().map(|s| PNodeKind::Sink { sink: s }));
    kinds.push(PNodeKind::Model { model: mid, payload: None });
    let mut ids = Vec::new();
    for (i, kind) in kinds.into_iter().enumerate() {
        let n = PNode::new(kind, [i as f32 * 120.0, 500.0]);
        ids.push(n.id);
        pl.nodes.insert(n.id, n);
    }

    let mut h = harness(p);
    h.state_mut().view = View::Pipeline;
    for id in ids {
        h.state_mut().sel.set(Selection::PNode(pid, id));
        h.run();
    }
}

/// GUI 디자이너: 모든 위젯 종류 + 바인딩 종류가 그려지는지.
#[test]
fn every_widget_kind_renders_in_the_designer() {
    use nl_core::gui::{Binding, BuiltinAction};
    use nl_core::{Widget, WidgetKind};

    let mut p = sample::xor_project();
    let pid = *p.pipelines.keys().next().unwrap();
    let first_node = *p.pipelines[&pid].nodes.keys().next().unwrap();
    let bindings = [
        None,
        Some(Binding::Action { action: BuiltinAction::Quit }),
        Some(Binding::PipelineInput { node: first_node }),
        Some(Binding::PipelineOutput { node: first_node }),
    ];
    let mut ids = Vec::new();
    for (i, kind) in WidgetKind::palette().into_iter().enumerate() {
        let mut w = Widget::new(kind, [10.0, 10.0 + i as f32 * 30.0, 120.0, 24.0]);
        w.binding = bindings[i % bindings.len()].clone();
        ids.push(w.id);
        p.gui.add(w);
    }

    let mut h = harness(p);
    h.state_mut().view = View::Gui;
    for id in ids {
        h.state_mut().sel.set(Selection::Widget(id));
        h.run();
    }
    // 미리보기(Run 모드)도 같은 레이아웃으로 그려진다.
    h.state_mut().gui_preview = true;
    h.run();
    h.state_mut().gui_preview = false;
    h.run();
}

/// HTTP 서버 소스와 그 짝인 응답 싱크가 인스펙터에서 그려지고, 검증이 짝을 알아보는지.
#[test]
fn http_server_and_reply_render_and_validate() {
    use nl_core::pipeline::{PNode, PNodeKind, Sink, Source};

    let mut p = sample::xor_project();
    let pid = *p.pipelines.keys().next().unwrap();
    let (server_id, reply_id) = {
        let pl = p.pipelines.get_mut(&pid).unwrap();
        let server = PNode::new(
            PNodeKind::Source {
                source: Source::HttpServer { bind: "127.0.0.1:8787".into(), path: "/infer".into() },
            },
            [80.0, 460.0],
        );
        let sid = server.id;
        let reply = PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server: sid } }, [600.0, 460.0]);
        let rid = reply.id;
        pl.nodes.insert(sid, server);
        pl.nodes.insert(rid, reply);
        pl.add_link(sid, rid).expect("서버 → 응답");
        (sid, rid)
    };
    // 짝이 맞으면 검증 오류가 없다.
    let errors = nl_core::validate(&p)
        .into_iter()
        .filter(|i| i.severity == nl_core::Severity::Error)
        .count();
    assert_eq!(errors, 0, "짝이 맞는 서버/응답은 오류가 아니다");

    let mut h = harness(p);
    h.state_mut().view = View::Pipeline;
    for id in [server_id, reply_id] {
        h.state_mut().sel.set(Selection::PNode(pid, id));
        h.run();
    }
}

/// 빌드 뷰: 아이콘·업데이트 설정이 있는 문서도 그려지는지.
#[test]
fn build_view_renders_with_icon_and_update_settings() {
    let mut p = sample::xor_project();
    let mut settings = p.settings.clone();
    let mut spec = settings.build.clone().unwrap();
    spec.icon = Some("icon.png".into());
    spec.update_base_url = Some("https://example.com/앱/0.1.0".into());
    spec.update_url = Some("https://example.com/앱/latest.json".into());
    spec.auto_update = true;
    spec.targets = vec![nl_core::BuildTarget::LinuxX64, nl_core::BuildTarget::WindowsX64];
    settings.build = Some(spec);
    p.settings = settings;

    let mut h = harness(p);
    h.state_mut().view = View::Build;
    // 아이콘 파일이 없어도 오류 문구만 뜨고 패닉하지 않아야 한다.
    h.run();
    h.run();
}

/// 녹화 폼이 열린 상태로 데이터 뷰가 그려지는지 (녹화 자체는 화면이 있어야 하므로 폼까지).
#[test]
fn recording_form_renders() {
    let mut h = harness(sample::xor_project());
    h.state_mut().view = View::Data;
    h.state_mut().views.data.record_form = Some(nl_app::views::data::RecordForm::default());
    h.run();
    h.run();
}

/// 설치 동의 모달은 아무리 긴 설명이 들어와도 창 안에 머물러야 한다.
///
/// `Grid` 칸 안의 라벨은 줄바꿈되지 않아 예전에는 모달이 화면 밖까지 늘어나 버튼을 누를 수 없었다.
#[test]
fn the_consent_modal_stays_inside_a_small_window() {
    let size = [820.0, 620.0];
    let mut h = Harness::builder().with_size(size).build_eframe(|cc| {
        let mut app = NlApp::new(cc);
        app.disable_update_check();
        app.disable_device_probe();
        app
    });
    h.state_mut().view = View::Build;
    h.state_mut().pending_plan = Some(nl_app::tools::plan_inno_setup());
    h.run();
    h.run();

    let rect = h
        .ctx
        .memory(|m| m.area_rect(egui::Id::new("tool-consent")))
        .expect("모달이 떠 있어야 한다");
    assert!(rect.width() <= size[0], "모달이 창보다 넓다: {rect:?}");
    assert!(rect.height() <= size[1], "모달이 창보다 높다: {rect:?}");
    // 승인·거부 버튼이 실제로 화면 안에 있어야 누를 수 있다.
    for label in ["승인하고 설치", "거부"] {
        let node = h.get_by_label(label);
        let r = node.rect();
        assert!(r.max.x <= size[0] && r.max.y <= size[1], "{label} 버튼이 창 밖이다: {r:?}");
    }
}

/// 시작 화면은 장치 확인을 기다리지 않는다.
///
/// 예전에는 `NlApp::new` 가 `enumerate()` 를, 상태바가 매 프레임 `resolve()` 를 불러
/// 드라이버가 깨진 GPU 가 있으면 첫 프레임이 10초 넘게 늦었다. 둘 다 백그라운드로 옮겼다.
#[test]
fn the_first_frame_does_not_wait_for_device_probing() {
    // 장치 확인은 일부러 켜 둔 채로 첫 프레임 시간을 잰다 — 그것이 이 테스트가 지키는 것이다.
    let started = std::time::Instant::now();
    let mut h = Harness::builder().with_size([1280.0, 800.0]).build_eframe(|cc| {
        let mut app = NlApp::new(cc);
        app.disable_update_check();
        app
    });
    h.step();
    let elapsed = started.elapsed();

    // 넉넉한 상한이다 — 문제였던 경로는 20초 타임아웃까지 갔다.
    assert!(elapsed.as_secs() < 5, "첫 프레임이 너무 늦다: {elapsed:?}");
    // 시작 뷰는 모델이어야 한다.
    assert_eq!(h.state().view, View::Model);
    h.state_mut().disable_device_probe();
}

/// `Ctrl+숫자` 가 뷰 바 순서대로 뷰를 바꾼다. `Ctrl+2` 는 데이터 뷰다.
#[test]
fn ctrl_digit_switches_views_in_bar_order() {
    let mut h = harness(sample::xor_project());
    h.state_mut().view = View::Model;
    h.run();

    for (i, expected) in View::ALL.iter().enumerate() {
        let key = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
        ][i];
        // 다른 뷰에서 출발해야 "안 바뀐 것"과 "원래 그 뷰"를 구별할 수 있다.
        h.state_mut().view = if *expected == View::Model { View::Resources } else { View::Model };
        h.key_press_modifiers(egui::Modifiers::COMMAND, key);
        h.run();
        assert_eq!(h.state().view, *expected, "Ctrl+{} 가 {expected:?} 로 가지 않는다", i + 1);
    }
}

#[test]
fn the_smallest_window_still_lays_out() {
    // 좁은 창에서도 툴바·뷰 바가 패널을 밀어내지 않아야 한다.
    let mut h = Harness::builder().with_size([960.0, 600.0]).build_eframe(|cc| {
        let mut app = NlApp::new(cc);
        app.disable_update_check();
        app.disable_device_probe();
        app
    });
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
