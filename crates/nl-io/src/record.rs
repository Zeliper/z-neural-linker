//! 화면 녹화 → 학습용 데이터셋 폴더.
//!
//! 빌더의 "녹화" 기능이 쓴다. 화면을 정해진 fps 로 찍어 PNG 로 쌓고, 그때그때의 라벨을 함께 적는다.
//! 만들어진 폴더는 `nl_core::DataSource::Recorded` 가 그대로 읽는 형식이다.
//!
//! ```text
//! <dir>/
//!   frames/000001.png  000002.png  …   ← RGBA 캡처를 PNG 로
//!   labels.jsonl                       ← 프레임마다 한 줄
//!   meta.json                          ← 영역·fps·시작 시각·백엔드
//! ```
//!
//! `labels.jsonl` 한 줄은 `{"frame":"000001.png","label":3,"t_ms":1234}` 이다.
//! `nl-engine` 의 적재기는 `frame` 과 `label` 만 보고 `t_ms` 는 무시하므로, 나중에 필드를 더해도 안전하다.
//!
//! ## 스레드 구성
//! 캡처 스레드와 저장 스레드가 따로 돈다. PNG 인코딩과 디스크 쓰기가 캡처 주기를 밀지 않게 하려는 것이다.
//! 둘 사이 큐는 [`WRITE_QUEUE`] 칸짜리이고, 디스크가 못 따라가 큐가 차면 **그 프레임을 버린다**.
//! 버린 수는 [`RecorderHandle::frames_dropped`] 로 볼 수 있다. 캡처를 막아 주기가 흔들리는 것보다,
//! 프레임이 몇 장 비는 편이 학습 데이터로서 낫다 — 남은 프레임은 저마다 올바른 라벨을 갖는다.
//!
//! 파일 이름은 **저장된 순서**로 매긴다. 그래서 중간에 버려진 프레임이 있어도 번호는 끊기지 않는다.

use crate::screen::{Capturer, Frame};
use anyhow::{bail, Context};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use nl_core::pipeline::Region;
use parking_lot::Mutex;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 캡처 → 저장 큐 길이. 짧게 두어 밀린 프레임이 메모리를 오래 물고 있지 않게 한다.
pub const WRITE_QUEUE: usize = 32;
/// 이만큼 연속으로 캡처에 실패하면 녹화를 접는다 (화면이 잠겼거나 권한이 끊긴 경우).
pub const MAX_CONSECUTIVE_FAILURES: u32 = 30;
/// `stop()` 응답 지연의 상한.
const SLEEP_SLICE: Duration = Duration::from_millis(10);
const MIN_FPS: f32 = 0.01;
const MAX_FPS: f32 = 240.0;

/// 프레임을 어디서 가져올지. 실제 화면 대신 가짜를 끼울 수 있게 트레이트로 뺐다
/// (그래서 화면이 없는 환경에서도 저장 경로와 라벨 파일 형식을 시험할 수 있다).
pub trait FrameSource: Send {
    /// 진단·`meta.json` 에 적을 백엔드 이름. 모르면 `None`.
    fn backend_label(&self) -> Option<String>;
    /// 한 장 찍는다.
    fn grab(&mut self, region: &Region) -> anyhow::Result<Frame>;
}

/// 실제 화면 캡처. 연결을 유지하는 [`Capturer`] 를 그대로 쓴다.
struct ScreenSource {
    cap: Capturer,
}

impl FrameSource for ScreenSource {
    fn backend_label(&self) -> Option<String> {
        self.cap.backend().map(|b| b.label().to_owned())
    }
    fn grab(&mut self, region: &Region) -> anyhow::Result<Frame> {
        self.cap.capture(region)
    }
}

/// 녹화를 시작하는 네임스페이스.
pub struct Recorder;

impl Recorder {
    /// 화면을 찍어 `dir` 에 쌓는다. 즉시 돌아온다.
    ///
    /// `region.width == 0` 이면 모니터 전체다. 캡처 백엔드는 [`Capturer`] 가 알아서 고른다.
    pub fn start(dir: impl Into<PathBuf>, region: Region, fps: f32) -> anyhow::Result<RecorderHandle> {
        let cap = Capturer::new().context("녹화를 시작할 화면 캡처 백엔드를 열지 못했다")?;
        Self::start_with(dir, region, fps, Box::new(ScreenSource { cap }))
    }

    /// 프레임 공급원을 직접 주고 시작한다 (시험용, 또는 화면이 아닌 소스).
    pub fn start_with(
        dir: impl Into<PathBuf>,
        region: Region,
        fps: f32,
        source: Box<dyn FrameSource>,
    ) -> anyhow::Result<RecorderHandle> {
        let dir: PathBuf = dir.into();
        if fps <= 0.0 || !fps.is_finite() {
            bail!("fps 는 0 보다 큰 수여야 한다 (받은 값 {fps})");
        }
        let fps = fps.clamp(MIN_FPS, MAX_FPS);
        let frames_dir = dir.join("frames");
        std::fs::create_dir_all(&frames_dir)
            .with_context(|| format!("{} 폴더를 만들지 못했다", frames_dir.display()))?;

        let labels_path = dir.join("labels.jsonl");
        // 이어 쓰지 않는다 — 같은 폴더에 두 번 녹화하면 프레임 번호가 겹쳐 데이터가 섞인다.
        let labels = std::fs::File::create(&labels_path)
            .with_context(|| format!("{} 를 만들지 못했다", labels_path.display()))?;

        let started = chrono::Utc::now();
        write_meta(&dir, region, fps, started, None)?;

        let shared = Shared {
            stop: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicBool::new(false)),
            label: Arc::new(AtomicI64::new(0)),
            written: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
            last: Arc::new(Mutex::new(None)),
            error: Arc::new(Mutex::new(None)),
        };

        let (tx, rx) = crossbeam_channel::bounded::<Shot>(WRITE_QUEUE);
        let writer = {
            let s = shared.clone();
            std::thread::Builder::new()
                .name("nl-record-write".into())
                .spawn(move || write_loop(&frames_dir, labels, &rx, &s))
                .context("녹화 저장 스레드를 만들지 못했다")?
        };
        let capture = {
            let (s, dir2) = (shared.clone(), dir.clone());
            std::thread::Builder::new()
                .name("nl-record-grab".into())
                .spawn(move || {
                    capture_loop(source, region, fps, &tx, &s, &dir2, started);
                })
                .context("녹화 캡처 스레드를 만들지 못했다")?
        };

        Ok(RecorderHandle {
            dir,
            shared,
            threads: Some((capture, writer)),
        })
    }
}

/// 캡처 스레드 → 저장 스레드로 넘기는 한 장.
struct Shot {
    frame: Frame,
    label: i64,
    t_ms: u64,
}

/// 두 스레드와 호출자가 함께 보는 상태.
#[derive(Clone)]
struct Shared {
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    label: Arc<AtomicI64>,
    written: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    last: Arc<Mutex<Option<Frame>>>,
    error: Arc<Mutex<Option<String>>>,
}

impl Shared {
    fn set_error(&self, msg: String) {
        log::warn!("녹화: {msg}");
        *self.error.lock() = Some(msg);
    }
}

pub struct RecorderHandle {
    dir: PathBuf,
    shared: Shared,
    threads: Option<(std::thread::JoinHandle<()>, std::thread::JoinHandle<()>)>,
}

impl RecorderHandle {
    /// 녹화를 멈춘다. 즉시 돌아오며, 이미 큐에 있는 프레임은 저장 스레드가 마저 쓴다.
    /// 파일이 모두 자리 잡을 때까지 기다리려면 [`RecorderHandle::finish`] 를 쓴다.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }

    /// 멈추고 두 스레드가 끝날 때까지 기다린다. 돌아오면 모든 파일이 디스크에 있다.
    pub fn finish(mut self) -> usize {
        self.stop();
        if let Some((c, w)) = self.threads.take() {
            let _ = c.join();
            let _ = w.join();
        }
        self.frames_written()
    }

    /// 두 스레드가 모두 끝났는가.
    pub fn is_done(&self) -> bool {
        self.shared.done.load(Ordering::SeqCst)
    }

    /// 끝날 때까지 기다린다. `timeout` 안에 안 끝나면 `false`.
    pub fn wait_done(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if self.is_done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        self.is_done()
    }

    /// 지금부터 찍히는 프레임에 붙을 라벨. 빌더가 키 입력으로 바꾼다. 기본 0.
    pub fn set_label(&self, label: i64) {
        self.shared.label.store(label, Ordering::SeqCst);
    }

    /// 현재 라벨.
    pub fn label(&self) -> i64 {
        self.shared.label.load(Ordering::SeqCst)
    }

    /// **디스크에 다 쓰인** 프레임 수. PNG 와 라벨 줄이 모두 끝난 것만 센다.
    pub fn frames_written(&self) -> usize {
        self.shared.written.load(Ordering::SeqCst)
    }

    /// 저장이 못 따라가 버린 프레임 수.
    pub fn frames_dropped(&self) -> usize {
        self.shared.dropped.load(Ordering::SeqCst)
    }

    /// 미리보기용 마지막 캡처. 프레임 하나를 통째로 복제하므로 자주 부르면 그만큼 비싸다.
    pub fn last_frame(&self) -> Option<Frame> {
        self.shared.last.lock().clone()
    }

    /// 마지막으로 난 오류. 녹화는 실패해도 계속 시도하므로, 값이 있다고 멈춘 것은 아니다.
    /// 연속 실패가 [`MAX_CONSECUTIVE_FAILURES`] 를 넘으면 스스로 멈춘다 ([`RecorderHandle::is_done`] 으로 확인).
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().clone()
    }

    /// 녹화 폴더.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for RecorderHandle {
    fn drop(&mut self) {
        // 핸들을 놓으면 녹화도 멈춘다 (스레드가 남아 계속 디스크를 쓰지 않게).
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

// ───────────────────────────── 캡처 스레드 ─────────────────────────────

fn capture_loop(
    mut source: Box<dyn FrameSource>,
    region: Region,
    fps: f32,
    tx: &Sender<Shot>,
    shared: &Shared,
    dir: &Path,
    started: chrono::DateTime<chrono::Utc>,
) {
    let interval = Duration::from_secs_f32(1.0 / fps);
    let t0 = Instant::now();
    let mut next = t0;
    let mut failures = 0u32;
    let mut backend_written = false;

    while !shared.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now < next {
            sleep_until(next, &shared.stop);
            continue;
        }
        // 밀린 주기는 건너뛴다 (지연이 누적되지 않게).
        next += interval;
        while next <= now {
            next += interval;
        }

        match source.grab(&region) {
            Ok(frame) => {
                failures = 0;
                *shared.last.lock() = Some(frame.clone());
                // 백엔드는 첫 캡처가 성공해야 정해진다. 그때 meta.json 을 한 번 다시 쓴다.
                if !backend_written {
                    backend_written = true;
                    if let Err(e) = write_meta(dir, region, fps, started, source.backend_label().as_deref()) {
                        shared.set_error(format!("meta.json 갱신 실패: {e:#}"));
                    }
                }
                let shot = Shot {
                    frame,
                    label: shared.label.load(Ordering::SeqCst),
                    t_ms: t0.elapsed().as_millis() as u64,
                };
                match tx.try_send(shot) {
                    Ok(()) => {}
                    // 저장이 밀렸다. 캡처를 막지 않고 이 장을 버린다.
                    Err(TrySendError::Full(_)) => {
                        shared.dropped.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(e) => {
                failures += 1;
                shared.set_error(format!("캡처 실패({failures}회 연속): {e:#}"));
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    shared.set_error(format!(
                        "캡처가 {MAX_CONSECUTIVE_FAILURES}회 연속 실패해 녹화를 멈춘다. 마지막 오류: {e:#}"
                    ));
                    break;
                }
            }
        }
    }
    // 저장 스레드가 큐를 비우고 끝나도록 보낸 쪽을 닫는다.
    shared.stop.store(true, Ordering::SeqCst);
}

fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(SLEEP_SLICE));
    }
}

// ───────────────────────────── 저장 스레드 ─────────────────────────────

fn write_loop(frames_dir: &Path, mut labels: std::fs::File, rx: &Receiver<Shot>, shared: &Shared) {
    // 보낸 쪽이 닫히면 남은 큐를 다 비운 뒤 반복이 끝난다.
    for shot in rx.iter() {
        let index = shared.written.load(Ordering::SeqCst) + 1;
        let name = format!("{index:06}.png");
        if let Err(e) = save_png(&frames_dir.join(&name), &shot.frame) {
            shared.set_error(format!("{name} 저장 실패: {e:#}"));
            continue;
        }
        // 라벨 줄은 PNG 가 자리 잡은 뒤에 쓴다. 그래야 줄이 있으면 파일도 반드시 있다.
        let line = serde_json::json!({ "frame": name, "label": shot.label, "t_ms": shot.t_ms });
        if let Err(e) = writeln!(labels, "{line}").and_then(|()| labels.flush()) {
            shared.set_error(format!("labels.jsonl 쓰기 실패: {e}"));
            // PNG 만 남으면 적재기가 무시하므로 데이터가 어긋나지는 않는다.
            let _ = std::fs::remove_file(frames_dir.join(&name));
            continue;
        }
        shared.written.fetch_add(1, Ordering::SeqCst);
    }
    shared.done.store(true, Ordering::SeqCst);
}

fn save_png(path: &Path, frame: &Frame) -> anyhow::Result<()> {
    let expect = frame.width as usize * frame.height as usize * 4;
    if frame.rgba.len() != expect {
        bail!(
            "프레임 크기가 맞지 않는다: {}x{} 인데 {} 바이트",
            frame.width,
            frame.height,
            frame.rgba.len()
        );
    }
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| anyhow::anyhow!("RGBA 버퍼를 이미지로 만들지 못했다"))?;
    img.save(path).with_context(|| format!("{} 저장 실패", path.display()))
}

fn write_meta(
    dir: &Path,
    region: Region,
    fps: f32,
    started: chrono::DateTime<chrono::Utc>,
    backend: Option<&str>,
) -> anyhow::Result<()> {
    let meta = serde_json::json!({
        "region": region,
        "fps": fps,
        "started": started.to_rfc3339(),
        "backend": backend,
    });
    let path = dir.join("meta.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&meta)?).with_context(|| format!("{} 저장 실패", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("nl-record-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).expect("임시 폴더 생성 실패");
        p
    }

    /// 매번 다른 색의 작은 프레임을 내는 가짜 공급원.
    struct FakeSource {
        n: u32,
        w: u32,
        h: u32,
        /// 이 횟수만큼은 실패를 돌려준다.
        fail_first: u32,
    }

    impl FrameSource for FakeSource {
        fn backend_label(&self) -> Option<String> {
            Some("fake".into())
        }
        fn grab(&mut self, _region: &Region) -> anyhow::Result<Frame> {
            self.n += 1;
            if self.n <= self.fail_first {
                anyhow::bail!("일부러 낸 실패 {}", self.n);
            }
            let px = (self.n % 256) as u8;
            Ok(Frame {
                width: self.w,
                height: self.h,
                rgba: vec![px; (self.w * self.h * 4) as usize],
            })
        }
    }

    fn read_labels(dir: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(dir.join("labels.jsonl")).expect("labels.jsonl 이 없다");
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("JSON 이 아니다"))
            .collect()
    }

    #[test]
    fn writes_frames_labels_and_meta_in_the_recorded_layout() {
        let dir = tmp("layout");
        let region = Region {
            monitor: 0,
            x: 0,
            y: 0,
            width: 8,
            height: 6,
        };
        let rec = Recorder::start_with(
            &dir,
            region,
            120.0,
            Box::new(FakeSource {
                n: 0,
                w: 8,
                h: 6,
                fail_first: 0,
            }),
        )
        .unwrap();

        rec.set_label(0);
        while rec.frames_written() < 3 {
            std::thread::sleep(Duration::from_millis(5));
        }
        rec.set_label(7);
        let before = rec.frames_written();
        while rec.frames_written() < before + 3 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let total = rec.finish();

        assert!(total >= 6, "프레임이 너무 적다: {total}");
        let labels = read_labels(&dir);
        assert_eq!(labels.len(), total, "라벨 줄 수와 프레임 수가 다르다");

        // 파일 이름은 저장 순서대로 끊기지 않는다.
        for (i, l) in labels.iter().enumerate() {
            let name = format!("{:06}.png", i + 1);
            assert_eq!(l["frame"], serde_json::json!(name), "{i}번째 줄");
            assert!(dir.join("frames").join(&name).is_file(), "{name} 파일이 없다");
            assert!(l["t_ms"].is_u64(), "t_ms 가 없다: {l}");
        }
        // 라벨은 바뀐 시점 이후로 7 이어야 한다.
        assert_eq!(labels[0]["label"], serde_json::json!(0));
        assert_eq!(labels.last().unwrap()["label"], serde_json::json!(7));

        // meta.json.
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["fps"], serde_json::json!(120.0));
        assert_eq!(meta["backend"], serde_json::json!("fake"));
        assert_eq!(meta["region"]["width"], serde_json::json!(8));
        assert!(
            meta["started"].as_str().unwrap().contains('T'),
            "시작 시각이 RFC 3339 가 아니다"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_recorded_folder_loads_back_through_nl_engine() {
        let dir = tmp("loadback");
        let region = Region {
            monitor: 0,
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let rec = Recorder::start_with(
            &dir,
            region,
            120.0,
            Box::new(FakeSource {
                n: 0,
                w: 4,
                h: 4,
                fail_first: 0,
            }),
        )
        .unwrap();
        rec.set_label(2);
        while rec.frames_written() < 4 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let total = rec.finish();

        let spec = nl_core::DatasetSpec::new(
            "녹화",
            nl_core::DataSource::Recorded {
                path: dir.to_string_lossy().into_owned(),
            },
        );
        let info = nl_engine::scan(&spec, Path::new(".")).expect("녹화 폴더를 다시 읽지 못했다");
        assert_eq!(info.samples, total, "적재기가 센 샘플 수가 녹화한 프레임 수와 다르다");
        assert_eq!(
            info.input_shape,
            vec![3, 4, 4],
            "RGBA 회색 프레임은 3채널 4x4 로 읽힌다"
        );
        assert_eq!(info.target_shape, vec![1]);
        // 라벨 2 를 썼으니 클래스는 0..=2.
        assert_eq!(info.classes.len(), 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn capture_failures_are_reported_and_recording_gives_up() {
        let dir = tmp("failing");
        let region = Region {
            monitor: 0,
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let rec = Recorder::start_with(
            &dir,
            region,
            240.0,
            // 계속 실패한다 (fail_first 를 아주 크게).
            Box::new(FakeSource {
                n: 0,
                w: 4,
                h: 4,
                fail_first: u32::MAX,
            }),
        )
        .unwrap();
        assert!(
            rec.wait_done(Duration::from_secs(5)),
            "연속 실패인데 녹화가 끝나지 않았다"
        );
        let err = rec.error().expect("오류가 기록되지 않았다");
        assert!(err.contains("멈춘다"), "{err}");
        assert_eq!(rec.frames_written(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transient_failures_do_not_stop_the_recording() {
        let dir = tmp("transient");
        let region = Region {
            monitor: 0,
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let rec = Recorder::start_with(
            &dir,
            region,
            240.0,
            Box::new(FakeSource {
                n: 0,
                w: 4,
                h: 4,
                fail_first: 3,
            }),
        )
        .unwrap();
        while rec.frames_written() < 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // 앞의 세 번은 실패했고 그 사실이 남아 있다. 그래도 녹화는 이어졌다.
        let err = rec.error().expect("일시적 실패가 기록되지 않았다");
        assert!(err.contains("일부러 낸 실패"), "{err}");
        assert!(!rec.is_done(), "일시적 실패로 녹화가 멈췄다");

        let total = rec.finish();
        assert!(total >= 2, "프레임이 너무 적다: {total}");
        assert_eq!(read_labels(&dir).len(), total);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn zero_or_negative_fps_is_rejected() {
        let dir = tmp("badfps");
        let region = Region::default();
        for bad in [0.0f32, -1.0, f32::NAN] {
            assert!(
                Recorder::start_with(
                    &dir,
                    region,
                    bad,
                    Box::new(FakeSource {
                        n: 0,
                        w: 2,
                        h: 2,
                        fail_first: 0
                    })
                )
                .is_err(),
                "fps {bad} 가 통과했다"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mismatched_frame_buffer_is_an_error_not_a_panic() {
        let bad = Frame {
            width: 4,
            height: 4,
            rgba: vec![0; 10],
        };
        let dir = tmp("badframe");
        assert!(save_png(&dir.join("x.png"), &bad).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 실제 화면이 있으면 1초 녹화해 본다. 없으면 조용히 통과.
    #[test]
    fn records_the_real_screen_when_a_session_exists() {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            eprintln!("그래픽 세션이 없어 실제 녹화 시험을 건너뛴다");
            return;
        }
        let dir = tmp("realscreen");
        let region = Region {
            monitor: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let rec = match Recorder::start(&dir, region, 4.0) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("녹화를 시작하지 못했다(환경 문제로 보고 통과): {e:#}");
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
        };
        rec.set_label(1);
        std::thread::sleep(Duration::from_secs(1));
        let last_error = rec.error();
        let dropped = rec.frames_dropped();
        let total = rec.finish();
        if total == 0 {
            eprintln!("프레임을 한 장도 찍지 못했다(환경 문제로 보고 통과): {last_error:?}");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        eprintln!("버린 프레임 {dropped}장");
        assert_eq!(read_labels(&dir).len(), total, "라벨 줄 수와 프레임 수가 다르다");
        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        eprintln!("실제 녹화: {total}장, 백엔드 {}", meta["backend"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
