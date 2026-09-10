//! 화면 캡처. Linux: X11(x11rb) → wlroots(wlr-screencopy, libwayshot) 순으로 시도. Windows/macOS: xcap.
//! GNOME/KDE Wayland 는 M1 에서 xdg-desktop-portal 로 붙인다.

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

pub fn monitors() -> anyhow::Result<Vec<MonitorInfo>> {
    anyhow::bail!("화면 캡처 미구현")
}

/// `region.width == 0` 이면 모니터 전체.
pub fn capture(_region: &Region) -> anyhow::Result<Frame> {
    anyhow::bail!("화면 캡처 미구현")
}
