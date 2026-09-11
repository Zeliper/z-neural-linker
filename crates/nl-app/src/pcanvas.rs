//! 파이프라인 노드 캔버스. 레이어 캔버스(`canvas.rs`)의 카메라·기하·베지어를 그대로 쓰고
//! 노드 종류와 연결 규칙만 다르다. 링크는 포트가 아니라 **노드 → 노드** 이고,
//! 허용 규칙은 `nl_core::Pipeline::add_link` 와 정확히 같아야 한다 (캔버스가 core 가 거부할 링크를 만들면 안 된다).

use crate::canvas::{
    bezier_near, control_points, input_port_pos, node_rect, output_port_pos, Camera, Selection, SelectionState, NODE_H,
    NODE_W, PORT_R,
};
use eframe::egui::{
    self, epaint::CubicBezierShape, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape as EShape, Stroke,
    StrokeKind, Vec2,
};
use nl_core::ids::*;
use nl_core::pipeline::{Logic, PNode, PNodeKind, Pipeline, Sink, Source};
use std::collections::{BTreeMap, BTreeSet};

/// 포트 클릭 판정 반지름(월드). 레이어 캔버스와 같은 값.
const PORT_HIT: f32 = 11.0;
/// 팔레트로 만들 때 노드 가로 간격.
const NODE_GAP: f32 = 250.0;
const ZOOM_MIN: f32 = 0.2;
const ZOOM_MAX: f32 = 2.5;
const FIT_MARGIN: f32 = 70.0;
/// 새 `Source::HttpServer` 의 기본 주소·경로. 바깥에서 함부로 닿지 못하게 루프백에 연다.
pub const DEFAULT_HTTP_BIND: &str = "127.0.0.1:8787";
/// 기본 요청 경로.
pub const DEFAULT_HTTP_PATH: &str = "/infer";

const COL_BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);
const COL_GRID: Color32 = Color32::from_rgb(0x21, 0x24, 0x2a);
const COL_GRID_STRONG: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x36);
const COL_TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xec);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0xa1, 0xad);
const COL_LINK: Color32 = Color32::from_rgba_premultiplied(0x70, 0x78, 0x86, 0xc0);
const COL_SELECT: Color32 = Color32::from_rgb(0x4f, 0x9e, 0xea);
const COL_ERROR: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
const COL_OK: Color32 = Color32::from_rgb(0x57, 0xab, 0x5a);
const COL_GHOST: Color32 = Color32::from_rgba_premultiplied(0x2c, 0x4a, 0x6b, 0x66);
const COL_LABEL_BG: Color32 = Color32::from_rgba_premultiplied(0x14, 0x16, 0x1a, 0xd8);

/// 노드 종류별 색 (소스 초록 · 모델 파랑 · 로직 주황 · 싱크 보라).
pub fn kind_color(kind: &PNodeKind) -> Color32 {
    match kind {
        PNodeKind::Source { .. } => Color32::from_rgb(0x4c, 0xaf, 0x50),
        PNodeKind::Model { .. } => Color32::from_rgb(0x42, 0x85, 0xf4),
        PNodeKind::Logic { .. } => Color32::from_rgb(0xff, 0x98, 0x00),
        PNodeKind::Sink { .. } => Color32::from_rgb(0x9c, 0x27, 0xb0),
    }
}

/// 노드 부제: 종류별 핵심 파라미터 한 줄.
pub fn kind_summary(kind: &PNodeKind, project: &nl_core::Project) -> String {
    match kind {
        PNodeKind::Source { source } => match source {
            Source::ScreenCapture { region, fps } => {
                if region.width == 0 {
                    format!("모니터 {} 전체 · {fps:.0}fps", region.monitor)
                } else {
                    format!("{}×{} @{},{} · {fps:.0}fps", region.width, region.height, region.x, region.y)
                }
            }
            Source::HttpPoll { url, interval_ms, .. } => format!("{} · {interval_ms}ms", short_url(url)),
            Source::WebSocket { url } => short_url(url),
            Source::StdinJson => "표준 입력".into(),
            Source::File { path, interval_ms } => format!("{} · {interval_ms}ms", crate::views::short_path(path)),
            Source::Timer { interval_ms } => format!("{interval_ms}ms 마다"),
            Source::GuiEvent { widget } => format!("위젯 {}", widget.short()),
            Source::Manual => "인스펙터에서 값 보내기".into(),
            Source::HttpServer { bind, path } => format!("{bind}{path}"),
        },
        PNodeKind::Model { model, payload } => {
            let name = project.models.get(model).map(|m| m.name.clone()).unwrap_or_else(|| "(없는 모델)".into());
            match payload.and_then(|p| project.payloads.get(&p)) {
                Some(p) => format!("{name} · {}", p.name),
                None => name,
            }
        }
        PNodeKind::Logic { logic } => match logic {
            Logic::Threshold { value } => format!("≥ {value}"),
            Logic::Debounce { ms } => format!("{ms}ms"),
            Logic::Select { index } => format!("[{index}]"),
            Logic::Map { table } => format!("{}개 대응", table.len()),
            Logic::Majority { window } => format!("최근 {window}개"),
        },
        PNodeKind::Sink { sink } => match sink {
            Sink::MouseKeyboard { actions, cooldown_ms } => format!("액션 {}개 · {cooldown_ms}ms", actions.len()),
            Sink::HttpCall { method, url, .. } => format!("{method} {}", short_url(url)),
            Sink::WebSocketSend { url } => short_url(url),
            Sink::StdoutJson => "표준 출력".into(),
            Sink::GuiWidget { widget } => format!("위젯 {}", widget.short()),
            Sink::File { path, append } => {
                format!("{}{}", crate::views::short_path(path), if *append { " (덧붙임)" } else { "" })
            }
            Sink::Log => "로그".into(),
            Sink::HttpReply { server } => format!("← 서버 {}", server.short()),
        },
    }
}

fn short_url(url: &str) -> String {
    let s = url.trim_start_matches("https://").trim_start_matches("http://");
    if s.len() > 30 {
        format!("{}…", &s[..29])
    } else {
        s.to_string()
    }
}

// ── 앱에 돌려주는 편집 의도 ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum PipelineAction {
    AddNode { kind: PNodeKind, pos: [f32; 2] },
    MoveNodes(Vec<(PNodeId, [f32; 2])>),
    Link { from: PNodeId, to: PNodeId },
    DeleteNodes(Vec<PNodeId>),
    DeleteLinks(Vec<LinkId>),
    DuplicateNodes(Vec<PNodeId>),
    DisconnectNode(PNodeId),
}

/// 실행 중 노드 옆에 붙이는 정보. 실행이 없으면 빈 맵을 넘긴다.
#[derive(Default)]
pub struct LiveView {
    /// 노드가 마지막으로 낸 값 (`nl_gui::format_value` 로 만든 문자열).
    pub values: BTreeMap<PNodeId, String>,
    /// 노드의 마지막 오류.
    pub errors: BTreeMap<PNodeId, String>,
    pub running: bool,
}

// ── 내부 상태 ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Body,
    Output,
    Input,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NodeDrag {
    id: PNodeId,
    delta: Vec2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LinkDrag {
    from: PNodeId,
    /// 입력에서 떼어 온 기존 링크.
    detach: Option<LinkId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BoxSelect {
    origin: Pos2,
    current: Pos2,
    additive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CtxTarget {
    Canvas { world: Pos2 },
    Link(LinkId),
}

pub struct PipelineCanvas {
    pub camera: Camera,
    pub visible_nodes: usize,
    pub total_nodes: usize,
    fit_requested: bool,
    initialized: bool,
    current: Option<PipelineId>,
    last_viewport: Rect,
    node_drag: Option<NodeDrag>,
    link_drag: Option<LinkDrag>,
    box_select: Option<BoxSelect>,
    ctx_target: Option<CtxTarget>,
    swallow_drag: bool,
}

impl Default for PipelineCanvas {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineCanvas {
    pub fn new() -> Self {
        Self {
            camera: Camera::default(),
            visible_nodes: 0,
            total_nodes: 0,
            fit_requested: false,
            initialized: false,
            current: None,
            last_viewport: Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0)),
            node_drag: None,
            link_drag: None,
            box_select: None,
            ctx_target: None,
            swallow_drag: false,
        }
    }

    pub fn request_fit(&mut self) {
        self.fit_requested = true;
    }

    pub fn interaction_active(&self) -> bool {
        self.node_drag.is_some() || self.link_drag.is_some() || self.box_select.is_some()
    }

    pub fn cancel_interaction(&mut self) {
        if self.interaction_active() {
            self.swallow_drag = true;
        }
        self.node_drag = None;
        self.link_drag = None;
        self.box_select = None;
    }

    fn fit(&mut self, pipeline: &Pipeline, viewport: Rect) {
        let Some(b) = content_bounds(pipeline) else {
            self.camera = Camera::default();
            return;
        };
        let b = b.expand(20.0);
        let avail = Vec2::new((viewport.width() - FIT_MARGIN).max(80.0), (viewport.height() - FIT_MARGIN).max(80.0));
        let zoom = (avail.x / b.width().max(1.0)).min(avail.y / b.height().max(1.0)).clamp(ZOOM_MIN, ZOOM_MAX);
        self.camera.zoom = zoom;
        self.camera.pan = b.center().to_vec2() - viewport.size() / (2.0 * zoom);
    }

    /// 캔버스 한 프레임.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        pid: PipelineId,
        pipeline: &Pipeline,
        project: &nl_core::Project,
        live: &LiveView,
        sel: &mut SelectionState,
    ) -> Vec<PipelineAction> {
        let mut actions: Vec<PipelineAction> = Vec::new();

        if self.current != Some(pid) {
            self.current = Some(pid);
            self.node_drag = None;
            self.link_drag = None;
            self.box_select = None;
            self.initialized = false;
        }
        if self.node_drag.map(|d| !pipeline.nodes.contains_key(&d.id)).unwrap_or(false) {
            self.node_drag = None;
        }
        if self.link_drag.map(|d| !pipeline.nodes.contains_key(&d.from)).unwrap_or(false) {
            self.link_drag = None;
        }
        sel.prune_pipeline(pipeline);

        let viewport = ui.available_rect_before_wrap();
        let prev = self.last_viewport;
        self.last_viewport = viewport;
        if !self.initialized {
            self.initialized = true;
            self.fit(pipeline, viewport);
        } else if (viewport.width() - prev.width()).abs() > 0.5 || (viewport.height() - prev.height()).abs() > 0.5 {
            let center = self.camera.to_world(prev, prev.center());
            self.camera.pan = center.to_vec2() - viewport.size() / (2.0 * self.camera.zoom);
            ui.ctx().request_repaint();
        }
        if self.fit_requested {
            self.fit_requested = false;
            self.fit(pipeline, viewport);
        }

        let bg = ui.interact(viewport, ui.id().with("pipe-bg"), Sense::click_and_drag());
        let mods = ui.input(|i| i.modifiers);
        let (pointer, press_origin, released) = ui.input(|i| {
            (i.pointer.interact_pos(), i.pointer.press_origin(), i.pointer.button_released(egui::PointerButton::Primary))
        });
        if self.swallow_drag && !ui.input(|i| i.pointer.any_down()) {
            self.swallow_drag = false;
        }

        if bg.drag_started_by(egui::PointerButton::Primary) && mods.alt {
            let origin = press_origin.or(pointer).unwrap_or(viewport.center());
            self.box_select = Some(BoxSelect { origin, current: origin, additive: mods.shift || mods.command });
        }
        let panning = bg.dragged_by(egui::PointerButton::Primary) || bg.dragged_by(egui::PointerButton::Middle);
        if panning && self.box_select.is_none() && !self.swallow_drag {
            self.camera.pan -= bg.drag_delta() / self.camera.zoom;
        }
        if bg.contains_pointer() {
            let (zoom_delta, scroll, hover) =
                ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
            if zoom_delta != 1.0 {
                zoom_at(&mut self.camera, viewport, hover.unwrap_or_else(|| viewport.center()), zoom_delta);
            }
            if scroll != Vec2::ZERO {
                self.camera.pan -= scroll / self.camera.zoom;
            }
        }
        let zoom = self.camera.zoom;
        let painter = ui.painter_at(viewport);
        painter.rect_filled(viewport, CornerRadius::ZERO, COL_BG);
        draw_grid(&painter, &self.camera, viewport);

        let world_view = Rect::from_min_max(
            self.camera.to_world(viewport, viewport.min),
            self.camera.to_world(viewport, viewport.max),
        );
        let node_drag = self.node_drag;
        let dragging: BTreeSet<PNodeId> = sel.pnode_list().into_iter().collect();
        let offset = |id: PNodeId| -> Vec2 {
            match node_drag {
                Some(d) if dragging.contains(&d.id) && dragging.contains(&id) => d.delta,
                Some(d) if d.id == id => d.delta,
                _ => Vec2::ZERO,
            }
        };
        let world_rect = |id: PNodeId| -> Option<Rect> {
            let n = pipeline.nodes.get(&id)?;
            Some(node_rect(n.pos).translate(offset(id)))
        };

        let mut visible: Vec<(PNodeId, Rect)> = Vec::new();
        for &id in pipeline.nodes.keys() {
            if let Some(wr) = world_rect(id) {
                if wr.intersects(world_view.expand(120.0)) {
                    visible.push((id, screen_rect(&self.camera, viewport, wr)));
                }
            }
        }
        self.visible_nodes = visible.len();
        self.total_nodes = pipeline.nodes.len();

        // ── 링크 ────────────────────────────────────────────────
        let detached = self.link_drag.and_then(|d| d.detach);
        let mut hovered_link: Option<LinkId> = None;
        for (&lid, link) in &pipeline.links {
            if Some(lid) == detached {
                continue;
            }
            let (Some(fw), Some(tw)) = (world_rect(link.from), world_rect(link.to)) else { continue };
            let p0 = self.camera.to_screen(viewport, output_port_pos(fw));
            let p3 = self.camera.to_screen(viewport, input_port_pos(tw, 0, 1));
            let (p1, p2) = control_points(p0, p3);
            let selected = sel.primary == Selection::Link(pid, lid);
            let color = if selected { COL_SELECT } else { COL_LINK };
            let stroke = Stroke::new(if selected { 2.6 } else { 1.8 } * zoom.clamp(0.6, 1.4), color);
            painter.add(CubicBezierShape::from_points_stroke([p0, p1, p2, p3], false, Color32::TRANSPARENT, stroke));
            let dir = (p3 - p2).normalized();
            let orth = Vec2::new(-dir.y, dir.x);
            let s = 5.0 * zoom.clamp(0.6, 1.4);
            painter.add(EShape::convex_polygon(
                vec![p3, p3 - dir * s * 2.0 + orth * s, p3 - dir * s * 2.0 - orth * s],
                color,
                Stroke::NONE,
            ));
            if let Some(pp) = pointer {
                if hovered_link.is_none()
                    && Rect::from_two_pos(p0, p3).union(Rect::from_two_pos(p1, p2)).expand(14.0).contains(pp)
                    && bezier_near(p0, p1, p2, p3, pp, 7.0)
                {
                    hovered_link = Some(lid);
                }
            }
        }

        // ── 노드 ────────────────────────────────────────────────
        let mut drop_target: Option<PNodeId> = None;
        let mut cursor: Option<egui::CursorIcon> = None;
        let idle = !self.interaction_active();
        for &(id, sr) in &visible {
            let Some(node) = pipeline.nodes.get(&id) else { continue };
            let port_hit = (PORT_HIT * zoom).clamp(7.0, 14.0);
            let resp = ui.interact(sr.expand(port_hit), ui.id().with(("pnode", id)), Sense::click_and_drag());
            let hovered = resp.hovered();
            let has_in = !node.kind.is_source();
            let has_out = !node.kind.is_sink();
            let zone_at = |p: Pos2| zone_of(sr, has_in, has_out, p, port_hit);

            if resp.clicked() {
                if mods.shift || mods.command {
                    sel.toggle_pnode(pid, id);
                } else {
                    sel.set(Selection::PNode(pid, id));
                }
            }
            if resp.secondary_clicked() && !sel.is_pnode_selected(id) {
                sel.set(Selection::PNode(pid, id));
            }
            if resp.drag_started_by(egui::PointerButton::Primary) {
                let origin = press_origin.or_else(|| resp.interact_pointer_pos()).unwrap_or_else(|| sr.center());
                match zone_at(origin) {
                    Some(Zone::Output) if has_out => self.link_drag = Some(LinkDrag { from: id, detach: None }),
                    Some(Zone::Input) if has_in => {
                        // 입력 포트에서 끌면 그 노드로 들어오는 링크 하나를 떼어 재연결한다.
                        match pipeline.links.values().find(|l| l.to == id) {
                            Some(l) => self.link_drag = Some(LinkDrag { from: l.from, detach: Some(l.id) }),
                            None => self.node_drag = Some(NodeDrag { id, delta: Vec2::ZERO }),
                        }
                    }
                    _ => {
                        if !sel.is_pnode_selected(id) {
                            sel.set(Selection::PNode(pid, id));
                        }
                        self.node_drag = Some(NodeDrag { id, delta: Vec2::ZERO });
                    }
                }
            }
            if resp.dragged_by(egui::PointerButton::Primary) {
                if let Some(d) = self.node_drag.as_mut() {
                    if d.id == id {
                        d.delta += resp.drag_delta() / zoom;
                    }
                }
            }
            if let Some(l) = self.link_drag {
                if l.from != id && has_in {
                    if let Some(pp) = pointer {
                        if sr.expand(port_hit).contains(pp) {
                            drop_target = Some(id);
                        }
                    }
                }
            }
            let zone = (hovered && idle).then(|| pointer.and_then(zone_at)).flatten();
            if let Some(z) = zone {
                cursor = Some(match z {
                    Zone::Body => egui::CursorIcon::Grab,
                    _ => egui::CursorIcon::Crosshair,
                });
            }

            let err = live.errors.get(&id);
            draw_node(
                &painter,
                node,
                sr,
                NodeStyle {
                    selected: sel.is_pnode_selected(id),
                    hovered,
                    error: err.is_some(),
                    zone,
                    has_in,
                    has_out,
                    connected_in: pipeline.links.values().any(|l| l.to == id),
                },
                zoom,
                project,
                live.values.get(&id).map(String::as_str),
            );
            let resp = match err {
                Some(e) if hovered => resp.on_hover_text(format!("{}\n{e}", node_title(node))),
                _ => resp,
            };
            let group: Vec<PNodeId> =
                if sel.count() > 1 && sel.is_pnode_selected(id) { sel.pnode_list() } else { vec![id] };
            resp.context_menu(|ui| node_menu(ui, id, &group, &mut actions));
        }

        // ── 러버밴드 ────────────────────────────────────────────
        if let Some(mut b) = self.box_select {
            if let Some(pp) = pointer {
                b.current = pp;
            }
            let r = Rect::from_two_pos(b.origin, b.current);
            painter.rect_filled(r, CornerRadius::ZERO, COL_GHOST);
            painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, COL_SELECT), StrokeKind::Outside);
            self.box_select = Some(b);
            if released {
                self.box_select = None;
                let picked: Vec<PNodeId> =
                    visible.iter().filter(|(_, sr)| sr.intersects(r)).map(|(id, _)| *id).collect();
                if !picked.is_empty() || !b.additive {
                    sel.select_pnodes(pid, picked, b.additive);
                }
            }
        }

        // ── 연결 고스트 ─────────────────────────────────────────
        if let Some(l) = self.link_drag {
            if let (Some(pp), Some(fw)) = (pointer, world_rect(l.from)) {
                let p0 = self.camera.to_screen(viewport, output_port_pos(fw));
                let (target_pos, why) = match drop_target {
                    Some(to) => {
                        let tw = world_rect(to).unwrap_or(fw);
                        (self.camera.to_screen(viewport, input_port_pos(tw, 0, 1)), link_check(pipeline, l.from, to, l.detach))
                    }
                    None => (pp, None),
                };
                let color = match (drop_target.is_some(), why.is_some()) {
                    (true, false) => COL_OK,
                    (true, true) => COL_ERROR,
                    _ => COL_SELECT,
                };
                let (p1, p2) = control_points(p0, target_pos);
                painter.add(CubicBezierShape::from_points_stroke(
                    [p0, p1, p2, target_pos],
                    false,
                    Color32::TRANSPARENT,
                    Stroke::new(2.2, color),
                ));
                painter.circle_filled(target_pos, 4.5, color);
                if let Some(msg) = &why {
                    pill_right(&painter, Pos2::new(target_pos.x - 12.0, target_pos.y - 6.0), msg, COL_ERROR);
                }
            }
            if released {
                let l = self.link_drag.take().unwrap();
                match drop_target {
                    Some(to) if link_check(pipeline, l.from, to, l.detach).is_none() => {
                        if let Some(old) = l.detach {
                            actions.push(PipelineAction::DeleteLinks(vec![old]));
                        }
                        actions.push(PipelineAction::Link { from: l.from, to });
                    }
                    Some(_) => {}
                    None => {
                        if let Some(old) = l.detach {
                            actions.push(PipelineAction::DeleteLinks(vec![old]));
                        }
                    }
                }
            }
        }

        // ── 이동 확정 ───────────────────────────────────────────
        if let Some(d) = self.node_drag {
            if released {
                self.node_drag = None;
                if d.delta.length() > 0.5 {
                    let group: Vec<PNodeId> = if sel.is_pnode_selected(d.id) { sel.pnode_list() } else { vec![d.id] };
                    let items: Vec<(PNodeId, [f32; 2])> = group
                        .iter()
                        .filter_map(|&gid| {
                            let n = pipeline.nodes.get(&gid)?;
                            Some((gid, [n.pos[0] + d.delta.x, n.pos[1] + d.delta.y]))
                        })
                        .collect();
                    if !items.is_empty() {
                        actions.push(PipelineAction::MoveNodes(items));
                    }
                }
            }
        }

        if bg.clicked() && !(mods.shift || mods.command) {
            match hovered_link {
                Some(l) => sel.set(Selection::Link(pid, l)),
                None => sel.set(Selection::Pipeline(pid)),
            }
        }
        if bg.secondary_clicked() {
            self.ctx_target = Some(match hovered_link {
                Some(l) => CtxTarget::Link(l),
                None => CtxTarget::Canvas {
                    world: pointer.map(|pp| self.camera.to_world(viewport, pp)).unwrap_or(Pos2::ZERO),
                },
            });
        }
        if self.node_drag.is_some() {
            cursor = Some(egui::CursorIcon::Grabbing);
        } else if self.link_drag.is_some() || self.box_select.is_some() {
            cursor = Some(egui::CursorIcon::Crosshair);
        } else if cursor.is_none() && hovered_link.is_some() {
            cursor = Some(egui::CursorIcon::PointingHand);
        }
        if let Some(c) = cursor {
            ui.ctx().set_cursor_icon(c);
        }

        let target = self.ctx_target;
        bg.context_menu(|ui| {
            ui.set_min_width(200.0);
            match target {
                Some(CtxTarget::Link(lid)) => {
                    if ui.add(egui::Button::new("🗑 연결 삭제").shortcut_text("Del")).clicked() {
                        actions.push(PipelineAction::DeleteLinks(vec![lid]));
                        ui.close();
                    }
                }
                Some(CtxTarget::Canvas { world }) => palette_menu(ui, world, project, &mut actions),
                None => ui.close(),
            }
        });

        if pipeline.nodes.is_empty() {
            painter.text(
                viewport.center(),
                Align2::CENTER_CENTER,
                "빈 곳을 우클릭해 소스·모델·싱크를 추가하세요",
                FontId::proportional(15.0),
                COL_TEXT_DIM,
            );
        }
        if self.interaction_active() || live.running {
            ui.ctx().request_repaint();
        }
        actions
    }
}

// ── 순수 함수 (테스트 대상) ─────────────────────────────────────────

pub fn content_bounds(pipeline: &Pipeline) -> Option<Rect> {
    let mut it = pipeline.nodes.values().map(|n| node_rect(n.pos));
    let first = it.next()?;
    Some(it.fold(first, |acc, r| acc.union(r)))
}

fn zone_of(rect: Rect, has_in: bool, has_out: bool, p: Pos2, hit: f32) -> Option<Zone> {
    if has_out && (p - output_port_pos(rect)).length() <= hit {
        return Some(Zone::Output);
    }
    if has_in && (p - input_port_pos(rect, 0, 1)).length() <= hit {
        return Some(Zone::Input);
    }
    rect.contains(p).then_some(Zone::Body)
}

/// 이 링크가 `Pipeline::add_link` 를 통과하는가. 막는 이유가 있으면 한국어 한 줄.
/// `replace` 는 지금 떼어 끌고 있는 링크(중복 판정에서 뺀다).
pub fn link_check(pipeline: &Pipeline, from: PNodeId, to: PNodeId, replace: Option<LinkId>) -> Option<String> {
    if from == to {
        return Some("자기 자신".into());
    }
    let (Some(f), Some(t)) = (pipeline.nodes.get(&from), pipeline.nodes.get(&to)) else {
        return Some("없는 노드".into());
    };
    if f.kind.is_sink() {
        return Some("싱크에서는 나갈 수 없음".into());
    }
    if t.kind.is_source() {
        return Some("소스로는 들어올 수 없음".into());
    }
    if pipeline.links.values().any(|l| l.from == from && l.to == to && Some(l.id) != replace) {
        return Some("이미 연결됨".into());
    }
    None
}

/// 복제 노드의 위치 오프셋.
pub fn duplicate_offset() -> Vec2 {
    Vec2::new(40.0, NODE_H + 28.0)
}

/// 팔레트로 만들 때 클릭 지점이 노드 가운데가 되도록.
pub fn spawn_pos(world: Pos2) -> [f32; 2] {
    [world.x - NODE_W * 0.5, world.y - NODE_H * 0.5]
}

/// 소스 → 모델 → 싱크 기본 배치 (샘플·자동 생성).
pub fn chain_pos(index: usize, row: f32) -> [f32; 2] {
    [80.0 + index as f32 * NODE_GAP, row]
}

pub fn node_title(node: &PNode) -> String {
    if node.name.is_empty() {
        node.kind.label().to_string()
    } else {
        node.name.clone()
    }
}

/// 소스 8종 · 로직 5종 · 싱크 7종 팔레트 (모델은 프로젝트 모델별로 따로).
pub fn source_palette() -> Vec<Source> {
    vec![
        Source::Manual,
        Source::Timer { interval_ms: 1000 },
        Source::ScreenCapture { region: nl_core::pipeline::Region::default(), fps: 5.0 },
        Source::HttpPoll { url: "https://example.com/api".into(), interval_ms: 1000, headers: BTreeMap::new() },
        Source::WebSocket { url: "wss://example.com/ws".into() },
        Source::StdinJson,
        Source::File { path: "input.json".into(), interval_ms: 1000 },
        Source::GuiEvent { widget: WidgetId::from_u128(0) },
        Source::HttpServer { bind: DEFAULT_HTTP_BIND.into(), path: DEFAULT_HTTP_PATH.into() },
    ]
}

pub fn logic_palette() -> Vec<Logic> {
    vec![
        Logic::Threshold { value: 0.5 },
        Logic::Debounce { ms: 200 },
        Logic::Select { index: 0 },
        Logic::Map { table: BTreeMap::new() },
        Logic::Majority { window: 5 },
    ]
}

pub fn sink_palette() -> Vec<Sink> {
    vec![
        Sink::Log,
        Sink::StdoutJson,
        Sink::GuiWidget { widget: WidgetId::from_u128(0) },
        Sink::File { path: "output.jsonl".into(), append: true },
        Sink::HttpCall {
            method: "POST".into(),
            url: "https://example.com/api".into(),
            headers: BTreeMap::new(),
            body_template: "{\"value\": {{value}}}".into(),
        },
        Sink::WebSocketSend { url: "wss://example.com/ws".into() },
        Sink::MouseKeyboard { actions: vec![nl_core::InputAction::None], cooldown_ms: 200 },
        Sink::HttpReply { server: PNodeId::from_u128(0) },
    ]
}

// ── 그리기 ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct NodeStyle {
    selected: bool,
    hovered: bool,
    error: bool,
    zone: Option<Zone>,
    has_in: bool,
    has_out: bool,
    connected_in: bool,
}

fn zoom_at(camera: &mut Camera, viewport: Rect, pivot: Pos2, factor: f32) {
    let pivot_world = camera.to_world(viewport, pivot);
    camera.zoom = (camera.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
    camera.pan = pivot_world.to_vec2() - (pivot - viewport.min) / camera.zoom;
}

fn screen_rect(camera: &Camera, viewport: Rect, world: Rect) -> Rect {
    Rect::from_min_max(camera.to_screen(viewport, world.min), camera.to_screen(viewport, world.max))
}

fn draw_grid(painter: &egui::Painter, camera: &Camera, viewport: Rect) {
    let step = 40.0 * camera.zoom;
    if step < 6.0 {
        return;
    }
    let start = camera.to_world(viewport, viewport.min);
    let mut x = viewport.min.x - start.x.rem_euclid(40.0) * camera.zoom;
    let mut i = (start.x / 40.0).floor() as i64;
    while x < viewport.max.x {
        let c = if i % 5 == 0 { COL_GRID_STRONG } else { COL_GRID };
        painter.line_segment([Pos2::new(x, viewport.min.y), Pos2::new(x, viewport.max.y)], Stroke::new(1.0, c));
        x += step;
        i += 1;
    }
    let mut y = viewport.min.y - start.y.rem_euclid(40.0) * camera.zoom;
    let mut j = (start.y / 40.0).floor() as i64;
    while y < viewport.max.y {
        let c = if j % 5 == 0 { COL_GRID_STRONG } else { COL_GRID };
        painter.line_segment([Pos2::new(viewport.min.x, y), Pos2::new(viewport.max.x, y)], Stroke::new(1.0, c));
        y += step;
        j += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_node(
    painter: &egui::Painter,
    node: &PNode,
    sr: Rect,
    style: NodeStyle,
    zoom: f32,
    project: &nl_core::Project,
    value: Option<&str>,
) {
    let base = kind_color(&node.kind);
    let fill = mix(base, Color32::from_rgb(0x20, 0x23, 0x29), 0.78);
    let cr = CornerRadius::same((7.0 * zoom).clamp(2.0, 10.0) as u8);
    painter.rect_filled(sr, cr, if style.hovered { lighten(fill, 10) } else { fill });
    let border = if style.error {
        COL_ERROR
    } else if style.selected {
        COL_SELECT
    } else {
        base.gamma_multiply(0.9)
    };
    let bw = if style.selected || style.error { 2.4 } else { 1.4 };
    painter.rect_stroke(sr, cr, Stroke::new(bw * zoom.clamp(0.6, 1.4), border), StrokeKind::Outside);
    let stripe = Rect::from_min_max(sr.min, Pos2::new(sr.min.x + 4.0 * zoom.clamp(0.6, 1.4), sr.max.y));
    painter.rect_filled(stripe, cr, base);

    if zoom >= 0.35 {
        let pad = 12.0 * zoom;
        painter.text(
            Pos2::new(sr.min.x + pad, sr.min.y + 9.0 * zoom),
            Align2::LEFT_TOP,
            node_title(node),
            FontId::proportional((13.0 * zoom).clamp(7.0, 20.0)),
            COL_TEXT,
        );
        painter.text(
            Pos2::new(sr.min.x + pad, sr.min.y + 28.0 * zoom),
            Align2::LEFT_TOP,
            kind_summary(&node.kind, project),
            FontId::proportional((10.5 * zoom).clamp(6.0, 16.0)),
            COL_TEXT_DIM,
        );
    }

    let pr = (PORT_R * zoom).clamp(2.0, 9.0);
    if style.has_in {
        let p = input_port_pos(sr, 0, 1);
        let hot = style.zone == Some(Zone::Input);
        let c = if style.connected_in { COL_LINK } else { COL_TEXT_DIM };
        painter.circle(
            p,
            if hot { pr * 1.5 } else { pr },
            if style.connected_in { c } else { COL_BG },
            Stroke::new(1.5, c),
        );
    }
    if style.has_out {
        let p = output_port_pos(sr);
        let hot = style.zone == Some(Zone::Output);
        painter.circle(p, if hot { pr * 1.5 } else { pr }, COL_LINK, Stroke::new(1.5, COL_LINK));
    }
    // 실행 중 마지막 값 / 오류 점.
    if zoom >= 0.4 {
        if style.error {
            painter.circle_filled(Pos2::new(sr.max.x - 9.0 * zoom, sr.min.y + 9.0 * zoom), 4.0 * zoom.min(1.3), COL_ERROR);
        }
        if let Some(v) = value {
            pill_right(painter, Pos2::new(sr.max.x - 8.0 * zoom, sr.max.y - 7.0 * zoom), v, COL_OK);
        }
    }
}

fn pill_right(painter: &egui::Painter, anchor: Pos2, text: &str, color: Color32) {
    let text = if text.chars().count() > 26 {
        format!("{}…", text.chars().take(25).collect::<String>())
    } else {
        text.to_owned()
    };
    let galley = painter.layout_no_wrap(text, FontId::proportional(10.5), color);
    let size = galley.size() + Vec2::new(10.0, 4.0);
    let rect = Rect::from_min_size(Pos2::new(anchor.x - size.x, anchor.y - size.y), size);
    painter.rect_filled(rect, CornerRadius::same(3), COL_LABEL_BG);
    painter.galley(rect.min + Vec2::new(5.0, 2.0), galley, color);
}

fn node_menu(ui: &mut egui::Ui, id: PNodeId, group: &[PNodeId], actions: &mut Vec<PipelineAction>) {
    ui.set_min_width(180.0);
    if group.len() > 1 {
        ui.label(egui::RichText::new(format!("{}개 선택", group.len())).weak());
    }
    if ui.add(egui::Button::new("⎘ 복제").shortcut_text("Ctrl+D")).clicked() {
        actions.push(PipelineAction::DuplicateNodes(group.to_vec()));
        ui.close();
    }
    if ui.button("⊘ 연결 끊기").clicked() {
        actions.push(PipelineAction::DisconnectNode(id));
        ui.close();
    }
    ui.separator();
    if ui.add(egui::Button::new(egui::RichText::new("🗑 삭제").color(COL_ERROR)).shortcut_text("Del")).clicked() {
        actions.push(PipelineAction::DeleteNodes(group.to_vec()));
        ui.close();
    }
}

fn palette_menu(ui: &mut egui::Ui, world: Pos2, project: &nl_core::Project, actions: &mut Vec<PipelineAction>) {
    ui.label(egui::RichText::new("노드 추가").strong());
    let pos = spawn_pos(world);
    ui.menu_button("소스", |ui| {
        ui.set_min_width(190.0);
        for s in source_palette() {
            let kind = PNodeKind::Source { source: s };
            if ui.button(kind.label()).clicked() {
                actions.push(PipelineAction::AddNode { kind, pos });
                ui.close();
            }
        }
    });
    ui.menu_button("모델", |ui| {
        ui.set_min_width(190.0);
        if project.models.is_empty() {
            ui.label(egui::RichText::new("모델이 없습니다").weak());
        }
        for (id, m) in &project.models {
            if ui.button(&m.name).clicked() {
                actions.push(PipelineAction::AddNode {
                    kind: PNodeKind::Model { model: *id, payload: m.payload },
                    pos,
                });
                ui.close();
            }
        }
    });
    ui.menu_button("로직", |ui| {
        ui.set_min_width(190.0);
        for l in logic_palette() {
            let kind = PNodeKind::Logic { logic: l };
            if ui.button(kind.label()).clicked() {
                actions.push(PipelineAction::AddNode { kind, pos });
                ui.close();
            }
        }
    });
    ui.menu_button("싱크", |ui| {
        ui.set_min_width(190.0);
        for s in sink_palette() {
            let kind = PNodeKind::Sink { sink: s };
            if ui.button(kind.label()).clicked() {
                actions.push(PipelineAction::AddNode { kind, pos });
                ui.close();
            }
        }
    });
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t) as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

fn lighten(c: Color32, amount: u8) -> Color32 {
    Color32::from_rgb(c.r().saturating_add(amount), c.g().saturating_add(amount), c.b().saturating_add(amount))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::pipeline::PNode;

    fn pipe() -> (Pipeline, PNodeId, PNodeId, PNodeId) {
        let mut p = Pipeline::new("t");
        let src = p.add_node(PNode::new(PNodeKind::Source { source: Source::Manual }, [0.0, 0.0]));
        let logic = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Threshold { value: 0.5 } }, [250.0, 0.0]));
        let sink = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [500.0, 0.0]));
        (p, src, logic, sink)
    }

    #[test]
    fn link_rules_match_core() {
        let (mut p, src, logic, sink) = pipe();
        assert_eq!(link_check(&p, src, logic, None), None);
        p.add_link(src, logic).unwrap();
        // 중복
        assert_eq!(link_check(&p, src, logic, None).as_deref(), Some("이미 연결됨"));
        // 소스로 들어오기
        assert_eq!(link_check(&p, logic, src, None).as_deref(), Some("소스로는 들어올 수 없음"));
        // 싱크에서 나가기
        p.add_link(logic, sink).unwrap();
        assert_eq!(link_check(&p, sink, logic, None).as_deref(), Some("싱크에서는 나갈 수 없음"));
        // 자기 자신
        assert_eq!(link_check(&p, logic, logic, None).as_deref(), Some("자기 자신"));
    }

    /// 캔버스가 통과시키는 링크는 core 도 반드시 받아들여야 한다 (그 반대도).
    #[test]
    fn canvas_never_proposes_a_link_core_rejects() {
        let (p, src, logic, sink) = pipe();
        for (a, b) in [(src, logic), (src, sink), (logic, sink), (logic, src), (sink, src), (src, src)] {
            let mut copy = p.clone();
            let allowed = link_check(&copy, a, b, None).is_none();
            let accepted = copy.add_link(a, b).is_some();
            assert_eq!(allowed, accepted, "{a:?} → {b:?} 판정이 core 와 다르다");
        }
    }

    #[test]
    fn detached_link_frees_the_duplicate_check() {
        let (mut p, src, logic, _sink) = pipe();
        let l = p.add_link(src, logic).unwrap();
        assert!(link_check(&p, src, logic, None).is_some());
        assert!(link_check(&p, src, logic, Some(l)).is_none());
    }

    #[test]
    fn zones_prefer_ports() {
        let r = node_rect([0.0, 0.0]);
        assert_eq!(zone_of(r, true, true, output_port_pos(r), 10.0), Some(Zone::Output));
        assert_eq!(zone_of(r, true, true, input_port_pos(r, 0, 1), 10.0), Some(Zone::Input));
        assert_eq!(zone_of(r, true, true, r.center(), 10.0), Some(Zone::Body));
        // 소스는 입력 포트가 없다 — 그 자리도 바디다.
        assert_eq!(zone_of(r, false, true, input_port_pos(r, 0, 1), 10.0), Some(Zone::Body));
        assert_eq!(zone_of(r, true, true, Pos2::new(-50.0, -50.0), 10.0), None);
    }

    #[test]
    fn palettes_cover_every_variant() {
        assert_eq!(source_palette().len(), 9, "Source 변형 9종");
        assert_eq!(logic_palette().len(), 5, "Logic 변형 5종");
        assert_eq!(sink_palette().len(), 8, "Sink 변형 8종");
        // 라벨이 겹치면 팔레트에서 구분되지 않는다.
        let mut labels: Vec<&str> = source_palette()
            .into_iter()
            .map(|s| PNodeKind::Source { source: s }.label())
            .chain(logic_palette().into_iter().map(|l| PNodeKind::Logic { logic: l }.label()))
            .chain(sink_palette().into_iter().map(|s| PNodeKind::Sink { sink: s }.label()))
            .collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n);
    }

    #[test]
    fn kind_colors_differ_per_family() {
        let colors = [
            kind_color(&PNodeKind::Source { source: Source::Manual }),
            kind_color(&PNodeKind::Model { model: ModelId::from_u128(1), payload: None }),
            kind_color(&PNodeKind::Logic { logic: Logic::Debounce { ms: 1 } }),
            kind_color(&PNodeKind::Sink { sink: Sink::Log }),
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j]);
            }
        }
    }

    #[test]
    fn bounds_cover_every_node() {
        let (p, _, _, _) = pipe();
        let b = content_bounds(&p).unwrap();
        assert_eq!(b.min, Pos2::new(0.0, 0.0));
        assert_eq!(b.max, Pos2::new(500.0 + NODE_W, NODE_H));
        assert!(content_bounds(&Pipeline::new("x")).is_none());
    }
}
