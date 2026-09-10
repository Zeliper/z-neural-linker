//! 레이어 그래프 캔버스. trust-pms `canvas.rs` 의 구조를 그대로 잇는다:
//! `Camera { pan, zoom }` 월드 좌표 · 누른 지점의 `Zone`(포트 > 바디)이 드래그 종류를 정한다 ·
//! 놓을 때 `CanvasAction` 하나로 앱에 넘겨 `DocState` 가 op 로 적용한다.

use eframe::egui::{
    self, epaint::CubicBezierShape, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape as EShape, Stroke,
    StrokeKind, Vec2,
};
use nl_core::ids::*;
use nl_core::shape::{Shape, ShapeReport};
use nl_core::{Graph, LayerCategory, LayerKind, Node, Port};
use std::collections::{BTreeMap, BTreeSet};

// ── 크기 (월드 좌표, 줌 1.0 기준 px) ────────────────────────────────

pub const NODE_W: f32 = 190.0;
pub const NODE_H: f32 = 64.0;
/// 포트 원의 반지름.
pub const PORT_R: f32 = 5.5;
/// 포트 클릭 판정 반지름 (그리는 원보다 넉넉하게).
pub const PORT_HIT: f32 = 11.0;
/// 베지어 제어점이 뻗는 최대 거리.
const BEZIER_BULGE: f32 = 140.0;
/// 줌 범위.
const ZOOM_MIN: f32 = 0.15;
const ZOOM_MAX: f32 = 3.0;
/// 화면 맞춤 여백 (화면 px).
const FIT_MARGIN: f32 = 60.0;

// ── 색 ──────────────────────────────────────────────────────────────

const COL_BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1c);
const COL_GRID: Color32 = Color32::from_rgb(0x21, 0x24, 0x2a);
const COL_GRID_STRONG: Color32 = Color32::from_rgb(0x2a, 0x2e, 0x36);
const COL_TEXT: Color32 = Color32::from_rgb(0xe6, 0xe8, 0xec);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(0x9a, 0xa1, 0xad);
const COL_EDGE: Color32 = Color32::from_rgba_premultiplied(0x70, 0x78, 0x86, 0xc0);
const COL_SELECT: Color32 = Color32::from_rgb(0x4f, 0x9e, 0xea);
const COL_ERROR: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
const COL_OK: Color32 = Color32::from_rgb(0x57, 0xab, 0x5a);
const COL_GHOST: Color32 = Color32::from_rgba_premultiplied(0x2c, 0x4a, 0x6b, 0x66);
const COL_LABEL_BG: Color32 = Color32::from_rgba_premultiplied(0x14, 0x16, 0x1a, 0xd0);

// ── 선택 ────────────────────────────────────────────────────────────

/// 앱 전체가 공유하는 선택 대상. 인스펙터가 무엇을 보여줄지 정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    None,
    Project,
    Model(ModelId),
    Node(ModelId, NodeId),
    Edge(ModelId, EdgeId),
    Dataset(DatasetId),
    Payload(PayloadId),
    Pipeline(PipelineId),
    Run(RunId),
}

impl Selection {
    /// 이 선택이 가리키는 모델 (노드·엣지 선택이면 그 부모 모델).
    pub fn model(&self) -> Option<ModelId> {
        match self {
            Selection::Model(m) | Selection::Node(m, _) | Selection::Edge(m, _) => Some(*m),
            _ => None,
        }
    }
}

// ── 앱에 돌려주는 편집 의도 ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum CanvasAction {
    /// 팔레트에서 만든 새 레이어.
    AddNode { kind: LayerKind, pos: [f32; 2] },
    /// 드래그로 옮긴 노드들 (다중 선택은 한 묶음 = undo 한 번).
    MoveNodes(Vec<(NodeId, [f32; 2])>),
    /// 포트 → 포트 연결. `replace` 가 있으면 그 엣지를 떼어 다시 붙이는 것이다.
    Connect { from: NodeId, to: Port, replace: Option<EdgeId> },
    DeleteNodes(Vec<NodeId>),
    DeleteEdges(Vec<EdgeId>),
    DuplicateNodes(Vec<NodeId>),
    /// 이 노드에 붙은 엣지를 모두 뗀다.
    DisconnectNode(NodeId),
}

// ── 내부 상태 ───────────────────────────────────────────────────────

/// 포인터가 노드의 어느 부분 위에 있는지 — 드래그 종류와 커서를 정한다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Body,
    /// 오른쪽 출력 포트.
    Output,
    /// 왼쪽 입력 포트 (슬롯 번호).
    Input(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NodeDrag {
    id: NodeId,
    /// 누적 이동량(월드). 놓을 때 문서의 현재 위치에 더해 확정한다.
    delta: Vec2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct LinkDrag {
    /// 선이 나오는 출력 노드.
    from: NodeId,
    /// 입력 포트에서 끌어 뗀 기존 엣지 (놓을 곳이 없으면 삭제, 다른 곳이면 재연결).
    detach: Option<EdgeId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BoxSelect {
    origin: Pos2,
    current: Pos2,
    additive: bool,
}

impl BoxSelect {
    fn rect(&self) -> Rect {
        Rect::from_two_pos(self.origin, self.current)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CtxTarget {
    /// 빈 곳 — 레이어 팔레트. 월드 좌표에 만든다.
    Canvas { world: Pos2 },
    Edge(EdgeId),
}

pub struct Camera {
    /// 뷰포트 좌상단의 월드 좌표.
    pub pan: Vec2,
    pub zoom: f32,
}

impl Camera {
    pub fn to_screen(&self, viewport: Rect, world: Pos2) -> Pos2 {
        viewport.min + (world.to_vec2() - self.pan) * self.zoom
    }
    pub fn to_world(&self, viewport: Rect, screen: Pos2) -> Pos2 {
        Pos2::ZERO + (screen - viewport.min) / self.zoom + self.pan
    }
    fn zoom_at(&mut self, viewport: Rect, pivot: Pos2, factor: f32) {
        let pivot_world = self.to_world(viewport, pivot);
        self.zoom = (self.zoom * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        self.pan = pivot_world.to_vec2() - (pivot - viewport.min) / self.zoom;
    }
    fn rect_to_screen(&self, viewport: Rect, world: Rect) -> Rect {
        Rect::from_min_max(self.to_screen(viewport, world.min), self.to_screen(viewport, world.max))
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self { pan: Vec2::new(-40.0, -40.0), zoom: 1.0 }
    }
}

pub struct CanvasState {
    pub camera: Camera,
    pub selection: Selection,
    /// 다중 선택된 노드. 단일 선택이면 비어 있다.
    pub multi: BTreeSet<NodeId>,
    /// 외부(아웃라인·문제 목록)에서 이 노드로 화면을 옮겨 달라는 요청.
    pub pending_focus: Option<NodeId>,
    pub visible_nodes: usize,
    pub total_nodes: usize,
    fit_requested: bool,
    /// 아직 한 번도 화면을 잡지 않았다 (첫 프레임 또는 모델 전환 직후).
    initialized: bool,
    /// 지금 보고 있는 모델. 바뀌면 드래그·선택·카메라를 다시 잡는다.
    current_model: Option<ModelId>,
    last_viewport: Rect,
    node_drag: Option<NodeDrag>,
    link_drag: Option<LinkDrag>,
    box_select: Option<BoxSelect>,
    ctx_target: Option<CtxTarget>,
    /// Esc 로 취소한 드래그가 팬으로 바뀌지 않게 버튼을 뗄 때까지 삼킨다.
    swallow_drag: bool,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self::new()
    }
}

impl CanvasState {
    pub fn new() -> Self {
        Self {
            camera: Camera::default(),
            selection: Selection::None,
            multi: BTreeSet::new(),
            pending_focus: None,
            visible_nodes: 0,
            total_nodes: 0,
            fit_requested: false,
            initialized: false,
            current_model: None,
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

    pub fn reset_zoom(&mut self) {
        self.camera.zoom = 1.0;
    }

    pub fn interaction_active(&self) -> bool {
        self.node_drag.is_some() || self.link_drag.is_some() || self.box_select.is_some()
    }

    /// Esc: 진행 중인 드래그를 없던 일로.
    pub fn cancel_interaction(&mut self) {
        if self.interaction_active() {
            self.swallow_drag = true;
        }
        self.node_drag = None;
        self.link_drag = None;
        self.box_select = None;
    }

    pub fn set_selection(&mut self, sel: Selection) {
        self.selection = sel;
        self.multi.clear();
        if let Selection::Node(_, id) = sel {
            self.multi.insert(id);
        }
    }

    pub fn is_node_selected(&self, id: NodeId) -> bool {
        self.multi.contains(&id) || matches!(self.selection, Selection::Node(_, n) if n == id)
    }

    /// 선택된 노드 전부 (주 선택 포함).
    pub fn selected_nodes(&self) -> Vec<NodeId> {
        if self.multi.is_empty() {
            match self.selection {
                Selection::Node(_, id) => vec![id],
                _ => vec![],
            }
        } else {
            self.multi.iter().copied().collect()
        }
    }

    pub fn selection_count(&self) -> usize {
        self.selected_nodes().len()
    }

    pub fn toggle_node(&mut self, model: ModelId, id: NodeId) {
        if self.multi.contains(&id) {
            self.multi.remove(&id);
            if matches!(self.selection, Selection::Node(_, n) if n == id) {
                self.selection = match self.multi.iter().next() {
                    Some(&first) => Selection::Node(model, first),
                    None => Selection::Model(model),
                };
            }
        } else {
            self.multi.insert(id);
            self.selection = Selection::Node(model, id);
        }
    }

    pub fn select_nodes(&mut self, model: ModelId, ids: impl IntoIterator<Item = NodeId>, additive: bool) {
        if !additive {
            self.multi.clear();
        }
        self.multi.extend(ids);
        self.selection = match self.multi.iter().next() {
            Some(&first) => Selection::Node(model, first),
            None => Selection::Model(model),
        };
    }

    /// 문서에서 사라진 노드가 선택에 남아 있으면 유령을 지운다.
    pub fn prune(&mut self, graph: &Graph) {
        self.multi.retain(|id| graph.nodes.contains_key(id));
        match self.selection {
            Selection::Node(m, id) if !graph.nodes.contains_key(&id) => self.selection = Selection::Model(m),
            Selection::Edge(m, id) if !graph.edges.contains_key(&id) => self.selection = Selection::Model(m),
            _ => {}
        }
    }

    // ── 카메라 ──────────────────────────────────────────────────

    fn fit(&mut self, graph: &Graph, viewport: Rect) {
        let Some(b) = content_bounds(graph) else {
            self.camera = Camera::default();
            return;
        };
        let b = b.expand(20.0);
        let avail = Vec2::new((viewport.width() - FIT_MARGIN).max(80.0), (viewport.height() - FIT_MARGIN).max(80.0));
        let zoom = (avail.x / b.width().max(1.0)).min(avail.y / b.height().max(1.0)).clamp(ZOOM_MIN, ZOOM_MAX);
        self.camera.zoom = zoom;
        self.camera.pan = b.center().to_vec2() - viewport.size() / (2.0 * zoom);
    }

    fn center_on(&mut self, graph: &Graph, id: NodeId, viewport: Rect) {
        let Some(node) = graph.nodes.get(&id) else { return };
        let r = node_rect(node.pos);
        self.camera.pan = r.center().to_vec2() - viewport.size() / (2.0 * self.camera.zoom);
    }

    // ── 프레임 ──────────────────────────────────────────────────

    /// 캔버스 한 프레임. 문서는 읽기만 하고 편집 의도는 `CanvasAction` 으로 돌려준다.
    pub fn show(&mut self, ui: &mut egui::Ui, model: ModelId, graph: &Graph, report: &ShapeReport) -> Vec<CanvasAction> {
        let mut actions: Vec<CanvasAction> = Vec::new();

        // 모델이 바뀌었으면 드래그·카메라를 새로 잡는다 (다른 그래프의 노드 id 로 유령이 남지 않게).
        if self.current_model != Some(model) {
            self.current_model = Some(model);
            self.node_drag = None;
            self.link_drag = None;
            self.box_select = None;
            self.initialized = false;
        }
        // 다른 경로(undo·원격 삭제)로 사라진 노드 정리.
        if self.node_drag.map(|d| !graph.nodes.contains_key(&d.id)).unwrap_or(false) {
            self.node_drag = None;
        }
        if self.link_drag.map(|d| !graph.nodes.contains_key(&d.from)).unwrap_or(false) {
            self.link_drag = None;
        }
        self.prune(graph);

        let viewport = ui.available_rect_before_wrap();
        let prev_viewport = self.last_viewport;
        self.last_viewport = viewport;

        if !self.initialized {
            self.initialized = true;
            self.fit(graph, viewport);
        } else if (viewport.width() - prev_viewport.width()).abs() > 0.5
            || (viewport.height() - prev_viewport.height()).abs() > 0.5
        {
            // 크기가 바뀌면 화면 가운데의 월드 좌표를 유지한다.
            let center = self.camera.to_world(prev_viewport, prev_viewport.center());
            self.camera.pan = center.to_vec2() - viewport.size() / (2.0 * self.camera.zoom);
            ui.ctx().request_repaint();
        }
        if self.fit_requested {
            self.fit_requested = false;
            self.fit(graph, viewport);
            ui.ctx().request_repaint();
        }
        if let Some(id) = self.pending_focus.take() {
            self.center_on(graph, id, viewport);
            self.set_selection(Selection::Node(model, id));
            ui.ctx().request_repaint();
        }

        let bg = ui.interact(viewport, ui.id().with("canvas-bg"), Sense::click_and_drag());
        let mods = ui.input(|i| i.modifiers);
        let (pointer, press_origin, primary_released) = ui.input(|i| {
            (i.pointer.interact_pos(), i.pointer.press_origin(), i.pointer.button_released(egui::PointerButton::Primary))
        });

        if self.swallow_drag && !ui.input(|i| i.pointer.any_down()) {
            self.swallow_drag = false;
        }

        // ── 팬 / 줌 / 러버밴드 ─────────────────────────────────
        if bg.drag_started_by(egui::PointerButton::Primary) && mods.alt {
            let origin = press_origin.or(pointer).unwrap_or(viewport.center());
            self.box_select = Some(BoxSelect { origin, current: origin, additive: mods.shift || mods.command });
        }
        let panning = bg.dragged_by(egui::PointerButton::Primary) || bg.dragged_by(egui::PointerButton::Middle);
        if panning && self.box_select.is_none() && !self.swallow_drag {
            self.camera.pan -= bg.drag_delta() / self.camera.zoom;
        }
        if bg.contains_pointer() {
            let (zoom_delta, scroll, hover) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta, i.pointer.hover_pos()));
            if zoom_delta != 1.0 {
                self.camera.zoom_at(viewport, hover.unwrap_or_else(|| viewport.center()), zoom_delta);
            }
            if scroll != Vec2::ZERO {
                // 휠은 스크롤(세로/가로), Ctrl+휠은 egui 가 zoom_delta 로 바꿔 준다.
                self.camera.pan -= scroll / self.camera.zoom;
            }
        }
        let zoom = self.camera.zoom;

        let painter = ui.painter_at(viewport);
        painter.rect_filled(viewport, CornerRadius::ZERO, COL_BG);
        draw_grid(&painter, &self.camera, viewport);

        // ── 화면에 걸치는 노드만 (상태바 카운트) ────────────────
        let world_view = Rect::from_min_max(
            self.camera.to_world(viewport, viewport.min),
            self.camera.to_world(viewport, viewport.max),
        );
        // 드래그로 임시로 옮겨진 위치 (커밋 전). 선택 집합을 미리 복사해 클로저가 `self` 를 빌지 않게 한다
        // — 그래야 아래에서 `self.visible_nodes` 같은 필드를 계속 쓸 수 있다.
        let node_drag = self.node_drag;
        let dragging: BTreeSet<NodeId> = self.selected_nodes().into_iter().collect();
        let drag_offset = |id: NodeId| -> Vec2 {
            match node_drag {
                // 선택 전체를 함께 끌고 있다.
                Some(d) if dragging.contains(&d.id) && dragging.contains(&id) => d.delta,
                Some(d) if d.id == id => d.delta,
                _ => Vec2::ZERO,
            }
        };
        let world_rect = |id: NodeId| -> Option<Rect> {
            let n = graph.nodes.get(&id)?;
            Some(node_rect(n.pos).translate(drag_offset(id)))
        };

        let mut visible: Vec<(NodeId, Rect)> = Vec::new();
        for &id in graph.nodes.keys() {
            if let Some(wr) = world_rect(id) {
                if wr.intersects(world_view.expand(120.0)) {
                    visible.push((id, self.camera.rect_to_screen(viewport, wr)));
                }
            }
        }
        self.visible_nodes = visible.len();
        self.total_nodes = graph.nodes.len();

        // ── 엣지 ────────────────────────────────────────────────
        let detached = self.link_drag.and_then(|d| d.detach);
        let mut hovered_edge: Option<EdgeId> = None;
        for (&eid, edge) in &graph.edges {
            if Some(eid) == detached {
                continue; // 떼어 끌고 있는 중 — 고스트만 보인다
            }
            let (Some(fw), Some(tw)) = (world_rect(edge.from), world_rect(edge.to.node)) else { continue };
            let slots = graph.nodes.get(&edge.to.node).map(|n| n.kind.spec().inputs).unwrap_or(1);
            let p0 = self.camera.to_screen(viewport, output_port_pos(fw));
            let p3 = self.camera.to_screen(viewport, input_port_pos(tw, edge.to.slot, slots));
            let (p1, p2) = control_points(p0, p3);
            let selected = self.selection == Selection::Edge(model, eid);
            let bad = report.errors.contains_key(&edge.to.node) || report.errors.contains_key(&edge.from);
            let (color, width) = if selected {
                (COL_SELECT, 2.6)
            } else if bad {
                (COL_ERROR.gamma_multiply(0.7), 1.8)
            } else {
                (COL_EDGE, 1.8)
            };
            let stroke = Stroke::new(width * zoom.clamp(0.6, 1.4), color);
            painter.add(CubicBezierShape::from_points_stroke([p0, p1, p2, p3], false, Color32::TRANSPARENT, stroke));
            // 화살촉
            let dir = (p3 - p2).normalized();
            let orth = Vec2::new(-dir.y, dir.x);
            let s = 5.0 * zoom.clamp(0.6, 1.4);
            painter.add(EShape::convex_polygon(
                vec![p3, p3 - dir * s * 2.0 + orth * s, p3 - dir * s * 2.0 - orth * s],
                color,
                Stroke::NONE,
            ));
            if let Some(pp) = pointer {
                if hovered_edge.is_none()
                    && Rect::from_two_pos(p0, p3).union(Rect::from_two_pos(p1, p2)).expand(14.0).contains(pp)
                    && bezier_near(p0, p1, p2, p3, pp, 7.0)
                {
                    hovered_edge = Some(eid);
                }
            }
        }

        // ── 노드 ────────────────────────────────────────────────
        let mut drop_target: Option<(NodeId, usize)> = None;
        let mut cursor: Option<egui::CursorIcon> = None;
        let idle = !self.interaction_active();

        for &(id, sr) in &visible {
            let Some(node) = graph.nodes.get(&id) else { continue };
            let spec = node.kind.spec();
            // 포트는 노드 테두리에 걸쳐 있으므로 판정 영역을 그만큼 넓힌다 (노드 간격보다는 좁게).
            let port_hit = (PORT_HIT * zoom).clamp(7.0, 14.0);
            let hit = sr.expand(port_hit);
            let resp = ui.interact(hit, ui.id().with(("nl-node", id)), Sense::click_and_drag());
            let hovered = resp.hovered();
            let zone_at = |p: Pos2| hit_zone(sr, spec.inputs, spec.has_output, p, port_hit);

            if resp.clicked() {
                if mods.shift || mods.command {
                    self.toggle_node(model, id);
                } else {
                    self.set_selection(Selection::Node(model, id));
                }
            }
            if resp.secondary_clicked() && !self.is_node_selected(id) {
                self.set_selection(Selection::Node(model, id));
            }
            if resp.drag_started_by(egui::PointerButton::Primary) {
                let origin = press_origin.or_else(|| resp.interact_pointer_pos()).unwrap_or_else(|| sr.center());
                match zone_at(origin) {
                    Some(Zone::Output) if spec.has_output => {
                        self.link_drag = Some(LinkDrag { from: id, detach: None });
                    }
                    Some(Zone::Input(slot)) => {
                        // 이미 꽂혀 있으면 떼어 재연결, 비어 있으면 그냥 이동.
                        let existing = graph.edges.values().find(|e| e.to == Port::new(id, slot));
                        match existing {
                            Some(e) => self.link_drag = Some(LinkDrag { from: e.from, detach: Some(e.id) }),
                            None => self.node_drag = Some(NodeDrag { id, delta: Vec2::ZERO }),
                        }
                    }
                    _ => {
                        if !self.is_node_selected(id) {
                            self.set_selection(Selection::Node(model, id));
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
            // 연결 드래그 중 이 노드의 입력 포트 위인가.
            if let Some(link) = self.link_drag {
                if link.from != id && spec.inputs > 0 {
                    if let Some(pp) = pointer {
                        if let Some(Zone::Input(slot)) = zone_at(pp) {
                            drop_target = Some((id, slot));
                        } else if drop_target.is_none() && sr.contains(pp) {
                            // 노드 몸통에 놓으면 비어 있는 첫 슬롯으로.
                            let taken: BTreeSet<usize> = graph.inputs_of(id).keys().copied().collect();
                            if let Some(free) = (0..spec.inputs).find(|s| !taken.contains(s)) {
                                drop_target = Some((id, free));
                            }
                        }
                    }
                }
            }

            let zone = (hovered && idle).then(|| pointer.and_then(zone_at)).flatten();
            if let Some(z) = zone {
                cursor = Some(match z {
                    Zone::Output | Zone::Input(_) => egui::CursorIcon::Crosshair,
                    Zone::Body => egui::CursorIcon::Grab,
                });
            }

            let err = report.errors.get(&id);
            let style = NodeStyle { selected: self.is_node_selected(id), hovered, error: err.is_some(), zone };
            draw_node(&painter, node, sr, style, zoom, report, graph);

            // 오류 노드는 사유를 툴팁으로 (검증 도크의 문구와 같은 출처).
            let resp = match err {
                Some(e) if hovered => resp.on_hover_text(format!("{}\n{e}", node.display_name())),
                _ => resp,
            };
            // 컨텍스트 메뉴는 노드 응답에 붙인다 — 선택 전체에 적용할지는 이 노드가 선택에 든 지로 정한다.
            let group: Vec<NodeId> =
                if self.selection_count() > 1 && self.is_node_selected(id) { self.selected_nodes() } else { vec![id] };
            resp.context_menu(|ui| node_menu(ui, id, &group, &mut actions));
        }

        // ── 러버밴드 ────────────────────────────────────────────
        if let Some(mut b) = self.box_select {
            if let Some(pp) = pointer {
                b.current = pp;
            }
            let r = b.rect();
            painter.rect_filled(r, CornerRadius::ZERO, COL_GHOST);
            painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, COL_SELECT), StrokeKind::Outside);
            self.box_select = Some(b);
            if primary_released {
                self.box_select = None;
                let picked: Vec<NodeId> =
                    visible.iter().filter(|(_, sr)| sr.intersects(r)).map(|(id, _)| *id).collect();
                if !picked.is_empty() || !b.additive {
                    self.select_nodes(model, picked, b.additive);
                }
            }
        }

        // ── 연결 고스트 ─────────────────────────────────────────
        if let Some(link) = self.link_drag {
            if let (Some(pp), Some(fw)) = (pointer, world_rect(link.from)) {
                let p0 = self.camera.to_screen(viewport, output_port_pos(fw));
                let (target_pos, ok) = match drop_target {
                    Some((to, slot)) => {
                        let ok = connect_allowed(graph, link.from, Port::new(to, slot), link.detach);
                        let tw = world_rect(to).unwrap_or(fw);
                        let slots = graph.nodes.get(&to).map(|n| n.kind.spec().inputs).unwrap_or(1);
                        (self.camera.to_screen(viewport, input_port_pos(tw, slot, slots)), ok)
                    }
                    None => (pp, true),
                };
                let color = match (drop_target.is_some(), ok) {
                    (true, true) => COL_OK,
                    (true, false) => COL_ERROR,
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
                if drop_target.is_some() && !ok {
                    let why = connect_error(graph, link.from, drop_target.map(|(n, s)| Port::new(n, s)).unwrap());
                    label_pill_right(&painter, Pos2::new(target_pos.x - 12.0, target_pos.y - 6.0), &why, COL_ERROR);
                }
            }
            if primary_released {
                let link = self.link_drag.take().unwrap();
                match drop_target {
                    Some((to, slot)) => {
                        let port = Port::new(to, slot);
                        if connect_allowed(graph, link.from, port, link.detach) {
                            actions.push(CanvasAction::Connect { from: link.from, to: port, replace: link.detach });
                        }
                    }
                    // 빈 곳에 놓았다: 떼어 온 엣지는 삭제, 새로 만들던 것은 없던 일로.
                    None => {
                        if let Some(e) = link.detach {
                            actions.push(CanvasAction::DeleteEdges(vec![e]));
                        }
                    }
                }
            }
        }

        // ── 노드 이동 확정 ──────────────────────────────────────
        if let Some(d) = self.node_drag {
            if primary_released {
                self.node_drag = None;
                if d.delta.length() > 0.5 {
                    let group: Vec<NodeId> =
                        if self.is_node_selected(d.id) { self.selected_nodes() } else { vec![d.id] };
                    let items: Vec<(NodeId, [f32; 2])> = group
                        .iter()
                        .filter_map(|&gid| {
                            let n = graph.nodes.get(&gid)?;
                            Some((gid, [n.pos[0] + d.delta.x, n.pos[1] + d.delta.y]))
                        })
                        .collect();
                    if !items.is_empty() {
                        actions.push(CanvasAction::MoveNodes(items));
                    }
                }
            }
        }

        // ── 배경 클릭: 엣지 선택 / 선택 해제 ────────────────────
        if bg.clicked() && !(mods.shift || mods.command) {
            match hovered_edge {
                Some(e) => self.set_selection(Selection::Edge(model, e)),
                None => self.set_selection(Selection::Model(model)),
            }
        }
        if bg.secondary_clicked() {
            self.ctx_target = Some(match hovered_edge {
                Some(e) => CtxTarget::Edge(e),
                None => CtxTarget::Canvas {
                    world: pointer.map(|pp| self.camera.to_world(viewport, pp)).unwrap_or(Pos2::ZERO),
                },
            });
        }

        // 커서
        if self.node_drag.is_some() {
            cursor = Some(egui::CursorIcon::Grabbing);
        } else if self.link_drag.is_some() || self.box_select.is_some() {
            cursor = Some(egui::CursorIcon::Crosshair);
        } else if cursor.is_none() && hovered_edge.is_some() {
            cursor = Some(egui::CursorIcon::PointingHand);
        }
        if let Some(c) = cursor {
            ui.ctx().set_cursor_icon(c);
        }

        // ── 배경·엣지 컨텍스트 메뉴 ────────────────────────────
        let target = self.ctx_target;
        bg.context_menu(|ui| {
            ui.set_min_width(190.0);
            match target {
                Some(CtxTarget::Edge(eid)) => {
                    if ui.add(egui::Button::new("🗑 연결 삭제").shortcut_text("Del")).clicked() {
                        actions.push(CanvasAction::DeleteEdges(vec![eid]));
                        ui.close();
                    }
                }
                Some(CtxTarget::Canvas { world }) => palette_menu(ui, world, &mut actions),
                None => {
                    ui.close();
                }
            }
        });

        if graph.nodes.is_empty() {
            painter.text(
                viewport.center(),
                Align2::CENTER_CENTER,
                "빈 곳을 우클릭해 레이어를 추가하세요",
                FontId::proportional(15.0),
                COL_TEXT_DIM,
            );
        }
        if self.interaction_active() {
            ui.ctx().request_repaint();
        }
        actions
    }
}

// ── 순수 기하 · 판정 (테스트 대상) ──────────────────────────────────

/// 노드 하나의 월드 사각형.
pub fn node_rect(pos: [f32; 2]) -> Rect {
    Rect::from_min_size(Pos2::new(pos[0], pos[1]), Vec2::new(NODE_W, NODE_H))
}

/// 그래프 전체를 감싸는 월드 사각형. 노드가 없으면 `None`.
pub fn content_bounds(graph: &Graph) -> Option<Rect> {
    let mut it = graph.nodes.values().map(|n| node_rect(n.pos));
    let first = it.next()?;
    Some(it.fold(first, |acc, r| acc.union(r)))
}

/// 입력 포트 `slot` 의 월드 좌표. 슬롯이 여럿이면 왼쪽 변을 균등 분할한다.
pub fn input_port_pos(rect: Rect, slot: usize, count: usize) -> Pos2 {
    let count = count.max(1);
    let t = (slot as f32 + 1.0) / (count as f32 + 1.0);
    Pos2::new(rect.min.x, rect.min.y + rect.height() * t)
}

/// 출력 포트의 월드 좌표 (오른쪽 변 가운데).
pub fn output_port_pos(rect: Rect) -> Pos2 {
    Pos2::new(rect.max.x, rect.center().y)
}

/// 누른 지점이 노드의 어느 부분인가. 포트가 바디보다 먼저다.
/// `rect` 와 `p` 는 같은 좌표계(화면)여야 하고 `hit` 은 그 좌표계의 판정 반지름이다.
pub fn hit_zone(rect: Rect, inputs: usize, has_output: bool, p: Pos2, hit: f32) -> Option<Zone> {
    if has_output && (p - output_port_pos(rect)).length() <= hit {
        return Some(Zone::Output);
    }
    for slot in 0..inputs {
        if (p - input_port_pos(rect, slot, inputs)).length() <= hit {
            return Some(Zone::Input(slot));
        }
    }
    rect.contains(p).then_some(Zone::Body)
}

/// 3차 베지어 위의 점.
pub fn bezier_point(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    Pos2::new(
        a * p0.x + b * p1.x + c * p2.x + d * p3.x,
        a * p0.y + b * p1.y + c * p2.y + d * p3.y,
    )
}

/// 포인터가 곡선에서 `tol` 안쪽인가 (32 등분 근사).
pub fn bezier_near(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, p: Pos2, tol: f32) -> bool {
    (0..=32).any(|i| (bezier_point(p0, p1, p2, p3, i as f32 / 32.0) - p).length() <= tol)
}

/// 엣지 제어점 — 출력은 오른쪽으로, 입력은 왼쪽으로 뻗는다.
pub fn control_points(p0: Pos2, p3: Pos2) -> (Pos2, Pos2) {
    let dx = ((p3.x - p0.x).abs() * 0.5).clamp(28.0, BEZIER_BULGE);
    (Pos2::new(p0.x + dx, p0.y), Pos2::new(p3.x - dx, p3.y))
}

/// 포트 옆에 붙는 형상 라벨. 추론 실패면 `?`.
pub fn shape_label(shape: Option<&Shape>) -> String {
    match shape {
        Some(s) => s.to_string(),
        None => "?".into(),
    }
}

/// 이 연결이 허용되는가. `replace` 는 지금 떼어 끌고 있는 엣지(그 슬롯은 비어 있는 것으로 본다).
pub fn connect_allowed(graph: &Graph, from: NodeId, to: Port, replace: Option<EdgeId>) -> bool {
    connect_check(graph, from, to, replace).is_none()
}

/// 막는 이유가 있으면 한국어 한 줄로.
pub fn connect_error(graph: &Graph, from: NodeId, to: Port) -> String {
    connect_check(graph, from, to, None).unwrap_or_default()
}

fn connect_check(graph: &Graph, from: NodeId, to: Port, replace: Option<EdgeId>) -> Option<String> {
    if from == to.node {
        return Some("자기 자신".into());
    }
    if !graph.nodes.contains_key(&from) {
        return Some("없는 노드".into());
    }
    let Some(target) = graph.nodes.get(&to.node) else { return Some("없는 노드".into()) };
    if to.slot >= target.kind.spec().inputs {
        return Some("입력 슬롯 없음".into());
    }
    let taken = graph.edges.values().any(|e| e.to == to && Some(e.id) != replace);
    if taken {
        return Some("이미 연결된 슬롯".into());
    }
    let dup = graph.edges.values().any(|e| e.from == from && e.to.node == to.node && Some(e.id) != replace);
    if dup {
        return Some("이미 연결됨".into());
    }
    if graph.would_create_cycle(from, to.node) {
        return Some("순환 연결".into());
    }
    None
}

/// 복제된 노드들의 새 위치 (오른쪽 아래로 한 칸).
pub fn duplicate_offset() -> Vec2 {
    Vec2::new(40.0, NODE_H + 26.0)
}

/// 팔레트에서 새 레이어를 만들 때 클릭 지점이 노드 가운데가 되도록 좌상단을 계산한다.
pub fn spawn_pos(world: Pos2) -> [f32; 2] {
    [world.x - NODE_W * 0.5, world.y - NODE_H * 0.5]
}

// ── 그리기 ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct NodeStyle {
    selected: bool,
    hovered: bool,
    error: bool,
    zone: Option<Zone>,
}

fn draw_grid(painter: &egui::Painter, camera: &Camera, viewport: Rect) {
    let step = 40.0 * camera.zoom;
    if step < 6.0 {
        return;
    }
    let start = camera.to_world(viewport, viewport.min);
    let ox = viewport.min.x - (start.x.rem_euclid(40.0)) * camera.zoom;
    let oy = viewport.min.y - (start.y.rem_euclid(40.0)) * camera.zoom;
    let mut x = ox;
    let mut i = (start.x / 40.0).floor() as i64;
    while x < viewport.max.x {
        let c = if i % 5 == 0 { COL_GRID_STRONG } else { COL_GRID };
        painter.line_segment([Pos2::new(x, viewport.min.y), Pos2::new(x, viewport.max.y)], Stroke::new(1.0, c));
        x += step;
        i += 1;
    }
    let mut y = oy;
    let mut j = (start.y / 40.0).floor() as i64;
    while y < viewport.max.y {
        let c = if j % 5 == 0 { COL_GRID_STRONG } else { COL_GRID };
        painter.line_segment([Pos2::new(viewport.min.x, y), Pos2::new(viewport.max.x, y)], Stroke::new(1.0, c));
        y += step;
        j += 1;
    }
}

fn draw_node(
    painter: &egui::Painter,
    node: &Node,
    sr: Rect,
    style: NodeStyle,
    zoom: f32,
    report: &ShapeReport,
    graph: &Graph,
) {
    let spec = node.kind.spec();
    let base = Color32::from_rgb(spec.color[0], spec.color[1], spec.color[2]);
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
    // 왼쪽 종류 색 띠
    let stripe = Rect::from_min_max(sr.min, Pos2::new(sr.min.x + 4.0 * zoom.clamp(0.6, 1.4), sr.max.y));
    painter.rect_filled(stripe, cr, base);

    // 글자는 줌이 작으면 생략한다 (읽히지도 않고 비싸다).
    if zoom >= 0.35 {
        let pad = 12.0 * zoom;
        let title = node.display_name();
        painter.text(
            Pos2::new(sr.min.x + pad, sr.min.y + 10.0 * zoom),
            Align2::LEFT_TOP,
            &title,
            FontId::proportional((13.0 * zoom).clamp(7.0, 20.0)),
            COL_TEXT,
        );
        let sub = {
            let s = node.kind.summary();
            if s.is_empty() {
                spec.label.to_string()
            } else {
                format!("{} · {s}", spec.label)
            }
        };
        painter.text(
            Pos2::new(sr.min.x + pad, sr.min.y + 30.0 * zoom),
            Align2::LEFT_TOP,
            sub,
            FontId::proportional((10.5 * zoom).clamp(6.0, 16.0)),
            COL_TEXT_DIM,
        );
    }

    // 입력 포트
    let pr = (PORT_R * zoom).clamp(2.0, 9.0);
    for slot in 0..spec.inputs {
        let p = input_port_pos(sr, slot, spec.inputs);
        let filled = graph.inputs_of(node.id).contains_key(&slot);
        let hot = style.zone == Some(Zone::Input(slot));
        let c = if filled { COL_EDGE } else { COL_TEXT_DIM };
        painter.circle(p, if hot { pr * 1.5 } else { pr }, if filled { c } else { COL_BG }, Stroke::new(1.5, c));
    }
    // 출력 포트
    if spec.has_output {
        let p = output_port_pos(sr);
        let hot = style.zone == Some(Zone::Output);
        painter.circle(p, if hot { pr * 1.5 } else { pr }, COL_EDGE, Stroke::new(1.5, COL_EDGE));
    }
    // 형상 라벨은 출력 포트 안쪽(노드 우하단)에 붙인다 — 노드 사이 간격에 두면 다음 노드에 가린다.
    if zoom >= 0.4 && (spec.has_output || spec.inputs > 0) {
        let text = match report.errors.get(&node.id) {
            Some(_) => "오류".to_string(),
            None => shape_label(report.shape(node.id)),
        };
        let color = if report.errors.contains_key(&node.id) { COL_ERROR } else { COL_TEXT_DIM };
        label_pill_right(painter, Pos2::new(sr.max.x - 8.0 * zoom, sr.max.y - 7.0 * zoom), &text, color);
    }
}

/// 오른쪽 아래를 기준점으로 붙는 작은 알약 라벨.
fn label_pill_right(painter: &egui::Painter, anchor: Pos2, text: &str, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), FontId::proportional(10.5), color);
    let size = galley.size() + Vec2::new(10.0, 4.0);
    let rect = Rect::from_min_size(Pos2::new(anchor.x - size.x, anchor.y - size.y), size);
    painter.rect_filled(rect, CornerRadius::same(3), COL_LABEL_BG);
    painter.galley(rect.min + Vec2::new(5.0, 2.0), galley, color);
}

/// 노드 우클릭 메뉴. `group` 이 여럿이면 선택 전체가 대상이다.
fn node_menu(ui: &mut egui::Ui, id: NodeId, group: &[NodeId], actions: &mut Vec<CanvasAction>) {
    ui.set_min_width(180.0);
    if group.len() > 1 {
        ui.label(egui::RichText::new(format!("{}개 선택", group.len())).weak());
    }
    if ui.add(egui::Button::new("⎘ 복제").shortcut_text("Ctrl+D")).clicked() {
        actions.push(CanvasAction::DuplicateNodes(group.to_vec()));
        ui.close();
    }
    if ui.button("⊘ 연결 끊기").on_hover_text("이 노드에 붙은 엣지를 모두 뗀다").clicked() {
        actions.push(CanvasAction::DisconnectNode(id));
        ui.close();
    }
    ui.separator();
    if ui
        .add(egui::Button::new(egui::RichText::new("🗑 삭제").color(COL_ERROR)).shortcut_text("Del"))
        .clicked()
    {
        actions.push(CanvasAction::DeleteNodes(group.to_vec()));
        ui.close();
    }
}

/// 레이어 팔레트: 카테고리별 서브메뉴.
fn palette_menu(ui: &mut egui::Ui, world: Pos2, actions: &mut Vec<CanvasAction>) {
    ui.label(egui::RichText::new("레이어 추가").strong());
    let mut by_cat: BTreeMap<usize, (LayerCategory, Vec<LayerKind>)> = BTreeMap::new();
    for kind in LayerKind::palette() {
        let cat = kind.spec().category;
        by_cat.entry(category_order(cat)).or_insert_with(|| (cat, Vec::new())).1.push(kind);
    }
    for (_, (cat, kinds)) in by_cat {
        ui.menu_button(cat.label(), |ui| {
            ui.set_min_width(170.0);
            for kind in kinds {
                let spec = kind.spec();
                let text = if kind.summary().is_empty() {
                    spec.label.to_string()
                } else {
                    format!("{}  {}", spec.label, kind.summary())
                };
                if ui.button(text).clicked() {
                    actions.push(CanvasAction::AddNode { kind: kind.clone(), pos: spawn_pos(world) });
                    ui.close();
                }
            }
        });
    }
}

/// 팔레트 서브메뉴 순서 (입출력이 맨 위, 나머지는 자주 쓰는 순서).
fn category_order(c: LayerCategory) -> usize {
    match c {
        LayerCategory::Io => 0,
        LayerCategory::Dense => 1,
        LayerCategory::Conv => 2,
        LayerCategory::Pool => 3,
        LayerCategory::Activation => 4,
        LayerCategory::Shape => 5,
        LayerCategory::Normalize => 6,
        LayerCategory::Regularize => 7,
        LayerCategory::Merge => 8,
        LayerCategory::Embed => 9,
    }
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
    use nl_core::{Act, Node};

    fn graph_with(kinds: Vec<LayerKind>) -> (Graph, Vec<NodeId>) {
        let mut g = Graph::default();
        let ids = kinds
            .into_iter()
            .enumerate()
            .map(|(i, k)| g.add_node(Node::new(k, [i as f32 * 220.0, 100.0])))
            .collect();
        (g, ids)
    }

    #[test]
    fn ports_spread_evenly_on_the_left_edge() {
        let r = node_rect([0.0, 0.0]);
        // 슬롯 하나면 한가운데.
        assert_eq!(input_port_pos(r, 0, 1), Pos2::new(0.0, NODE_H / 2.0));
        // 둘이면 1/3, 2/3.
        let a = input_port_pos(r, 0, 2);
        let b = input_port_pos(r, 1, 2);
        assert!((a.y - NODE_H / 3.0).abs() < 1e-4, "{a:?}");
        assert!((b.y - NODE_H * 2.0 / 3.0).abs() < 1e-4, "{b:?}");
        assert_eq!(a.x, 0.0);
        assert_eq!(output_port_pos(r), Pos2::new(NODE_W, NODE_H / 2.0));
    }

    #[test]
    fn hit_zone_prefers_ports_over_body() {
        let r = node_rect([0.0, 0.0]);
        // 출력 포트 바로 위 — 사각형 안쪽이기도 하지만 포트가 이긴다.
        let out = output_port_pos(r);
        assert_eq!(hit_zone(r, 1, true, out - Vec2::new(2.0, 0.0), 10.0), Some(Zone::Output));
        // 입력 포트.
        let inp = input_port_pos(r, 0, 2);
        assert_eq!(hit_zone(r, 2, true, inp + Vec2::new(3.0, 0.0), 10.0), Some(Zone::Input(0)));
        // 가운데는 바디.
        assert_eq!(hit_zone(r, 1, true, r.center(), 10.0), Some(Zone::Body));
        // 바깥은 아무것도 아니다.
        assert_eq!(hit_zone(r, 1, true, Pos2::new(-40.0, -40.0), 10.0), None);
        // 출력이 없는 노드(Output)는 오른쪽 변에서도 바디다.
        assert_eq!(hit_zone(r, 1, false, out, 10.0), Some(Zone::Body));
    }

    #[test]
    fn bezier_passes_through_its_endpoints_and_hits_nearby() {
        let p0 = Pos2::new(0.0, 0.0);
        let p3 = Pos2::new(200.0, 80.0);
        let (p1, p2) = control_points(p0, p3);
        assert_eq!(bezier_point(p0, p1, p2, p3, 0.0), p0);
        assert_eq!(bezier_point(p0, p1, p2, p3, 1.0), p3);
        let mid = bezier_point(p0, p1, p2, p3, 0.5);
        assert!(bezier_near(p0, p1, p2, p3, mid, 1.0));
        assert!(!bezier_near(p0, p1, p2, p3, mid + Vec2::new(0.0, 60.0), 5.0));
        // 제어점은 출력에서 오른쪽으로, 입력에서 왼쪽으로 뻗는다.
        assert!(p1.x > p0.x && p2.x < p3.x);
    }

    #[test]
    fn shape_label_shows_batch_symbol_or_question_mark() {
        let s = Shape::from_sample(&[10]);
        assert_eq!(shape_label(Some(&s)), "[B, 10]");
        assert_eq!(shape_label(None), "?");
    }

    #[test]
    fn connect_rules_block_cycles_taken_slots_and_duplicates() {
        let (mut g, ids) = graph_with(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Linear { out_features: 4, bias: true },
            LayerKind::Add,
        ]);
        let (i, l, a) = (ids[0], ids[1], ids[2]);
        assert!(connect_allowed(&g, i, Port::new(l, 0), None));
        g.add_edge(i, Port::new(l, 0)).unwrap();
        // 같은 슬롯을 다시 → 막힌다.
        assert!(!connect_allowed(&g, a, Port::new(l, 0), None));
        assert_eq!(connect_error(&g, a, Port::new(l, 0)), "이미 연결된 슬롯");
        // 같은 쌍 중복 → 막힌다 (Add 의 슬롯 1 로 i 를 두 번).
        g.add_edge(i, Port::new(a, 0)).unwrap();
        assert_eq!(connect_error(&g, i, Port::new(a, 1)), "이미 연결됨");
        // 순환 → 막힌다.
        g.add_edge(l, Port::new(a, 1)).unwrap();
        assert_eq!(connect_error(&g, a, Port::new(l, 0)), "이미 연결된 슬롯");
        // 슬롯 범위 밖.
        assert_eq!(connect_error(&g, i, Port::new(l, 3)), "입력 슬롯 없음");
        // 자기 자신.
        assert_eq!(connect_error(&g, l, Port::new(l, 0)), "자기 자신");
    }

    #[test]
    fn detaching_an_edge_frees_its_slot_for_reconnection() {
        let (mut g, ids) = graph_with(vec![
            LayerKind::Input { shape: vec![4] },
            LayerKind::Activation { act: Act::Relu },
            LayerKind::Linear { out_features: 2, bias: true },
        ]);
        let (i, act, lin) = (ids[0], ids[1], ids[2]);
        let e = g.add_edge(i, Port::new(lin, 0)).unwrap();
        // 그대로면 막히지만, 그 엣지를 떼어 끌고 있는 중이면 같은 슬롯에 다시 붙일 수 있다.
        assert!(!connect_allowed(&g, act, Port::new(lin, 0), None));
        assert!(connect_allowed(&g, act, Port::new(lin, 0), Some(e)));
    }

    #[test]
    fn cycle_is_blocked_even_through_a_longer_path() {
        let (mut g, ids) = graph_with(vec![
            LayerKind::Linear { out_features: 4, bias: true },
            LayerKind::Linear { out_features: 4, bias: true },
            LayerKind::Linear { out_features: 4, bias: true },
        ]);
        g.add_edge(ids[0], Port::new(ids[1], 0)).unwrap();
        g.add_edge(ids[1], Port::new(ids[2], 0)).unwrap();
        assert_eq!(connect_error(&g, ids[2], Port::new(ids[0], 0)), "순환 연결");
    }

    #[test]
    fn content_bounds_covers_every_node() {
        let (g, _) = graph_with(vec![LayerKind::Flatten, LayerKind::Flatten, LayerKind::Flatten]);
        let b = content_bounds(&g).unwrap();
        assert_eq!(b.min, Pos2::new(0.0, 100.0));
        assert_eq!(b.max, Pos2::new(440.0 + NODE_W, 100.0 + NODE_H));
        assert!(content_bounds(&Graph::default()).is_none());
    }

    #[test]
    fn camera_zoom_keeps_the_pivot_in_place() {
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let mut cam = Camera { pan: Vec2::new(10.0, 20.0), zoom: 1.0 };
        let pivot = Pos2::new(300.0, 200.0);
        let before = cam.to_world(viewport, pivot);
        cam.zoom_at(viewport, pivot, 1.4);
        let after = cam.to_world(viewport, pivot);
        assert!((before - after).length() < 1e-3, "{before:?} vs {after:?}");
        assert!((cam.zoom - 1.4).abs() < 1e-6);
        // 범위를 벗어나는 줌은 잘린다.
        cam.zoom_at(viewport, pivot, 100.0);
        assert!(cam.zoom <= ZOOM_MAX + 1e-6);
    }

    #[test]
    fn selection_helpers_keep_multi_and_primary_in_sync() {
        let model = ModelId::from_u128(1);
        let (g, ids) = graph_with(vec![LayerKind::Flatten, LayerKind::Flatten]);
        let mut c = CanvasState::new();
        c.set_selection(Selection::Node(model, ids[0]));
        assert_eq!(c.selected_nodes(), vec![ids[0]]);
        c.toggle_node(model, ids[1]);
        assert_eq!(c.selection_count(), 2);
        c.toggle_node(model, ids[1]);
        assert_eq!(c.selected_nodes(), vec![ids[0]]);
        // 문서에서 사라지면 선택도 정리된다.
        let mut g2 = g.clone();
        g2.remove_node(ids[0]);
        c.prune(&g2);
        assert_eq!(c.selection, Selection::Model(model));
        assert!(c.selected_nodes().is_empty());
    }
}
