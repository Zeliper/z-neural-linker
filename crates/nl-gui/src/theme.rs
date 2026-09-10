//! 다크 테마 (trust-pms `dark_visuals` 계승).

use egui::{Color32, Visuals};

pub fn dark_visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.panel_fill = Color32::from_rgb(30, 32, 36);
    v.window_fill = Color32::from_rgb(36, 38, 43);
    v.extreme_bg_color = Color32::from_rgb(22, 23, 26);
    v.selection.bg_fill = Color32::from_rgb(52, 101, 164);
    v.widgets.noninteractive.bg_fill = Color32::from_rgb(40, 42, 47);
    v
}
