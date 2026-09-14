//! 화면 녹화 세션과 한 장 캡처. 실제 캡처·저장은 `nl_io::record::Recorder` 가 한다.
//!
//! 만들어진 폴더는 그대로 `DataSource::Recorded` 형식이라, 정지하면 데이터셋으로 바로 등록할 수 있다.
//! 캡처 백엔드는 **첫 캡처가 성공한 뒤에야** 알 수 있으므로(`Capturer::backend`), 미리보기 한 장을 찍거나
//! 녹화가 한 프레임을 남긴 뒤에 힌트를 보여 준다.

use eframe::egui;
use nl_core::pipeline::Region;
use nl_io::record::{Recorder, RecorderHandle};
use nl_io::{Backend, Capturer, Frame};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};

/// 미리보기를 새로 그리는 간격(초). `last_frame()` 은 프레임 한 장을 통째로 복사하므로 자주 부르지 않는다.
pub const PREVIEW_INTERVAL: f64 = 0.5;
/// 라벨 키 개수 (숫자키 0~9).
pub const LABEL_KEYS: usize = 10;

/// 한 장 캡처 결과.
pub struct Shot {
    pub frame: Frame,
    pub backend: Option<Backend>,
}

/// 화면 한 장을 백그라운드에서 찍는다. 포털 경로는 권한 대화상자 때문에 수 초가 걸릴 수 있어
/// UI 스레드에서 부르면 안 된다.
pub fn spawn_shot(region: Region) -> Receiver<Result<Shot, String>> {
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new().name("nl-shot".into()).spawn(move || {
        let _ = tx.send(grab_once(region));
    });
    if spawned.is_err() {
        let (tx2, rx2) = channel();
        let _ = tx2.send(Err("캡처 스레드를 만들지 못했습니다".to_string()));
        return rx2;
    }
    rx
}

fn grab_once(region: Region) -> Result<Shot, String> {
    let mut cap = Capturer::new().map_err(|e| format!("{e:#}"))?;
    let frame = cap.capture(&region).map_err(|e| format!("{e:#}"))?;
    Ok(Shot {
        frame,
        backend: cap.backend(),
    })
}

/// "지금 한 장 캡처" 결과. 캡처 소스 인스펙터와 녹화 폼이 함께 쓴다.
#[derive(Default)]
pub struct ShotPreview {
    pub texture: Option<egui::TextureHandle>,
    pub backend: Option<Backend>,
    pub size: (u32, u32),
    pub error: Option<String>,
    /// 아직 찍는 중인가.
    pub busy: bool,
}

impl ShotPreview {
    /// 찍은 프레임을 텍스처로 올린다.
    pub fn set(&mut self, ctx: &egui::Context, shot: Shot) {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [shot.frame.width as usize, shot.frame.height as usize],
            &shot.frame.rgba,
        );
        self.size = (shot.frame.width, shot.frame.height);
        self.backend = shot.backend;
        self.error = None;
        self.busy = false;
        match &mut self.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("nl-shot", image, egui::TextureOptions::LINEAR)),
        }
    }

    pub fn fail(&mut self, message: String) {
        self.error = Some(message);
        self.busy = false;
    }
}

/// 진행 중인 녹화.
pub struct RecordSession {
    pub handle: RecorderHandle,
    pub dir: PathBuf,
    /// 만들 데이터셋 이름.
    pub name: String,
    pub region: Region,
    pub fps: f32,
    /// 숫자키 → 라벨 이름. 인덱스가 곧 라벨 값이다.
    pub labels: Vec<String>,
    pub started_at: f64,
    /// 마지막으로 미리보기를 갱신한 앱 시계.
    pub last_preview: f64,
    pub texture: Option<egui::TextureHandle>,
    /// 첫 프레임 뒤에 알게 되는 캡처 백엔드.
    pub backend: Option<Backend>,
}

impl RecordSession {
    /// 녹화를 시작한다. 폴더가 이미 있으면 그대로 쓰되 프레임 번호가 겹치지 않도록 `Recorder` 가 비우고 시작한다.
    pub fn start(
        dir: PathBuf,
        name: String,
        region: Region,
        fps: f32,
        labels: Vec<String>,
        now: f64,
    ) -> Result<Self, String> {
        let handle = Recorder::start(&dir, region, fps).map_err(|e| format!("{e:#}"))?;
        Ok(Self {
            handle,
            dir,
            name,
            region,
            fps,
            labels,
            started_at: now,
            last_preview: f64::NEG_INFINITY,
            texture: None,
            backend: None,
        })
    }

    pub fn is_running(&self) -> bool {
        !self.handle.is_done()
    }

    pub fn stop(&self) {
        self.handle.stop();
    }

    /// 미리보기를 주기에 맞춰 갱신하고, 백엔드를 아직 모르면 `meta.json` 에서 읽는다.
    pub fn tick(&mut self, ctx: &egui::Context, now: f64) {
        if now - self.last_preview < PREVIEW_INTERVAL {
            return;
        }
        self.last_preview = now;
        if let Some(frame) = self.handle.last_frame() {
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
            let name = format!("record-preview-{}", self.started_at as i64);
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture(name, image, egui::TextureOptions::LINEAR)),
            }
        }
        if self.backend.is_none() {
            self.backend = read_backend(&self.dir);
        }
    }

    /// 지금 라벨.
    pub fn label(&self) -> i64 {
        self.handle.label()
    }

    /// 숫자키로 라벨을 바꾼다. 범위 밖이면 아무 일도 하지 않는다.
    pub fn set_label(&self, value: i64) {
        if (0..self.labels.len() as i64).contains(&value) {
            self.handle.set_label(value);
        }
    }

    /// 라벨 값의 이름 (비어 있으면 번호).
    pub fn label_name(&self, value: i64) -> String {
        match self.labels.get(value.max(0) as usize) {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => format!("{value}"),
        }
    }

    pub fn frames(&self) -> usize {
        self.handle.frames_written()
    }

    pub fn dropped(&self) -> usize {
        self.handle.frames_dropped()
    }

    pub fn error(&self) -> Option<String> {
        self.handle.error()
    }
}

/// `meta.json` 에 적힌 백엔드 이름을 `Backend` 로 되돌린다.
fn read_backend(dir: &Path) -> Option<Backend> {
    let text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let label = v.get("backend")?.as_str()?;
    backend_from_label(label)
}

/// `Backend::label()` 의 역방향.
pub fn backend_from_label(label: &str) -> Option<Backend> {
    [Backend::Wayland, Backend::Portal, Backend::X11, Backend::XCap]
        .into_iter()
        .find(|b| b.label() == label)
}

/// 녹화 폴더 기본 자리: `<프로젝트 폴더>/recordings/<이름>`.
pub fn default_dir(base: &Path, name: &str) -> PathBuf {
    base.join("recordings").join(sanitize_dir(name))
}

/// 폴더 이름으로 쓸 수 없는 글자를 바꾼다.
pub fn sanitize_dir(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let t = cleaned.trim();
    if t.is_empty() {
        "recording".into()
    } else {
        t.to_string()
    }
}

/// 기본 라벨 이름 (0~9). 사용자가 비워 두면 번호가 그대로 라벨이다.
pub fn default_labels() -> Vec<String> {
    (0..LABEL_KEYS).map(|_| String::new()).collect()
}

/// 실제로 쓰이는 라벨 개수 = 이름이 채워진 마지막 칸 + 1. 하나도 없으면 2(0/1)로 본다.
pub fn used_labels(labels: &[String]) -> usize {
    let last = labels.iter().rposition(|l| !l.trim().is_empty());
    match last {
        Some(i) => i + 1,
        None => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_labels_round_trip() {
        for b in [Backend::Wayland, Backend::Portal, Backend::X11, Backend::XCap] {
            assert_eq!(backend_from_label(b.label()), Some(b));
        }
        assert_eq!(backend_from_label("없는 백엔드"), None);
    }

    #[test]
    fn default_dir_is_under_recordings() {
        let d = default_dir(Path::new("/proj"), "책상 화면");
        assert_eq!(d, PathBuf::from("/proj/recordings/책상 화면"));
        // 경로 구분자는 접힌다 — 폴더가 엉뚱한 곳에 생기면 안 된다.
        assert_eq!(
            default_dir(Path::new("/proj"), "a/b"),
            PathBuf::from("/proj/recordings/a_b")
        );
        assert_eq!(
            default_dir(Path::new("/proj"), "  "),
            PathBuf::from("/proj/recordings/recording")
        );
    }

    #[test]
    fn used_labels_counts_up_to_the_last_named_one() {
        let mut l = default_labels();
        assert_eq!(used_labels(&l), 2, "이름이 없으면 0/1 두 가지로 본다");
        l[0] = "왼쪽".into();
        l[2] = "오른쪽".into();
        assert_eq!(used_labels(&l), 3, "가운데가 비어도 마지막 이름까지 센다");
        assert_eq!(l.len(), LABEL_KEYS);
    }

    #[test]
    fn meta_backend_is_read_back_from_the_folder() {
        let dir = std::env::temp_dir().join(format!("nl-record-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            r#"{"backend":"portal(xdg-desktop-portal)","fps":2.0}"#,
        )
        .unwrap();
        assert_eq!(read_backend(&dir), Some(Backend::Portal));
        // 파일이 없거나 필드가 없으면 조용히 None.
        std::fs::write(dir.join("meta.json"), "{}").unwrap();
        assert_eq!(read_backend(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
