//! 뷰는 문서를 절대 직접 고치지 않는다: 읽기 전용 `ViewCtx` 를 받아 그리고,
//! 편집 의도는 `ViewAction` 으로 돌려주면 `app.rs` 가 `DocState` 를 통해 적용한다.
//! (undo 경로를 한 곳으로 모으기 위한 규칙 — trust-pms `views/mod.rs` 계승)

pub mod build;
pub mod data;
pub mod gui;
pub mod model;
pub mod pipeline;
pub mod resources;
pub mod train;

use crate::app::View;
use crate::canvas::Selection;
use eframe::egui::{self, Color32};
use nl_core::dataset::{DataSource, SyntheticKind};
use nl_core::{BuildTarget, DatasetId, ModelId, NodeId, Op, PNodeId, PipelineId, Project, RunId};
use nl_engine::{DeviceInfo, Value};
use nl_io::MonitorInfo;
use std::path::{Path, PathBuf};

// ── 공통 색 ─────────────────────────────────────────────────────────

pub const COL_OK: Color32 = Color32::from_rgb(0x57, 0xab, 0x5a);
pub const COL_WARN: Color32 = Color32::from_rgb(0xf0, 0xc6, 0x74);
pub const COL_ERROR: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
pub const COL_WEAK: Color32 = Color32::from_rgb(0x8b, 0x93, 0xa1);
pub const COL_SELECT: Color32 = Color32::from_rgb(0x4f, 0x9e, 0xea);
pub const COL_SURFACE: Color32 = Color32::from_rgb(0x1a, 0x1d, 0x23);

// ── 뷰 ↔ 앱 계약 ────────────────────────────────────────────────────

/// 뷰가 그리는 데 필요한 읽기 전용 상태.
pub struct ViewCtx<'a> {
    pub project: &'a Project,
    pub selection: Selection,
    /// `nl_engine::enumerate()` 결과 (프레임마다 다시 부르지 않도록 앱이 캐시한다).
    pub devices: &'a [DeviceInfo],
    /// 프로젝트 파일이 있는 폴더. 저장 전이면 `None`.
    pub base_dir: Option<&'a Path>,
    /// 진행 중인 학습.
    pub training: Option<&'a train::TrainSession>,
    /// `nl_io::monitors()` 결과 캐시 (화면 캡처 소스 편집용).
    pub monitors: &'a [MonitorInfo],
    /// 모니터 목록을 못 읽었을 때의 이유.
    pub monitors_error: Option<&'a str>,
    /// 진행 중인 녹화.
    pub recording: Option<&'a crate::record::RecordSession>,
    /// "지금 한 장 캡처" 결과.
    pub shot: &'a crate::record::ShotPreview,
    /// 시작할 때 빌더 업데이트를 확인하는 설정이 켜져 있는가.
    pub update_check: bool,
    /// 원본 파일 자동 저장 설정이 켜져 있는가.
    pub autosave_file: bool,
    /// 복구 스냅샷을 두는 폴더 (자원 뷰에 보여 준다).
    pub recovery_dir: &'a Path,
    /// 빌더 업데이트의 지금 상태. 확인을 시작한 적이 없으면 `None`.
    pub update_state: Option<&'a nl_update::State>,
    /// 앱 시계(초) — 같은 프레임 안에서 모두 같은 값을 쓴다.
    pub now: f64,
}

impl ViewCtx<'_> {
    pub fn saved(&self) -> bool {
        self.base_dir.is_some()
    }
    /// 지금 편집 중인 모델 (선택이 가리키는 것, 없으면 첫 모델).
    pub fn active_model(&self) -> Option<ModelId> {
        self.selection
            .model()
            .filter(|m| self.project.models.contains_key(m))
            .or_else(|| self.project.models.keys().next().copied())
    }

    /// 지금 편집 중인 파이프라인 (같은 규칙).
    pub fn active_pipeline(&self) -> Option<PipelineId> {
        self.selection
            .pipeline()
            .filter(|p| self.project.pipelines.contains_key(p))
            .or_else(|| self.project.pipelines.keys().next().copied())
    }
}

/// 뷰가 앱에 부탁하는 일. 문서 편집은 전부 `Ops` 로 모인다.
#[derive(Clone, Debug)]
pub enum ViewAction {
    Select(Selection),
    /// 구조 편집 — `DocState::apply_local` 로 undo 한 단위가 된다.
    Ops(Vec<Op>),
    /// 타이핑·드래그처럼 잦은 편집 — burst 경로로 모아 1초 무입력 시 undo 한 단위가 된다.
    Edit(Vec<Op>),
    Toast(String),
    /// 모델 뷰로 가서 이 노드를 비춘다.
    Focus(ModelId, NodeId),
    SetView(View),
    // 데이터
    /// 파일 대화상자를 띄워 CSV 데이터셋을 만든다.
    PickCsv,
    PickImageFolder,
    PickRecordedFolder,
    AddSynthetic(SyntheticKind),
    ScanDataset(DatasetId),
    PreviewDataset(DatasetId),
    // 학습
    StartTrain {
        model: ModelId,
        dataset: DatasetId,
    },
    PauseTrain,
    ResumeTrain,
    StopTrain,
    /// 이 실행의 체크포인트를 모델의 가중치로 삼는다.
    ApplyRunWeights(RunId),
    // 파이프라인
    StartPipeline(PipelineId),
    StopPipeline,
    /// 마우스·키보드 싱크 무장 스위치.
    SetArmInput(bool),
    /// `Source::Manual` 노드에 값 보내기.
    SendManual {
        node: PNodeId,
        value: Value,
    },
    // GUI
    SetGuiPreview(bool),
    // 빌드
    BuildStart,
    /// 도구 설치 계획을 만든다 (네트워크를 타므로 앱이 스레드에서 처리).
    ToolPlan(BuildTarget),
    /// Inno Setup 설치 계획 (네트워크 없이 바로 만든다).
    ToolPlanInno,
    RecheckTools,
    OpenPath(PathBuf),
    /// 만든 배포 아카이브를 풀어 실행한다.
    RunArtifact(PathBuf),
    /// 아이콘 PNG 고르기.
    PickIcon,
    /// HTTP 서버 노드의 TLS 인증서·키 PEM 을 고른다.
    PickTlsFile {
        pipeline: PipelineId,
        node: PNodeId,
        key: bool,
    },
    // 녹화
    /// 녹화 폼을 연다.
    StartRecordForm,
    /// 녹화 폴더를 직접 고른다.
    PickRecordDir,
    StartRecording {
        dir: PathBuf,
        name: String,
        region: nl_core::pipeline::Region,
        fps: f32,
        labels: Vec<String>,
    },
    StopRecording,
    /// 화면 한 장을 찍어 미리보기에 올린다.
    CaptureShot(nl_core::pipeline::Region),
    // 빌더 업데이트
    ShowUpdateWindow(bool),
    /// 시작할 때 업데이트를 확인할지 바꾼다.
    SetUpdateCheck(bool),
    /// 지금 한 번 확인한다.
    CheckUpdateNow,
    // 자동 저장·복구
    /// 파일로 연 문서를 주기적으로 원본에 저장할지.
    SetAutosaveFile(bool),
    /// 복구 폴더를 다시 훑는다.
    FindRecoveryFiles,
    /// CSV 행 수를 정확히 센다 (백그라운드).
    CountDatasetRows(DatasetId),
    /// 모델을 ONNX 파일로 내보낸다.
    ///
    /// `run` 이 있으면 그 실행의 체크포인트를, 없으면 모델에 붙은 가중치를 쓴다.
    ExportOnnx {
        model: ModelId,
        run: Option<RunId>,
    },
}

/// 뷰가 프레임 사이에 들고 있는 UI 상태 (문서가 아닌 것). 앱이 소유한다.
#[derive(Default)]
pub struct ViewState {
    pub data: data::DataState,
    pub train: train::TrainViewState,
    pub resources: resources::ResourceState,
    pub pipeline: pipeline::PipelineViewState,
    pub gui: gui::GuiViewState,
    pub build: build::BuildViewState,
}

// ── 표기 헬퍼 ───────────────────────────────────────────────────────

/// 샘플 형상을 "3×28×28" 로. 인스펙터의 텍스트 필드가 쓰는 표기다.
pub fn shape_text(shape: &[usize]) -> String {
    shape.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("×")
}

/// "3×28×28" · "3x28x28" · "3, 28, 28" 을 형상으로. 0 이나 빈 값은 거부한다.
pub fn parse_shape_text(text: &str) -> Option<Vec<usize>> {
    let parts: Vec<usize> = text
        .split(|c: char| ['×', 'x', 'X', ',', '*'].contains(&c) || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().parse::<usize>().ok())
        .collect::<Option<Vec<_>>>()?;
    if parts.is_empty() || parts.contains(&0) {
        None
    } else {
        Some(parts)
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 초 → "1분 23초" / "12.3초".
pub fn fmt_duration(seconds: f64) -> String {
    if !seconds.is_finite() || seconds < 0.0 {
        return "-".into();
    }
    let s = seconds as u64;
    if s >= 3600 {
        format!("{}시간 {}분", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}분 {}초", s / 60, s % 60)
    } else {
        format!("{seconds:.1}초")
    }
}

/// 손실·지표처럼 자릿수가 크게 흔들리는 값.
pub fn fmt_metric(v: f64) -> String {
    if !v.is_finite() {
        return "-".into();
    }
    let a = v.abs();
    if a != 0.0 && (a < 1e-3 || a >= 1e5) {
        format!("{v:.3e}")
    } else {
        format!("{v:.4}")
    }
}

pub fn device_label(d: &DeviceInfo) -> String {
    match d.vram_bytes {
        Some(v) => format!("{} ({}, {})", d.name, d.backend, fmt_bytes(v)),
        None => match d.cores {
            Some(c) => format!("{} ({c}코어)", d.name),
            None => format!("{} ({})", d.name, d.backend),
        },
    }
}

pub fn source_label(s: &DataSource) -> String {
    match s {
        DataSource::Csv { path, .. } => format!("CSV · {}", short_path(path)),
        DataSource::ImageFolder { path } => format!("이미지 폴더 · {}", short_path(path)),
        DataSource::Recorded { path } => format!("녹화 · {}", short_path(path)),
        DataSource::Synthetic { kind, samples } => format!("합성 {} · {samples}개", kind.label()),
    }
}

/// 소스 종류 이름만 (표 열·콤보).
pub fn source_kind_label(s: &DataSource) -> &'static str {
    match s {
        DataSource::Csv { .. } => "CSV",
        DataSource::ImageFolder { .. } => "이미지 폴더",
        DataSource::Recorded { .. } => "녹화",
        DataSource::Synthetic { .. } => "합성",
    }
}

/// 긴 경로는 마지막 두 조각만.
pub fn short_path(path: &str) -> String {
    let p = Path::new(path);
    let mut it = p.components().rev();
    let last = it.next().map(|c| c.as_os_str().to_string_lossy().to_string());
    let prev = it.next().map(|c| c.as_os_str().to_string_lossy().to_string());
    match (prev, last) {
        (Some(a), Some(b)) => format!("{a}/{b}"),
        (None, Some(b)) => b,
        _ => path.to_string(),
    }
}

/// 샘플 수 표기. 추정치면 "약" 을 붙인다.
///
/// 큰 CSV 는 스캔할 때 전부 읽지 않고 앞부분과 파일 크기로 어림한다. 그 값을 정확한 수처럼 보여 주면
/// 배치 수나 학습 시간을 잘못 가늠하게 된다.
pub fn sample_count(info: &nl_core::dataset::DatasetInfo) -> String {
    if info.samples_estimated {
        format!("약 {}개", info.samples)
    } else {
        format!("{}개", info.samples)
    }
}

/// 로그·토스트에 쓸 경로. 홈 아래면 `~` 로 줄인다 (보안 리뷰 L5).
///
/// 빌드 로그나 스크린샷을 남에게 보내는 일이 흔한데, 전체 경로에는 사용자 이름이 들어 있다.
/// `~/projects/a.nlproj` 면 어느 파일인지는 그대로 알아볼 수 있으면서 계정 이름은 드러나지 않는다.
/// 홈 밖 경로는 그대로 둔다 — `/etc` 나 `/mnt` 는 줄이면 오히려 어디인지 알 수 없다.
pub fn tilde(path: &Path) -> String {
    let text = path.display().to_string();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return text;
    };
    if home.as_os_str().is_empty() {
        return text;
    }
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => text,
    }
}

/// 표 한 줄의 라벨 + 값.
pub fn kv(ui: &mut egui::Ui, key: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(key).color(COL_WEAK).size(11.5));
        ui.label(value.into());
    });
}

#[cfg(test)]
mod tests {
    /// 추정치는 "약" 을 붙여 구분한다. 정확한 수처럼 보이면 배치 수나 학습 시간을 잘못 가늠한다.
    #[test]
    fn estimated_sample_counts_are_marked() {
        let mut info = nl_core::dataset::DatasetInfo {
            samples: 1200,
            ..Default::default()
        };
        assert_eq!(sample_count(&info), "1200개");
        info.samples_estimated = true;
        assert_eq!(sample_count(&info), "약 1200개");
    }

    /// 홈 아래 경로만 줄인다. 로그를 남에게 보낼 때 계정 이름이 드러나지 않게 하는 것이 목적이라,
    /// 홈 밖 경로까지 줄이면 어디인지 알 수 없어져 오히려 손해다.
    #[test]
    fn tilde_shortens_only_inside_home() {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) else {
            eprintln!("HOME 이 없어 건너뜀");
            return;
        };
        assert_eq!(tilde(&home.join("projects/a.nlproj")), "~/projects/a.nlproj");
        assert_eq!(tilde(&home), "~");
        // 홈 밖은 그대로.
        assert_eq!(tilde(Path::new("/etc/hosts")), "/etc/hosts");
        // 홈 이름이 앞부분만 겹치는 경로를 홈으로 착각하면 안 된다.
        let sibling = std::path::PathBuf::from(format!("{}-backup/x", home.display()));
        assert_eq!(tilde(&sibling), sibling.display().to_string());
    }

    use super::*;

    #[test]
    fn shape_text_round_trips() {
        assert_eq!(shape_text(&[3, 28, 28]), "3×28×28");
        assert_eq!(parse_shape_text("3×28×28"), Some(vec![3, 28, 28]));
        assert_eq!(parse_shape_text("3x28x28"), Some(vec![3, 28, 28]));
        assert_eq!(parse_shape_text(" 3 , 28 ,28 "), Some(vec![3, 28, 28]));
        assert_eq!(parse_shape_text("16"), Some(vec![16]));
    }

    #[test]
    fn shape_text_rejects_garbage_and_zero() {
        assert_eq!(parse_shape_text(""), None);
        assert_eq!(parse_shape_text("×××"), None);
        assert_eq!(parse_shape_text("3×0"), None);
        assert_eq!(parse_shape_text("3×a"), None);
        assert_eq!(parse_shape_text("-3"), None);
    }

    #[test]
    fn byte_and_duration_formats_stay_readable() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(8 * 1024 * 1024 * 1024), "8.0 GB");
        assert_eq!(fmt_duration(12.34), "12.3초");
        assert_eq!(fmt_duration(83.0), "1분 23초");
        assert_eq!(fmt_duration(3700.0), "1시간 1분");
        assert_eq!(fmt_duration(f64::NAN), "-");
    }

    #[test]
    fn metric_format_switches_to_exponent_for_extremes() {
        assert_eq!(fmt_metric(0.5), "0.5000");
        assert!(fmt_metric(1e-7).contains('e'));
        assert!(fmt_metric(1e9).contains('e'));
        assert_eq!(fmt_metric(f64::INFINITY), "-");
    }

    #[test]
    fn short_path_keeps_the_last_two_parts() {
        assert_eq!(short_path("/a/b/c/data.csv"), "c/data.csv");
        assert_eq!(short_path("data.csv"), "data.csv");
    }
}
