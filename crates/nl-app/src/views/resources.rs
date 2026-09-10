//! 자원 뷰: `nl_io::snapshot()` + `nl_engine::enumerate()` 를 1초마다 갱신해 CPU/메모리/GPU 를 보여 준다.

use super::{device_label, fmt_bytes, ViewAction, ViewCtx, COL_OK, COL_SELECT, COL_SURFACE, COL_WARN, COL_WEAK};
use eframe::egui::{self, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoints};
use nl_io::ResourceSnapshot;

/// 갱신 주기(초).
pub const POLL_SECS: f64 = 1.0;
/// 그래프에 남기는 점 개수 (= 최근 2분).
pub const HISTORY: usize = 120;

pub struct ResourceState {
    pub snapshot: ResourceSnapshot,
    /// 마지막 갱신 시각(앱 시계). 음수면 아직 한 번도 안 읽었다.
    pub last_poll: f64,
    /// (초, %) 사용률 이력.
    pub cpu: Vec<[f64; 2]>,
    pub mem: Vec<[f64; 2]>,
}

impl Default for ResourceState {
    fn default() -> Self {
        Self { snapshot: ResourceSnapshot::default(), last_poll: f64::NEG_INFINITY, cpu: Vec::new(), mem: Vec::new() }
    }
}

impl ResourceState {
    /// 주기가 됐으면 새 스냅샷을 읽는다. 읽었으면 `true`.
    pub fn tick(&mut self, now: f64) -> bool {
        if now - self.last_poll < POLL_SECS {
            return false;
        }
        self.last_poll = now;
        self.snapshot = nl_io::snapshot();
        push_capped(&mut self.cpu, [now, self.snapshot.cpu_usage_percent as f64], HISTORY);
        push_capped(&mut self.mem, [now, mem_percent(&self.snapshot)], HISTORY);
        true
    }
}

/// 메모리 사용률(%). 전체가 0 이면 0.
pub fn mem_percent(s: &ResourceSnapshot) -> f64 {
    if s.mem_total_bytes == 0 {
        0.0
    } else {
        s.mem_used_bytes as f64 * 100.0 / s.mem_total_bytes as f64
    }
}

fn push_capped(v: &mut Vec<[f64; 2]>, point: [f64; 2], cap: usize) {
    v.push(point);
    if v.len() > cap {
        let cut = v.len() - cap;
        v.drain(..cut);
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut ResourceState) -> Vec<ViewAction> {
    if state.tick(ctx.now) {
        // 다음 갱신 때 스스로 깨어난다 — 사용자가 마우스를 안 움직여도 표가 갱신된다.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(POLL_SECS));
    }
    let s = &state.snapshot;

    egui::ScrollArea::vertical().id_salt("resources-scroll").show(ui, |ui| {
        egui::Frame::NONE.fill(COL_SURFACE).inner_margin(10).corner_radius(5).show(ui, |ui| {
            ui.label(RichText::new("시스템").size(15.0).strong());
            ui.separator();
            egui::Grid::new("res-sys").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("CPU").color(COL_WEAK));
                ui.label(if s.cpu_name.is_empty() {
                    format!("{}코어", s.cpu_cores)
                } else {
                    format!("{} · {}코어", s.cpu_name, s.cpu_cores)
                });
                ui.end_row();
                ui.label(RichText::new("CPU 사용률").color(COL_WEAK));
                ui.add(
                    egui::ProgressBar::new((s.cpu_usage_percent / 100.0).clamp(0.0, 1.0))
                        .desired_width(220.0)
                        .text(format!("{:.0}%", s.cpu_usage_percent)),
                );
                ui.end_row();
                ui.label(RichText::new("메모리").color(COL_WEAK));
                ui.add(
                    egui::ProgressBar::new((mem_percent(s) / 100.0).clamp(0.0, 1.0) as f32)
                        .desired_width(220.0)
                        .text(format!("{} / {}", fmt_bytes(s.mem_used_bytes), fmt_bytes(s.mem_total_bytes))),
                );
                ui.end_row();
            });
        });

        ui.add_space(8.0);
        egui::Frame::NONE.fill(COL_SURFACE).inner_margin(10).corner_radius(5).show(ui, |ui| {
            ui.label(RichText::new("사용률 (최근 2분)").size(13.0).strong());
            // x 축은 "몇 초 전" — 가장 최근 점이 0 이다.
            let ago = |v: &[[f64; 2]]| -> Vec<[f64; 2]> { v.iter().map(|p| [p[0] - ctx.now, p[1]]).collect() };
            let cpu = ago(&state.cpu);
            let mem = ago(&state.mem);
            Plot::new("res-plot")
                .height(170.0)
                .legend(Legend::default())
                .include_y(0.0)
                .include_y(100.0)
                .include_x(-(HISTORY as f64) * POLL_SECS)
                .include_x(0.0)
                .y_axis_label("%")
                .x_axis_label("초 전")
                .show(ui, |p| {
                    if !cpu.is_empty() {
                        p.line(Line::new("CPU", PlotPoints::from(cpu)).width(2.0).color(COL_SELECT));
                    }
                    if !mem.is_empty() {
                        p.line(Line::new("메모리", PlotPoints::from(mem)).width(2.0).color(COL_OK));
                    }
                });
        });

        ui.add_space(8.0);
        egui::Frame::NONE.fill(COL_SURFACE).inner_margin(10).corner_radius(5).show(ui, |ui| {
            ui.label(RichText::new("학습 장치").size(15.0).strong());
            ui.separator();
            if ctx.devices.is_empty() {
                ui.label(RichText::new("장치를 찾지 못했습니다").color(COL_WARN));
                return;
            }
            egui::Grid::new("res-dev").striped(true).num_columns(4).spacing([14.0, 4.0]).show(ui, |ui| {
                for h in ["장치", "백엔드", "종류", "메모리"] {
                    ui.label(RichText::new(h).color(COL_WEAK).size(11.0));
                }
                ui.end_row();
                for d in ctx.devices {
                    ui.label(&d.name).on_hover_text(device_label(d));
                    ui.label(&d.backend);
                    ui.label(kind_label(d.kind));
                    ui.label(d.vram_bytes.map(fmt_bytes).unwrap_or_else(|| "-".into()));
                    ui.end_row();
                }
            });
            // nl-io 가 보고한 GPU 와 엔진 열거가 어긋나면 알린다 (드라이버 문제 진단용).
            if s.gpus.len() > ctx.devices.iter().filter(|d| d.kind != nl_engine::DeviceKind::Cpu).count() {
                ui.label(RichText::new("일부 GPU 가 학습 장치로 열거되지 않았습니다").color(COL_WARN).size(11.5));
            }
        });
    });
    Vec::new()
}

pub fn kind_label(k: nl_engine::DeviceKind) -> &'static str {
    use nl_engine::DeviceKind as K;
    match k {
        K::Cpu => "CPU",
        K::DiscreteGpu => "외장 GPU",
        K::IntegratedGpu => "내장 GPU",
        K::OtherGpu => "기타 GPU",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_percent_handles_an_empty_snapshot() {
        let mut s = ResourceSnapshot::default();
        assert_eq!(mem_percent(&s), 0.0);
        s.mem_total_bytes = 1000;
        s.mem_used_bytes = 250;
        assert!((mem_percent(&s) - 25.0).abs() < 1e-9);
    }

    #[test]
    fn history_is_capped_and_keeps_the_newest() {
        let mut v = Vec::new();
        for i in 0..(HISTORY + 10) {
            push_capped(&mut v, [i as f64, 0.0], HISTORY);
        }
        assert_eq!(v.len(), HISTORY);
        assert_eq!(v[0][0], 10.0);
        assert_eq!(v.last().unwrap()[0], (HISTORY + 9) as f64);
    }

    #[test]
    fn tick_respects_the_poll_interval() {
        let mut s = ResourceState::default();
        assert!(s.tick(100.0), "첫 호출은 항상 읽는다");
        assert!(!s.tick(100.5), "주기 안에서는 읽지 않는다");
        assert!(s.tick(101.5));
        assert_eq!(s.cpu.len(), 2);
    }
}
