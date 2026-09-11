//! 인프로세스 골든 이미지 테스트. `egui_kittest` 가 wgpu 로 오프스크린 렌더해 `tests/snapshots/` 의 PNG 와 견준다.
//!
//! 컴포지터가 필요 없어 CI 에서 그대로 돈다 (`tools/uitest` 의 sway 하네스와 역할이 다르다 —
//! 저쪽은 실제 창·실제 입력을 보고, 이쪽은 위젯이 그려진 픽셀만 본다).
//!
//! 갱신: `UPDATE_SNAPSHOTS=1 cargo test -p nl-gui`
//!
//! 글꼴은 일부러 얹지 않는다. `nl_gui::font_definitions()` 가 고르는 시스템 CJK 글꼴은 기계마다 달라
//! 골든이 그 기계 전용이 된다. 대신 라벨을 ASCII 로 두고 egui 기본 글꼴만 쓴다 — 어디서 돌려도 같은 픽셀이 나온다.

use egui_kittest::{Harness, SnapshotOptions};
use nl_core::gui::{Widget, WidgetKind};
use nl_core::{GuiLayout, WidgetId};
use nl_engine::Value;
use nl_gui::{render_layout, GuiState, RenderMode};

/// 건너뛴 이유를 알린다. `NL_SNAPSHOT_REQUIRED=1` 이면 건너뛰지 않고 실패시킨다 —
/// CI 는 이 변수를 켜 두어야 렌더 백엔드나 글꼴이 빠진 채 조용히 초록불이 뜨지 않는다.
/// (cargo 는 통과한 테스트의 출력을 삼키므로 `eprintln!` 만으로는 눈에 띄지 않는다.)
fn skip(reason: &str) -> bool {
    let message = format!("스냅샷 건너뜀: {reason}");
    if std::env::var("NL_SNAPSHOT_REQUIRED").is_ok_and(|v| v != "0") {
        panic!("{message} (NL_SNAPSHOT_REQUIRED 가 켜져 있어 실패로 처리한다)");
    }
    eprintln!("{message}");
    eprintln!("  건너뜀을 실패로 보려면 NL_SNAPSHOT_REQUIRED=1 로 돌린다.");
    false
}

/// 렌더 백엔드가 없을 때 조용히 통과하지 않도록 이유를 찍고 건너뛴다.
/// `false` 면 테스트 본문을 돌리지 않는다.
fn renderer_ready() -> bool {
    // 렌더러 초기화는 어댑터가 없으면 패닉한다. 작은 하네스로 미리 찔러 보고 그때만 건너뛴다.
    // 실제 스냅샷 비교는 `try_snapshot` 으로 하므로 이 catch_unwind 가 불일치를 삼키지 않는다.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let probed = std::panic::catch_unwind(|| {
        let mut h = Harness::builder().with_size(egui::vec2(32.0, 32.0)).build_ui(|ui| {
            ui.label("probe");
        });
        h.run_steps(1);
        h.render().is_ok()
    });
    std::panic::set_hook(hook);

    match probed {
        Ok(true) => true,
        Ok(false) => skip("wgpu 렌더가 이미지를 내지 못했습니다 (어댑터는 있으나 렌더 실패)"),
        Err(_) => skip(
            "wgpu 어댑터가 없습니다. Linux 라면 소프트웨어 래스터라이저(mesa 의 lavapipe, \
             Fedora `mesa-vulkan-drivers`)를 깔면 돕니다",
        ),
    }
}

/// 잔 떨림(글꼴 안티에일리어싱·백엔드 차이)은 흡수하고 진짜 변화는 잡는 정도.
fn options() -> SnapshotOptions {
    SnapshotOptions::new().threshold(0.7).max_failed_pixels(64)
}

/// 9종 위젯이 모두 있는 레이아웃. id 는 결정적이라 여러 번 만들어도 같은 그림이 된다.
/// 라벨은 ASCII 로 둔다 (맨 위 주석 참고).
fn layout() -> GuiLayout {
    fn w(n: u128, kind: WidgetKind, rect: [f32; 4]) -> Widget {
        let mut w = Widget::new(kind, rect);
        w.id = WidgetId::from_u128(n);
        w
    }

    let mut l = GuiLayout::default();
    let group = l.add(w(1, WidgetKind::Group { title: "Settings".into() }, [12.0, 12.0, 260.0, 122.0]));

    let mut label = w(2, WidgetKind::Label { text: "Threshold".into() }, [12.0, 26.0, 120.0, 20.0]);
    label.parent = Some(group);
    l.add(label);

    let mut slider = w(3, WidgetKind::Slider { min: 0.0, max: 10.0, value: 3.0 }, [12.0, 54.0, 220.0, 22.0]);
    slider.parent = Some(group);
    l.add(slider);

    let mut toggle = w(4, WidgetKind::Toggle { text: "Enabled".into() }, [12.0, 86.0, 120.0, 22.0]);
    toggle.parent = Some(group);
    l.add(toggle);

    l.add(w(5, WidgetKind::Button { text: "Start".into() }, [300.0, 14.0, 110.0, 32.0]));
    l.add(w(6, WidgetKind::TextInput { hint: "name".into() }, [300.0, 56.0, 180.0, 24.0]));
    l.add(w(7, WidgetKind::Image, [12.0, 152.0, 130.0, 100.0]));
    l.add(w(8, WidgetKind::Plot { max_points: 60 }, [160.0, 152.0, 230.0, 100.0]));
    l.add(w(9, WidgetKind::Value { prefix: "Result: ".into() }, [410.0, 152.0, 190.0, 34.0]));
    l
}

fn widget(n: u128) -> WidgetId {
    WidgetId::from_u128(n)
}

/// 값이 채워진 상태: 플롯 30점, 값 위젯 숫자, 이미지 16×16 체커보드.
fn filled_state() -> GuiState {
    let mut state = GuiState::default();
    for i in 0..30 {
        let v = (f64::from(i) * 0.35).sin() * 4.0 + 5.0;
        state.push_value(widget(8), Value::Number(v), 60);
    }
    state.values.insert(widget(9), Value::Number(0.8125));
    state.values.insert(widget(3), Value::Number(7.5));
    state.values.insert(widget(4), Value::Number(1.0));
    state.texts.insert(widget(6), "sample".into());
    state.values.insert(widget(7), checkerboard(16));
    state
}

/// 16×16 체커보드 RGBA. 텍스처 업로드 경로와 배율 조정까지 그림으로 확인한다.
fn checkerboard(side: u32) -> Value {
    let mut rgba = Vec::with_capacity((side * side * 4) as usize);
    for y in 0..side {
        for x in 0..side {
            let dark = (x / 2 + y / 2) % 2 == 0;
            let c = if dark { 40 } else { 210 };
            rgba.extend_from_slice(&[c, c, u8::try_from(x * 16).unwrap_or(255), 255]);
        }
    }
    Value::Image { width: side, height: side, rgba }
}

struct Fixture {
    layout: GuiLayout,
    state: GuiState,
    mode: RenderMode,
}

fn harness(mode: RenderMode, state: GuiState) -> Harness<'static, Fixture> {
    Harness::builder()
        .with_size(egui::vec2(620.0, 280.0))
        .with_step_dt(1.0 / 60.0)
        .with_max_steps(60)
        .build_ui_state(
            |ui, f: &mut Fixture| {
                render_layout(ui, &f.layout, &mut f.state, f.mode);
            },
            Fixture { layout: layout(), state, mode },
        )
}

#[test]
fn run_mode_snapshot() {
    if !renderer_ready() {
        return;
    }
    let mut h = harness(RenderMode::Run, GuiState::default());
    h.run_steps(3);
    h.try_snapshot_options("gui-run", &options()).unwrap();
}

#[test]
fn run_mode_with_values_snapshot() {
    if !renderer_ready() {
        return;
    }
    let mut h = harness(RenderMode::Run, filled_state());
    h.run_steps(3);
    h.try_snapshot_options("gui-run-filled", &options()).unwrap();
}

#[test]
fn design_mode_snapshot() {
    if !renderer_ready() {
        return;
    }
    // 선택 강조와 모서리 핸들이 그려지도록 버튼을 골라 둔다.
    let mut state = filled_state();
    state.selected = Some(widget(5));
    let mut h = harness(RenderMode::Design, state);
    h.run_steps(3);
    h.try_snapshot_options("gui-design", &options()).unwrap();
}
