//! 뷰 골든 이미지. `egui_kittest` 가 wgpu 로 오프스크린 렌더해 `tests/snapshots/` 의 PNG 와 견준다.
//!
//! `tools/uitest` 의 sway 하네스와 역할이 다르다. 저쪽은 실제 창·실제 입력·실제 글꼴을 보고 글자까지
//! 읽히는 그림을 남기지만 컴포지터가 필요하다. 이쪽은 **컴포지터 없이 CI 에서 돌며** 패널 폭, 위젯 배치,
//! 색, 간격 같은 레이아웃 회귀를 잡는다.
//!
//! 갱신: `UPDATE_SNAPSHOTS=1 cargo test -p nl-app --test snapshot`
//!
//! **글꼴은 얹지 않는다.** 앱은 평소 `nl_app::fonts::font_definitions()` 로 시스템 CJK 글꼴을 찾는데,
//! 그 글꼴은 기계마다 달라 골든이 그 기계 전용이 된다. 여기서는 egui 기본 글꼴만 써서 어디서 돌려도
//! 같은 픽셀이 나오게 한다 — 대신 한글은 네모로 보인다. 글자 내용은 uitest 골든과 렌더 테스트가 본다.

use eframe::egui;
use egui_kittest::{Harness, SnapshotOptions};
use nl_app::app::{DocState, NlApp, View};
use nl_app::canvas::Selection;
use nl_app::sample;

/// 건너뛴 이유를 알린다. `NL_SNAPSHOT_REQUIRED=1` 이면 건너뛰지 않고 실패시킨다 —
/// CI 는 이 변수를 켜 두어야 렌더 백엔드가 빠진 채 조용히 초록불이 뜨지 않는다.
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
fn renderer_ready() -> bool {
    // 렌더러 초기화는 어댑터가 없으면 패닉한다. 작은 하네스로 미리 찔러 보고 그때만 건너뛴다.
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

/// 잔 떨림(안티에일리어싱·백엔드 차이)은 흡수하고 진짜 변화는 잡는 정도.
fn options() -> SnapshotOptions {
    SnapshotOptions::new().threshold(0.7).max_failed_pixels(64)
}

/// 창 크기. 모든 패널이 한 화면에 들어가면서 파일이 지나치게 커지지 않는 선.
const SIZE: [f32; 2] = [1280.0, 800.0];

/// XOR 샘플을 연 앱. 시계에 기대는 것(장치 확인·업데이트 확인)은 꺼서 그림을 결정적으로 만든다.
fn app<'a>() -> Harness<'a, NlApp> {
    let mut h = Harness::builder().with_size(SIZE).build_eframe(|cc| {
        let mut app = NlApp::new(cc);
        // 둘 다 백그라운드에서 돌며 결과가 올 때마다 화면이 바뀐다 — 골든이 실행마다 달라진다.
        app.disable_update_check();
        app.disable_device_probe();
        app.doc = DocState::new(sample::xor_project());
        let first = app.doc.project.models.keys().next().copied();
        app.sel.set(match first {
            Some(id) => Selection::Model(id),
            None => Selection::Project,
        });
        // 자원 뷰는 1초마다 실제 CPU·메모리를 읽는다 — 그대로 두면 실행마다 숫자와 막대가 달라진다.
        // 고정값을 넣고 다시 읽을 시각을 멀리 밀어 둔다.
        app.views.resources.snapshot = nl_io::ResourceSnapshot {
            cpu_usage_percent: 42.0,
            cpu_cores: 12,
            cpu_name: "Test CPU 3.00GHz".into(),
            mem_used_bytes: 8 * 1024 * 1024 * 1024,
            mem_total_bytes: 32 * 1024 * 1024 * 1024,
            gpus: Vec::new(),
        };
        app.views.resources.cpu = vec![[0.0, 40.0], [1.0, 42.0]];
        app.views.resources.mem = vec![[0.0, 25.0], [1.0, 25.0]];
        app.views.resources.last_poll = f64::INFINITY;
        app
    });
    // 패널 크기가 자리를 잡을 때까지 몇 프레임.
    h.run_steps(4);
    h
}

/// 스냅샷 하나를 찍고 실패하면 이유를 돌려준다.
fn shot(name: &str, h: &mut Harness<'_, NlApp>) -> Option<String> {
    // 스크롤 영역은 처음 몇 프레임 동안 막대 크기를 재며 흔들린다. 멎을 때까지 돌린 뒤 찍는다.
    h.run_steps(12);
    h.try_snapshot_options(name, &options())
        .err()
        .map(|e| format!("{name}: {e}"))
}

/// 뷰와 모달을 한 번에 찍는다.
///
/// 테스트 하나로 묶은 이유: `#[test]` 마다 wgpu 장치를 새로 여는데, cargo 가 테스트를 병렬로 돌리면
/// 여러 스레드가 동시에 어댑터를 잡다가 죽는다(이 기계에서 SIGSEGV 로 재현). 순서대로 찍으면
/// 장치를 하나씩만 쓴다. 실패는 모아서 한 번에 보고하므로 어느 그림이 틀렸는지는 그대로 드러난다.
#[test]
fn view_snapshots() {
    if !renderer_ready() {
        return;
    }
    let views = [
        (View::Model, "view-model"),
        (View::Data, "view-data"),
        (View::Train, "view-train"),
        (View::Pipeline, "view-pipeline"),
        (View::Gui, "view-gui"),
        (View::Build, "view-build"),
        (View::Resources, "view-resources"),
    ];
    let mut failures = Vec::new();
    for (view, name) in views {
        let mut h = app();
        h.state_mut().view = view;
        failures.extend(shot(name, &mut h));
    }

    // 비정상 종료 뒤 뜨는 복구 모달. 시각은 고정값이라 실행마다 같은 그림이 된다.
    {
        let mut h = app();
        h.state_mut().recover_candidates = vec![nl_app::recovery::Entry {
            path: std::path::PathBuf::from("/tmp/nl/recovery/file-1.recovery.json"),
            project_name: "XOR sample".into(),
            original_path: Some("/home/me/projects/xor.nlproj".into()),
            saved_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("고정 시각"),
        }];
        failures.extend(shot("modal-recovery", &mut h));
    }

    // 도구 설치 동의 모달. 검증 범위와 단계 목록이 모두 보이는 상태.
    {
        let mut h = app();
        h.state_mut().view = View::Build;
        h.state_mut().pending_plan = Some(nl_app::tools::plan_inno_setup());
        failures.extend(shot("modal-tool-consent", &mut h));
    }

    assert!(
        failures.is_empty(),
        "골든과 다른 그림 {}개:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
