//! `.nlapp` 번들 (zip) 읽기/쓰기와 런타임 바이너리 첨부. 규약은 `nl_core::bundle`.

use nl_core::{BundleManifest, Project};
use std::path::{Path, PathBuf};

/// 메모리에 풀린 번들.
#[derive(Clone, Debug)]
pub struct Bundle {
    pub manifest: BundleManifest,
    pub project: Project,
    /// `weights/<file>` → 바이트.
    pub weights: std::collections::BTreeMap<String, Vec<u8>>,
    /// `assets/<path>` → 바이트.
    pub assets: std::collections::BTreeMap<String, Vec<u8>>,
}

impl Bundle {
    /// zip 바이트로 직렬화.
    pub fn to_zip(&self) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("번들 미구현")
    }
    pub fn from_zip(_bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::bail!("번들 미구현")
    }
    /// 가중치를 임시 폴더에 풀어 `Session::load` 가 읽을 경로를 돌려준다.
    pub fn materialize_weights(&self, _dir: &Path) -> anyhow::Result<std::collections::BTreeMap<String, PathBuf>> {
        anyhow::bail!("번들 미구현")
    }
}

/// 런타임 실행 파일 + 번들 + 꼬리표 → `out`. 실행 권한 유지.
pub fn attach(_runtime_exe: &Path, _bundle_zip: &[u8], _out: &Path) -> anyhow::Result<()> {
    anyhow::bail!("번들 미구현")
}

/// 현재 실행 파일(또는 주어진 파일)에 첨부된 번들을 읽는다. 없으면 `Ok(None)`.
pub fn read_attached(_exe: &Path) -> anyhow::Result<Option<Bundle>> {
    anyhow::bail!("번들 미구현")
}

/// 배포 아카이브: Linux `tar.gz`(실행 파일 + install.sh + .desktop), Windows `zip`. 산출물 경로와 sha256 을 돌려준다.
pub fn archive(_target: Target, _app_exe: &Path, _app_name: &str, _version: &str, _out_dir: &Path) -> anyhow::Result<Artifact> {
    anyhow::bail!("번들 미구현")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LinuxX64,
    WindowsX64,
}

impl Target {
    pub fn label(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "Linux x86_64",
            Target::WindowsX64 => "Windows x86_64",
        }
    }
    pub fn triple(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "x86_64-unknown-linux-gnu",
            Target::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }
    pub fn runtime_file_name(&self) -> &'static str {
        match self {
            Target::LinuxX64 => "nl-runtime",
            Target::WindowsX64 => "nl-runtime.exe",
        }
    }
    pub fn host() -> Option<Target> {
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            Some(Target::LinuxX64)
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Target::WindowsX64)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}
