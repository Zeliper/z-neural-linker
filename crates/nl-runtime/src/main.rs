#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Neural Linker 배포 런타임. `.nlapp` 번들을 인자로 받거나 자기 실행 파일 꼬리표에서 꺼내 실행한다.

mod app;
mod cli;
mod console;
mod envfile;
mod signals;
mod update;

use app::{RuntimeApp, WorkDir};
use cli::{Command, Options};
use nl_bundle::Bundle;
use nl_core::DevicePref;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    env_logger::init();
    let command = cli::parse(std::env::args().skip(1));
    // Windows 릴리스 빌드는 GUI 서브시스템이라 표준 출력이 갈 곳이 없다.
    // 터미널에서 부른 것이 분명한 경로에서만 부모 콘솔에 붙는다 (창을 띄우는 경로는 그대로 둔다).
    if wants_console(&command) {
        console::attach_parent();
    }
    match command {
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

/// `--log-json` 이 켜졌는가. 사람용 안내를 어디로 보낼지 정한다.
static LOG_JSON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 사람용 안내 한 줄.
///
/// JSON 모드에서는 **stderr** 로 보낸다. stdout 은 이벤트 전용이어야 수집기가 한 줄씩 그대로
/// 먹는다 — 배너나 안내가 섞이면 그 줄에서 파싱이 깨진다. 안내를 없애지 않고 옮기는 이유는
/// journald 가 두 스트림을 모두 받되 구분해 주기 때문이다.
fn note(line: impl AsRef<str>) {
    if LOG_JSON.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{}", line.as_ref());
    } else {
        println!("{}", line.as_ref());
    }
}

/// 터미널에 글을 쓰는 경로인가. 창을 띄우는 GUI 실행만 아니면 전부 그렇다.
fn wants_console(command: &Command) -> bool {
    match command {
        Command::Version | Command::Help | Command::Error(_) => true,
        // 헤드리스는 로그가 전부고, GUI 실행도 시작 실패 메시지는 터미널에 보여야 한다.
        Command::Run(opts) => opts.headless || opts.bundle_path.is_some(),
    }
}

fn run(opts: Options) -> anyhow::Result<ExitCode> {
    LOG_JSON.store(opts.log_json, std::sync::atomic::Ordering::Relaxed);
    // 환경 파일을 가장 먼저 읽는다. 토큰은 서버를 열 때 읽히므로 그전에 자리를 잡아야 하고,
    // 스레드가 뜨기 전이어야 환경 변수를 만지는 것이 안전하다.
    if let Some(path) = opts.env_file.as_deref() {
        let names = envfile::load(path)?;
        // 값은 비밀이라 찍지 않는다. 무엇이 들어갔는지만 알린다.
        note(format!(
            "환경 파일 {} ({}개 적용: {})",
            path.display(),
            names.len(),
            names.join(", ")
        ));
    }
    let Some(bundle) = load_bundle(opts.bundle_path.as_deref())? else {
        eprintln!("실행할 번들이 없습니다. `.nlapp` 파일을 인자로 주거나 번들이 첨부된 실행 파일로 실행하세요.\n");
        eprintln!("{}", cli::USAGE);
        return Ok(ExitCode::from(2));
    };
    let device = opts.device.unwrap_or(bundle.manifest.default_device);
    let work = open_work_dir(opts.work_dir.as_deref(), &bundle)?;
    if opts.headless {
        run_headless(
            bundle,
            device,
            opts.run_for.map(Duration::from_secs_f64),
            !opts.no_update,
            work,
            opts.log_json,
        )?;
    } else {
        run_gui(bundle, device, !opts.no_update, work)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// 작업 폴더를 열고 번들을 푼다.
///
/// `--work-dir` 을 주면 그 폴더를 쓰고 **그대로 남긴다**. 같은 번들이 이미 풀려 있으면 다시 풀지 않아
/// 서비스 재시작이 빠르고, `local/` 에 둔 인증서 같은 파일이 그대로 있다.
/// 주지 않으면 끝나면 지워지는 임시 폴더다.
fn open_work_dir(fixed: Option<&Path>, bundle: &Bundle) -> anyhow::Result<WorkDir> {
    let Some(path) = fixed else {
        let work = WorkDir::create()?;
        app::prepare_workspace(bundle, work.path())?;
        return Ok(work);
    };
    let work =
        WorkDir::fixed(path).map_err(|e| anyhow::anyhow!("작업 폴더를 열지 못했습니다 ({}): {e}", path.display()))?;
    let fingerprint = app::bundle_fingerprint(bundle);
    let extracted = app::sync_workspace(bundle, &fingerprint, work.path())?;
    note(format!(
        "작업 폴더 {} ({})",
        work.path().display(),
        if extracted {
            "번들을 풀었습니다"
        } else {
            "이미 풀려 있어 그대로 씁니다"
        }
    ));
    // 처음 풀 때 한 번만 알린다. 인증서를 어디 둬야 하는지가 가장 자주 막히는 지점이다.
    if extracted && work.is_persistent() {
        note(format!(
            "  인증서 같은 파일은 {}/ 에 두세요 — 번들을 갱신해도 남습니다 (예: cert_pem \"{}/server.crt\")",
            work.path().join(app::LOCAL_DIR).display(),
            app::LOCAL_DIR
        ));
    }
    Ok(work)
}

/// `.nlapp` 파일 크기 상한. 압축된 상태로 메모리에 통째로 올라가므로 여기서 한 번 막는다.
/// 압축을 푼 뒤의 상한은 `nl_bundle::MAX_BUNDLE_BYTES` 가 따로 본다.
const MAX_BUNDLE_FILE_BYTES: u64 = 2_000_000_000;

/// 인자로 받은 파일이 있으면 그것을, 없으면 자기 실행 파일의 첨부 번들을 읽는다.
/// 인자로 받은 파일이 번들이 첨부된 실행 파일이어도 그대로 열린다.
fn load_bundle(path: Option<&Path>) -> anyhow::Result<Option<Bundle>> {
    match path {
        Some(p) => {
            if let Some(b) = nl_bundle::read_attached(p)? {
                return Ok(Some(b));
            }
            // 통째로 메모리에 올리므로 먼저 크기를 본다 — 파일 하나로 프로세스를 죽이지 못하게.
            let size = std::fs::metadata(p)
                .map_err(|e| anyhow::anyhow!("번들 파일을 읽지 못했습니다 ({}): {e}", p.display()))?
                .len();
            anyhow::ensure!(
                size <= MAX_BUNDLE_FILE_BYTES,
                "번들 파일이 너무 큽니다 ({size} 바이트, 상한 {MAX_BUNDLE_FILE_BYTES} 바이트): {}",
                p.display()
            );
            let bytes =
                std::fs::read(p).map_err(|e| anyhow::anyhow!("번들 파일을 읽지 못했습니다 ({}): {e}", p.display()))?;
            Ok(Some(Bundle::from_zip(&bytes)?))
        }
        None => {
            let exe = std::env::current_exe()?;
            nl_bundle::read_attached(&exe)
        }
    }
}

fn run_gui(bundle: Bundle, device: DevicePref, updates: bool, work: WorkDir) -> anyhow::Result<()> {
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
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };

    let result = eframe::run_native(
        "nl-runtime",
        options,
        Box::new(move |cc| {
            let app = RuntimeApp::new(&bundle, base_dir, device).with_updates(updates);
            app.install_style(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    // 창이 닫히면 임시 폴더를 지운다.
    drop(work);
    result.map_err(|e| anyhow::anyhow!("창을 열지 못했습니다: {e}"))
}

fn run_headless(
    bundle: Bundle,
    device: DevicePref,
    run_for: Option<Duration>,
    updates: bool,
    work: WorkDir,
    // 사람용 줄 대신 한 줄 JSON. stdout 한 줄이 곧 이벤트 하나여야 수집기가 그대로 먹는다.
    log_json: bool,
) -> anyhow::Result<()> {
    let Some(pipeline) = app::entry_pipeline(&bundle.project, &bundle.manifest) else {
        anyhow::bail!("실행할 파이프라인이 없습니다 (매니페스트의 entry_pipeline 확인)");
    };

    signals::install();
    note(format!(
        "{} {} 헤드리스 실행 · 파이프라인 {} · 장치 {}",
        bundle.manifest.app_name,
        bundle.manifest.app_version,
        pipeline.name,
        device.label()
    ));
    match run_for {
        Some(d) => note(format!(
            "{:.1}초 뒤 자동 종료합니다 (Ctrl+C 로 먼저 종료 가능).",
            d.as_secs_f64()
        )),
        None => note("Ctrl+C 로 종료합니다."),
    }
    let deadline = run_for.map(|d| std::time::Instant::now() + d);

    // 헤드리스는 확인만 한다 — 서버형 배포를 사람 확인 없이 바꿔치우지 않는다.
    let mut update = updates.then(|| update::UpdateUi::new(&bundle.manifest)).flatten();
    if let Some(u) = &mut update {
        note(format!("업데이트를 확인합니다: {}", u.manifest_url()));
        u.start_check();
    }

    if bundle.manifest.arm_input {
        note(app::ARM_INPUT_NOTICE);
    }
    let handle = app::spawn_runner(
        &bundle.project,
        &pipeline,
        work.path(),
        device,
        bundle.manifest.arm_input,
    )?;
    // 로그 줄에 붙일 이름. 짧은 id 는 부딪혀서 어느 노드인지 알 수 없다.
    let names = nl_io::EventNames::with_gui(&pipeline, &bundle.project.gui);
    let mut asked_to_stop = false;
    loop {
        drain_update(&mut update);
        let timed_out = deadline.is_some_and(|t| std::time::Instant::now() >= t);
        if (signals::interrupted() || timed_out) && !asked_to_stop {
            asked_to_stop = true;
            note(if timed_out {
                "실행 시간이 끝났습니다. 파이프라인을 정지합니다."
            } else {
                "종료 신호를 받았습니다. 파이프라인을 정지합니다."
            });
            handle.stop();
        }
        match handle.events.recv_timeout(Duration::from_millis(200)) {
            Ok(ev) => {
                // 모양을 정하는 곳은 `nl_io::event_json` 한 군데다 — `nl run` 과 같은 줄이 나와야
                // 빌더·명령줄·배포판 로그를 한데 모아 볼 수 있다.
                if log_json {
                    println!("{}", nl_io::event_json(&ev, &names));
                } else {
                    println!("{}", app::describe_event(&ev));
                }
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
    // 파이프라인이 먼저 끝났어도 확인 결과가 와 있으면 알려 준다.
    drain_update(&mut update);
    Ok(())
}

/// 업데이트 이벤트를 stdout 으로 흘린다. 헤드리스는 적용하지 않는다.
fn drain_update(update: &mut Option<update::UpdateUi>) {
    let Some(u) = update else { return };
    for ev in u.poll() {
        if let Some(line) = update::describe(&ev) {
            note(line);
        }
    }
}
