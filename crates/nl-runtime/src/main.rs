#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Neural Linker 배포 런타임. `.nlapp` 번들을 인자로 받거나 자기 실행 파일 꼬리표에서 꺼내 실행한다.

mod app;
mod cli;
mod signals;

use app::{RuntimeApp, WorkDir};
use cli::{Command, Options};
use nl_bundle::Bundle;
use nl_core::DevicePref;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    env_logger::init();
    match cli::parse(std::env::args().skip(1)) {
        Command::Version => {
            println!("nl-runtime {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Help => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Command::Error(msg) => {
            eprintln!("{msg}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
        Command::Run(opts) => match run(opts) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("nl-runtime: {e:#}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run(opts: Options) -> anyhow::Result<ExitCode> {
    let Some(bundle) = load_bundle(opts.bundle_path.as_deref())? else {
        eprintln!("실행할 번들이 없습니다. `.nlapp` 파일을 인자로 주거나 번들이 첨부된 실행 파일로 실행하세요.\n");
        eprintln!("{}", cli::USAGE);
        return Ok(ExitCode::from(2));
    };
    let device = opts.device.unwrap_or(bundle.manifest.default_device);
    if opts.headless {
        run_headless(bundle, device, opts.run_for.map(Duration::from_secs_f64))?;
    } else {
        run_gui(bundle, device)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// 인자로 받은 파일이 있으면 그것을, 없으면 자기 실행 파일의 첨부 번들을 읽는다.
/// 인자로 받은 파일이 번들이 첨부된 실행 파일이어도 그대로 열린다.
fn load_bundle(path: Option<&Path>) -> anyhow::Result<Option<Bundle>> {
    match path {
        Some(p) => {
            if let Some(b) = nl_bundle::read_attached(p)? {
                return Ok(Some(b));
            }
            let bytes = std::fs::read(p)
                .map_err(|e| anyhow::anyhow!("번들 파일을 읽지 못했습니다 ({}): {e}", p.display()))?;
            Ok(Some(Bundle::from_zip(&bytes)?))
        }
        None => {
            let exe = std::env::current_exe()?;
            nl_bundle::read_attached(&exe)
        }
    }
}

fn run_gui(bundle: Bundle, device: DevicePref) -> anyhow::Result<()> {
    let work = WorkDir::create()?;
    app::prepare_workspace(&bundle, work.path())?;
    let base_dir = work.path().to_path_buf();
    let window = bundle.project.gui.window.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window.title.clone())
            .with_app_id("nl-runtime")
            .with_inner_size([window.width.max(240.0), window.height.max(180.0)])
            .with_min_inner_size([320.0, 240.0]),
        renderer: eframe::Renderer::Glow,
        // Wayland 에서 vsync 대기가 이벤트 루프를 통째로 막는 문제가 있어 끈다 (trust-pms 교훈).
        glow_options: eframe::egui_glow::GlowConfiguration { vsync: false, ..Default::default() },
        ..Default::default()
    };

    let result = eframe::run_native(
        "nl-runtime",
        options,
        Box::new(move |cc| {
            let app = RuntimeApp::new(&bundle, base_dir, device);
            app.install_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    // 창이 닫히면 임시 폴더를 지운다.
    drop(work);
    result.map_err(|e| anyhow::anyhow!("창을 열지 못했습니다: {e}"))
}

fn run_headless(bundle: Bundle, device: DevicePref, run_for: Option<Duration>) -> anyhow::Result<()> {
    let work = WorkDir::create()?;
    app::prepare_workspace(&bundle, work.path())?;
    let Some(pipeline) = app::entry_pipeline(&bundle.project, &bundle.manifest) else {
        anyhow::bail!("실행할 파이프라인이 없습니다 (매니페스트의 entry_pipeline 확인)");
    };

    signals::install();
    println!(
        "{} {} 헤드리스 실행 · 파이프라인 {} · 장치 {}",
        bundle.manifest.app_name,
        bundle.manifest.app_version,
        pipeline.name,
        device.label()
    );
    match run_for {
        Some(d) => println!("{:.1}초 뒤 자동 종료합니다 (Ctrl+C 로 먼저 종료 가능).", d.as_secs_f64()),
        None => println!("Ctrl+C 로 종료합니다."),
    }
    let deadline = run_for.map(|d| std::time::Instant::now() + d);

    let handle = app::spawn_runner(&bundle.project, &pipeline, work.path(), device)?;
    let mut asked_to_stop = false;
    loop {
        let timed_out = deadline.is_some_and(|t| std::time::Instant::now() >= t);
        if (signals::interrupted() || timed_out) && !asked_to_stop {
            asked_to_stop = true;
            println!("{}", if timed_out { "실행 시간이 끝났습니다. 파이프라인을 정지합니다." } else { "종료 신호를 받았습니다. 파이프라인을 정지합니다." });
            handle.stop();
        }
        match handle.events.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                println!("{}", app::describe_event(&ev));
                if matches!(ev, nl_io::RunnerEvent::Stopped) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if handle.is_done() {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}
