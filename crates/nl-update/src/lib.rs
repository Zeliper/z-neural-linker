//! 자동 업데이트 코어. GUI 의존성이 없어 빌더(`nl-app`)와 배포 런타임(`nl-runtime`) 둘 다 붙일 수 있다.
//!
//! 배포 서버의 `latest.json`(매니페스트)을 읽어 현재 버전과 비교하고, 이 플랫폼 자산을 내려받아
//! sha256 을 검증한 뒤 적용한다. 형식은 `packaging/make-manifest.sh` 가 만드는 것과 같다.
//!
//! 적용 방식은 자산 종류가 정한다:
//! - [`AssetKind::Installer`] (Windows Inno Setup): 설치 프로그램을 조용히 실행하고 앱을 끝낸다.
//!   설치가 끝나면 설치 프로그램이 앱을 다시 띄운다.
//! - [`AssetKind::Binary`] (Linux 단일 실행 파일): 현재 실행 파일을 rename 으로 바꿔치기하고 다시 띄운다.
//!   시스템 경로라 쓸 수 없으면 무엇을 해야 하는지 알려 주는 오류를 낸다.
//!
//! 네트워크 호출은 전부 타임아웃을 받는다. ureq 3 은 기본 타임아웃이 없어 호스트가 응답하지 않으면
//! 스레드가 세션 내내 걸린다 (trust-pms 교훈).

pub mod apply;
pub mod download;
pub mod signature;
pub mod updater;

pub use apply::{apply, apply_to, replace_binary, Applied};
pub use download::{download, prune_downloads, sha256_hex, Progress};
pub use signature::{signature_url, verify_manifest};
pub use updater::{Event, State, Updater};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// 매니페스트 요청 기본 타임아웃. 시작할 때 조용히 확인하는 경로라 짧게 잡는다.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// 연결 타임아웃. 전체 타임아웃과 별개로 먼저 끊는다.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 매니페스트 본문 상한. 이보다 큰 응답은 매니페스트가 아니다.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// 자산 종류. 적용 방법을 정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    /// 설치 프로그램 (Windows). 실행하면 스스로 교체·재시작한다.
    Installer,
    /// 단일 실행 파일 (Linux). 현재 실행 파일을 바꿔치기한다.
    Binary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    /// 소문자 16진수 sha256.
    pub sha256: String,
    pub kind: AssetKind,
    /// 알려진 크기(바이트). 0 이면 모름 — 진행률은 Content-Length 로만 계산한다.
    #[serde(default)]
    pub size: u64,
}

/// `latest.json` 의 내용.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    /// `linux-x86_64` 같은 대상 키 → 자산.
    #[serde(default)]
    pub assets: BTreeMap<String, Asset>,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        serde_json::from_slice(bytes).context("매니페스트 형식이 잘못됐습니다")
    }

    /// 현재 버전보다 새롭고 이 대상의 자산이 있으면 `Some`.
    /// 버전 문자열이 semver 가 아니면 조용히 `None` — 깨진 매니페스트로 엉뚱한 것을 설치하지 않는다.
    pub fn newer_for(&self, current: &semver::Version, target: &str) -> Option<Available> {
        let version = semver::Version::parse(self.version.trim()).ok()?;
        if version <= *current {
            return None;
        }
        let asset = self.assets.get(target)?.clone();
        Some(Available { version, notes: self.notes.clone(), asset, target: target.to_string() })
    }
}

/// 확인 결과 중 "새 버전이 있다".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Available {
    pub version: semver::Version,
    pub notes: String,
    pub asset: Asset,
    /// 어느 대상 키의 자산인지.
    pub target: String,
}

/// 이 빌드의 자산 키: `linux-x86_64`, `windows-x86_64`, `macos-aarch64` …
pub fn target_key() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// 호출하는 크레이트의 버전. 파싱에 실패하면 `0.0.0` 이라 어떤 매니페스트든 새 버전으로 보인다.
#[macro_export]
macro_rules! current_version {
    () => {
        ::semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(|_| ::semver::Version::new(0, 0, 0))
    };
}

/// 타임아웃이 설정된 ureq 에이전트. 큰 본문을 받을 때는 `recv_body` 를 따로 늘려 잡는다.
pub(crate) fn agent(global: Duration, recv_body: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .user_agent(concat!("neural-linker-update/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(CONNECT_TIMEOUT.min(global)))
        .timeout_global(Some(global))
        .timeout_recv_body(Some(recv_body))
        .build();
    ureq::Agent::new_with_config(config)
}

/// 매니페스트를 받아 현재 버전과 비교한다. 새 버전이 없으면 `Ok(None)`.
/// 서명 검증까지 하려면 [`check_signed`] 를 쓴다.
pub fn check(url: &str, current: &semver::Version, timeout: Duration) -> anyhow::Result<Option<Available>> {
    check_signed(url, current, timeout, None)
}

/// [`check`] 에 minisign 서명 검증을 더한 것.
///
/// `public_key` 가 있으면 `<url>.minisig` 를 함께 받아 검증하고, 검증에 실패하면 매니페스트를 아예 읽지 않는다.
/// `None` 이면 검증을 건너뛰고 경고 로그만 남긴다.
pub fn check_signed(
    url: &str,
    current: &semver::Version,
    timeout: Duration,
    public_key: Option<&str>,
) -> anyhow::Result<Option<Available>> {
    let raw = fetch_manifest_bytes(url, timeout)?;

    match public_key {
        Some(key) => {
            let sig_url = signature_url(url);
            let sig = fetch_text(&sig_url, timeout)
                .with_context(|| format!("서명을 받지 못했습니다: {sig_url}"))?;
            verify_manifest(&raw, &sig, Some(key)).context("매니페스트 서명 검증에 실패했습니다")?;
        }
        None => log::warn!("서명 공개키가 없어 매니페스트 검증을 건너뜁니다: {url}"),
    }

    let manifest = Manifest::parse(&raw)?;
    let target = target_key();
    let found = manifest.newer_for(current, &target);
    if found.is_none() && !manifest.assets.contains_key(&target) {
        log::info!("매니페스트에 이 플랫폼({target}) 자산이 없습니다");
    }
    Ok(found)
}

fn fetch_manifest_bytes(url: &str, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let agent = agent(timeout, timeout);
    let mut res =
        agent.get(url).call().with_context(|| format!("매니페스트를 받지 못했습니다: {url}"))?;
    let text = res
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST_BYTES)
        .read_to_string()
        .with_context(|| format!("매니페스트를 읽지 못했습니다: {url}"))?;
    Ok(text.into_bytes())
}

fn fetch_text(url: &str, timeout: Duration) -> anyhow::Result<String> {
    let agent = agent(timeout, timeout);
    let mut res = agent.get(url).call()?;
    Ok(res.body_mut().with_config().limit(MAX_MANIFEST_BYTES).read_to_string()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, target: &str) -> Manifest {
        let mut assets = BTreeMap::new();
        assets.insert(
            target.to_owned(),
            Asset { url: "https://x/y".into(), sha256: "ab".into(), kind: AssetKind::Binary, size: 0 },
        );
        Manifest { version: version.into(), notes: "고침".into(), assets }
    }

    #[test]
    fn newer_only_when_version_is_higher_and_asset_exists() {
        let cur = semver::Version::new(0, 1, 0);
        assert!(manifest("0.1.0", "linux-x86_64").newer_for(&cur, "linux-x86_64").is_none(), "같은 버전");
        assert!(manifest("0.0.9", "linux-x86_64").newer_for(&cur, "linux-x86_64").is_none(), "낮은 버전");

        let r = manifest("0.2.0", "linux-x86_64").newer_for(&cur, "linux-x86_64").unwrap();
        assert_eq!(r.version, semver::Version::new(0, 2, 0));
        assert_eq!(r.target, "linux-x86_64");
        assert_eq!(r.notes, "고침");

        assert!(manifest("0.2.0", "linux-x86_64").newer_for(&cur, "windows-x86_64").is_none(), "대상 자산 없음");
        assert!(manifest("무엇", "linux-x86_64").newer_for(&cur, "linux-x86_64").is_none(), "깨진 버전 문자열");
        // 프리릴리스는 같은 번호의 정식 버전보다 낮다.
        assert!(manifest("0.1.0-beta.1", "linux-x86_64").newer_for(&cur, "linux-x86_64").is_none());
        // 앞뒤 공백은 무시한다.
        assert!(manifest(" 0.2.0 ", "linux-x86_64").newer_for(&cur, "linux-x86_64").is_some());
    }

    #[test]
    fn manifest_json_shape_matches_make_manifest_sh() {
        let text = r#"{"version":"1.2.3","notes":"고침","assets":{"windows-x86_64":{"url":"https://h/s.exe","sha256":"AB","kind":"installer","size":12}}}"#.as_bytes();
        let m = Manifest::parse(text).unwrap();
        assert_eq!(m.assets["windows-x86_64"].kind, AssetKind::Installer);
        assert_eq!(m.assets["windows-x86_64"].size, 12);
        assert!(Manifest::parse(b"{").is_err());
        // 자산이 없어도, notes 가 없어도 읽힌다.
        assert!(Manifest::parse(r#"{"version":"0.1.0"}"#.as_bytes()).unwrap().assets.is_empty());
    }

    #[test]
    fn target_key_is_os_dash_arch() {
        let k = target_key();
        assert!(k.starts_with(std::env::consts::OS), "{k}");
        assert!(k.ends_with(std::env::consts::ARCH), "{k}");
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(k, "linux-x86_64");
        }
    }

    #[test]
    fn current_version_macro_parses_this_crate() {
        let v: semver::Version = current_version!();
        assert_eq!(v.to_string(), env!("CARGO_PKG_VERSION"));
    }
}
