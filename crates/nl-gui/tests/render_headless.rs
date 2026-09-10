//! 헤드리스 렌더 테스트. 9종 위젯이 Run/Design 두 모드에서 패닉 없이 그려지고,
//! Design 모드에서 위젯을 클릭하면 `Selected` 이벤트가 나오는지 확인한다.

use egui_kittest::Harness;
use nl_core::gui::{Binding, BuiltinAction, GuiLayout, Widget, WidgetKind};
use nl_core::WidgetId;
use nl_engine::Value;
use nl_gui::{render_layout, GuiEvent, GuiState, RenderMode};

/// 9종 위젯 전부 (Group 과 그 자식 포함). id 는 결정적이라 여러 번 만들어도 같은 레이아웃이다.
fn full_layout() -> (GuiLayout, WidgetId) {
    fn w(n: u128, kind: WidgetKind, rect: [f32; 4]) -> Widget {
        let mut w = Widget::new(kind, rect);
        w.id = WidgetId::from_u128(n);
        w
    }

    let mut l = GuiLayout::default();
    let group = l.add(w(1, WidgetKind::Group { title: "설정".into() }, [10.0, 10.0, 260.0, 120.0]));

    let mut label = w(2, WidgetKind::Label { text: "라벨 텍스트".into() }, [10.0, 24.0, 120.0, 20.0]);
    label.parent = Some(group);
    l.add(label);

    let mut slider = w(3, WidgetKind::Slider { min: 0.0, max: 10.0, value: 3.0 }, [10.0, 52.0, 220.0, 22.0]);
    slider.parent = Some(group);
    l.add(slider);

    let mut toggle = w(4, WidgetKind::Toggle { text: "켜기".into() }, [10.0, 82.0, 120.0, 22.0]);
    toggle.parent = Some(group);
    l.add(toggle);

    let mut button = w(5, WidgetKind::Button { text: "시작".into() }, [300.0, 10.0, 100.0, 30.0]);
    button.binding = Some(Binding::Action { action: BuiltinAction::StartPipeline });
    let button_id = l.add(button);

    l.add(w(6, WidgetKind::TextInput { hint: "이름".into() }, [300.0, 50.0, 160.0, 24.0]));
    l.add(w(7, WidgetKind::Image, [10.0, 150.0, 120.0, 90.0]));
    l.add(w(8, WidgetKind::Plot { max_points: 50 }, [150.0, 150.0, 200.0, 90.0]));
    l.add(w(9, WidgetKind::Value { prefix: "결과: ".into() }, [370.0, 150.0, 180.0, 30.0]));

    (l, button_id)
}

struct Fixture {
    layout: GuiLayout,
    state: GuiState,
    mode: RenderMode,
    events: Vec<GuiEvent>,
    origin: egui::Pos2,
}

impl Fixture {
    fn new(mode: RenderMode) -> Self {
        let (layout, _) = full_layout();
        let mut state = GuiState::default();
        // 이미지·플롯·값 위젯이 실제 데이터 경로를 타도록 값을 채운다.
        for w in layout.widgets.values() {
            match w.kind {
                WidgetKind::Image => {
                    state.values.insert(w.id, Value::Image { width: 4, height: 3, rgba: vec![128; 4 * 3 * 4] });
                }
                WidgetKind::Plot { .. } => {
                    for i in 0..20 {
                        state.push_value(w.id, Value::Number(f64::from(i) * 0.5), 50);
                    }
                }
                WidgetKind::Value { .. } => {
                    state.values.insert(w.id, Value::Numbers(vec![0.1, 0.2, 0.7]));
                }
                _ => {}
            }
        }
        Self { layout, state, mode, events: Vec::new(), origin: egui::Pos2::ZERO }
    }
}

fn harness(mode: RenderMode) -> Harness<'static, Fixture> {
    Harness::builder()
        .with_size(egui::vec2(640.0, 420.0))
        .with_step_dt(1.0 / 60.0)
        .with_max_steps(60)
        .build_ui_state(
            |ui, f: &mut Fixture| {
                f.origin = ui.max_rect().min;
                let evs = render_layout(ui, &f.layout, &mut f.state, f.mode);
                f.events.extend(evs);
            },
            Fixture::new(mode),
        )
}

#[test]
fn all_widget_kinds_render_in_both_modes() {
    // 팔레트가 9종이라는 전제가 깨지면 이 테스트부터 알려 준다.
    assert_eq!(WidgetKind::palette().len(), 9);

    for mode in [RenderMode::Run, RenderMode::Design] {
        let mut h = harness(mode);
        h.run_steps(3);
        let f = h.state();
        assert_eq!(f.layout.widgets.len(), 9, "위젯 9개가 모두 있어야 합니다");
        // Run 모드에서는 상호작용 없이 이벤트가 나오지 않아야 한다.
        if mode == RenderMode::Run {
            assert!(f.events.iter().all(|e| matches!(e, GuiEvent::Changed(..))), "예상 밖 이벤트: {:?}", f.events);
        }
    }
}

#[test]
fn design_click_selects_widget() {
    let (layout, button_id) = full_layout();
    let button_rect = layout.widgets[&button_id].rect;

    let mut h = harness(RenderMode::Design);
    h.run_steps(2);
    let origin = h.state().origin;
    let pos = origin + egui::vec2(button_rect[0] + button_rect[2] / 2.0, button_rect[1] + button_rect[3] / 2.0);

    h.event(egui::Event::PointerMoved(pos));
    h.step();
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    h.step();
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.step();

    let f = h.state();
    assert!(
        f.events.contains(&GuiEvent::Selected(Some(button_id))),
        "버튼 클릭에서 Selected 이벤트가 나오지 않았습니다: {:?}",
        f.events
    );
    assert_eq!(f.state.selected, Some(button_id));
}

#[test]
fn design_click_on_empty_space_clears_selection() {
    let mut h = harness(RenderMode::Design);
    h.state_mut().state.selected = Some(WidgetId::from_u128(42));
    h.run_steps(2);
    let origin = h.state().origin;
    // 어떤 위젯도 없는 오른쪽 아래.
    let pos = origin + egui::vec2(600.0, 380.0);

    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }

    let f = h.state();
    assert!(f.events.contains(&GuiEvent::Selected(None)), "빈 곳 클릭 이벤트가 없습니다: {:?}", f.events);
    assert_eq!(f.state.selected, None);
}

#[test]
fn run_mode_button_click_emits_clicked() {
    let (layout, button_id) = full_layout();
    let button_rect = layout.widgets[&button_id].rect;

    let mut h = harness(RenderMode::Run);
    h.run_steps(2);
    let origin = h.state().origin;
    let pos = origin + egui::vec2(button_rect[0] + button_rect[2] / 2.0, button_rect[1] + button_rect[3] / 2.0);

    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }

    let f = h.state();
    assert!(f.events.contains(&GuiEvent::Clicked(button_id)), "버튼 클릭 이벤트가 없습니다: {:?}", f.events);
}
