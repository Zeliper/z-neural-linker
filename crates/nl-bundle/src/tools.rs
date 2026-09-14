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
/// 최대 리다이렉트 횟수. https→http 강등 사슬과 무한 리다이렉트를 막는다.
const MAX_REDIRECTS: u32 = 3;

/// 고정해 둔 Inno Setup 버전. 올릴 때는 [`INNO_SHA256`] 과 [`INNO_SIZE`] 도 함께 고쳐야 한다
/// (`curl -sL <주소> | sha256sum`).
const INNO_VERSION: &str = "6.7.3";
/// GitHub 릴리스 태그 표기(`.` 대신 `_`).
const INNO_TAG: &str = "6_7_3";
const INNO_FILE: &str = "innosetup-6.7.3.exe";
/// 2026-09-14 에 실제로 내려받아 확인한 값.
const INNO_SHA256: &str = "9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732";
const INNO_SIZE: u64 = 10_592_232;

/// 도구 하나를 설치하는 계획. 사용자에게 보여 줄 내용이 전부 들어 있다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPlan {
    /// 사람이 읽는 도구 이름.
    pub name: String,
    /// 내려받을 주소.
    pub url: String,
    /// 알려진 sha256. `None` 이면 **내려받기만 하고 실행하지 않는다** — [`ToolPlan::verified`] 를 보라.
    pub sha256: Option<String>,
    /// 내려받은 것을 검증할 수 있는가. `sha256` 이 있는지를 그대로 요약한 값이고,
    /// 승인 모달이 "이 파일은 검증됩니다 / 검증할 수 없습니다" 를 그릴 때 쓴다.
    ///
    /// `false` 면 [`run_tool_plan`] 이 **실행 단계를 거부한다.** 검증하지 않은 실행 파일을
    /// 조용히 돌리는 것이 H6 이었다.
    pub verified: bool,
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

impl ToolPlan {
    /// `verified` 를 `sha256` 에서 뽑아 만든다. 두 값이 어긋나지 않게 하는 유일한 생성 경로다.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        url: impl Into<String>,
        sha256: Option<String>,
        size_hint: u64,
        dest: PathBuf,
        steps: Vec<String>,
        commands: Vec<Vec<String>>,
        disk_hint: u64,
    ) -> Self {
        let sha256 = sha256.filter(|s| !s.trim().is_empty());
        Self {
            name: name.into(),
            url: url.into(),
            verified: sha256.is_some(),
            sha256,
            size_hint,
            dest,
            steps,
            commands,
            disk_hint,
        }
    }

    /// 승인 모달 한 줄. 무엇을 보장하고 무엇을 보장하지 못하는지 그대로 적는다.
    pub fn verification_note(&self) -> String {
        match &self.sha256 {
            Some(sha) => format!("내려받은 뒤 sha256 을 확인합니다 ({sha})."),
            None => "이 계획에는 알려진 sha256 이 없어 내려받기만 하고 실행하지 않습니다.".to_string(),
        }
    }
}

/// 설치 진행 상황. 채널로 흘려 보내 빌드 UI 가 진행률을 그린다.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolProgress {
    Started {
        name: String,
    },
    /// `total` 은 서버가 길이를 알려 줄 때만 채워진다.
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Downloaded {
        path: PathBuf,
    },
    Running {
        command: String,
    },
    /// 실행 중인 명령이 뱉은 한 줄 (stdout·stderr 합친 것).
    Output(String),
    Done,
    Failed {
        message: String,
    },
}

/// Inno Setup 6 설치 계획.
///
/// 주소는 **버전이 박힌 GitHub 릴리스 자산**이고 sha256 도 고정이다. 예전에 쓰던
/// `jrsoftware.org/download.php/is.exe` 는 내려받기 안내 **HTML 페이지로 302** 하는 주소라
/// 실제 설치본이 아니었고, 버전이 계속 바뀌어 해시를 박을 수도 없었다.
///
/// 해시가 있으므로 [`run_tool_plan`] 이 내려받은 뒤 검증하고 조용한 설치까지 진행한다.
/// 해시가 없는 계획은 내려받기만 하고 실행을 거부한다.
pub fn install_inno_setup_plan() -> ToolPlan {
    let dest = tools_dir().join(INNO_FILE);
    let mut steps = vec![
        format!("Inno Setup {INNO_VERSION} 설치본을 내려받습니다 (약 10 MB)."),
        format!("sha256 을 확인합니다 (기대값 {INNO_SHA256})."),
        format!("내려받은 파일을 {} 에 둡니다.", dest.display()),
    ];
    if cfg!(windows) {
        steps.push("설치본을 /VERYSILENT 로 실행합니다. 기본 경로에 설치되며 창은 뜨지 않습니다.".into());
        steps.push(r"설치 뒤 %ProgramFiles(x86)%\Inno Setup 6\ISCC.exe 를 씁니다.".into());
    } else {
        steps.push("wine 이 필요합니다. 설치돼 있지 않으면 배포판 패키지 관리자로 먼저 설치하세요.".into());
        steps.push(format!(
            "`wine {INNO_FILE} /VERYSILENT` 로 wine 접두사 안에 설치합니다."
        ));
        steps.push(r"설치 뒤 ~/.wine/drive_c/Program Files (x86)/Inno Setup 6/ISCC.exe 를 씁니다.".into());
    }
    steps.push("설치 프로그램을 만들지 않을 거라면 건너뛰어도 됩니다 — 대신 zip 으로 배포됩니다.".into());

    ToolPlan::new(
        "Inno Setup 6",
        format!("https://github.com/jrsoftware/issrc/releases/download/is-{INNO_TAG}/{INNO_FILE}"),
        Some(INNO_SHA256.to_string()),
        INNO_SIZE,
        dest,
        steps,
        Vec::new(),
        60 * 1024 * 1024,
    )
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
        Target::WindowsX64 => ToolPlan::new(
            format!("Windows 런타임 빌드 ({triple})"),
            "https://crates.io/crates/cargo-xwin",
            None,
            XWIN_DOWNLOAD,
            PathBuf::from(&out),
            vec![
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
            vec![
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
            XWIN_DISK,
        ),
        Target::LinuxX64 => ToolPlan::new(
            format!("Linux 런타임 빌드 ({triple})"),
            String::new(),
            None,
            0,
            PathBuf::from("target/release/nl-runtime"),
            vec![
                "지금 기계가 Linux x86_64 라 크로스 빌드가 아니라 그냥 빌드입니다.".into(),
                "cargo build --release -p nl-runtime 를 돌립니다.".into(),
                "처음 빌드는 10~20분, 디스크는 4 GB 쯤 씁니다.".into(),
            ],
            vec![vec![
                "cargo".into(),
                "build".into(),
                "--release".into(),
                "-p".into(),
                "nl-runtime".into(),
            ]],
            4 * 1024 * 1024 * 1024,
        ),
    }
}

/// 계획의 명령을 순서대로 실행하고 출력을 한 줄씩 흘려보낸다.
/// `cwd` 는 워크스페이스 뿌리여야 한다 (`cargo` 가 `-p nl-runtime` 를 찾는 곳).
/// **네트워크와 디스크를 크게 쓴다.** 테스트에서는 부르지 않는다.
pub fn run_cross_build(plan: &ToolPlan, cwd: &Path, progress: Sender<ToolProgress>) -> anyhow::Result<()> {
    let send = |p: ToolProgress| {
        let _ = progress.send(p);
    };
    send(ToolProgress::Started {
        name: plan.name.clone(),
    });

    match build_all(plan, cwd, &send) {
        Ok(()) => {
            send(ToolProgress::Done);
            Ok(())
        }
        Err(e) => {
            send(ToolProgress::Failed {
                message: format!("{e:#}"),
            });
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
        send(ToolProgress::Output(format!(
            "lld-link 대체본을 만들었습니다: {}",
            dir.display()
        )));
    }

    for argv in &plan.commands {
        let (program, args) = argv.split_first().context("빈 명령")?;
        send(ToolProgress::Running {
            command: argv.join(" "),
        });
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

    let mut child = cmd
        .spawn()
        .with_context(|| format!("{program} 을(를) 실행하지 못했습니다"))?;
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
    // 이 폴더는 빌드 내내 PATH 맨 앞에 놓인다 — 소유자 전용이어야 한다.
    create_private_dir(dir)?;
    let shim = dir.join("lld-link");
    // rustc 가 링커 이름을 보고 스스로 `-flavor link` 를 붙여 줄 때가 있다.
    // 그때 또 붙이면 rust-lld 가 두 번째 "link" 를 입력 파일로 읽고 죽는다.
    // 셸 스크립트라 중괄호·달러가 섞인다. 서식 문자열 대신 이어 붙여 읽기 쉽게 둔다.
    let lld = crate::shell_quote(&rust_lld.to_string_lossy());
    let script = String::new()
        + "#!/bin/sh\n"
        + "# nl-bundle 이 만든 lld-link 대체본 (rustup 의 rust-lld 를 MSVC 링커 모드로 부른다).\n"
        + "case \"$1\" in\n"
        + "  -flavor) exec "
        + &lld
        + " \"$@\" ;;\n"
        + "  *)       exec "
        + &lld
        + " -flavor link \"$@\" ;;\n"
        + "esac\n";
    // 심볼릭 링크를 따라가 남의 파일을 덮어쓰지 않도록 지우고 새로 만든다.
    let _ = std::fs::remove_file(&shim);
    std::fs::write(&shim, script)?;
    set_executable(&shim)?;
    Ok(Some(dir.to_path_buf()))
}

fn rust_lld_path() -> Option<PathBuf> {
    let out = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
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

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // 0700 — 소유자만. PATH 앞에 놓이는 스크립트를 남이 고칠 수 있으면 안 된다.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 내려받은 도구를 두는 폴더. **반드시 사용자 전용 경로여야 한다.**
///
/// 여기에는 (a) 내려받은 설치본이 놓이고 (b) `lld-link` 대체 스크립트가 만들어지며
/// (c) 그 폴더가 15분짜리 빌드 내내 `PATH` **맨 앞**에 놓인다. 공용 `/tmp` 로 떨어지면
/// 같은 호스트의 다른 사용자가 스크립트를 미리 놓아 두는 것만으로 빌드를 장악한다.
/// 그래서 `/tmp` 폴백을 없애고 `$XDG_RUNTIME_DIR`(사용자 전용, 0700) → 홈 순으로 물러선다.
pub fn tools_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("dev", "trustanc", "neural-linker") {
        return dirs.cache_dir().join("tools");
    }
    // XDG_RUNTIME_DIR 은 명세상 그 사용자만 접근할 수 있는(0700) 폴더다.
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("neural-linker-tools");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(home).join(".cache/neural-linker/tools");
    }
    // 여기까지 왔으면 사용자 폴더를 알 수 없다. 현재 폴더 아래에 두어 공용 경로를 피한다.
    PathBuf::from(".neural-linker-tools")
}

/// 소유자만 드나들 수 있는 폴더를 만든다. 이미 있으면 권한만 다시 조인다.
///
/// `PATH` 앞에 놓이는 폴더라 다른 사용자가 쓸 수 있으면 안 된다.
pub(crate) fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("폴더를 만들지 못했습니다: {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 이미 있던 폴더라면 위 mode 가 먹지 않으므로 여기서 조인다.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// 계획을 실제로 실행한다: 내려받기 → (해시 검증) → 조용한 설치.
/// **네트워크를 쓴다.** 테스트에서는 부르지 않는다.
pub fn run_tool_plan(plan: &ToolPlan, progress: Sender<ToolProgress>) -> anyhow::Result<()> {
    let send = |p: ToolProgress| {
        let _ = progress.send(p);
    };
    send(ToolProgress::Started {
        name: plan.name.clone(),
    });

    match install(plan, &send) {
        Ok(()) => {
            send(ToolProgress::Done);
            Ok(())
        }
        Err(e) => {
            send(ToolProgress::Failed {
                message: format!("{e:#}"),
            });
            Err(e)
        }
    }
}

fn install(plan: &ToolPlan, send: &impl Fn(ToolProgress)) -> anyhow::Result<()> {
    // 주소부터 본다. 평문 http 로 받은 설치본은 중간자가 갈아치울 수 있다.
    nl_update::require_https(&plan.url).with_context(|| format!("도구 주소 ({})", plan.name))?;

    let bytes = download(&plan.url, plan.size_hint, send)?;

    if let Some(parent) = plan.dest.parent() {
        create_private_dir(parent)?;
    }

    match &plan.sha256 {
        Some(expected) => {
            let actual = sha256_hex(&bytes);
            if !actual.eq_ignore_ascii_case(expected) {
                anyhow::bail!("내려받은 파일의 sha256 이 다릅니다 (기대 {expected}, 실제 {actual})");
            }
        }
        None => {
            // 검증할 수 없는 파일은 **실행하지 않는다.** 받은 것은 남겨 두어 사용자가 직접 확인할 수 있게 한다.
            std::fs::write(&plan.dest, &bytes)
                .with_context(|| format!("도구를 저장하지 못했습니다: {}", plan.dest.display()))?;
            send(ToolProgress::Downloaded {
                path: plan.dest.clone(),
            });
            anyhow::bail!(
                "알려진 sha256 이 없어 실행하지 않았습니다. 내려받은 파일은 {} 에 있습니다 — \
                 발행처에서 해시를 확인한 뒤 직접 실행하세요.",
                plan.dest.display()
            );
        }
    }

    std::fs::write(&plan.dest, &bytes)
        .with_context(|| format!("도구를 저장하지 못했습니다: {}", plan.dest.display()))?;
    send(ToolProgress::Downloaded {
        path: plan.dest.clone(),
    });

    let (program, args) = silent_install_command(&plan.dest);
    send(ToolProgress::Running {
        command: format!("{program} {}", args.join(" ")),
    });

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
    let flags = [
        "/VERYSILENT".to_string(),
        "/SUPPRESSMSGBOXES".to_string(),
        "/NORESTART".to_string(),
    ]
    .to_vec();
    if cfg!(windows) {
        (path, flags)
    } else {
        let mut args = vec![path];
        args.extend(flags);
        ("wine".to_string(), args)
    }
}

fn download(url: &str, size_hint: u64, send: &impl Fn(ToolProgress)) -> anyhow::Result<Vec<u8>> {
    nl_update::require_https(url).context("도구 주소")?;
    let config = ureq::Agent::config_builder()
        .user_agent(concat!("neural-linker/", env!("CARGO_PKG_VERSION")))
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        // 리다이렉트로도 평문 http 로 내려갈 수 없다. 루프백 시험만 이 빗장을 함께 내린다.
        .https_only(!nl_update::plain_http_allowed())
        .max_redirects(MAX_REDIRECTS)
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut res = agent
        .get(url)
        .call()
        .with_context(|| format!("내려받지 못했습니다: {url}"))?;
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
        send(ToolProgress::Downloading {
            received: out.len() as u64,
            total,
        });
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
        // jrsoftware 의 공식 배포처는 GitHub 릴리스다 (isdl.php 가 여기로 보낸다).
        assert!(
            plan.url.contains("github.com/jrsoftware/issrc/releases"),
            "{}",
            plan.url
        );
        assert!(plan.size_hint > 0);
        assert!(plan.dest.ends_with(INNO_FILE), "{}", plan.dest.display());
        assert!(plan.steps.len() >= 4, "{:?}", plan.steps);
        assert!(plan.steps.iter().any(|s| s.contains("내려받")));
        assert!(
            plan.steps.iter().any(|s| s.contains("zip")),
            "대체 경로를 안내해야 합니다"
        );
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
        assert!(
            steps.contains("관리자 권한"),
            "권한이 필요 없다는 점이 핵심입니다: {steps}"
        );
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

    /// H6: 계획의 주소는 버전이 박혀 있고 해시가 고정돼 있어야 한다.
    #[test]
    fn the_inno_plan_is_pinned_and_verifiable() {
        let plan = install_inno_setup_plan();
        assert!(plan.verified, "해시가 없으면 실행 단계가 막힌다");
        assert_eq!(plan.sha256.as_deref(), Some(INNO_SHA256));
        assert_eq!(plan.size_hint, INNO_SIZE);
        // 버전이 주소에 박혀 있어야 내용이 바뀌지 않는다.
        assert!(plan.url.contains(INNO_VERSION), "{}", plan.url);
        assert!(plan.url.starts_with("https://"), "{}", plan.url);
        // 예전 주소는 설치본이 아니라 안내 페이지로 302 했다.
        assert!(!plan.url.contains("download.php"), "{}", plan.url);
        assert!(plan.verification_note().contains(INNO_SHA256));
    }

    #[test]
    fn a_plan_without_a_hash_is_marked_unverified() {
        let plan = ToolPlan::new("x", "https://h/x", None, 1, PathBuf::from("/tmp/x"), vec![], vec![], 0);
        assert!(!plan.verified);
        assert!(plan.verification_note().contains("실행하지 않습니다"));
        // 빈 문자열은 해시가 없는 것으로 본다.
        let plan = ToolPlan::new(
            "x",
            "https://h/x",
            Some("  ".into()),
            1,
            PathBuf::from("/tmp/x"),
            vec![],
            vec![],
            0,
        );
        assert!(!plan.verified);
        assert!(plan.sha256.is_none());
    }

    /// H6: 해시 없는 계획은 파일만 남기고 실행을 거부한다.
    #[test]
    fn an_unverified_plan_downloads_but_refuses_to_run() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tool.exe");
        let body = "가짜 설치본".as_bytes().to_vec();
        let server = TestServer::start("/tool.exe", body.clone());

        let plan = ToolPlan::new(
            "시험 도구",
            server.url("/tool.exe"),
            None,
            body.len() as u64,
            dest.clone(),
            vec![],
            vec![],
            0,
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        let err = run_tool_plan(&plan, tx).unwrap_err().to_string();

        assert!(err.contains("알려진 sha256 이 없어 실행하지 않았습니다"), "{err}");
        // 받은 파일은 남는다 — 사용자가 직접 확인할 수 있게.
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // 실행 단계는 시작조차 하지 않는다.
        let events: Vec<ToolProgress> = rx.into_iter().collect();
        assert!(
            !events.iter().any(|e| matches!(e, ToolProgress::Running { .. })),
            "실행 단계가 돌았습니다: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, ToolProgress::Downloaded { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn a_wrong_hash_stops_before_running() {
        let dir = tempfile::tempdir().unwrap();
        let body = "가짜 설치본".as_bytes().to_vec();
        let server = TestServer::start("/tool.exe", body);

        let plan = ToolPlan::new(
            "시험 도구",
            server.url("/tool.exe"),
            Some("00".repeat(32)),
            0,
            dir.path().join("tool.exe"),
            vec![],
            vec![],
            0,
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        let err = run_tool_plan(&plan, tx).unwrap_err().to_string();
        assert!(err.contains("sha256 이 다릅니다"), "{err}");
        let events: Vec<ToolProgress> = rx.into_iter().collect();
        assert!(
            !events.iter().any(|e| matches!(e, ToolProgress::Running { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn a_plain_http_tool_url_is_refused_before_any_request() {
        let plan = ToolPlan::new(
            "시험 도구",
            "http://evil.example/tool.exe",
            Some("ab".into()),
            0,
            PathBuf::from("/tmp/x"),
            vec![],
            vec![],
            0,
        );
        let (tx, _rx) = crossbeam_channel::unbounded();
        let err = run_tool_plan(&plan, tx).unwrap_err().to_string();
        assert!(err.contains("도구 주소"), "{err}");
    }

    #[test]
    fn the_tools_folder_is_never_a_shared_temp_path() {
        let dir = tools_dir();
        assert!(dir.is_absolute() || dir.starts_with("."), "{}", dir.display());
        let shown = dir.display().to_string();
        assert!(!shown.starts_with("/tmp/"), "공용 /tmp 로 떨어졌습니다: {shown}");
        assert!(
            !shown.starts_with("/var/tmp/"),
            "공용 임시 폴더로 떨어졌습니다: {shown}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_tools_folder_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        create_private_dir(&nested).unwrap();
        assert_eq!(std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
        // 이미 있던 폴더의 권한도 조인다.
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&nested).unwrap();
        assert_eq!(std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
    }

    /// 시험용 최소 HTTP 서버. 루프백이라 `NL_ALLOW_HTTP=1` 이 있어야 한다.
    struct TestServer {
        addr: String,
        server: std::sync::Arc<tiny_http::Server>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(path: &'static str, body: Vec<u8>) -> Self {
            allow_loopback_http();
            let server = std::sync::Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("서버"));
            let addr = server.server_addr().to_ip().expect("IP").to_string();
            let worker = server.clone();
            let handle = std::thread::spawn(move || {
                for req in worker.incoming_requests() {
                    if req.url().split('?').next() == Some(path) {
                        let _ = req.respond(tiny_http::Response::from_data(body.clone()));
                    } else {
                        let _ = req.respond(tiny_http::Response::empty(404));
                    }
                }
            });
            Self {
                addr,
                server,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// 환경 변수는 프로세스 전역이라 한 번만 세운다.
    fn allow_loopback_http() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| std::env::set_var(nl_update::ALLOW_HTTP_ENV, "1"));
    }
}
