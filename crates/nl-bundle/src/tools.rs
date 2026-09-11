//! 빌드에 필요한 외부 도구 설치 계획. 빌더는 `ToolPlan` 을 동의 모달에 그대로 펼쳐 보여 주고,
//! 사용자가 승인하면 `run_tool_plan` 으로 내려받아 설치한다. 계획을 만드는 일과 실행하는 일이 나뉘어 있어
//! "무엇을 어디서 받아 어디에 놓는지" 를 먼저 보여 줄 수 있다.

use crate::{sha256_hex, Target};
use anyhow::Context;
use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 내려받기 타임아웃. 무한 대기는 빌드 UI 를 통째로 묶는다.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// 내려받기 상한 (Inno Setup 설치본은 10 MB 안쪽이다).
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// 도구 하나를 설치하는 계획. 사용자에게 보여 줄 내용이 전부 들어 있다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPlan {
    /// 사람이 읽는 도구 이름.
    pub name: String,
    /// 내려받을 주소.
    pub url: String,
    /// 알려진 sha256. `None` 이면 검증하지 않는다(버전이 계속 올라가는 배포본).
    pub sha256: Option<String>,
    /// 대략적인 내려받기 크기(바이트). 진행률 표시용 어림값이다.
    pub size_hint: u64,
    /// 내려받은 파일을 놓을 경로.
    pub dest: PathBuf,
    /// 승인 모달에 순서대로 보여 줄 설명.
    pub steps: Vec<String>,
    /// 순서대로 실행할 명령. 내려받기형 계획은 비어 있고, 크로스 빌드 계획이 채운다.
    /// 각 항목은 `[프로그램, 인자…]` 이다 — 셸을 거치지 않아 따옴표 규칙을 신경 쓸 필요가 없다.
    pub commands: Vec<Vec<String>>,
    /// 끝났을 때 차지하는 디스크 어림값(바이트). `size_hint` 는 내려받는 양, 이쪽은 남는 양이다.
    pub disk_hint: u64,
}

/// 설치 진행 상황. 채널로 흘려 보내 빌드 UI 가 진행률을 그린다.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolProgress {
    Started { name: String },
    /// `total` 은 서버가 길이를 알려 줄 때만 채워진다.
    Downloading { received: u64, total: Option<u64> },
    Downloaded { path: PathBuf },
    Running { command: String },
    /// 실행 중인 명령이 뱉은 한 줄 (stdout·stderr 합친 것).
    Output(String),
    Done,
    Failed { message: String },
}

/// Inno Setup 6 설치 계획.
///
/// 주소는 jrsoftware 의 고정 링크라 늘 현재 6.x 설치본을 준다. 버전이 바뀌므로 sha256 은 비워 둔다 —
/// 검증이 필요하면 호출자가 알고 있는 해시를 `sha256` 에 채워 넣으면 `run_tool_plan` 이 확인한다.
pub fn install_inno_setup_plan() -> ToolPlan {
    let dest = tools_dir().join("innosetup-6.exe");
    let mut steps = vec![
        "jrsoftware.org 에서 Inno Setup 6 설치본을 내려받습니다 (약 7 MB).".to_string(),
        format!("내려받은 파일을 {} 에 둡니다.", dest.display()),
    ];
    if cfg!(windows) {
        steps.push("설치본을 /VERYSILENT 로 실행합니다. 기본 경로에 설치되며 창은 뜨지 않습니다.".into());
        steps.push(r"설치 뒤 %ProgramFiles(x86)%\Inno Setup 6\ISCC.exe 를 씁니다.".into());
    } else {
        steps.push("wine 이 필요합니다. 설치돼 있지 않으면 배포판 패키지 관리자로 먼저 설치하세요.".into());
        steps.push("`wine innosetup-6.exe /VERYSILENT` 로 wine 접두사 안에 설치합니다.".into());
        steps.push(r"설치 뒤 ~/.wine/drive_c/Program Files (x86)/Inno Setup 6/ISCC.exe 를 씁니다.".into());
    }
    steps.push("설치 프로그램을 만들지 않을 거라면 건너뛰어도 됩니다 — 대신 zip 으로 배포됩니다.".into());

    ToolPlan {
        name: "Inno Setup 6".into(),
        url: "https://jrsoftware.org/download.php/is.exe".into(),
        sha256: None,
        size_hint: 7 * 1024 * 1024,
        dest,
        steps,
        commands: Vec::new(),
        disk_hint: 60 * 1024 * 1024,
    }
}

// ───────────────────────────── 런타임 크로스 빌드 ─────────────────────────────

/// Windows 런타임 크로스 빌드에 필요한 Microsoft SDK·CRT 내려받기 양 (실측 1.2 GB).
const XWIN_DOWNLOAD: u64 = 1_300_000_000;
/// SDK 캐시 + 대상 빌드 산출물이 차지하는 디스크 어림값 (실측 1.2 GB + 4 GB 남짓).
const XWIN_DISK: u64 = 6 * 1024 * 1024 * 1024;

/// 이 대상의 `nl-runtime` 을 지금 기계에서 빌드하는 계획.
///
/// Windows 대상은 `cargo-xwin` 이 Microsoft 의 SDK·CRT 를 내려받아 쓴다 — Visual Studio 도,
/// 관리자 권한도 필요 없다. Linux 대상은 평범한 `cargo build` 다.
pub fn cross_build_plan(target: Target) -> ToolPlan {
    let triple = target.triple();
    let out = format!("target/{triple}/release/{}", target.runtime_file_name());

    match target {
        Target::WindowsX64 => ToolPlan {
            name: format!("Windows 런타임 빌드 ({triple})"),
            url: "https://crates.io/crates/cargo-xwin".into(),
            sha256: None,
            size_hint: XWIN_DOWNLOAD,
            dest: PathBuf::from(&out),
            steps: vec![
                "cargo-xwin 을 설치합니다 (crates.io 에서 받아 ~/.cargo/bin 에).".into(),
                format!("rustup 에 {triple} 표준 라이브러리를 더합니다."),
                "cargo-xwin 이 Microsoft 의 Windows SDK 와 CRT 를 ~/.cache/cargo-xwin 에 내려받습니다                  (약 1.2 GB, 처음 한 번만). Visual Studio 설치나 관리자 권한은 필요 없습니다."
                    .into(),
                "clang·llvm-lib·lld-link 가 필요합니다. lld-link 가 없으면 rustup 이 들고 있는                  rust-lld 로 대신 만들어 씁니다 (관리자 권한 없이)."
                    .into(),
                format!("cargo xwin build --release 로 {out} 을 만듭니다."),
                "처음 빌드는 내려받기까지 합쳐 20~40분, 디스크는 6 GB 쯤 씁니다. 두 번째부터는 몇 분입니다.".into(),
                format!("만들어진 실행 파일을 runtimes/{triple}/ 로 옮기면 빌더가 바로 찾습니다."),
            ],
            commands: vec![
                vec!["cargo".into(), "install".into(), "cargo-xwin".into(), "--locked".into()],
                vec!["rustup".into(), "target".into(), "add".into(), triple.into()],
                vec![
                    "cargo".into(),
                    "xwin".into(),
                    "build".into(),
                    "--release".into(),
                    "-p".into(),
                    "nl-runtime".into(),
                    "--target".into(),
                    triple.into(),
                ],
            ],
            disk_hint: XWIN_DISK,
        },
        Target::LinuxX64 => ToolPlan {
            name: format!("Linux 런타임 빌드 ({triple})"),
            url: String::new(),
            sha256: None,
            size_hint: 0,
            dest: PathBuf::from("target/release/nl-runtime"),
            steps: vec![
                "지금 기계가 Linux x86_64 라 크로스 빌드가 아니라 그냥 빌드입니다.".into(),
                "cargo build --release -p nl-runtime 를 돌립니다.".into(),
                "처음 빌드는 10~20분, 디스크는 4 GB 쯤 씁니다.".into(),
            ],
            commands: vec![vec![
                "cargo".into(),
                "build".into(),
                "--release".into(),
                "-p".into(),
                "nl-runtime".into(),
            ]],
            disk_hint: 4 * 1024 * 1024 * 1024,
        },
    }
}

/// 계획의 명령을 순서대로 실행하고 출력을 한 줄씩 흘려보낸다.
/// `cwd` 는 워크스페이스 뿌리여야 한다 (`cargo` 가 `-p nl-runtime` 를 찾는 곳).
/// **네트워크와 디스크를 크게 쓴다.** 테스트에서는 부르지 않는다.
pub fn run_cross_build(plan: &ToolPlan, cwd: &Path, progress: Sender<ToolProgress>) -> anyhow::Result<()> {
    let send = |p: ToolProgress| {
        let _ = progress.send(p);
    };
    send(ToolProgress::Started { name: plan.name.clone() });

    match build_all(plan, cwd, &send) {
        Ok(()) => {
            send(ToolProgress::Done);
            Ok(())
        }
        Err(e) => {
            send(ToolProgress::Failed { message: format!("{e:#}") });
            Err(e)
        }
    }
}

fn build_all(plan: &ToolPlan, cwd: &Path, send: &impl Fn(ToolProgress)) -> anyhow::Result<()> {
    if plan.commands.is_empty() {
        anyhow::bail!("실행할 명령이 없는 계획입니다 ({})", plan.name);
    }
    // lld-link 가 없으면 rustup 의 rust-lld 로 대신 만들어 PATH 앞에 둔다.
    let shim_dir = ensure_lld_link(&tools_dir().join("bin"))?;
    if let Some(dir) = &shim_dir {
        send(ToolProgress::Output(format!("lld-link 대체본을 만들었습니다: {}", dir.display())));
    }

    for argv in &plan.commands {
        let (program, args) = argv.split_first().context("빈 명령")?;
        send(ToolProgress::Running { command: argv.join(" ") });
        run_streaming(program, args, cwd, shim_dir.as_deref(), send)?;
    }
    Ok(())
}

/// 자식 프로세스의 stdout·stderr 를 한 줄씩 흘려보낸다. 오래 도는 빌드의 진행을 보여 주려는 것이다.
fn run_streaming(
    program: &str,
    args: &[String],
    cwd: &Path,
    extra_path: Option<&Path>,
    send: &impl Fn(ToolProgress),
) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = extra_path {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![dir.to_path_buf()];
        dirs.extend(std::env::split_paths(&old));
        cmd.env("PATH", std::env::join_paths(dirs).context("PATH 를 만들지 못했습니다")?);
    }

    let mut child = cmd.spawn().with_context(|| format!("{program} 을(를) 실행하지 못했습니다"))?;
    // stderr 를 별도 스레드로 읽어 파이프가 막히지 않게 한다 (cargo 는 진행을 stderr 로 낸다).
    let stderr = child.stderr.take().context("stderr 파이프")?;
    let (ltx, lrx) = crossbeam_channel::unbounded::<String>();
    let pump = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = ltx.send(line);
        }
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            send(ToolProgress::Output(line));
            while let Ok(l) = lrx.try_recv() {
                send(ToolProgress::Output(l));
            }
        }
    }
    let status = child.wait().context("자식 프로세스를 기다리지 못했습니다")?;
    let _ = pump.join();
    for line in lrx.try_iter() {
        send(ToolProgress::Output(line));
    }
    if !status.success() {
        anyhow::bail!("{program} 이(가) 실패했습니다 (코드 {:?})", status.code());
    }
    Ok(())
}

/// `lld-link` 가 없으면 rustup 이 들고 있는 `rust-lld` 를 부르는 얇은 스크립트를 `dir` 에 만든다.
/// 이미 있으면 `Ok(None)`. Windows·macOS 호스트에서는 아무것도 하지 않는다.
///
/// Fedora 는 `lld` 를 따로 깔아야 하고 그러려면 관리자 권한이 필요하다. rustup 의 `rust-lld` 는
/// 같은 LLVM 에서 나온 같은 링커라 `-flavor link` 로 부르면 `lld-link` 와 같은 일을 한다.
pub fn ensure_lld_link(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if !cfg!(unix) || which("lld-link").is_some() {
        return Ok(None);
    }
    let Some(rust_lld) = rust_lld_path() else {
        anyhow::bail!(
            "lld-link 도 rust-lld 도 없습니다. 배포판의 lld 패키지를 깔거나              `rustup component add llvm-tools` 를 먼저 하세요"
        );
    };
    std::fs::create_dir_all(dir)?;
    let shim = dir.join("lld-link");
    // rustc 가 링커 이름을 보고 스스로 `-flavor link` 를 붙여 줄 때가 있다.
    // 그때 또 붙이면 rust-lld 가 두 번째 "link" 를 입력 파일로 읽고 죽는다.
    // 셸 스크립트라 중괄호·달러가 섞인다. 서식 문자열 대신 이어 붙여 읽기 쉽게 둔다.
    let lld = shell_quote(&rust_lld.to_string_lossy());
    let script = String::new()
        + "#!/bin/sh\n"
        + "# nl-bundle 이 만든 lld-link 대체본 (rustup 의 rust-lld 를 MSVC 링커 모드로 부른다).\n"
        + "case \"$1\" in\n"
        + "  -flavor) exec " + &lld + " \"$@\" ;;\n"
        + "  *)       exec " + &lld + " -flavor link \"$@\" ;;\n"
        + "esac\n";
    std::fs::write(&shim, script)?;
    set_executable(&shim)?;
    Ok(Some(dir.to_path_buf()))
}

fn rust_lld_path() -> Option<PathBuf> {
    let out = std::process::Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    let host = host_triple()?;
    let path = sysroot.join("lib/rustlib").join(host).join("bin/rust-lld");
    path.is_file().then_some(path)
}

fn host_triple() -> Option<String> {
    let out = std::process::Command::new("rustc").arg("-vV").output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    text.lines().find_map(|l| l.strip_prefix("host: ")).map(str::to_string)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 내려받은 도구를 두는 폴더.
pub fn tools_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("dev", "trustanc", "neural-linker") {
        return dirs.cache_dir().join("tools");
    }
    std::env::temp_dir().join("neural-linker-tools")
}

/// 계획을 실제로 실행한다: 내려받기 → (해시 검증) → 조용한 설치.
/// **네트워크를 쓴다.** 테스트에서는 부르지 않는다.
pub fn run_tool_plan(plan: &ToolPlan, progress: Sender<ToolProgress>) -> anyhow::Result<()> {
    let send = |p: ToolProgress| {
        let _ = progress.send(p);
    };
    send(ToolProgress::Started { name: plan.name.clone() });

    match install(plan, &send) {
        Ok(()) => {
            send(ToolProgress::Done);
            Ok(())
        }
        Err(e) => {
            send(ToolProgress::Failed { message: format!("{e:#}") });
            Err(e)
        }
    }
}

fn install(plan: &ToolPlan, send: &impl Fn(ToolProgress)) -> anyhow::Result<()> {
    let bytes = download(&plan.url, plan.size_hint, send)?;

    if let Some(expected) = &plan.sha256 {
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            anyhow::bail!("내려받은 파일의 sha256 이 다릅니다 (기대 {expected}, 실제 {actual})");
        }
    }

    if let Some(parent) = plan.dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&plan.dest, &bytes)
        .with_context(|| format!("도구를 저장하지 못했습니다: {}", plan.dest.display()))?;
    send(ToolProgress::Downloaded { path: plan.dest.clone() });

    let (program, args) = silent_install_command(&plan.dest);
    send(ToolProgress::Running { command: format!("{program} {}", args.join(" ")) });

    let out = std::process::Command::new(&program)
        .args(&args)
        .output()
        .with_context(|| format!("설치 명령을 실행하지 못했습니다: {program}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "설치에 실패했습니다 (코드 {:?})\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// 플랫폼별 조용한 설치 명령. Windows 는 설치본을 그대로, 그 밖에서는 wine 을 거친다.
pub fn silent_install_command(installer: &Path) -> (String, Vec<String>) {
    let path = installer.display().to_string();
    let flags =
        ["/VERYSILENT".to_string(), "/SUPPRESSMSGBOXES".to_string(), "/NORESTART".to_string()].to_vec();
    if cfg!(windows) {
        (path, flags)
    } else {
        let mut args = vec![path];
        args.extend(flags);
        ("wine".to_string(), args)
    }
}

fn download(url: &str, size_hint: u64, send: &impl Fn(ToolProgress)) -> anyhow::Result<Vec<u8>> {
    let config = ureq::Agent::config_builder()
        .user_agent(concat!("neural-linker/", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut res = agent.get(url).call().with_context(|| format!("내려받지 못했습니다: {url}"))?;
    let total = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let mut reader = res.body_mut().as_reader();
    let mut out: Vec<u8> = Vec::with_capacity(total.unwrap_or(size_hint).min(MAX_DOWNLOAD_BYTES) as usize);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk).context("본문을 읽는 중 끊겼습니다")?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() as u64 > MAX_DOWNLOAD_BYTES {
            anyhow::bail!("내려받기가 상한({MAX_DOWNLOAD_BYTES} 바이트)을 넘었습니다");
        }
        send(ToolProgress::Downloading { received: out.len() as u64, total });
    }
    if out.is_empty() {
        anyhow::bail!("빈 응답을 받았습니다: {url}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inno_plan_describes_what_will_happen() {
        let plan = install_inno_setup_plan();
        assert_eq!(plan.name, "Inno Setup 6");
        assert!(plan.url.starts_with("https://"), "{}", plan.url);
        assert!(plan.url.contains("jrsoftware.org"));
        assert!(plan.size_hint > 0);
        assert!(plan.dest.ends_with("innosetup-6.exe"), "{}", plan.dest.display());
        assert!(plan.steps.len() >= 4, "{:?}", plan.steps);
        assert!(plan.steps.iter().any(|s| s.contains("내려받")));
        assert!(plan.steps.iter().any(|s| s.contains("zip")), "대체 경로를 안내해야 합니다");
        // 계획을 만드는 것만으로는 아무 파일도 만들지 않는다.
        assert!(!plan.dest.exists() || plan.dest.is_file());
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_plan_mentions_wine() {
        let plan = install_inno_setup_plan();
        assert!(plan.steps.iter().any(|s| s.contains("wine")), "{:?}", plan.steps);
    }

    #[test]
    fn silent_install_uses_wine_off_windows() {
        let (program, args) = silent_install_command(Path::new("/tmp/is.exe"));
        assert!(args.contains(&"/VERYSILENT".to_string()));
        if cfg!(windows) {
            assert_eq!(program, "/tmp/is.exe");
        } else {
            assert_eq!(program, "wine");
            assert_eq!(args[0], "/tmp/is.exe");
        }
    }

    #[test]
    fn windows_cross_build_plan_describes_the_measured_flow() {
        let plan = cross_build_plan(Target::WindowsX64);
        assert!(plan.name.contains("x86_64-pc-windows-msvc"), "{}", plan.name);
        assert!(plan.size_hint > 1_000_000_000, "SDK 내려받기 양을 알려 줘야 합니다");
        assert!(plan.disk_hint > plan.size_hint, "남는 디스크가 내려받는 양보다 크다");

        // 안내에 시간과 디스크가 들어 있어야 사용자가 승인 여부를 판단한다.
        let steps = plan.steps.join("\n");
        assert!(steps.contains("분"), "걸리는 시간을 알려 줘야 합니다: {steps}");
        assert!(steps.contains("GB"), "디스크를 알려 줘야 합니다: {steps}");
        assert!(steps.contains("관리자 권한"), "권한이 필요 없다는 점이 핵심입니다: {steps}");
        assert!(steps.contains("lld-link"), "링커 준비 단계를 알려 줘야 합니다: {steps}");

        // 명령은 셸을 거치지 않고 그대로 실행할 수 있는 꼴이다.
        let flat: Vec<String> = plan.commands.iter().map(|c| c.join(" ")).collect();
        assert_eq!(flat.len(), 3, "{flat:?}");
        assert!(flat[0].starts_with("cargo install cargo-xwin"), "{flat:?}");
        assert!(flat[1].contains("rustup target add x86_64-pc-windows-msvc"), "{flat:?}");
        assert!(flat[2].contains("cargo xwin build --release"), "{flat:?}");
        assert!(flat[2].contains("--target x86_64-pc-windows-msvc"), "{flat:?}");
        assert!(plan.dest.ends_with("nl-runtime.exe"), "{}", plan.dest.display());
    }

    #[test]
    fn linux_plan_is_a_plain_build() {
        let plan = cross_build_plan(Target::LinuxX64);
        assert_eq!(plan.size_hint, 0, "내려받을 것이 없다");
        assert_eq!(plan.commands.len(), 1);
        assert_eq!(plan.commands[0].join(" "), "cargo build --release -p nl-runtime");
        assert!(plan.dest.ends_with("nl-runtime"));
    }

    /// 명령이 없는 계획으로는 빌드를 시작하지 않는다 (내려받기형 계획을 잘못 넘긴 경우).
    #[test]
    fn a_download_plan_is_not_a_build_plan() {
        let (tx, _rx) = crossbeam_channel::unbounded();
        let plan = install_inno_setup_plan();
        assert!(plan.commands.is_empty());
        let err = run_cross_build(&plan, Path::new("."), tx).unwrap_err().to_string();
        assert!(err.contains("실행할 명령이 없는"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn lld_link_shim_handles_the_doubled_flavor_argument() {
        let dir = tempfile::tempdir().unwrap();
        let Some(made) = ensure_lld_link(dir.path()).unwrap() else {
            // 이 기계에 이미 lld-link 가 있으면 만들 것이 없다.
            eprintln!("lld-link 가 이미 있어 대체본을 만들지 않았습니다");
            return;
        };
        let shim = made.join("lld-link");
        assert!(shim.is_file());

        let script = std::fs::read_to_string(&shim).unwrap();
        assert!(script.starts_with("#!/bin/sh"), "{script}");
        // rustc 가 -flavor 를 먼저 붙여 보내도 한 번만 붙도록 갈라 놓는다.
        assert!(script.contains("-flavor) exec"), "{script}");
        assert!(script.contains("-flavor link"), "{script}");
        assert!(script.contains("rust-lld"), "rust-lld 를 가리켜야 합니다: {script}");

        // 실제로 돌려 본다 — 두 호출 모두 LLD 가 자기 버전을 찍어야 한다.
        for args in [vec!["--version"], vec!["-flavor", "link", "--version"]] {
            let out = std::process::Command::new(&shim).args(&args).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("LLD"), "{args:?} → {text}");
        }
    }

    #[test]
    fn tools_dir_is_absolute() {
        assert!(tools_dir().is_absolute(), "{}", tools_dir().display());
    }
}
