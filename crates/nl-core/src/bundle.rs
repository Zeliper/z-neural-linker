//! `.nlapp` 번들 매니페스트와 실행 파일 꼬리표 규약.
//!
//! 번들 = zip { manifest.json, project.json, weights/<model_id>.safetensors, assets/… }.
//! 배포 바이너리 = 런타임 실행 파일 ‖ 번들 바이트 ‖ u64 LE 번들 길이 ‖ `NLAPP1`.
//! 런타임은 자기 실행 파일의 마지막 14 바이트를 읽어 매직을 확인하고 길이만큼 앞을 번들로 읽는다.

use crate::ids::{ModelId, PipelineId};
use crate::train::DevicePref;
use serde::{Deserialize, Serialize};

pub const BUNDLE_TRAILER_MAGIC: &[u8; 6] = b"NLAPP1";
pub const BUNDLE_EXT: &str = "nlapp";
pub const MANIFEST_NAME: &str = "manifest.json";
pub const PROJECT_NAME: &str = "project.json";
pub const WEIGHTS_DIR: &str = "weights";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundleManifest {
    /// 매니페스트 형식 버전.
    pub format: u32,
    pub app_name: String,
    pub app_version: String,
    /// 빌더 버전 (진단용).
    #[serde(default)]
    pub built_with: String,
    /// 시작 시 자동 실행할 파이프라인.
    #[serde(default)]
    pub entry_pipeline: Option<PipelineId>,
    /// 포함된 모델과 가중치 파일 이름 (`weights/` 아래).
    #[serde(default)]
    pub models: Vec<BundledModel>,
    #[serde(default)]
    pub default_device: DevicePref,
    /// 파이프라인을 자동으로 시작할지 (false 면 GUI 의 시작 버튼/액션으로).
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BundledModel {
    pub model: ModelId,
    pub weights_file: String,
}

impl BundleManifest {
    pub const FORMAT: u32 = 1;
    pub fn new(app_name: impl Into<String>, app_version: impl Into<String>) -> Self {
        Self {
            format: Self::FORMAT,
            app_name: app_name.into(),
            app_version: app_version.into(),
            built_with: String::new(),
            entry_pipeline: None,
            models: vec![],
            default_device: DevicePref::Auto,
            autostart: true,
        }
    }
}

// ───────────────────────────── 빌드 설정 ─────────────────────────────

/// 배포 산출물을 만들 대상 플랫폼. `nl_bundle::Target` 과 1:1 이지만, nl-core 는 nl-bundle 에
/// 의존하지 않으므로 여기에 직렬화 가능한 형태로 따로 둔다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BuildTarget {
    LinuxX64,
    WindowsX64,
}

impl BuildTarget {
    pub const ALL: [BuildTarget; 2] = [BuildTarget::LinuxX64, BuildTarget::WindowsX64];

    pub fn label(&self) -> &'static str {
        match self {
            BuildTarget::LinuxX64 => "Linux x86_64",
            BuildTarget::WindowsX64 => "Windows x86_64",
        }
    }

    pub fn triple(&self) -> &'static str {
        match self {
            BuildTarget::LinuxX64 => "x86_64-unknown-linux-gnu",
            BuildTarget::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }
}

/// 빌드 설정. 프로젝트 문서에 남아 다시 열어도 같은 산출물을 만든다.
/// 편집은 `Op::SetSettings` 로 들어가므로 되돌리기도 다른 편집과 똑같이 동작한다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// 배포 앱 이름. 파일 이름에는 슬러그로 접혀 들어간다.
    pub app_name: String,
    pub app_version: String,
    #[serde(default)]
    pub targets: Vec<BuildTarget>,
    /// 시작 시 자동 실행할 파이프라인.
    #[serde(default)]
    pub entry_pipeline: Option<PipelineId>,
    #[serde(default = "yes")]
    pub autostart: bool,
    #[serde(default)]
    pub default_device: DevicePref,
    /// 번들에 넣을 모델. 가중치가 있는 모델만 뜻이 있다.
    #[serde(default)]
    pub models: Vec<ModelId>,
    /// 산출물 폴더 (프로젝트 폴더 기준 상대 경로 또는 절대 경로). 없으면 `dist`.
    #[serde(default)]
    pub output_dir: Option<String>,
}

fn yes() -> bool {
    true
}

/// 산출물 폴더 기본 이름.
pub const DEFAULT_OUTPUT_DIR: &str = "dist";

impl Default for BuildSpec {
    fn default() -> Self {
        Self {
            app_name: "Neural Linker App".into(),
            app_version: "0.1.0".into(),
            targets: vec![],
            entry_pipeline: None,
            autostart: true,
            default_device: DevicePref::Auto,
            models: vec![],
            output_dir: None,
        }
    }
}

impl BuildSpec {
    /// 프로젝트에서 기본값을 뽑는다: 앱 이름 = 프로젝트 이름, 진입 파이프라인·모델 = 첫 번째.
    pub fn from_project(p: &crate::model::Project) -> Self {
        Self {
            app_name: p.name.clone(),
            entry_pipeline: p.pipelines.keys().next().copied(),
            default_device: p.settings.default_device,
            models: p.models.values().filter(|m| m.weights.is_some()).map(|m| m.id).collect(),
            targets: vec![],
            ..Self::default()
        }
    }
}

/// 꼬리표를 만든다: 번들 뒤에 붙일 바이트.
pub fn trailer(bundle_len: u64) -> [u8; 14] {
    let mut t = [0u8; 14];
    t[..8].copy_from_slice(&bundle_len.to_le_bytes());
    t[8..].copy_from_slice(BUNDLE_TRAILER_MAGIC);
    t
}

/// 실행 파일 바이트에서 첨부된 번들 범위를 찾는다. 없으면 `None`.
pub fn find_attached(exe: &[u8]) -> Option<std::ops::Range<usize>> {
    if exe.len() < 14 || &exe[exe.len() - 6..] != BUNDLE_TRAILER_MAGIC {
        return None;
    }
    let len_bytes: [u8; 8] = exe[exe.len() - 14..exe.len() - 6].try_into().ok()?;
    let len = u64::from_le_bytes(len_bytes) as usize;
    let end = exe.len() - 14;
    if len > end {
        return None;
    }
    Some(end - len..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailer_round_trips() {
        let mut exe = b"ELF...binary...".to_vec();
        let bundle = b"PK zip bytes";
        exe.extend_from_slice(bundle);
        exe.extend_from_slice(&trailer(bundle.len() as u64));
        let r = find_attached(&exe).unwrap();
        assert_eq!(&exe[r], bundle);
        assert!(find_attached(b"plain").is_none());
    }
}
