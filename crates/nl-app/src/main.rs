#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use nl_app::app;
use std::path::PathBuf;

fn main() -> eframe::Result<()> {
    env_logger::init();
    // 파일 연결(.nlproj 더블클릭)이나 셸에서 `nl-app 경로` 로 열기. `--version` 은 설치 스크립트용.
    let mut initial_file: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--version" || arg == "-V" {
            println!("neural-linker {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // 개발·자동화용: 샘플 프로젝트를 파일로 써 놓고 끝낸다 (헤드리스 GUI 테스트가 파일 대화상자를 피하려고 쓴다).
        if arg == "--write-sample" {
            let Some(path) = args.next().map(PathBuf::from) else {
                eprintln!("--write-sample 뒤에 파일 경로가 필요합니다");
                std::process::exit(2);
            };
            let name = args
                .next()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "xor".into());
            let project = match name.as_str() {
                "cnn" => nl_app::sample::cnn_project(),
                "new" => nl_app::sample::new_project(),
                _ => nl_app::sample::xor_project(),
            };
            match nl_app::project::save(&path, &project) {
                Ok(()) => println!("{}", path.display()),
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        if initial_file.is_none() && !arg.to_string_lossy().starts_with('-') {
            initial_file = Some(PathBuf::from(arg));
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Neural Linker")
            .with_app_id("neural-linker")
            .with_inner_size([1480.0, 920.0])
            .with_min_inner_size([960.0, 600.0]),
        glow_options: eframe::egui_glow::GlowConfiguration {
            // Wayland 에서 vsync 대기(eglSwapBuffers)가 컴포지터 frame callback 을 기다리며
            // 이벤트 루프 전체를 블로킹하는 문제가 있어 끈다 (trust-pms 와 같은 이유).
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Neural Linker",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(nl_app::fonts::font_definitions());
            Ok(Box::new(app::NlApp::new_with_file(cc, initial_file)))
        }),
    )
}
