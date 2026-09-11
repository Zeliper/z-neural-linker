//! 빌드에 필요한 외부 도구 설치 계획. 빌더는 `ToolPlan` 을 동의 모달에 그대로 펼쳐 보여 주고,
//! 사용자가 승인하면 `run_tool_plan` 으로 내려받아 설치한다. 계획을 만드는 일과 실행하는 일이 나뉘어 있어
//! "무엇을 어디서 받아 어디에 놓는지" 를 먼저 보여 줄 수 있다.

use crate::sha256_hex;
use anyhow::Context;
use crossbeam_channel::Sender;
use std::io::Read;
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
}

/// 설치 진행 상황. 채널로 흘려 보내 빌드 UI 가 진행률을 그린다.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolProgress {
    Started { name: String },
    /// `total` 은 서버가 길이를 알려 줄 때만 채워진다.
    Downloading { received: u64, total: Option<u64> },
    Downloaded { path: PathBuf },
    Running { command: String },
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
    }
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
    fn tools_dir_is_absolute() {
        assert!(tools_dir().is_absolute(), "{}", tools_dir().display());
    }
}
