//! 빌드에 필요한 외부 요소의 상태 점검과 동의 기반 설치.
//!
//! 원칙(`docs/ARCHITECTURE.md` "도구 설치"): 앱은 사용자가 **승인하기 전에는 아무것도 내려받지 않는다**.
//! `check()` 가 상태를 보고하고, 없는 항목은 [`Plan`] 으로 "무엇을 · 어디서 · 어디에 · 얼마나" 를 명시한 뒤
//! 승인 시에만 [`spawn`] 으로 백그라운드 작업을 시작한다. 내려받은 파일은 sha256 으로 검증한다.

use crate::views::fmt_bytes;
use nl_core::BuildTarget;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

/// 런타임 매니페스트 기본 주소. `NL_RUNTIME_MANIFEST` 환경 변수가 있으면 그것이 우선한다.
pub const DEFAULT_MANIFEST_URL: &str = "https://updates.trustanc.dev/neural-linker/runtimes/latest.json";
/// 매니페스트·다운로드 타임아웃.
const NET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// 내려받기 허용 최대 크기 (런타임 바이너리는 수십 MB 수준).
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

// ───────────────────────────── 도구 ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// 대상 플랫폼의 런타임 실행 파일 (번들을 붙일 바탕).
    Runtime(BuildTarget),
    /// Windows 설치 프로그램 제작기.
    InnoSetup,
}

impl ToolKind {
    pub fn label(&self) -> String {
        match self {
            ToolKind::Runtime(t) => format!("런타임 바이너리 · {}", t.label()),
            ToolKind::InnoSetup => "Inno Setup (Windows 설치 프로그램)".into(),
        }
    }

    /// 이 도구가 없으면 무엇을 못 하는가.
    pub fn why(&self) -> &'static str {
        match self {
            ToolKind::Runtime(_) => "이 대상의 배포 산출물을 만들 수 없습니다",
            ToolKind::InnoSetup => "setup.exe 대신 zip 으로만 배포합니다",
        }
    }

    /// 없어도 빌드를 막지 않는가.
    pub fn optional(&self) -> bool {
        matches!(self, ToolKind::InnoSetup)
    }
}

/// 한 도구의 현재 상태.
#[derive(Clone, Debug)]
pub struct ToolState {
    pub kind: ToolKind,
    /// 찾은 경로. `None` 이면 없음.
    pub path: Option<PathBuf>,
    /// 어디서 찾았는지 / 없으면 어디를 봤는지.
    pub note: String,
}

impl ToolState {
    pub fn ok(&self) -> bool {
        self.path.is_some()
    }
}

/// 지정한 대상들에 필요한 도구 상태. Windows 대상이 있을 때만 Inno Setup 을 본다.
pub fn check(targets: &[BuildTarget]) -> Vec<ToolState> {
    let mut out: Vec<ToolState> = targets.iter().map(|t| check_runtime(*t)).collect();
    if targets.contains(&BuildTarget::WindowsX64) {
        out.push(check_inno());
    }
    out
}

/// 런타임 실행 파일을 찾는다. nl-bundle 의 규칙(실행 파일 옆 · `NL_RUNTIMES_DIR`) 을 먼저 보고,
/// 그다음 이 앱이 내려받아 두는 사용자 데이터 폴더를 본다.
pub fn find_runtime(target: BuildTarget) -> Option<PathBuf> {
    if let Some(p) = nl_bundle::find_runtime(bundle_target(target)) {
        return Some(p);
    }
    let p = runtimes_dir()?.join(target.triple()).join(runtime_file_name(target));
    p.is_file().then_some(p)
}

fn check_runtime(target: BuildTarget) -> ToolState {
    let path = find_runtime(target);
    let note = match &path {
        Some(p) => p.display().to_string(),
        None => {
            let mut places = vec!["앱 실행 파일 옆".to_string()];
            if std::env::var_os("NL_RUNTIMES_DIR").is_some() {
                places.push("NL_RUNTIMES_DIR".into());
            }
            if let Some(d) = runtimes_dir() {
                places.push(d.join(target.triple()).display().to_string());
            }
            format!("찾은 곳 없음 — {}", places.join(" · "))
        }
    };
    ToolState {
        kind: ToolKind::Runtime(target),
        path,
        note,
    }
}

fn check_inno() -> ToolState {
    // 탐지 규칙은 nl-bundle 이 소유한다 — 빌드할 때 실제로 쓰는 것과 같은 경로여야 한다.
    if let Some(found) = nl_bundle::find_inno_setup() {
        let note = found.describe();
        return ToolState {
            kind: ToolKind::InnoSetup,
            path: Some(found.path.clone()),
            note,
        };
    }
    let note = match which("wine") {
        Some(w) => format!("wine 은 있음({}) — Inno Setup 컴파일러는 없음", crate::views::tilde(&w)),
        None => "Inno Setup 도 wine 도 찾지 못함".into(),
    };
    ToolState {
        kind: ToolKind::InnoSetup,
        path: None,
        note,
    }
}

/// 내려받은 런타임을 두는 폴더: `<사용자 데이터>/neural-linker/runtimes/`.
pub fn runtimes_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "trustanc", "neural-linker").map(|d| d.data_dir().join("runtimes"))
}

pub fn runtime_file_name(target: BuildTarget) -> &'static str {
    match target {
        BuildTarget::LinuxX64 => "nl-runtime",
        BuildTarget::WindowsX64 => "nl-runtime.exe",
    }
}

pub fn bundle_target(t: BuildTarget) -> nl_bundle::Target {
    match t {
        BuildTarget::LinuxX64 => nl_bundle::Target::LinuxX64,
        BuildTarget::WindowsX64 => nl_bundle::Target::WindowsX64,
    }
}

/// 이 실행 파일이 도는 호스트에 해당하는 대상.
pub fn host_target() -> Option<BuildTarget> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(BuildTarget::LinuxX64)
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(BuildTarget::WindowsX64)
    } else {
        None
    }
}

/// PATH 에서 실행 파일 찾기.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

// ───────────────────────────── 매니페스트 ─────────────────────────────

/// `latest.json` (packaging/make-manifest.sh 형식).
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub assets: std::collections::BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct Asset {
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

impl Manifest {
    /// 대상에 맞는 자산. 트리플 키(`x86_64-unknown-linux-gnu`)를 먼저 보고,
    /// 없으면 패키징 스크립트가 쓰는 짧은 키(`linux-x86_64`)를 본다.
    pub fn asset_for(&self, target: BuildTarget) -> Option<&Asset> {
        self.assets
            .get(target.triple())
            .or_else(|| self.assets.get(short_key(target)))
    }
}

pub fn short_key(target: BuildTarget) -> &'static str {
    match target {
        BuildTarget::LinuxX64 => "linux-x86_64",
        BuildTarget::WindowsX64 => "windows-x86_64",
    }
}

/// 설정에서 매니페스트 주소를 정한다: 환경 변수 → 저장된 값 → 기본값.
pub fn manifest_url(stored: Option<&str>) -> String {
    if let Some(v) = std::env::var("NL_RUNTIME_MANIFEST")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return v;
    }
    match stored.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => v.to_string(),
        None => DEFAULT_MANIFEST_URL.to_string(),
    }
}

fn fetch_manifest(url: &str) -> Result<Manifest, String> {
    let body = nl_io::call("GET", url, &Default::default(), None, NET_TIMEOUT)
        .map_err(|e| format!("{e:#}"))
        .and_then(|r| {
            if r.is_success() {
                Ok(r.body)
            } else {
                Err(format!("HTTP {}", r.status))
            }
        })?;
    serde_json::from_str(&body).map_err(|e| format!("매니페스트를 읽지 못했습니다: {e}"))
}

// ───────────────────────────── 설치 계획 ─────────────────────────────

/// 사용자에게 보여 주고 승인을 받는 내용. 승인 전에는 아무 일도 일어나지 않는다.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub tool: ToolKind,
    /// 무엇을 하는지 한 줄.
    pub what: String,
    /// 승인 화면에 차례대로 보여 줄 자세한 단계. 비어 있어도 된다.
    pub steps: Vec<String>,
    /// 어디서 (URL 또는 실행할 명령).
    pub from: String,
    /// 어디에 놓는지.
    pub to: PathBuf,
    /// 예상 크기 (모르면 0).
    pub size: u64,
    pub method: Method,
}

impl Plan {
    /// nl-bundle 이 준 검증 설명. 그 크레이트가 소유한 계획일 때만 있다.
    pub fn verification_note(&self) -> Option<String> {
        match &self.method {
            Method::BundleTool(inner) => Some(inner.verification_note()),
            _ => None,
        }
    }

    /// 이 계획이 무엇을 확인하는지.
    pub fn verification(&self) -> Verification {
        match &self.method {
            Method::Download { sha256, .. } => Verification {
                sha256: !sha256.trim().is_empty(),
                signature: false,
                downloads: true,
            },
            // nl-bundle 이 계획을 만들 때 이미 정해 둔 값을 그대로 쓴다 — 두 곳에서 따로 판단하면 어긋난다.
            Method::BundleTool(inner) => Verification {
                sha256: inner.verified,
                signature: false,
                downloads: true,
            },
            Method::CargoBuild { .. } => Verification {
                sha256: false,
                signature: false,
                downloads: false,
            },
        }
    }
}

/// 받은 파일을 무엇으로 확인하는지. 동의 화면이 그대로 보여 준다 (보안 리뷰 M21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verification {
    /// 내려받은 바이트를 sha256 으로 맞춰 보는가.
    pub sha256: bool,
    /// 서명을 검증하는가. 지금은 어느 경로도 하지 않는다.
    pub signature: bool,
    /// 네트워크에서 받아 오는가. 소스 빌드는 아니다.
    pub downloads: bool,
}

impl Verification {
    /// 사람이 읽는 줄 목록. 확인하지 **않는** 것도 적는다 — 빠진 항목이 보여야 판단할 수 있다.
    ///
    /// `note` 는 nl-bundle 이 준 한 줄이 있으면 그것을 쓴다 (해시 값까지 적혀 있다).
    pub fn lines_with(&self, note: Option<&str>) -> Vec<(String, bool)> {
        let mut v = self.lines();
        if let Some(n) = note {
            if !v.is_empty() {
                v[0] = (n.to_string(), self.sha256);
            }
        }
        v
    }

    /// 사람이 읽는 줄 목록.
    pub fn lines(&self) -> Vec<(String, bool)> {
        if !self.downloads {
            return vec![(
                "이 컴퓨터에서 직접 빌드합니다 — 내려받는 파일이 없습니다".to_string(),
                true,
            )];
        }
        vec![
            (
                if self.sha256 {
                    "sha256 으로 받은 파일을 확인합니다".into()
                } else {
                    "sha256 이 없어 확인하지 못합니다".to_string()
                },
                self.sha256,
            ),
            (
                if self.signature {
                    "서명을 검증합니다".into()
                } else {
                    "서명 검증은 하지 않습니다".to_string()
                },
                self.signature,
            ),
        ]
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Method {
    /// 매니페스트에서 받은 자산을 내려받아 검증한다.
    Download { url: String, sha256: String },
    /// 이 소스 워크스페이스에서 직접 빌드한다.
    CargoBuild { workspace: PathBuf },
    /// nl-bundle 이 소유한 도구 설치 계획(내려받기 + 조용한 설치)을 그대로 실행한다.
    BundleTool(Box<nl_bundle::ToolPlan>),
}

/// Inno Setup 설치 계획. 내용은 nl-bundle 이 정하고, 여기서는 동의 화면에 맞게 감싸기만 한다.
pub fn plan_inno_setup() -> Plan {
    let inner = nl_bundle::install_inno_setup_plan();
    Plan {
        tool: ToolKind::InnoSetup,
        what: format!("{} 을 내려받아 조용히 설치합니다", inner.name),
        steps: inner.steps.clone(),
        from: inner.url.clone(),
        to: inner.dest.clone(),
        size: inner.size_hint,
        method: Method::BundleTool(Box::new(inner)),
    }
}

/// 런타임을 구할 방법을 정한다. 매니페스트를 먼저 보고, 없거나 그 대상이 빠져 있으면
/// 호스트 대상에 한해 소스 빌드를 제안한다. 둘 다 안 되면 `Err` 에 이유를 담는다.
pub fn plan_runtime(target: BuildTarget, manifest_url: &str) -> Result<Plan, String> {
    let dest_dir = runtimes_dir().ok_or_else(|| "사용자 데이터 폴더를 찾지 못했습니다".to_string())?;
    let to = dest_dir.join(target.triple()).join(runtime_file_name(target));

    let manifest_err = match fetch_manifest(manifest_url) {
        Ok(m) => match m.asset_for(target) {
            // 해시가 없으면 내려받은 것이 무엇인지 확인할 방법이 없다. 실행 파일이라 더더욱
            // 그냥 실행할 수 없으므로 계획을 아예 만들지 않는다 (보안 리뷰 H5).
            Some(a) if a.sha256.trim().is_empty() => {
                format!(
                    "매니페스트의 {} 자산에 sha256 이 없습니다 — 받은 파일을 확인할 수 없어 내려받지 않습니다. \
                     매니페스트를 고치거나 런타임을 직접 빌드하세요",
                    target.label()
                )
            }
            Some(a) => {
                return Ok(Plan {
                    tool: ToolKind::Runtime(target),
                    what: format!("{} 런타임 실행 파일을 내려받습니다", target.label()),
                    steps: vec![
                        format!("매니페스트가 알려 준 자산을 내려받습니다 ({}).", fmt_bytes(a.size)),
                        "내려받은 파일의 sha256 을 매니페스트 값과 맞춰 봅니다.".to_string(),
                        format!("맞으면 {} 에 실행 권한을 주고 놓습니다.", crate::views::tilde(&to)),
                    ],
                    from: a.url.clone(),
                    to,
                    size: a.size,
                    method: Method::Download {
                        url: a.url.clone(),
                        sha256: a.sha256.clone(),
                    },
                })
            }
            None => format!("매니페스트에 {} 자산이 없습니다", target.label()),
        },
        Err(e) => format!("매니페스트를 가져오지 못했습니다: {e}"),
    };

    // 호스트 대상이고 소스 워크스페이스 안에서 돌고 있으면 직접 빌드할 수 있다.
    if host_target() == Some(target) {
        if let Some(ws) = workspace_root() {
            return Ok(Plan {
                tool: ToolKind::Runtime(target),
                what: "이 소스 워크스페이스에서 런타임을 직접 빌드합니다".to_string(),
                steps: vec![
                    format!("내려받기로는 구할 수 없었습니다: {manifest_err}"),
                    format!(
                        "워크스페이스 {} 에서 `cargo build --release -p nl-runtime` 를 돌립니다.",
                        ws.display()
                    ),
                    "빌드에는 몇 분이 걸릴 수 있고 그동안 네트워크로 의존성을 받습니다.".to_string(),
                ],
                from: "cargo build --release -p nl-runtime".into(),
                to: ws.join("target/release").join(runtime_file_name(target)),
                size: 0,
                method: Method::CargoBuild { workspace: ws },
            });
        }
    }
    Err(manifest_err)
}

/// 워크스페이스 루트(`[workspace]` 가 있는 `Cargo.toml`)를 위로 올라가며 찾는다.
pub fn workspace_root() -> Option<PathBuf> {
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::current_dir() {
        starts.push(d);
    }
    if let Ok(e) = std::env::current_exe() {
        if let Some(p) = e.parent() {
            starts.push(p.to_path_buf());
        }
    }
    for start in starts {
        let mut dir = Some(start.as_path());
        while let Some(d) = dir {
            let manifest = d.join("Cargo.toml");
            if let Ok(text) = std::fs::read_to_string(&manifest) {
                if text.contains("[workspace]") && d.join("crates/nl-runtime/Cargo.toml").is_file() {
                    return Some(d.to_path_buf());
                }
            }
            dir = d.parent();
        }
    }
    None
}

// ───────────────────────────── 실행 ─────────────────────────────

#[derive(Clone, Debug)]
pub enum ToolEvent {
    Log(String),
    /// 0.0..=1.0. 전체 크기를 모르면 오지 않는다.
    Progress(f32),
    Done(PathBuf),
    Failed(String),
}

/// 승인된 계획을 백그라운드에서 실행한다. UI 는 채널만 읽는다.
pub fn spawn(plan: Plan, cancel: Arc<AtomicBool>) -> Receiver<ToolEvent> {
    let (tx, rx) = channel();
    let name = "nl-tools".to_string();
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        let result = match plan.method.clone() {
            Method::Download { url, sha256 } => download(&url, &sha256, plan.size, &plan.to, &tx, &cancel),
            Method::CargoBuild { workspace } => cargo_build_runtime(&workspace, &plan.to, &tx),
            Method::BundleTool(inner) => run_bundle_tool(&inner, &tx),
        };
        let _ = match result {
            Ok(p) => tx.send(ToolEvent::Done(p)),
            Err(e) => tx.send(ToolEvent::Failed(e)),
        };
    });
    if let Err(e) = spawned {
        // 스레드를 못 만들면 채널로 알리고 끝낸다 (호출자는 같은 경로로 처리한다).
        let (tx2, rx2) = channel();
        let _ = tx2.send(ToolEvent::Failed(format!("작업 스레드를 만들지 못했습니다: {e}")));
        return rx2;
    }
    rx
}

fn download(
    url: &str,
    sha256: &str,
    expected_size: u64,
    dest: &Path,
    tx: &std::sync::mpsc::Sender<ToolEvent>,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let _ = tx.send(ToolEvent::Log(format!("내려받는 중: {url}")));
    let resp = ureq::get(url)
        .config()
        .timeout_global(Some(NET_TIMEOUT))
        .build()
        .call()
        .map_err(|e| format!("연결 실패: {e}"))?;
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(expected_size);
    if total > MAX_DOWNLOAD {
        return Err(format!("파일이 너무 큽니다: {total} 바이트"));
    }

    let mut reader = resp.into_body().into_reader();
    let mut buf = Vec::with_capacity(total.min(MAX_DOWNLOAD) as usize);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        // 취소는 청크 사이에서만 본다. 받은 것은 버리고 파일은 쓰지 않는다.
        if cancel.load(Ordering::Relaxed) {
            return Err("사용자가 취소했습니다".to_string());
        }
        let n = reader.read(&mut chunk).map_err(|e| format!("읽기 실패: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() as u64 > MAX_DOWNLOAD {
            return Err("파일이 너무 큽니다".into());
        }
        if total > 0 {
            let _ = tx.send(ToolEvent::Progress((buf.len() as f32 / total as f32).clamp(0.0, 1.0)));
        }
    }

    // 해시가 없는 계획은 `plan_runtime` 이 만들지 않는다. 여기까지 왔다면 어딘가 잘못된 것이라
    // 파일을 쓰지 않고 멈춘다 — 검증 없이 실행 파일을 놓는 경로를 남기지 않는다.
    if sha256.trim().is_empty() {
        return Err("sha256 이 없어 받은 파일을 확인할 수 없습니다 — 파일을 버렸습니다".to_string());
    }
    let got = nl_bundle::sha256_hex(&buf);
    if !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err(format!(
            "sha256 이 다릅니다 (기대 {sha256}, 실제 {got}) — 파일을 버렸습니다"
        ));
    }
    let _ = tx.send(ToolEvent::Log("sha256 검증 통과".into()));

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    crate::project::write_atomic(dest, &buf)?;
    make_executable(dest);
    let _ = tx.send(ToolEvent::Log(format!("저장: {}", crate::views::tilde(dest))));
    Ok(dest.to_path_buf())
}

fn cargo_build_runtime(
    workspace: &Path,
    dest: &Path,
    tx: &std::sync::mpsc::Sender<ToolEvent>,
) -> Result<PathBuf, String> {
    let _ = tx.send(ToolEvent::Log(format!(
        "cargo build --release -p nl-runtime ({})",
        workspace.display()
    )));
    let mut child = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "nl-runtime"])
        .current_dir(workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cargo 를 실행하지 못했습니다: {e}"))?;

    // cargo 는 진행 상황을 stderr 로 낸다. 두 줄기를 모두 로그로 흘린다.
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        let _ = std::thread::Builder::new().name("nl-cargo-out".into()).spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(ToolEvent::Log(line));
            }
        });
    }
    let mut err_lines: Vec<String> = Vec::new();
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send(ToolEvent::Log(line.clone()));
            err_lines.push(line);
        }
    }
    let status = child.wait().map_err(|e| format!("cargo 를 기다리지 못했습니다: {e}"))?;
    if !status.success() {
        let tail = err_lines
            .iter()
            .rev()
            .take(3)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" / ");
        return Err(format!("cargo 빌드 실패 ({status}): {tail}"));
    }
    if !dest.is_file() {
        return Err(format!(
            "빌드는 끝났지만 산출물이 없습니다: {}",
            crate::views::tilde(dest)
        ));
    }
    Ok(dest.to_path_buf())
}

/// nl-bundle 의 도구 설치를 돌리며 진행 상황을 우리 이벤트로 옮긴다.
fn run_bundle_tool(plan: &nl_bundle::ToolPlan, tx: &std::sync::mpsc::Sender<ToolEvent>) -> Result<PathBuf, String> {
    use nl_bundle::ToolProgress;
    // nl-bundle 은 crossbeam 채널을 받는다.
    let (ptx, prx) = crossbeam_channel::unbounded::<ToolProgress>();
    let out = tx.clone();
    // 진행 이벤트는 별도 스레드에서 옮긴다 — `run_tool_plan` 이 끝날 때까지 막히기 때문이다.
    let pump = std::thread::Builder::new().name("nl-tool-pump".into()).spawn(move || {
        for p in prx {
            let ev = match p {
                ToolProgress::Started { name } => ToolEvent::Log(format!("{name} 설치를 시작합니다")),
                ToolProgress::Downloading { received, total } => match total {
                    Some(t) if t > 0 => ToolEvent::Progress((received as f32 / t as f32).clamp(0.0, 1.0)),
                    _ => ToolEvent::Log(format!("내려받는 중… {received} 바이트")),
                },
                ToolProgress::Downloaded { path } => {
                    ToolEvent::Log(format!("내려받음: {}", crate::views::tilde(&path)))
                }
                ToolProgress::Running { command } => ToolEvent::Log(format!("실행: {command}")),
                // 설치 프로그램이 뱉는 줄. wine 이 왜 실패했는지는 여기에만 나온다.
                ToolProgress::Output(line) => ToolEvent::Log(format!("  {}", line.trim_end())),
                ToolProgress::Done => ToolEvent::Log("설치 완료".into()),
                ToolProgress::Failed { message } => ToolEvent::Log(format!("실패: {message}")),
            };
            let _ = out.send(ev);
        }
    });
    let result = nl_bundle::run_tool_plan(plan, ptx).map_err(|e| format!("{e:#}"));
    if let Ok(h) = pump {
        let _ = h.join();
    }
    result.map(|()| plan.dest.clone())
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perm = meta.permissions();
        perm.set_mode(perm.mode() | 0o755);
        let _ = std::fs::set_permissions(path, perm);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// 파일 탐색기에서 폴더 열기. 실패해도 조용히 무시할 수 있게 결과를 돌려준다.
pub fn open_in_file_manager(path: &Path) -> Result<(), String> {
    let cmd = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(cmd)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{cmd} 을(를) 실행하지 못했습니다: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_accepts_both_key_styles() {
        let json = r#"{"version":"0.1.0","assets":{
            "linux-x86_64":{"url":"https://x/a","sha256":"aa","size":10},
            "x86_64-pc-windows-msvc":{"url":"https://x/b","sha256":"bb","size":20}}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.asset_for(BuildTarget::LinuxX64).unwrap().url, "https://x/a");
        assert_eq!(m.asset_for(BuildTarget::WindowsX64).unwrap().size, 20);
    }

    #[test]
    fn manifest_url_prefers_env_then_stored() {
        // 환경 변수가 없을 때의 규칙만 확인한다 (환경을 건드리면 병렬 테스트가 흔들린다).
        if std::env::var_os("NL_RUNTIME_MANIFEST").is_none() {
            assert_eq!(manifest_url(None), DEFAULT_MANIFEST_URL);
            assert_eq!(manifest_url(Some("  ")), DEFAULT_MANIFEST_URL);
            assert_eq!(manifest_url(Some("https://내/latest.json")), "https://내/latest.json");
        }
    }

    #[test]
    fn runtime_file_names_match_the_target() {
        assert_eq!(runtime_file_name(BuildTarget::LinuxX64), "nl-runtime");
        assert_eq!(runtime_file_name(BuildTarget::WindowsX64), "nl-runtime.exe");
        assert_eq!(bundle_target(BuildTarget::LinuxX64), nl_bundle::Target::LinuxX64);
        assert_eq!(bundle_target(BuildTarget::WindowsX64), nl_bundle::Target::WindowsX64);
        assert_eq!(short_key(BuildTarget::LinuxX64), "linux-x86_64");
    }

    #[test]
    fn check_reports_inno_only_for_windows_targets() {
        let linux = check(&[BuildTarget::LinuxX64]);
        assert_eq!(linux.len(), 1);
        assert!(matches!(linux[0].kind, ToolKind::Runtime(BuildTarget::LinuxX64)));
        let both = check(&[BuildTarget::LinuxX64, BuildTarget::WindowsX64]);
        assert_eq!(both.len(), 3);
        assert!(both.iter().any(|t| t.kind == ToolKind::InnoSetup));
        // Inno Setup 이 없어도 빌드는 막지 않는다.
        assert!(ToolKind::InnoSetup.optional());
        assert!(!ToolKind::Runtime(BuildTarget::LinuxX64).optional());
    }

    /// Inno Setup 계획은 네트워크 없이 만들어지고, 동의 화면에 보여 줄 내용이 모두 채워져야 한다.
    #[test]
    fn inno_plan_is_ready_for_the_consent_modal() {
        let plan = plan_inno_setup();
        assert_eq!(plan.tool, ToolKind::InnoSetup);
        assert!(plan.from.starts_with("https://"), "어디서 받는지가 보여야 한다");
        assert!(!plan.what.trim().is_empty(), "무엇을 하는지가 보여야 한다");
        // 단계는 모달에 줄줄이 그려진다 — 한 줄로 이어 붙이면 창이 화면 밖까지 커진다.
        assert!(
            plan.steps.len() >= 3,
            "자세한 단계가 목록으로 있어야 한다: {:?}",
            plan.steps
        );
        assert!(plan.steps.iter().all(|s| !s.trim().is_empty()));
        assert!(plan.what.lines().count() == 1, "요약은 한 줄이어야 한다");
        assert!(plan.size > 0, "크기 어림값이 있어야 한다");
        assert!(matches!(plan.method, Method::BundleTool(_)));
        // 계획을 만드는 것만으로는 아무것도 설치되지 않는다.
        assert!(!plan.to.exists() || plan.to.is_file());
    }

    /// 동의 화면은 확인하는 것과 확인하지 못하는 것을 모두 말해야 한다.
    #[test]
    fn verification_tells_both_what_is_and_is_not_checked() {
        let inno = plan_inno_setup();
        let v = inno.verification();
        assert!(v.downloads, "내려받는 계획이다");
        assert!(!v.signature, "서명 검증은 아직 어느 경로도 하지 않는다");
        // 줄마다 참·거짓이 붙어 화면에서 ✔/✖ 로 갈린다.
        let lines = v.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|(t, _)| t.contains("서명")));
        // nl-bundle 이 준 설명이 있으면 첫 줄을 대신한다 — 해시 값까지 들어 있다.
        let note = inno.verification_note().expect("번들 계획에는 설명이 있다");
        let merged = v.lines_with(Some(&note));
        assert_eq!(merged[0].0, note);
        assert_eq!(merged[0].1, v.sha256, "표시와 판정이 어긋나면 안 된다");
    }

    /// 소스 빌드는 내려받는 것이 없으니 해시 이야기를 하지 않는다.
    #[test]
    fn a_source_build_says_it_downloads_nothing() {
        let v = Verification {
            sha256: false,
            signature: false,
            downloads: false,
        };
        let lines = v.lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].1, "직접 빌드는 경고할 일이 아니다");
        assert!(lines[0].0.contains("내려받는 파일이 없습니다"));
    }

    #[test]
    fn workspace_root_is_this_repo_when_tests_run_here() {
        // 테스트는 워크스페이스 안에서 돈다 — 루트를 찾지 못하면 빌드 폴백이 동작하지 않는다.
        let root = workspace_root().expect("워크스페이스 루트");
        assert!(root.join("crates/nl-runtime/Cargo.toml").is_file());
    }

    #[test]
    fn which_finds_a_standard_tool_and_misses_nonsense() {
        // 어떤 유닉스에도 있는 실행 파일 하나로 경로 탐색이 도는지만 본다.
        if cfg!(unix) {
            assert!(which("sh").is_some());
        }
        assert!(which("확실히-없는-도구-이름-12345").is_none());
    }
}
