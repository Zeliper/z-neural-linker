//! 화면 캡처. Linux 는 **wlroots(libwayshot) → xdg-desktop-portal → X11(x11rb)** 순으로 시도한다.
//! Windows/macOS 는 xcap.
//!
//! Linux 경로는 **시스템 개발 라이브러리 없이** 빌드된다(순수 Rust: x11rb + libwayshot + zbus).
//!
//! ## 백엔드 고르기
//! | 백엔드 | 언제 쓰이나 | 처리량 |
//! |---|---|---|
//! | [`Backend::Wayland`] | 컴포지터가 `zwlr_screencopy_v1` 이나 `ext_image_copy_capture_v1` 를 줄 때 (Sway, Hyprland …) | 높음 |
//! | [`Backend::Portal`] | Wayland 인데 위 프로토콜이 없을 때 (GNOME, KDE) | **낮음 — 초당 1~3장** |
//! | [`Backend::X11`] | 순수 X11 세션 | 높음 |
//!
//! ## 포털 경로의 성격
//! `org.freedesktop.portal.Screenshot` 은 한 장씩 PNG 파일로 돌려주는 인터페이스라 연속 캡처에 맞지 않는다.
//! 왕복마다 D-Bus 호출 · PNG 인코딩 · 파일 읽기가 끼어 초당 몇 장이 한계다.
//! **첫 호출에 권한 대화상자가 뜰 수 있고**, KDE 는 그 창에 "기억" 옵션을 준다. 사용자가 응답하지 않으면
//! [`PORTAL_TIMEOUT`] 뒤에 명확한 오류로 끝난다. GUI 는 [`Capturer::backend`] 와 [`Backend::hint`] 로
//! 지금 어떤 경로인지, 얼마나 나올지 사용자에게 알려 줘야 한다.
//!
//! 고 fps 가 필요한 GNOME/KDE 지원은 `org.freedesktop.portal.ScreenCast` + pipewire 경로이며,
//! pipewire 개발 패키지를 요구하므로 **M1 후반에 선택 feature 로** 붙인다. 이번 단계에서는 하지 않았다.
//!
//! ## 배포판 요구 사항 (Linux)
//! 화면 캡처 자체는 시스템 라이브러리를 쓰지 않는다(X11·Wayland 프로토콜을 소켓으로 직접 말한다).
//! 크레이트 전체로 보면 입력 시뮬레이션 쪽 `libxkbcommon.so.0` 하나만 실행 시 필요하고, 이것은 모든 데스크톱
//! Linux 에 기본 포함된다. 자세한 내용은 [`crate::input`] 모듈 문서 참고.
//!
//! ## 좌표 규약
//! - [`Region::monitor`] 는 [`monitors`] 가 돌려주는 목록의 인덱스다.
//! - [`Region::x`]/[`Region::y`] 는 **그 모니터 왼쪽 위 기준 상대 좌표**(물리 픽셀)다.
//! - [`Region::width`] 가 0 이면 모니터 전체를 찍는다.

use anyhow::{anyhow, bail};
use nl_core::pipeline::Region;

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// RGBA8 프레임.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// 실제로 쓰인 캡처 백엔드 (진단·보고용).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Wayland: zwlr_screencopy_v1 또는 ext-image-copy-capture-v1 (libwayshot).
    Wayland,
    /// Wayland: `org.freedesktop.portal.Screenshot` (zbus). GNOME·KDE 경로.
    Portal,
    /// X11: `GetImage` on root (x11rb).
    X11,
    /// Windows/macOS 네이티브 (xcap).
    XCap,
}

impl Backend {
    pub fn label(&self) -> &'static str {
        match self {
            Backend::Wayland => "wayland(libwayshot)",
            Backend::Portal => "portal(xdg-desktop-portal)",
            Backend::X11 => "x11(x11rb)",
            Backend::XCap => "xcap",
        }
    }

    /// 사용자에게 보여 줄 한 줄 안내 — 예상 처리량과 주의점.
    pub fn hint(&self) -> &'static str {
        match self {
            Backend::Wayland => "컴포지터에서 프레임을 직접 받는다. 초당 수십 장까지 나온다.",
            Backend::Portal => {
                "xdg-desktop-portal 을 거쳐 한 장씩 PNG 로 받는다. 초당 1~3장이 한계이고, 첫 호출에 \
                 권한 대화상자가 뜰 수 있다(KDE 는 '기억' 을 고르면 다음부터 묻지 않는다)."
            }
            Backend::X11 => "X 서버의 루트 창을 직접 읽는다. 초당 수십 장까지 나온다.",
            Backend::XCap => "OS 네이티브 캡처 API 를 쓴다. 초당 수십 장까지 나온다.",
        }
    }
}

/// 포털 응답을 기다리는 상한. 사용자가 권한 대화상자를 보고 누르는 시간을 포함한다.
pub const PORTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 연결을 유지하는 캡처기. 반복 캡처(파이프라인 `Source::ScreenCapture`)는 이것을 재사용해야 한다.
/// 매 프레임 새로 연결하면 X11 핸드셰이크·Wayland 레지스트리 왕복 비용이 그대로 들어간다.
pub struct Capturer {
    imp: imp::Imp,
}

impl Capturer {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { imp: imp::Imp::new()? })
    }

    /// 실제로 캡처에 성공한 백엔드. 첫 캡처 전에는 `None`.
    pub fn backend(&self) -> Option<Backend> {
        self.imp.backend()
    }

    pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
        self.imp.monitors()
    }

    /// `region.width == 0` 이면 모니터 전체.
    pub fn capture(&mut self, region: &Region) -> anyhow::Result<Frame> {
        self.imp.capture(region)
    }
}

pub fn monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    Capturer::new()?.monitors()
}

/// `region.width == 0` 이면 모니터 전체.
/// 반복 호출은 [`Capturer`] 를 직접 들고 쓰는 편이 빠르다.
pub fn capture(region: &Region) -> anyhow::Result<Frame> {
    Capturer::new()?.capture(region)
}

/// 모니터 목록에서 `index` 를 꺼낸다. 범위를 벗어나면 사람이 읽을 수 있는 오류.
fn pick(list: &[MonitorInfo], index: usize) -> anyhow::Result<&MonitorInfo> {
    list.get(index).ok_or_else(|| anyhow!("모니터 {index} 번이 없다 (연결된 모니터 {}개)", list.len()))
}

/// 모니터 상대 영역을 모니터 안으로 자른다. `width == 0` 이면 전체.
/// 반환은 `(모니터 상대 x, y, w, h)`.
fn clamp_region(m: &MonitorInfo, region: &Region) -> anyhow::Result<(u32, u32, u32, u32)> {
    if region.width == 0 || region.height == 0 {
        return Ok((0, 0, m.width, m.height));
    }
    let x = region.x.max(0) as u32;
    let y = region.y.max(0) as u32;
    if x >= m.width || y >= m.height {
        bail!("영역 시작점 ({x}, {y}) 이 모니터 {}x{} 밖이다", m.width, m.height);
    }
    let w = region.width.min(m.width - x);
    let h = region.height.min(m.height - y);
    Ok((x, y, w, h))
}

// ───────────────────────────── Linux ─────────────────────────────

#[cfg(target_os = "linux")]
mod imp {
    use super::{clamp_region, pick, Backend, Frame, MonitorInfo};
    use anyhow::{anyhow, bail};
    use nl_core::pipeline::Region;

    pub struct Imp {
        wayland: Option<wl::Conn>,
        portal: Option<portal::Conn>,
        x11: Option<x11::Conn>,
        chosen: Option<Backend>,
        /// 초기화 단계에서 모인 실패 사유 (전부 안 될 때 사용자에게 보여 준다).
        init_errors: Vec<String>,
    }

    /// 모니터 하나의 **논리 좌표계** 사각형. 포털이 준 전체 화면 이미지에서 위치를 찾는 데 쓴다.
    /// Wayland 는 스케일 적용 후 좌표, X11 은 물리 픽셀(= 논리와 같음).
    #[derive(Clone, Copy, Debug)]
    pub(super) struct LogicalRect {
        pub x: i32,
        pub y: i32,
        pub w: u32,
        pub h: u32,
    }

    /// D-Bus 세션 버스에 붙을 수 있는 환경인가.
    fn has_session_bus() -> bool {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return true;
        }
        // 주소가 없어도 XDG_RUNTIME_DIR/bus 가 있으면 zbus 가 알아서 찾는다.
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| std::path::Path::new(&d).join("bus").exists())
            .unwrap_or(false)
    }

    /// 왜 안 되는지, 무엇을 해야 하는지 한 줄 안내.
    fn advice() -> String {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        if wayland {
            let mut s = String::from(
                "Wayland 세션이다. 컴포지터가 wlr-screencopy(zwlr_screencopy_manager_v1) 도 \
                 ext-image-copy-capture(ext_image_copy_capture_manager_v1) 도 주지 않으면 \
                 xdg-desktop-portal 로 넘어가는데, 그것마저 안 되면 남는 길이 없다. \
                 포털이 실패했다면 xdg-desktop-portal 과 데스크톱용 백엔드(KDE: xdg-desktop-portal-kde, \
                 GNOME: xdg-desktop-portal-gnome)가 깔려 돌고 있는지, 권한 대화상자를 거부하지 않았는지 확인하라.",
            );
            if x11 {
                s.push_str(
                    " XWayland(DISPLAY) 로 물러나는 것도 안 된다 — XWayland 는 rootless 라 루트 창이 \
                     viewable 이 아니고, 그 위의 GetImage 는 BadMatch 로 거부된다.",
                );
            }
            s
        } else if x11 {
            "X11 세션인데 GetImage 가 실패했다. 화면 밖 영역이거나 X 서버가 거부한 경우다.".into()
        } else {
            "그래픽 세션이 없다(DISPLAY·WAYLAND_DISPLAY 둘 다 비어 있음).".into()
        }
    }

    /// 어떤 리눅스 세션인지 사람이 읽을 수 있게.
    fn session_hint() -> String {
        let wl = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        let dpy = std::env::var("DISPLAY").unwrap_or_default();
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "?".into());
        let stype = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into());
        format!(
            "세션={stype}, 데스크톱={desktop}, WAYLAND_DISPLAY={}, DISPLAY={}",
            if wl.is_empty() { "(없음)" } else { &wl },
            if dpy.is_empty() { "(없음)" } else { &dpy }
        )
    }

    impl Imp {
        pub fn new() -> anyhow::Result<Self> {
            let mut init_errors = Vec::new();

            let wayland = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                match wl::Conn::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        init_errors.push(format!("wayland: {e:#}"));
                        None
                    }
                }
            } else {
                init_errors.push("wayland: WAYLAND_DISPLAY 가 없다".into());
                None
            };

            // 포털은 Wayland 세션에서만 의미가 있다 (X11 세션이면 GetImage 가 더 빠르고 확실하다).
            let portal = if std::env::var_os("WAYLAND_DISPLAY").is_some() && has_session_bus() {
                match portal::Conn::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        init_errors.push(format!("portal: {e:#}"));
                        None
                    }
                }
            } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                init_errors.push("portal: D-Bus 세션 버스가 없다 (DBUS_SESSION_BUS_ADDRESS·XDG_RUNTIME_DIR 확인)".into());
                None
            } else {
                None
            };

            let x11 = if std::env::var_os("DISPLAY").is_some() {
                match x11::Conn::new() {
                    Ok(c) => Some(c),
                    Err(e) => {
                        init_errors.push(format!("x11: {e:#}"));
                        None
                    }
                }
            } else {
                init_errors.push("x11: DISPLAY 가 없다".into());
                None
            };

            if wayland.is_none() && portal.is_none() && x11.is_none() {
                bail!(
                    "화면 캡처 백엔드를 찾을 수 없다 ({}). 사유: {}",
                    session_hint(),
                    init_errors.join(" / ")
                );
            }
            Ok(Self { wayland, portal, x11, chosen: None, init_errors })
        }

        pub fn backend(&self) -> Option<Backend> {
            self.chosen
        }

        pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
            // 이미 캡처에 성공한 백엔드가 있으면 인덱스가 어긋나지 않도록 그쪽을 그대로 쓴다.
            // 포털은 모니터 정보를 주지 않으므로 Wayland 열거(wl_output)를 그대로 쓴다 — 아래 일반 경로와 같다.
            match self.chosen {
                Some(Backend::Wayland) => {
                    return self.wayland.as_mut().expect("wayland 백엔드가 선택됐는데 연결이 없다").monitors()
                }
                Some(Backend::X11) => {
                    return self.x11.as_mut().expect("x11 백엔드가 선택됐는데 연결이 없다").monitors()
                }
                _ => {}
            }
            let mut errs = self.init_errors.clone();
            if let Some(w) = self.wayland.as_mut() {
                match w.monitors() {
                    Ok(v) if !v.is_empty() => return Ok(v),
                    Ok(_) => errs.push("wayland: 출력이 하나도 없다".into()),
                    Err(e) => errs.push(format!("wayland: {e:#}")),
                }
            }
            if let Some(x) = self.x11.as_mut() {
                match x.monitors() {
                    Ok(v) if !v.is_empty() => return Ok(v),
                    Ok(_) => errs.push("x11: 화면이 하나도 없다".into()),
                    Err(e) => errs.push(format!("x11: {e:#}")),
                }
            }
            bail!("모니터 목록을 얻지 못했다 ({}). 사유: {}", session_hint(), errs.join(" / "))
        }

        pub fn capture(&mut self, region: &Region) -> anyhow::Result<Frame> {
            // 백엔드가 정해졌으면 그대로. 아니면 wayland → x11 순으로 실제 캡처를 시도해 정한다.
            // (프로토콜 지원 여부는 실제로 한 장 찍어 봐야 확실히 알 수 있다 — KDE/GNOME 는
            //  wlr-screencopy 가 없고 ext-image-copy-capture 만 있거나, 둘 다 없을 수 있다.)
            if let Some(b) = self.chosen {
                return match b {
                    Backend::Wayland => self.wayland.as_mut().expect("wayland 연결 없음").capture(region),
                    Backend::Portal => self.portal_capture(region),
                    Backend::X11 => self.x11.as_mut().expect("x11 연결 없음").capture(region),
                    Backend::XCap => unreachable!("리눅스에서 xcap 백엔드는 쓰지 않는다"),
                };
            }
            let mut errs = self.init_errors.clone();
            if let Some(w) = self.wayland.as_mut() {
                match w.capture(region) {
                    Ok(f) => {
                        self.chosen = Some(Backend::Wayland);
                        return Ok(f);
                    }
                    Err(e) => errs.push(format!("wayland: {e:#}")),
                }
            }
            if self.portal.is_some() {
                match self.portal_capture(region) {
                    Ok(f) => {
                        self.chosen = Some(Backend::Portal);
                        return Ok(f);
                    }
                    Err(e) => errs.push(format!("portal: {e:#}")),
                }
            }
            if let Some(x) = self.x11.as_mut() {
                match x.capture(region) {
                    Ok(f) => {
                        self.chosen = Some(Backend::X11);
                        return Ok(f);
                    }
                    Err(e) => errs.push(format!("x11: {e:#}")),
                }
            }
            bail!("화면 캡처 실패 ({}). {} 사유: {}", session_hint(), advice(), errs.join(" / "))
        }

        /// 모니터들의 논리 좌표 사각형. Wayland 열거를 먼저, 없으면 X11 randr.
        /// 포털이 준 전체 화면 이미지에서 어느 부분이 어느 모니터인지 찾는 데 쓴다.
        fn logical_rects(&mut self) -> anyhow::Result<Vec<LogicalRect>> {
            if let Some(w) = self.wayland.as_mut() {
                if let Ok(v) = w.logical_rects() {
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
            }
            let x = self.x11.as_mut().ok_or_else(|| {
                anyhow!("모니터 좌표를 알 수 없다 (Wayland 출력 열거도, X11 randr 도 되지 않는다)")
            })?;
            x.logical_rects()
        }

        /// 포털로 전체 화면을 한 장 받아 요청 영역만 잘라낸다.
        fn portal_capture(&mut self, region: &Region) -> anyhow::Result<Frame> {
            let list = self.monitors()?;
            let m = pick(&list, region.monitor)?.clone();
            let (rx, ry, rw, rh) = clamp_region(&m, region)?;
            let rects = self.logical_rects()?;
            let rect = *rects
                .get(region.monitor)
                .ok_or_else(|| anyhow!("모니터 {} 번의 좌표를 찾지 못했다", region.monitor))?;

            let img = self
                .portal
                .as_mut()
                .ok_or_else(|| anyhow!("포털 연결이 없다"))?
                .shoot(super::PORTAL_TIMEOUT)?;

            crop_from_desktop(&img, &rects, rect, &m, (rx, ry, rw, rh))
        }
    }

    /// 전체 데스크톱 이미지에서 모니터 안의 영역을 잘라낸다.
    ///
    /// 포털은 모든 출력을 합친 이미지 한 장만 준다. 그 이미지의 좌표계가 논리인지 물리인지 말해 주지 않으므로,
    /// **모든 모니터의 논리 경계 상자 대 이미지 크기 비율**로 환산한다. 배율이 하나뿐인 흔한 경우에는 정확하고,
    /// 모니터마다 배율이 다르면 근사가 된다(합성 방식이 컴포지터마다 달라 정확히 맞출 방법이 없다).
    fn crop_from_desktop(
        img: &image::RgbaImage,
        rects: &[LogicalRect],
        rect: LogicalRect,
        m: &MonitorInfo,
        area: (u32, u32, u32, u32),
    ) -> anyhow::Result<Frame> {
        let (rx, ry, rw, rh) = area;
        let (bx, by, bw, bh) = bounding_box(rects)?;
        if img.width() == 0 || img.height() == 0 {
            bail!("포털이 빈 이미지를 돌려줬다");
        }
        let sx = img.width() as f64 / bw as f64;
        let sy = img.height() as f64 / bh as f64;

        // 요청 영역은 모니터 기준 **물리** 픽셀이다. 그 모니터의 배율로 논리 좌표로 되돌린다.
        let mscale_x = if rect.w > 0 { m.width as f64 / rect.w as f64 } else { 1.0 };
        let mscale_y = if rect.h > 0 { m.height as f64 / rect.h as f64 } else { 1.0 };

        let lx = (rect.x - bx) as f64 + rx as f64 / mscale_x.max(f64::MIN_POSITIVE);
        let ly = (rect.y - by) as f64 + ry as f64 / mscale_y.max(f64::MIN_POSITIVE);
        let lw = rw as f64 / mscale_x.max(f64::MIN_POSITIVE);
        let lh = rh as f64 / mscale_y.max(f64::MIN_POSITIVE);

        let px = (lx * sx).round().max(0.0) as u32;
        let py = (ly * sy).round().max(0.0) as u32;
        let pw = ((lw * sx).round() as u32).max(1).min(img.width().saturating_sub(px));
        let ph = ((lh * sy).round() as u32).max(1).min(img.height().saturating_sub(py));
        if px >= img.width() || py >= img.height() || pw == 0 || ph == 0 {
            bail!(
                "잘라낼 영역 ({px}, {py}, {pw}x{ph}) 이 포털 이미지 {}x{} 밖이다",
                img.width(),
                img.height()
            );
        }

        let view = image::imageops::crop_imm(img, px, py, pw, ph).to_image();
        Ok(Frame { width: view.width(), height: view.height(), rgba: view.into_raw() })
    }

    /// 모든 모니터를 감싸는 논리 좌표 경계 상자 `(x, y, w, h)`.
    fn bounding_box(rects: &[LogicalRect]) -> anyhow::Result<(i32, i32, u32, u32)> {
        let first = rects.first().ok_or_else(|| anyhow!("모니터가 하나도 없다"))?;
        let (mut x0, mut y0) = (first.x, first.y);
        let (mut x1, mut y1) = (first.x + first.w as i32, first.y + first.h as i32);
        for r in rects.iter().skip(1) {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.x + r.w as i32);
            y1 = y1.max(r.y + r.h as i32);
        }
        let (w, h) = ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
        Ok((x0, y0, w, h))
    }

    #[cfg(test)]
    mod crop_tests {
        use super::*;

        fn mon(x: i32, y: i32, w: u32, h: u32) -> MonitorInfo {
            MonitorInfo { index: 0, name: "m".into(), x, y, width: w, height: h, primary: true }
        }

        fn rect(x: i32, y: i32, w: u32, h: u32) -> LogicalRect {
            LogicalRect { x, y, w, h }
        }

        #[test]
        fn bounding_box_covers_every_monitor() {
            let rects = [rect(0, 257, 1920, 1080), rect(1920, 0, 1920, 1080)];
            assert_eq!(bounding_box(&rects).unwrap(), (0, 0, 3840, 1337));
            assert!(bounding_box(&[]).is_err());
        }

        #[test]
        fn crops_second_monitor_at_scale_one() {
            // 3840x1080 데스크톱, 1920x1080 모니터 둘이 가로로 붙어 있다.
            let img = image::RgbaImage::from_pixel(3840, 1080, image::Rgba([1, 2, 3, 255]));
            let rects = [rect(0, 0, 1920, 1080), rect(1920, 0, 1920, 1080)];
            let m = mon(1920, 0, 1920, 1080);
            let f = crop_from_desktop(&img, &rects, rects[1], &m, (0, 0, 1920, 1080)).unwrap();
            assert_eq!((f.width, f.height), (1920, 1080));
            assert_eq!(f.rgba.len(), 1920 * 1080 * 4);
        }

        #[test]
        fn crops_sub_region_inside_a_monitor() {
            let img = image::RgbaImage::from_pixel(1920, 1080, image::Rgba([0, 0, 0, 255]));
            let rects = [rect(0, 0, 1920, 1080)];
            let m = mon(0, 0, 1920, 1080);
            let f = crop_from_desktop(&img, &rects, rects[0], &m, (100, 50, 640, 480)).unwrap();
            assert_eq!((f.width, f.height), (640, 480));
        }

        #[test]
        fn converts_physical_request_through_monitor_scale() {
            // 논리 1920x1080 / 물리 3840x2160 (배율 2). 포털 이미지도 물리 해상도.
            let img = image::RgbaImage::from_pixel(3840, 2160, image::Rgba([0, 0, 0, 255]));
            let rects = [rect(0, 0, 1920, 1080)];
            let m = mon(0, 0, 3840, 2160);
            // 모니터 전체 요청 → 물리 픽셀 그대로 나와야 한다.
            let f = crop_from_desktop(&img, &rects, rects[0], &m, (0, 0, 3840, 2160)).unwrap();
            assert_eq!((f.width, f.height), (3840, 2160));
            // 물리 기준 절반 영역 → 이미지에서도 절반.
            let half = crop_from_desktop(&img, &rects, rects[0], &m, (0, 0, 1920, 1080)).unwrap();
            assert_eq!((half.width, half.height), (1920, 1080));
        }

        #[test]
        fn image_smaller_than_the_desktop_maps_proportionally() {
            // 포털이 경계 상자보다 작은 이미지를 줘도(합성 방식 차이) 비율로 환산해 이미지 안에 머문다.
            // 잘라낸 결과가 작아질 뿐 오류는 아니다.
            let img = image::RgbaImage::from_pixel(1100, 100, image::Rgba([0, 0, 0, 255]));
            let rects = [rect(0, 0, 1000, 100), rect(1000, 0, 100, 100)];
            let m = mon(1000, 0, 100, 100);
            let f = crop_from_desktop(&img, &rects, rects[1], &m, (0, 0, 100, 100)).unwrap();
            assert_eq!((f.width, f.height), (100, 100));

            // 절반 크기 이미지면 결과도 절반.
            let small = image::RgbaImage::from_pixel(550, 50, image::Rgba([0, 0, 0, 255]));
            let g = crop_from_desktop(&small, &rects, rects[1], &m, (0, 0, 100, 100)).unwrap();
            assert_eq!((g.width, g.height), (50, 50));
        }

        #[test]
        fn crop_never_runs_past_the_image_edge() {
            let img = image::RgbaImage::from_pixel(200, 100, image::Rgba([0, 0, 0, 255]));
            let rects = [rect(0, 0, 200, 100)];
            let m = mon(0, 0, 200, 100);
            // 오른쪽 끝에 걸친 영역을 달라고 해도 이미지 경계에서 잘린다.
            let f = crop_from_desktop(&img, &rects, rects[0], &m, (190, 0, 100, 100)).unwrap();
            assert_eq!(f.width, 10);
            assert_eq!(f.rgba.len(), 10 * 100 * 4);
        }

        #[test]
        fn empty_image_is_rejected() {
            let img = image::RgbaImage::new(0, 0);
            let rects = [rect(0, 0, 10, 10)];
            assert!(crop_from_desktop(&img, &rects, rects[0], &mon(0, 0, 10, 10), (0, 0, 10, 10)).is_err());
        }
    }

    // ── xdg-desktop-portal (zbus) ──
    //
    // `org.freedesktop.portal.Screenshot` 의 호출 규약:
    //   1. 고유한 `handle_token` 을 정하고, 그것으로 `Request` 객체 경로를 미리 계산한다
    //      (`/org/freedesktop/portal/desktop/request/<버스이름>/<토큰>`).
    //   2. **먼저** 그 경로의 `Response` 신호를 구독한다. 메서드 호출 뒤에 구독하면 신호를 놓칠 수 있다.
    //   3. `Screenshot("", {handle_token, interactive: false})` 를 부른다.
    //   4. `Response(u code, a{sv} results)` 를 기다린다. code 0 = 성공, 1 = 사용자 취소, 2 = 그 밖의 종료.
    //   5. `results["uri"]` 의 PNG 파일을 읽고 지운다 (포털은 호출자가 치우기를 기대한다).
    mod portal {
        use anyhow::{anyhow, bail, Context};
        use std::collections::HashMap;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        use zbus::blocking::{Connection, Proxy};
        use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

        const DEST: &str = "org.freedesktop.portal.Desktop";
        const OBJ: &str = "/org/freedesktop/portal/desktop";
        const IFACE: &str = "org.freedesktop.portal.Screenshot";

        pub struct Conn {
            /// 타임아웃 때 연결을 닫아 대기를 끊으므로, 그 뒤에는 비어 있을 수 있다(다음 호출에 다시 연다).
            conn: Option<Connection>,
            /// `handle_token` 을 매번 다르게 만들기 위한 카운터.
            seq: u64,
        }

        impl Conn {
            pub fn new() -> anyhow::Result<Self> {
                let conn = Connection::session().context(
                    "D-Bus 세션 버스에 붙지 못했다 (DBUS_SESSION_BUS_ADDRESS 또는 XDG_RUNTIME_DIR/bus 확인)",
                )?;
                // Screenshot 인터페이스가 실제로 있는지 여기서 확인한다. 없으면 캡처를 시도할 필요가 없다.
                let p = Proxy::new(&conn, DEST, OBJ, IFACE).context("Screenshot 포털 프록시 생성 실패")?;
                let version: u32 = p.get_property("version").context(
                    "xdg-desktop-portal 의 Screenshot 인터페이스가 없다 \
                     (xdg-desktop-portal 과 데스크톱용 백엔드가 필요하다: KDE 는 xdg-desktop-portal-kde, GNOME 은 -gnome)",
                )?;
                log::debug!("xdg-desktop-portal Screenshot 버전 {version}");
                Ok(Self { conn: Some(conn), seq: 0 })
            }

            /// 전체 데스크톱을 한 장 찍어 RGBA 로 돌려준다.
            ///
            /// `timeout` 은 **사용자가 권한 대화상자를 보고 누르는 시간까지 포함**한다. 시간이 지나면
            /// D-Bus 연결을 닫아 대기를 끊고 오류를 낸다(그렇게 하지 않으면 신호 대기가 영원히 안 끝난다).
            pub fn shoot(&mut self, timeout: Duration) -> anyhow::Result<image::RgbaImage> {
                let conn = match self.conn.take() {
                    Some(c) => c,
                    None => Connection::session().context("D-Bus 세션 버스에 다시 붙지 못했다")?,
                };
                let out = self.shoot_once(&conn, timeout);
                // 타임아웃으로 닫힌 연결은 버린다.
                if !conn.is_closed() {
                    self.conn = Some(conn);
                }
                out
            }

            fn shoot_once(&mut self, conn: &Connection, timeout: Duration) -> anyhow::Result<image::RgbaImage> {
                self.seq += 1;
                let token = format!("nl_io_{}_{}", std::process::id(), self.seq);
                let unique = conn
                    .unique_name()
                    .ok_or_else(|| anyhow!("D-Bus 고유 이름이 없다 (버스에 붙지 않은 연결)"))?
                    .to_string();
                let sender = unique.trim_start_matches(':').replace('.', "_");
                let req_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

                // 1) 응답을 먼저 구독한다.
                let request = Proxy::new(conn, DEST, req_path.as_str(), "org.freedesktop.portal.Request")
                    .context("Request 프록시 생성 실패")?;
                let mut responses = request.receive_signal("Response").context("Response 신호 구독 실패")?;

                // 2) Screenshot 호출.
                let shot = Proxy::new(conn, DEST, OBJ, IFACE).context("Screenshot 프록시 생성 실패")?;
                let mut options: HashMap<&str, Value<'_>> = HashMap::new();
                options.insert("handle_token", Value::from(token.as_str()));
                options.insert("interactive", Value::from(false));
                let handle: OwnedObjectPath = shot
                    .call("Screenshot", &("", options))
                    .context("Screenshot 포털 호출 실패")?;
                if handle.as_str() != req_path {
                    bail!(
                        "포털이 handle_token 을 무시하고 다른 Request 경로를 돌려줬다 \
                         (기대 {req_path}, 실제 {handle}). 이 포털 구현은 지원하지 않는다"
                    );
                }

                // 3) 타임아웃 감시: 시간이 지나면 연결을 닫아 신호 대기를 끊는다.
                //    `Request.Close()` 는 Response 를 내보내지 않는다고 명세에 적혀 있어 쓸 수 없다.
                let (cancel_tx, cancel_rx) = crossbeam_channel::bounded::<()>(1);
                let fired = Arc::new(AtomicBool::new(false));
                let (watch_conn, watch_flag) = (conn.clone(), fired.clone());
                let watchdog = std::thread::Builder::new()
                    .name("nl-portal-timeout".into())
                    .spawn(move || {
                        if cancel_rx.recv_timeout(timeout).is_err() {
                            watch_flag.store(true, Ordering::SeqCst);
                            let _ = watch_conn.close();
                        }
                    })
                    .context("포털 타임아웃 감시 스레드 생성 실패")?;

                let msg = responses.next();
                let _ = cancel_tx.send(());
                let _ = watchdog.join();

                if fired.load(Ordering::SeqCst) {
                    bail!(
                        "포털이 {}초 안에 응답하지 않았다. 권한 대화상자가 떠 있는데 아무도 누르지 않았을 수 있다 \
                         (KDE 는 그 창의 '기억' 을 고르면 다음부터 묻지 않는다)",
                        timeout.as_secs()
                    );
                }
                let msg = msg.ok_or_else(|| anyhow!("포털 응답 신호가 끊겼다 (D-Bus 연결이 닫혔다)"))?;

                // 4) 응답 해석.
                let (code, results): (u32, HashMap<String, OwnedValue>) =
                    msg.body().deserialize().context("Response 신호 본문을 읽지 못했다")?;
                match code {
                    0 => {}
                    1 => bail!("사용자가 화면 캡처 권한 요청을 취소했다"),
                    other => bail!("포털이 화면 캡처를 끝냈다 (응답 코드 {other})"),
                }

                // 5) PNG 파일을 읽고 지운다.
                let uri = results
                    .get("uri")
                    .ok_or_else(|| anyhow!("포털 응답에 uri 가 없다 (받은 키: {:?})", results.keys()))?;
                let uri = String::try_from(uri.try_clone().context("uri 값을 복제하지 못했다")?)
                    .context("포털이 돌려준 uri 가 문자열이 아니다")?;
                let path = file_uri_to_path(&uri)?;
                let img = image::open(&path)
                    .with_context(|| format!("포털이 만든 {} 을 열지 못했다", path.display()))?;
                if let Err(e) = std::fs::remove_file(&path) {
                    log::warn!("포털 임시 파일 {} 을 지우지 못했다: {e}", path.display());
                }
                Ok(img.to_rgba8())
            }
        }

        /// `file:///tmp/a%20b.png` → `/tmp/a b.png`.
        fn file_uri_to_path(uri: &str) -> anyhow::Result<PathBuf> {
            let rest = uri
                .strip_prefix("file://")
                .ok_or_else(|| anyhow!("포털이 file:// URI 가 아닌 것을 돌려줬다: {uri}"))?;
            // `file://host/path` 형태를 대비해 첫 `/` 부터가 경로다.
            let slash = rest.find('/').ok_or_else(|| anyhow!("URI 에 경로가 없다: {uri}"))?;
            Ok(PathBuf::from(percent_decode(&rest[slash..])))
        }

        fn percent_decode(s: &str) -> String {
            let b = s.as_bytes();
            let mut out = Vec::with_capacity(b.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' && i + 2 < b.len() {
                    if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            String::from_utf8_lossy(&out).into_owned()
        }

        fn hex(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn decodes_file_uris() {
                assert_eq!(file_uri_to_path("file:///tmp/a.png").unwrap(), PathBuf::from("/tmp/a.png"));
                assert_eq!(
                    file_uri_to_path("file:///tmp/a%20b%2Fc.png").unwrap(),
                    PathBuf::from("/tmp/a b/c.png")
                );
                // 호스트가 붙은 형태도 경로만 남는다.
                assert_eq!(file_uri_to_path("file://localhost/tmp/x.png").unwrap(), PathBuf::from("/tmp/x.png"));
                assert!(file_uri_to_path("http://example.com/x.png").is_err());
            }

            #[test]
            fn percent_decode_leaves_bad_escapes_alone() {
                assert_eq!(percent_decode("a%zzb"), "a%zzb");
                assert_eq!(percent_decode("tail%2"), "tail%2");
                assert_eq!(percent_decode("%ED%95%9C"), "한");
            }
        }
    }

    // ── Wayland (libwayshot) ──
    mod wl {
        use super::{clamp_region, pick, MonitorInfo};
        use anyhow::{anyhow, Context};
        use libwayshot::{
            region::{LogicalRegion, Position, Region as WlRegion, Size},
            WayshotConnection,
        };
        use nl_core::pipeline::Region;

        pub struct Conn {
            conn: WayshotConnection,
        }

        /// libwayshot 오류는 `Display` 가 세부 내용을 버린다
        /// (`ProtocolNotFound("ZwlrScreencopyManagerV1 not found")` → "Cannot find required wayland protocol").
        /// 어떤 프로토콜이 없는지 알아야 진단이 되므로 `Debug` 를 함께 싣는다.
        fn wl_err(e: libwayshot::Error) -> anyhow::Error {
            anyhow!("{e} ({e:?})")
        }

        impl Conn {
            pub fn new() -> anyhow::Result<Self> {
                let conn = WayshotConnection::new().map_err(wl_err).context("wayland 연결 실패")?;
                Ok(Self { conn })
            }

            pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
                self.conn.refresh_outputs().map_err(wl_err).context("출력 목록 갱신 실패")?;
                Ok(self
                    .conn
                    .get_all_outputs()
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        // physical_size 는 모드(실제 픽셀) 크기, logical_region 은 스케일 적용 후 좌표계.
                        let phys = o.physical_size();
                        let log = o.logical_region.inner;
                        let (w, h) = if phys.width > 0 && phys.height > 0 {
                            (phys.width, phys.height)
                        } else {
                            (log.size.width, log.size.height)
                        };
                        MonitorInfo {
                            index: i,
                            name: if o.name.is_empty() { format!("output-{i}") } else { o.name.clone() },
                            x: log.position.x,
                            y: log.position.y,
                            width: w,
                            height: h,
                            // Wayland 에는 "주 모니터" 개념이 없다. 목록 첫 번째를 주 모니터로 본다.
                            primary: i == 0,
                        }
                    })
                    .collect())
            }

            /// 출력들의 논리 좌표 사각형 (스케일 적용 후).
            pub fn logical_rects(&mut self) -> anyhow::Result<Vec<super::LogicalRect>> {
                self.conn.refresh_outputs().map_err(wl_err).context("출력 목록 갱신 실패")?;
                Ok(self
                    .conn
                    .get_all_outputs()
                    .iter()
                    .map(|o| {
                        let r = o.logical_region.inner;
                        super::LogicalRect { x: r.position.x, y: r.position.y, w: r.size.width, h: r.size.height }
                    })
                    .collect())
            }

            pub fn capture(&mut self, region: &Region) -> anyhow::Result<super::super::Frame> {
                let list = self.monitors()?;
                let m = pick(&list, region.monitor)?;
                let outputs = self.conn.get_all_outputs();
                let out = outputs
                    .get(region.monitor)
                    .ok_or_else(|| anyhow!("모니터 {} 번 출력이 사라졌다", region.monitor))?
                    .clone();
                let (rx, ry, rw, rh) = clamp_region(m, region)?;

                let img = if rw == m.width && rh == m.height && rx == 0 && ry == 0 {
                    // 전체 출력은 논리 좌표 변환 없이 그대로 물리 픽셀을 받는다.
                    self.conn.screenshot_single_output(&out, false).map_err(wl_err)?
                } else {
                    // 부분 영역은 논리 좌표계로만 지정할 수 있다. 스케일이 1 이 아니면 근사가 된다.
                    let log = out.logical_region.inner;
                    let scale = if log.size.height > 0 && m.height > 0 {
                        m.height as f64 / log.size.height as f64
                    } else {
                        1.0
                    };
                    let to_log = |v: u32| ((v as f64 / scale).round() as i64).max(1) as u32;
                    let lr = LogicalRegion {
                        inner: WlRegion {
                            position: Position {
                                x: log.position.x + (rx as f64 / scale).round() as i32,
                                y: log.position.y + (ry as f64 / scale).round() as i32,
                            },
                            size: Size { width: to_log(rw), height: to_log(rh) },
                        },
                    };
                    self.conn.screenshot(lr, false).map_err(wl_err)?
                };

                let rgba = img.to_rgba8();
                Ok(super::super::Frame { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
            }
        }
    }

    // ── X11 (x11rb) ──
    mod x11 {
        use super::{clamp_region, pick, MonitorInfo};
        use anyhow::{anyhow, bail, Context};
        use nl_core::pipeline::Region;
        use x11rb::connection::Connection;
        use x11rb::protocol::randr::ConnectionExt as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Setup, Visualtype};
        use x11rb::rust_connection::RustConnection;

        pub struct Conn {
            conn: RustConnection,
            screen: usize,
        }

        impl Conn {
            pub fn new() -> anyhow::Result<Self> {
                let (conn, screen) = RustConnection::connect(None).context("X11 연결 실패")?;
                Ok(Self { conn, screen })
            }

            fn root(&self) -> u32 {
                self.conn.setup().roots[self.screen].root
            }

            pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
                let root = self.root();
                // RandR 1.5 GetMonitors 가 되면 물리 모니터별 좌표를 그대로 쓴다.
                if let Ok(cookie) = self.conn.randr_get_monitors(root, true) {
                    if let Ok(reply) = cookie.reply() {
                        if !reply.monitors.is_empty() {
                            let mut out = Vec::with_capacity(reply.monitors.len());
                            for (i, m) in reply.monitors.iter().enumerate() {
                                let name = self
                                    .conn
                                    .get_atom_name(m.name)
                                    .ok()
                                    .and_then(|c| c.reply().ok())
                                    .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                                    .unwrap_or_else(|| format!("monitor-{i}"));
                                out.push(MonitorInfo {
                                    index: i,
                                    name,
                                    x: m.x as i32,
                                    y: m.y as i32,
                                    width: m.width as u32,
                                    height: m.height as u32,
                                    primary: m.primary,
                                });
                            }
                            return Ok(out);
                        }
                    }
                }
                // RandR 이 없으면 화면 하나로 취급한다.
                let s = &self.conn.setup().roots[self.screen];
                Ok(vec![MonitorInfo {
                    index: 0,
                    name: format!("screen-{}", self.screen),
                    x: 0,
                    y: 0,
                    width: s.width_in_pixels as u32,
                    height: s.height_in_pixels as u32,
                    primary: true,
                }])
            }

            /// X11 은 스케일 개념이 없어 논리 좌표 = 물리 픽셀이다.
            pub fn logical_rects(&mut self) -> anyhow::Result<Vec<super::LogicalRect>> {
                Ok(self
                    .monitors()?
                    .into_iter()
                    .map(|m| super::LogicalRect { x: m.x, y: m.y, w: m.width, h: m.height })
                    .collect())
            }

            pub fn capture(&mut self, region: &Region) -> anyhow::Result<super::super::Frame> {
                let list = self.monitors()?;
                let m = pick(&list, region.monitor)?;
                let (rx, ry, rw, rh) = clamp_region(m, region)?;
                if rw == 0 || rh == 0 {
                    bail!("캡처 영역이 비어 있다");
                }
                if rw > u16::MAX as u32 || rh > u16::MAX as u32 {
                    bail!("캡처 영역 {rw}x{rh} 이 X11 한계(65535)를 넘는다");
                }
                let (ax, ay) = (m.x + rx as i32, m.y + ry as i32);
                let root = self.root();
                let reply = self
                    .conn
                    .get_image(ImageFormat::Z_PIXMAP, root, ax as i16, ay as i16, rw as u16, rh as u16, !0)
                    .context("GetImage 요청 실패")?
                    .reply()
                    .context("GetImage 응답 실패 (영역이 화면 밖이거나 서버가 거부)")?;

                let setup = self.conn.setup();
                zpixmap_to_rgba(setup, reply.depth, reply.visual, &reply.data, rw, rh)
            }
        }

        fn find_visual(setup: &Setup, visual: u32) -> Option<&Visualtype> {
            setup
                .roots
                .iter()
                .flat_map(|s| s.allowed_depths.iter())
                .flat_map(|d| d.visuals.iter())
                .find(|v| v.visual_id == visual)
        }

        /// `(shift, max)` — 마스크에서 채널을 꺼내 8비트로 펴는 데 쓴다.
        fn mask_parts(mask: u32) -> (u32, u32) {
            if mask == 0 {
                return (0, 1);
            }
            let shift = mask.trailing_zeros();
            (shift, (mask >> shift).max(1))
        }

        /// ZPixmap(BGRx/xRGB, depth 24 또는 32) → RGBA8.
        fn zpixmap_to_rgba(
            setup: &Setup,
            depth: u8,
            visual: u32,
            data: &[u8],
            width: u32,
            height: u32,
        ) -> anyhow::Result<super::super::Frame> {
            let fmt = setup
                .pixmap_formats
                .iter()
                .find(|f| f.depth == depth)
                .ok_or_else(|| anyhow!("depth {depth} 에 맞는 픽스맵 포맷이 서버에 없다"))?;
            let bpp = fmt.bits_per_pixel as usize;
            if bpp != 24 && bpp != 32 {
                bail!("지원하지 않는 픽셀 크기 {bpp}bpp (depth {depth}). 24/32bpp TrueColor 만 다룬다");
            }
            let pad = (fmt.scanline_pad as usize).max(8);
            let w = width as usize;
            let h = height as usize;
            // 스캔라인은 scanline_pad 비트 경계로 정렬된다.
            let stride = ((w * bpp).div_ceil(pad) * pad) / 8;
            if data.len() < stride * h {
                bail!("GetImage 데이터가 짧다: {} 바이트, 최소 {} 필요", data.len(), stride * h);
            }

            let vis = find_visual(setup, visual);
            let (rm, gm, bm) = match vis {
                Some(v) if v.red_mask | v.green_mask | v.blue_mask != 0 => (v.red_mask, v.green_mask, v.blue_mask),
                // 비주얼을 못 찾으면 가장 흔한 xRGB8888 로 가정한다.
                _ => (0x00ff_0000, 0x0000_ff00, 0x0000_00ff),
            };
            let (rs, rmax) = mask_parts(rm);
            let (gs, gmax) = mask_parts(gm);
            let (bs, bmax) = mask_parts(bm);
            let msb = setup.image_byte_order == ImageOrder::MSB_FIRST;
            let bytes = bpp / 8;

            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = &data[y * stride..y * stride + w * bytes];
                for x in 0..w {
                    let p = &row[x * bytes..x * bytes + bytes];
                    let v = if bytes == 4 {
                        let a = [p[0], p[1], p[2], p[3]];
                        if msb {
                            u32::from_be_bytes(a)
                        } else {
                            u32::from_le_bytes(a)
                        }
                    } else if msb {
                        (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32
                    } else {
                        p[0] as u32 | (p[1] as u32) << 8 | (p[2] as u32) << 16
                    };
                    let o = (y * w + x) * 4;
                    rgba[o] = (((v & rm) >> rs) * 255 / rmax) as u8;
                    rgba[o + 1] = (((v & gm) >> gs) * 255 / gmax) as u8;
                    rgba[o + 2] = (((v & bm) >> bs) * 255 / bmax) as u8;
                    rgba[o + 3] = 255;
                }
            }
            Ok(super::super::Frame { width, height, rgba })
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            /// depth 24 / 32bpp / LSB-first / xRGB8888 한 줄을 풀어 본다.
            #[test]
            fn zpixmap_bgrx_becomes_rgba() {
                // Setup 을 통째로 만들기는 번거로우니 마스크 계산만 직접 확인한다.
                let (rs, rmax) = mask_parts(0x00ff_0000);
                assert_eq!((rs, rmax), (16, 255));
                let (gs, gmax) = mask_parts(0x0000_ff00);
                assert_eq!((gs, gmax), (8, 255));
                let (bs, bmax) = mask_parts(0x0000_00ff);
                assert_eq!((bs, bmax), (0, 255));
                // BGRx 바이트 [B, G, R, x] → little endian u32 = 0x00RRGGBB
                let v = u32::from_le_bytes([0x10, 0x20, 0x30, 0x00]);
                assert_eq!(((v & 0x00ff_0000) >> rs) as u8, 0x30);
                assert_eq!(((v & 0x0000_ff00) >> gs) as u8, 0x20);
                assert_eq!(((v & 0x0000_00ff) >> bs) as u8, 0x10);
            }

            #[test]
            fn stride_is_padded_to_scanline_unit() {
                // 3픽셀 * 32bpp = 96비트, 32비트 정렬 → 12바이트
                assert_eq!(((3 * 32usize).div_ceil(32) * 32) / 8, 12);
                // 3픽셀 * 24bpp = 72비트, 32비트 정렬 → 96비트 = 12바이트
                assert_eq!(((3 * 24usize).div_ceil(32) * 32) / 8, 12);
            }
        }
    }
}

// ───────────────────────────── Windows / macOS ─────────────────────────────

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod imp {
    use super::{clamp_region, pick, Backend, Frame, MonitorInfo};
    use anyhow::{anyhow, Context};
    use nl_core::pipeline::Region;
    use xcap::Monitor;

    pub struct Imp {
        chosen: Option<Backend>,
    }

    impl Imp {
        pub fn new() -> anyhow::Result<Self> {
            // xcap 은 연결 객체를 들고 있지 않다. 목록 조회가 되는지만 확인한다.
            Monitor::all().map_err(|e| anyhow!("{e}")).context("모니터 목록 조회 실패")?;
            Ok(Self { chosen: None })
        }

        pub fn backend(&self) -> Option<Backend> {
            self.chosen
        }

        fn all() -> anyhow::Result<Vec<Monitor>> {
            Monitor::all().map_err(|e| anyhow!("{e}")).context("모니터 목록 조회 실패")
        }

        pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
            let mut out = Vec::new();
            for (i, m) in Self::all()?.into_iter().enumerate() {
                out.push(MonitorInfo {
                    index: i,
                    name: m.name().unwrap_or_else(|_| format!("monitor-{i}")),
                    x: m.x().map_err(|e| anyhow!("{e}"))?,
                    y: m.y().map_err(|e| anyhow!("{e}"))?,
                    width: m.width().map_err(|e| anyhow!("{e}"))?,
                    height: m.height().map_err(|e| anyhow!("{e}"))?,
                    primary: m.is_primary().unwrap_or(i == 0),
                });
            }
            Ok(out)
        }

        pub fn capture(&mut self, region: &Region) -> anyhow::Result<Frame> {
            let list = self.monitors()?;
            let m = pick(&list, region.monitor)?;
            let (rx, ry, rw, rh) = clamp_region(m, region)?;
            let mons = Self::all()?;
            let mon = mons
                .get(region.monitor)
                .ok_or_else(|| anyhow!("모니터 {} 번이 사라졌다", region.monitor))?;
            let img = if rx == 0 && ry == 0 && rw == m.width && rh == m.height {
                mon.capture_image().map_err(|e| anyhow!("{e}"))?
            } else {
                mon.capture_region(rx, ry, rw, rh).map_err(|e| anyhow!("{e}"))?
            };
            self.chosen = Some(Backend::XCap);
            let (width, height) = (img.width(), img.height());
            Ok(Frame { width, height, rgba: img.into_raw() })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_session() -> bool {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }

    #[test]
    fn clamp_full_monitor_when_width_zero() {
        let m = MonitorInfo { index: 0, name: "m".into(), x: 0, y: 0, width: 1920, height: 1080, primary: true };
        let r = Region { monitor: 0, x: 10, y: 10, width: 0, height: 0 };
        assert_eq!(clamp_region(&m, &r).unwrap(), (0, 0, 1920, 1080));
    }

    #[test]
    fn clamp_cuts_region_to_monitor() {
        let m = MonitorInfo { index: 0, name: "m".into(), x: 0, y: 0, width: 100, height: 100, primary: true };
        let r = Region { monitor: 0, x: 90, y: 90, width: 50, height: 50 };
        assert_eq!(clamp_region(&m, &r).unwrap(), (90, 90, 10, 10));
    }

    #[test]
    fn clamp_rejects_offscreen_origin() {
        let m = MonitorInfo { index: 0, name: "m".into(), x: 0, y: 0, width: 100, height: 100, primary: true };
        let r = Region { monitor: 0, x: 200, y: 0, width: 10, height: 10 };
        assert!(clamp_region(&m, &r).is_err());
    }

    #[test]
    fn pick_reports_missing_monitor() {
        let list: Vec<MonitorInfo> = vec![];
        assert!(pick(&list, 0).is_err());
    }

    /// 실제 세션이 있을 때만 캡처한다. 헤드리스 CI 에서는 조용히 통과.
    #[test]
    fn capture_primary_monitor_when_session_exists() {
        if !has_session() {
            eprintln!("DISPLAY/WAYLAND_DISPLAY 가 없어 화면 캡처 시험을 건너뛴다");
            return;
        }
        let mut cap = match Capturer::new() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("캡처기를 만들지 못했다(환경 문제로 보고 통과): {e:#}");
                return;
            }
        };
        let mons = match cap.monitors() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("모니터 목록 실패(환경 문제로 보고 통과): {e:#}");
                return;
            }
        };
        assert!(!mons.is_empty(), "세션이 있는데 모니터가 하나도 없다");
        for m in &mons {
            assert!(m.width > 0 && m.height > 0, "모니터 크기가 0 이다: {m:?}");
        }
        let region = Region { monitor: 0, x: 0, y: 0, width: 0, height: 0 };
        let f = match cap.capture(&region) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("캡처 실패(환경 문제로 보고 통과): {e:#}");
                return;
            }
        };
        assert_eq!(f.rgba.len(), f.width as usize * f.height as usize * 4, "RGBA 길이가 크기와 안 맞는다");
        assert_eq!(f.width, mons[0].width, "전체 캡처 너비가 모니터 너비와 다르다");
        assert_eq!(f.height, mons[0].height, "전체 캡처 높이가 모니터 높이와 다르다");
        let backend = cap.backend().expect("캡처에 성공했으면 백엔드가 정해져 있어야 한다");
        eprintln!("캡처 성공: {}x{} via {} — {}", f.width, f.height, backend.label(), backend.hint());

        // 부분 영역도 요청한 크기 그대로 나와야 한다.
        let part = Region { monitor: 0, x: 10, y: 20, width: 320, height: 240 };
        match cap.capture(&part) {
            Ok(p) => {
                assert_eq!((p.width, p.height), (320, 240), "부분 캡처 크기가 다르다");
                assert_eq!(p.rgba.len(), 320 * 240 * 4);
            }
            Err(e) => eprintln!("부분 캡처 실패: {e:#}"),
        }

        // 연결을 재사용해 연속 캡처가 되는지 + 실제 처리량이 어느 정도인지.
        let rounds = 3;
        let start = std::time::Instant::now();
        for i in 0..rounds {
            if let Err(e) = cap.capture(&region) {
                panic!("{i}번째 연속 캡처 실패(연결 재사용이 깨졌다): {e:#}");
            }
        }
        let per = start.elapsed() / rounds;
        eprintln!("연속 캡처 {rounds}장: 장당 {per:?} (약 {:.1} fps)", 1.0 / per.as_secs_f64());

        // 모니터가 여럿이면 두 번째도 찍어 본다 (좌표 계산 검증).
        if mons.len() > 1 {
            let r2 = Region { monitor: 1, x: 0, y: 0, width: 0, height: 0 };
            match cap.capture(&r2) {
                Ok(g) => {
                    assert_eq!((g.width, g.height), (mons[1].width, mons[1].height), "두 번째 모니터 크기가 다르다");
                    eprintln!("모니터 2 캡처 성공: {}x{}", g.width, g.height);
                }
                Err(e) => eprintln!("모니터 2 캡처 실패: {e:#}"),
            }
        }
    }
}
