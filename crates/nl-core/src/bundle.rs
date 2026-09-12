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
    /// 배포 앱의 업데이트 매니페스트(`latest.json`) 주소. 없으면 자동 업데이트를 끈다.
    /// 실행할 때 `NL_UPDATE_URL` 환경 변수가 우선한다.
    #[serde(default)]
    pub update_url: Option<String>,
    /// 매니페스트 서명 검증에 쓸 minisign 공개키. 없으면 검증을 건너뛴다(경고 로그).
    #[serde(default)]
    pub update_public_key: Option<String>,
    /// 새 버전을 알아서 내려받을지. 적용은 언제나 사용자 확인을 거친다.
    #[serde(default)]
    pub auto_update: bool,
    /// 배포 앱이 마우스·키보드 싱크로 **실제 입력을 보내도 되는지**. 기본은 꺼짐.
    ///
    /// 꺼져 있으면 `Sink::MouseKeyboard` 는 로그만 남긴다. 받은 사람이 모르는 사이 커서가 움직이는 일이
    /// 없도록 빌더에서 명시적으로 켜야 한다 (`nl build --arm-input`, 빌드 뷰 체크박스).
    #[serde(default)]
    pub arm_input: bool,
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
            update_url: None,
            update_public_key: None,
            auto_update: false,
            arm_input: false,
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
    /// 앱 아이콘 PNG (프로젝트 폴더 기준 상대 경로 또는 절대 경로).
    /// Linux 아카이브에는 정사각 PNG 로, Windows 설치 프로그램에는 `.ico` 로 들어간다.
    #[serde(default)]
    pub icon: Option<String>,
    /// 산출물을 올릴 기본 주소. `latest.json` 의 자산 URL 은 여기에 파일 이름을 붙여 만든다.
    #[serde(default)]
    pub update_base_url: Option<String>,
    /// 배포 앱이 읽을 업데이트 매니페스트 주소. 없으면 배포 앱의 자동 업데이트가 꺼진다.
    /// 보통 `update_base_url` + `/latest.json` 이지만 따로 둘 수 있다.
    #[serde(default)]
    pub update_url: Option<String>,
    /// 매니페스트 서명을 검증할 minisign 공개키. 없으면 배포 앱이 검증을 건너뛴다.
    #[serde(default)]
    pub update_public_key: Option<String>,
    /// 배포 앱이 새 버전을 알아서 내려받을지. 적용은 언제나 사용자 확인을 거친다.
    #[serde(default)]
    pub auto_update: bool,
    /// 배포 앱이 마우스·키보드 싱크로 실제 입력을 보내도 되는지. 기본은 꺼짐.
    /// 그대로 `BundleManifest::arm_input` 으로 들어간다.
    #[serde(default)]
    pub arm_input: bool,
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
            icon: None,
            update_base_url: None,
            update_url: None,
            update_public_key: None,
            auto_update: false,
            arm_input: false,
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
    fn arm_input_defaults_to_off_and_round_trips() {
        let m = BundleManifest::new("내 앱", "1.2.3");
        assert!(!m.arm_input, "기본은 입력 금지 — 받은 사람이 모르는 사이 커서가 움직이면 안 된다");

        // 켜서 왕복.
        let mut armed = m.clone();
        armed.arm_input = true;
        let back: BundleManifest = serde_json::from_str(&serde_json::to_string(&armed).unwrap()).unwrap();
        assert_eq!(back, armed);
        assert!(back.arm_input);

        // 필드가 없는 옛 매니페스트는 꺼진 것으로 읽힌다.
        let old = r#"{"format":1,"app_name":"옛 앱","app_version":"0.1.0"}"#;
        let parsed: BundleManifest = serde_json::from_str(old).unwrap();
        assert!(!parsed.arm_input, "옛 번들은 무장 없이 읽혀야 한다");
    }

    #[test]
    fn build_spec_arm_input_defaults_to_off_and_round_trips() {
        let spec = BuildSpec::default();
        assert!(!spec.arm_input);
        assert!(!BuildSpec::from_project(&crate::model::Project::new("p")).arm_input);

        let mut armed = spec.clone();
        armed.arm_input = true;
        let back: BuildSpec = serde_json::from_str(&serde_json::to_string(&armed).unwrap()).unwrap();
        assert_eq!(back, armed);
        assert!(back.arm_input);

        // 옛 프로젝트 파일에 필드가 없어도 읽힌다.
        let old = r#"{"app_name":"앱","app_version":"0.1.0"}"#;
        let parsed: BuildSpec = serde_json::from_str(old).unwrap();
        assert!(!parsed.arm_input);
    }

    #[test]
    fn update_fields_round_trip() {
        let mut m = BundleManifest::new("내 앱", "1.2.3");
        assert_eq!(m.update_url, None, "기본은 자동 업데이트 없음");
        assert_eq!(m.update_public_key, None);
        assert!(!m.auto_update, "자동 다운로드는 명시해야 켜진다");

        m.update_url = Some("https://updates.example/app/latest.json".into());
        m.update_public_key = Some("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".into());
        m.auto_update = true;

        let json = serde_json::to_string(&m).unwrap();
        let back: BundleManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    /// 업데이트 필드가 없던 옛 번들도 그대로 열려야 한다.
    #[test]
    fn older_manifests_without_update_fields_still_load() {
        let json = r#"{"format":1,"app_name":"옛 앱","app_version":"0.1.0"}"#;
        let m: BundleManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.app_name, "옛 앱");
        assert_eq!(m.update_url, None);
        assert_eq!(m.update_public_key, None);
        assert!(!m.auto_update);
        assert!(!m.autostart, "serde(default) 라 false 다");
    }

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
