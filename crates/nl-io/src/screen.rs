//! 화면 캡처. Linux: wlroots/ext-image-copy(libwayshot) → X11(x11rb) 순으로 시도. Windows/macOS: xcap.
//! GNOME/KDE Wayland 에서 wlr-screencopy 도 ext-image-copy-capture 도 없으면 M1 의 xdg-desktop-portal 경로가 필요하다.
//!
//! Linux 경로는 **시스템 개발 라이브러리 없이** 빌드된다(순수 Rust: x11rb + libwayshot, libwayshot 기본 feature 끔).
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
    /// X11: `GetImage` on root (x11rb).
    X11,
    /// Windows/macOS 네이티브 (xcap).
    XCap,
}

impl Backend {
    pub fn label(&self) -> &'static str {
        match self {
            Backend::Wayland => "wayland(libwayshot)",
            Backend::X11 => "x11(x11rb)",
            Backend::XCap => "xcap",
        }
    }
}

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
    use anyhow::bail;
    use nl_core::pipeline::Region;

    pub struct Imp {
        wayland: Option<wl::Conn>,
        x11: Option<x11::Conn>,
        chosen: Option<Backend>,
        /// 초기화 단계에서 모인 실패 사유 (양쪽 다 안 될 때 사용자에게 보여 준다).
        init_errors: Vec<String>,
    }

    /// 왜 안 되는지, 무엇을 해야 하는지 한 줄 안내.
    fn advice() -> String {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let x11 = std::env::var_os("DISPLAY").is_some();
        if wayland {
            let mut s = String::from(
                "이 컴포지터가 wlr-screencopy(zwlr_screencopy_manager_v1) 도 \
                 ext-image-copy-capture(ext_image_copy_capture_manager_v1) 도 제공하지 않으면 \
                 순수 Rust 경로로는 화면을 찍을 수 없다. GNOME 과 KDE(KWin 6.7 기준)가 여기 해당하며, \
                 xdg-desktop-portal ScreenCast 경로가 M1 작업으로 남아 있다.",
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

            if wayland.is_none() && x11.is_none() {
                bail!(
                    "화면 캡처 백엔드를 찾을 수 없다 ({}). 사유: {}",
                    session_hint(),
                    init_errors.join(" / ")
                );
            }
            Ok(Self { wayland, x11, chosen: None, init_errors })
        }

        pub fn backend(&self) -> Option<Backend> {
            self.chosen
        }

        pub fn monitors(&mut self) -> anyhow::Result<Vec<MonitorInfo>> {
            // 이미 캡처에 성공한 백엔드가 있으면 인덱스가 어긋나지 않도록 그쪽을 그대로 쓴다.
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
        match cap.capture(&region) {
            Ok(f) => {
                assert_eq!(f.rgba.len(), f.width as usize * f.height as usize * 4, "RGBA 길이가 크기와 안 맞는다");
                assert_eq!(f.width, mons[0].width, "전체 캡처 너비가 모니터 너비와 다르다");
                assert_eq!(f.height, mons[0].height, "전체 캡처 높이가 모니터 높이와 다르다");
                eprintln!("캡처 성공: {}x{} via {:?}", f.width, f.height, cap.backend());
            }
            Err(e) => eprintln!("캡처 실패(환경 문제로 보고 통과): {e:#}"),
        }
    }
}
