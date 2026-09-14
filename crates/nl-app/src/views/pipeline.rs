//! 파이프라인 뷰: 파이프라인 목록 · 노드 캔버스 · 시험 실행 · 노드 인스펙터.
//!
//! 실행은 `nl_io::Runner` 가 별도 스레드에서 돌고 UI 는 이벤트만 받는다. 마우스·키보드 싱크는
//! **입력 무장** 토글이 켜져 있을 때만 실제 입력을 보낸다(기본 꺼짐). 실행 중 `Esc` 를 길게 누르면 즉시 멈춘다.

use super::{ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_SELECT, COL_SURFACE, COL_WARN, COL_WEAK};

/// 인스펙터 축소판의 최대 표시 크기(px). 실제 축소판은 최장변 160px 로 온다.
const INSPECTOR_PREVIEW_MAX: f32 = 200.0;
use crate::canvas::{Selection, SelectionState};
use crate::pcanvas::{kind_summary, LiveView, PipelineAction, PipelineCanvas};
use eframe::egui::{self, DragValue, RichText};
use nl_core::pipeline::{InputAction, Logic, MouseButton, PNodeKind, Region, Sink, Source};
use nl_core::{LinkId, ModelId, Op, PNodeId, PayloadId, Pipeline, PipelineId, WidgetId};
use nl_engine::Value;
use std::collections::BTreeMap;

/// 수동 입력 값의 해석 방식.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManualKind {
    #[default]
    Number,
    Text,
    Json,
}

impl ManualKind {
    pub const ALL: [ManualKind; 3] = [ManualKind::Number, ManualKind::Text, ManualKind::Json];
    pub fn label(&self) -> &'static str {
        match self {
            ManualKind::Number => "숫자",
            ManualKind::Text => "텍스트",
            ManualKind::Json => "JSON",
        }
    }
}

/// 입력 문자열을 `Value` 로. 실패하면 이유를 돌려준다.
pub fn parse_manual(kind: ManualKind, text: &str) -> Result<Value, String> {
    match kind {
        ManualKind::Number => {
            // "1, 2, 3" 처럼 여러 개면 벡터로 보낸다 (모델 입력이 벡터인 경우가 흔하다).
            let parts: Vec<&str> = text.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                return Err("숫자를 입력하세요".into());
            }
            let nums: Result<Vec<f32>, _> = parts.iter().map(|p| p.parse::<f32>()).collect();
            let nums = nums.map_err(|_| format!("숫자로 읽을 수 없습니다: {text:?}"))?;
            if nums.len() == 1 {
                Ok(Value::Number(nums[0] as f64))
            } else {
                Ok(Value::Numbers(nums))
            }
        }
        ManualKind::Text => Ok(Value::Text(text.to_string())),
        ManualKind::Json => serde_json::from_str(text)
            .map(Value::Json)
            .map_err(|e| format!("JSON 오류: {e}")),
    }
}

/// 뷰가 프레임 사이에 들고 있는 상태.
#[derive(Default)]
pub struct PipelineViewState {
    pub manual_kind: ManualKind,
    pub manual_text: String,
    /// 헤더 표에 새로 추가할 줄.
    pub new_header: (String, String),
    /// `Logic::Map` 표에 새로 추가할 줄.
    pub new_map: (String, String),
}

#[derive(Default)]
pub struct PipelineViewOut {
    pub canvas: Vec<PipelineAction>,
    pub actions: Vec<ViewAction>,
}

/// 실행 상태 (툴바 표시용).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Running,
}

pub fn show(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    canvas: &mut PipelineCanvas,
    live: &LiveView,
    run: RunState,
    arm_input: bool,
    sel: &mut SelectionState,
) -> PipelineViewOut {
    let mut out = PipelineViewOut::default();
    let active = ctx.active_pipeline();

    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("파이프라인").color(COL_WEAK));
                let label = active
                    .and_then(|p| ctx.project.pipelines.get(&p))
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "(없음)".into());
                egui::ComboBox::from_id_salt("pipeline-picker")
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        for (id, p) in &ctx.project.pipelines {
                            if ui.selectable_label(active == Some(*id), &p.name).clicked() {
                                out.actions.push(ViewAction::Select(Selection::Pipeline(*id)));
                            }
                        }
                    });
                if ui.button("＋ 새 파이프라인").clicked() {
                    let pl = Pipeline::new(format!("파이프라인 {}", ctx.project.pipelines.len() + 1));
                    let id = pl.id;
                    out.actions.push(ViewAction::Ops(vec![Op::UpsertPipelineMeta {
                        id,
                        name: pl.name,
                        tick_hz: pl.tick_hz,
                    }]));
                    out.actions.push(ViewAction::Select(Selection::Pipeline(id)));
                }
                let Some(pid) = active else { return };
                if ui.button("⛶ 전체 보기").on_hover_text("F").clicked() {
                    canvas.request_fit();
                }
                ui.separator();

                // 틱 속도.
                if let Some(pl) = ctx.project.pipelines.get(&pid) {
                    let mut hz = pl.tick_hz;
                    ui.label(RichText::new("틱").color(COL_WEAK));
                    if ui
                        .add(DragValue::new(&mut hz).range(0.05..=240.0).speed(0.5).suffix(" Hz"))
                        .on_hover_text("소스가 시간 기반이 아닐 때의 최대 틱 속도")
                        .changed()
                    {
                        out.actions.push(ViewAction::Edit(vec![Op::UpsertPipelineMeta {
                            id: pid,
                            name: pl.name.clone(),
                            tick_hz: hz,
                        }]));
                    }
                }
                ui.separator();

                match run {
                    RunState::Idle => {
                        if ui.button(RichText::new("▶ 시험 실행").color(COL_OK)).clicked() {
                            out.actions.push(ViewAction::StartPipeline(pid));
                        }
                    }
                    RunState::Running => {
                        if ui
                            .button(RichText::new("⏹ 정지").color(COL_ERROR))
                            .on_hover_text("Esc 를 길게 눌러도 멈춥니다")
                            .clicked()
                        {
                            out.actions.push(ViewAction::StopPipeline);
                        }
                        ui.label(RichText::new("● 실행 중").color(COL_SELECT));
                    }
                }
                let mut armed = arm_input;
                let resp = ui.checkbox(&mut armed, "입력 무장").on_hover_text(
                    "켜면 마우스/키보드 싱크가 실제 입력을 보냅니다. 실행 중 Esc 를 길게 누르면 즉시 정지합니다.",
                );
                if resp.changed() {
                    out.actions.push(ViewAction::SetArmInput(armed));
                }
                if armed {
                    ui.label(RichText::new("⚠ 실제 입력").color(COL_WARN));
                }
            });
        });
    ui.separator();

    match active {
        Some(pid) => {
            let pl = &ctx.project.pipelines[&pid];
            out.canvas = canvas.show(ui, pid, pl, ctx.project, live, sel);
        }
        None => {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.32);
                ui.label(RichText::new("파이프라인이 없습니다").size(18.0));
                ui.add_space(6.0);
                ui.label(
                    RichText::new("소스 → 모델 → 싱크 로 바깥 세계와 모델을 잇습니다.")
                        .color(COL_WEAK)
                        .size(12.0),
                );
                ui.add_space(8.0);
                if ui.button("＋ 첫 파이프라인 만들기").clicked() {
                    let pl = Pipeline::new("파이프라인 1");
                    let id = pl.id;
                    out.actions.push(ViewAction::Ops(vec![Op::UpsertPipelineMeta {
                        id,
                        name: pl.name,
                        tick_hz: pl.tick_hz,
                    }]));
                    out.actions.push(ViewAction::Select(Selection::Pipeline(id)));
                }
            });
        }
    }
    out
}

// ───────────────────────────── 인스펙터 ─────────────────────────────

/// 파이프라인 노드 하나의 속성 편집. 편집은 burst(`Edit`), 구조 변화는 `Ops` 로 나간다.
pub fn inspect_node(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    state: &mut PipelineViewState,
    pid: PipelineId,
    nid: PNodeId,
    live: &LiveView,
) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    let Some(pl) = ctx.project.pipelines.get(&pid) else {
        return actions;
    };
    let Some(node) = pl.nodes.get(&nid) else {
        ui.label(RichText::new("노드가 없습니다").color(COL_ERROR));
        return actions;
    };
    let mut next = node.clone();
    let mut changed = false;

    ui.label(RichText::new(node.kind.label()).size(15.0).strong());
    ui.label(
        RichText::new(kind_summary(&node.kind, ctx.project))
            .color(COL_WEAK)
            .size(11.5),
    );
    ui.separator();

    ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
    changed |= ui
        .add(egui::TextEdit::singleline(&mut next.name).desired_width(f32::INFINITY))
        .changed();

    ui.add_space(6.0);
    match &mut next.kind {
        PNodeKind::Source { source } => {
            changed |= source_editor(ui, source, ctx, &mut actions, pid);
            if matches!(source, Source::Manual) {
                manual_sender(ui, state, nid, &mut actions, live.running);
            }
            if let Source::HttpServer { bind, path, token, .. } = source {
                http_server_tester(ui, bind, path, token.as_deref(), live.running);
            }
        }
        PNodeKind::Model { model, payload } => changed |= model_editor(ui, model, payload, ctx),
        PNodeKind::Logic { logic } => changed |= logic_editor(ui, logic, state),
        PNodeKind::Sink { sink } => changed |= sink_editor(ui, sink, ctx, &mut actions, pid, state),
    }

    ui.add_space(8.0);
    ui.label(RichText::new("위치").color(COL_WEAK).size(11.5));
    ui.horizontal(|ui| {
        changed |= ui
            .add(DragValue::new(&mut next.pos[0]).prefix("x ").speed(1.0))
            .changed();
        changed |= ui
            .add(DragValue::new(&mut next.pos[1]).prefix("y ").speed(1.0))
            .changed();
    });

    // 실행 중이면 마지막 값·오류·이미지 축소판.
    let preview = live.previews.get(&nid);
    if live.running || live.values.contains_key(&nid) || live.errors.contains_key(&nid) || preview.is_some() {
        ui.add_space(8.0);
        ui.separator();
        match live.values.get(&nid) {
            Some(v) => super::kv(ui, "마지막 값", v.clone()),
            None if preview.is_none() => super::kv(ui, "마지막 값", "—"),
            None => {}
        }
        if let Some(p) = preview {
            ui.label(RichText::new("마지막 이미지").color(COL_WEAK).size(11.0));
            let size = p.fit(ui.available_width().min(INSPECTOR_PREVIEW_MAX));
            ui.add(egui::Image::new(&p.texture).fit_to_exact_size(size));
            ui.label(
                RichText::new(format!("축소판 {}×{} — 원본이 아닙니다", p.size.0, p.size.1))
                    .color(COL_WEAK)
                    .size(10.5),
            );
        }
        if let Some(e) = live.errors.get(&nid) {
            ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
        }
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if ui.button("⊘ 연결 끊기").clicked() {
            let ops: Vec<Op> = pl
                .links
                .values()
                .filter(|l| l.from == nid || l.to == nid)
                .map(|l| Op::DeleteLink {
                    pipeline: pid,
                    id: l.id,
                })
                .collect();
            actions.push(ViewAction::Ops(ops));
        }
        if ui.button(RichText::new("🗑 노드 삭제").color(COL_ERROR)).clicked() {
            actions.push(ViewAction::Ops(vec![Op::DeletePNode { pipeline: pid, id: nid }]));
            actions.push(ViewAction::Select(Selection::Pipeline(pid)));
        }
    });

    if changed {
        actions.push(ViewAction::Edit(vec![Op::UpsertPNode {
            pipeline: pid,
            node: next,
        }]));
    }
    actions
}

pub fn inspect_link(ui: &mut egui::Ui, ctx: &ViewCtx, pid: PipelineId, lid: LinkId) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    let Some(pl) = ctx.project.pipelines.get(&pid) else {
        return actions;
    };
    let Some(link) = pl.links.get(&lid) else {
        ui.label(RichText::new("연결이 없습니다").color(COL_ERROR));
        return actions;
    };
    let name = |id: PNodeId| {
        pl.nodes
            .get(&id)
            .map(crate::pcanvas::node_title)
            .unwrap_or_else(|| "?".into())
    };
    ui.label(RichText::new("연결").size(15.0).strong());
    ui.separator();
    super::kv(ui, "출발", name(link.from));
    super::kv(ui, "도착", name(link.to));
    ui.add_space(8.0);
    if ui.button(RichText::new("🗑 연결 삭제").color(COL_ERROR)).clicked() {
        actions.push(ViewAction::Ops(vec![Op::DeleteLink { pipeline: pid, id: lid }]));
        actions.push(ViewAction::Select(Selection::Pipeline(pid)));
    }
    actions
}

// ── 소스 ────────────────────────────────────────────────────────────

fn source_editor(
    ui: &mut egui::Ui,
    source: &mut Source,
    ctx: &ViewCtx,
    actions: &mut Vec<ViewAction>,
    pid: PipelineId,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new("소스 종류").color(COL_WEAK).size(11.5));
    egui::ComboBox::from_id_salt("src-kind")
        .selected_text(source_label(source))
        .show_ui(ui, |ui| {
            for s in crate::pcanvas::source_palette() {
                let same = std::mem::discriminant(source) == std::mem::discriminant(&s);
                if ui.selectable_label(same, source_label(&s)).clicked() && !same {
                    *source = s;
                    changed = true;
                }
            }
        });
    ui.add_space(4.0);
    match source {
        Source::Manual => {
            ui.label(
                RichText::new("빌더에서 값을 직접 넣는 시험용 소스입니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        }
        Source::StdinJson => {
            ui.label(RichText::new("표준 입력 한 줄 = JSON 하나.").color(COL_WEAK).size(11.0));
        }
        Source::Timer { interval_ms } => {
            ui.horizontal(|ui| {
                ui.label("간격");
                changed |= ui
                    .add(DragValue::new(interval_ms).range(1..=3_600_000).suffix(" ms"))
                    .changed();
            });
        }
        Source::File { path, interval_ms } => {
            ui.label(RichText::new("파일 경로").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY))
                .changed();
            ui.horizontal(|ui| {
                ui.label("다시 읽는 간격");
                changed |= ui
                    .add(DragValue::new(interval_ms).range(1..=3_600_000).suffix(" ms"))
                    .changed();
            });
        }
        Source::WebSocket { url } => {
            ui.label(RichText::new("주소").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY))
                .changed();
        }
        Source::HttpPoll {
            url,
            interval_ms,
            headers,
        } => {
            ui.label(RichText::new("주소").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY))
                .changed();
            ui.horizontal(|ui| {
                ui.label("간격");
                changed |= ui
                    .add(DragValue::new(interval_ms).range(1..=3_600_000).suffix(" ms"))
                    .changed();
            });
            changed |= headers_editor(ui, headers, "src");
        }
        Source::ScreenCapture { region, fps } => {
            ui.horizontal(|ui| {
                ui.label("초당 프레임");
                changed |= ui.add(DragValue::new(fps).range(0.1..=60.0).speed(0.5)).changed();
            });
            changed |= region_editor(ui, region, ctx);
            ui.add_space(6.0);
            // 실제로 무엇이 찍히는지, 어떤 백엔드로 얼마나 나오는지는 한 장 찍어 봐야 안다.
            super::data::shot_block(ui, ctx, *region, actions);
        }
        Source::GuiEvent { widget } => {
            changed |= widget_picker(ui, widget, ctx, "이 위젯의 이벤트를 받습니다", "src-widget");
        }
        // `tls` 는 아직 인스펙터에 없다 — 인증서 파일 선택 UI 는 별도 작업이다.
        // 그때까지 프로젝트 파일에 적힌 값은 그대로 보존된다(여기서 건드리지 않으므로).
        Source::HttpServer { bind, path, token, .. } => {
            ui.label(RichText::new("주소:포트").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(bind).desired_width(f32::INFINITY))
                .changed();
            let loopback = nl_core::pipeline::is_loopback_bind(bind);
            match bind.parse::<std::net::SocketAddr>() {
                Ok(_) if loopback => {
                    ui.label(
                        RichText::new("✔ 루프백 — 이 컴퓨터에서만 닿습니다")
                            .color(COL_OK)
                            .size(11.0),
                    );
                }
                Ok(_) => {
                    ui.label(
                        RichText::new("⚠ 바깥에서 닿을 수 있는 주소입니다 — 방화벽을 확인하세요")
                            .color(COL_WARN)
                            .size(11.0),
                    );
                }
                Err(e) => {
                    ui.label(
                        RichText::new(format!("✖ 주소를 읽을 수 없습니다: {e}"))
                            .color(COL_ERROR)
                            .size(11.0),
                    );
                }
            }
            ui.label(RichText::new("경로").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY))
                .changed();
            if !path.starts_with('/') {
                ui.label(RichText::new("경로는 / 로 시작해야 합니다").color(COL_WARN).size(11.0));
            }
            changed |= token_editor(ui, token, loopback);
            ui.label(
                RichText::new("요청 본문이 값이 됩니다. 응답은 같은 파이프라인의 'HTTP 응답' 싱크가 돌려줍니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        }
    }
    let _ = pid;
    changed
}

/// HTTP 서버 노드의 토큰 편집기.
///
/// 토큰은 요청 헤더 `X-NL-Token` 으로 온다. 루프백에 묶인 서버는 이 컴퓨터에서만 닿으므로 비워 둬도
/// 되지만, 바깥에서 닿는 주소는 토큰이 없으면 누구나 파이프라인을 구동할 수 있다 — `nl_core::validate`
/// 가 그 조합을 오류로 잡고 여기서도 붉게 알린다.
fn token_editor(ui: &mut egui::Ui, token: &mut Option<String>, loopback: bool) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    ui.label(RichText::new("토큰").color(COL_WEAK).size(11.0));

    // 보이기/숨기기는 이 노드의 화면 상태일 뿐이라 문서에 남기지 않는다.
    let eye_id = ui.id().with("token-visible");
    let mut visible = ui.data_mut(|d| *d.get_temp_mut_or_default::<bool>(eye_id));

    let mut text = token.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(&mut text)
            .password(!visible)
            .desired_width(200.0);
        if ui.add(edit).changed() {
            *token = if text.trim().is_empty() {
                None
            } else {
                Some(text.clone())
            };
            changed = true;
        }
        let eye = if visible { "숨기기" } else { "보기" };
        if ui.small_button(eye).clicked() {
            visible = !visible;
            ui.data_mut(|d| d.insert_temp(eye_id, visible));
        }
    });
    ui.horizontal(|ui| {
        if ui
            .small_button("새로 만들기")
            .on_hover_text("무작위 토큰을 만들어 채웁니다")
            .clicked()
        {
            *token = Some(nl_core::pipeline::new_token());
            changed = true;
        }
        if ui
            .small_button("비우기")
            .on_hover_text("루프백에 묶은 서버만 토큰 없이 열 수 있습니다")
            .clicked()
            && token.is_some()
        {
            *token = None;
            changed = true;
        }
    });

    let empty = token.as_ref().is_none_or(|t| t.trim().is_empty());
    if empty && !loopback {
        ui.label(
            RichText::new("✖ 바깥에서 닿는 주소인데 토큰이 없습니다 — 누구나 이 파이프라인을 구동할 수 있습니다")
                .color(COL_ERROR)
                .size(11.0),
        );
    } else if empty {
        ui.label(
            RichText::new("토큰 없음 — 루프백이라 이 컴퓨터에서만 닿습니다")
                .color(COL_WEAK)
                .size(11.0),
        );
    }
    ui.label(
        RichText::new(
            "토큰은 프로젝트 파일에 그대로 저장됩니다. 배포한 앱에서는 NL_HTTP_TOKEN 환경 변수로 덮어쓸 수 있습니다.",
        )
        .color(COL_WEAK)
        .size(10.5),
    );
    changed
}

/// 이 서버 노드를 부르는 curl 한 줄.
pub fn curl_example(bind: &str, path: &str, token: Option<&str>) -> String {
    let host = if bind.starts_with("0.0.0.0") {
        bind.replacen("0.0.0.0", "127.0.0.1", 1)
    } else {
        bind.to_string()
    };
    // 토큰이 있으면 헤더가 필수다 — 빠뜨린 예시를 복사해 붙이면 401 만 보게 된다.
    let auth = match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => format!(" -H 'X-NL-Token: {t}'"),
        None => String::new(),
    };
    format!("curl -X POST http://{host}{path}{auth} -d '[0,1]'")
}

pub fn source_label(s: &Source) -> &'static str {
    PNodeKind::Source { source: s.clone() }.label()
}

/// 모니터 목록 콤보 + 상대 좌표 + "모니터 전체". 녹화 폼도 같은 편집기를 쓴다.
pub(crate) fn region_editor(ui: &mut egui::Ui, region: &mut Region, ctx: &ViewCtx) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    ui.label(RichText::new("모니터").color(COL_WEAK).size(11.0));
    let current = ctx.monitors.iter().find(|m| m.index == region.monitor);
    let label = match current {
        Some(m) => format!(
            "{} · {}×{}{}",
            m.name,
            m.width,
            m.height,
            if m.primary { " (주)" } else { "" }
        ),
        None => format!("모니터 {}", region.monitor),
    };
    egui::ComboBox::from_id_salt("region-monitor")
        .selected_text(label)
        .show_ui(ui, |ui| {
            if ctx.monitors.is_empty() {
                ui.label(RichText::new("목록 없음").weak());
            }
            for m in ctx.monitors {
                let text = format!(
                    "{} · {}×{}{}",
                    m.name,
                    m.width,
                    m.height,
                    if m.primary { " (주)" } else { "" }
                );
                if ui.selectable_label(region.monitor == m.index, text).clicked() && region.monitor != m.index {
                    region.monitor = m.index;
                    changed = true;
                }
            }
        });
    if let Some(e) = ctx.monitors_error {
        ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.0));
    }
    ui.label(
        RichText::new("영역 (모니터 왼쪽 위 기준 물리 픽셀)")
            .color(COL_WEAK)
            .size(11.0),
    );
    ui.horizontal(|ui| {
        changed |= ui.add(DragValue::new(&mut region.x).prefix("x ").speed(2.0)).changed();
        changed |= ui.add(DragValue::new(&mut region.y).prefix("y ").speed(2.0)).changed();
    });
    ui.horizontal(|ui| {
        changed |= ui
            .add(DragValue::new(&mut region.width).prefix("w ").speed(2.0))
            .changed();
        changed |= ui
            .add(DragValue::new(&mut region.height).prefix("h ").speed(2.0))
            .changed();
    });
    ui.horizontal(|ui| {
        if ui
            .button("모니터 전체")
            .on_hover_text("폭을 0 으로 두면 그 모니터 전체를 찍습니다")
            .clicked()
        {
            region.x = 0;
            region.y = 0;
            region.width = 0;
            region.height = 0;
            changed = true;
        }
        if let Some(m) = current {
            if ui.button("이 모니터 크기로").clicked() {
                region.x = 0;
                region.y = 0;
                region.width = m.width;
                region.height = m.height;
                changed = true;
            }
        }
    });
    if region.width == 0 {
        ui.label(RichText::new("모니터 전체를 캡처합니다").color(COL_OK).size(11.0));
    }
    changed
}

fn widget_picker(ui: &mut egui::Ui, widget: &mut WidgetId, ctx: &ViewCtx, hint: &str, salt: &str) -> bool {
    let mut changed = false;
    ui.label(RichText::new("위젯").color(COL_WEAK).size(11.0));
    let label = ctx
        .project
        .gui
        .widgets
        .get(widget)
        .map(|w| format!("{} · {}", w.kind.label(), widget.short()))
        .unwrap_or_else(|| "(고르세요)".into());
    egui::ComboBox::from_id_salt(salt)
        .selected_text(label)
        .show_ui(ui, |ui| {
            if ctx.project.gui.widgets.is_empty() {
                ui.label(RichText::new("GUI 뷰에서 위젯을 먼저 만드세요").weak());
            }
            for (id, w) in &ctx.project.gui.widgets {
                let text = format!("{} · {}", w.kind.label(), id.short());
                if ui.selectable_label(widget == id, text).clicked() && widget != id {
                    *widget = *id;
                    changed = true;
                }
            }
        });
    ui.label(RichText::new(hint).color(COL_WEAK).size(11.0));
    changed
}

/// `Source::Manual` 노드에 값을 보내는 칸.
fn manual_sender(
    ui: &mut egui::Ui,
    state: &mut PipelineViewState,
    nid: PNodeId,
    actions: &mut Vec<ViewAction>,
    running: bool,
) {
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(8)
        .corner_radius(4)
        .show(ui, |ui| {
            ui.label(RichText::new("값 보내기").strong());
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("manual-kind")
                    .selected_text(state.manual_kind.label())
                    .show_ui(ui, |ui| {
                        for k in ManualKind::ALL {
                            if ui.selectable_label(state.manual_kind == k, k.label()).clicked() {
                                state.manual_kind = k;
                            }
                        }
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut state.manual_text)
                        .desired_width(f32::INFINITY)
                        .hint_text(match state.manual_kind {
                            ManualKind::Number => "1  또는  0, 1",
                            ManualKind::Text => "보낼 문자열",
                            ManualKind::Json => "{\"a\": 1}",
                        }),
                );
            });
            let parsed = parse_manual(state.manual_kind, &state.manual_text);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(running && parsed.is_ok(), |ui| {
                    if ui.button("▶ 보내기").clicked() {
                        if let Ok(v) = &parsed {
                            actions.push(ViewAction::SendManual {
                                node: nid,
                                value: v.clone(),
                            });
                        }
                    }
                });
                if !running {
                    ui.label(
                        RichText::new("실행 중일 때만 보낼 수 있습니다")
                            .color(COL_WEAK)
                            .size(11.0),
                    );
                } else if let Err(e) = &parsed {
                    ui.label(RichText::new(e).color(COL_WARN).size(11.0));
                }
            });
        });
}

/// 실행 중인 HTTP 서버 노드를 바깥에서 불러 보는 칸.
fn http_server_tester(ui: &mut egui::Ui, bind: &str, path: &str, token: Option<&str>, running: bool) {
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(8)
        .corner_radius(4)
        .show(ui, |ui| {
            ui.label(RichText::new("바깥에서 불러 보기").strong());
            let cmd = curl_example(bind, path, token);
            ui.label(RichText::new(&cmd).size(11.0).monospace());
            ui.horizontal(|ui| {
                if ui.button("복사").clicked() {
                    ui.ctx().copy_text(cmd.clone());
                }
                if running {
                    ui.label(RichText::new("● 서버가 열려 있습니다").color(COL_OK).size(11.0));
                } else {
                    ui.label(RichText::new("시험 실행 중에만 열립니다").color(COL_WEAK).size(11.0));
                }
            });
        });
}

// ── 모델 ────────────────────────────────────────────────────────────

fn model_editor(ui: &mut egui::Ui, model: &mut ModelId, payload: &mut Option<PayloadId>, ctx: &ViewCtx) -> bool {
    let mut changed = false;
    ui.label(RichText::new("모델").color(COL_WEAK).size(11.0));
    let label = ctx
        .project
        .models
        .get(model)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "(없는 모델)".into());
    egui::ComboBox::from_id_salt("pnode-model")
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (id, m) in &ctx.project.models {
                if ui.selectable_label(model == id, &m.name).clicked() && model != id {
                    *model = *id;
                    changed = true;
                }
            }
        });
    match ctx.project.models.get(model) {
        None => {
            ui.label(
                RichText::new("✖ 프로젝트에 없는 모델입니다")
                    .color(COL_ERROR)
                    .size(11.0),
            );
        }
        Some(m) if m.weights.is_none() => {
            ui.label(
                RichText::new("⚠ 학습된 가중치가 없습니다 — 무작위 초기값으로 추론합니다")
                    .color(COL_WARN)
                    .size(11.0),
            );
        }
        Some(m) => {
            ui.label(
                RichText::new(format!(
                    "✔ 가중치 {}",
                    super::short_path(m.weights.as_deref().unwrap_or(""))
                ))
                .color(COL_OK)
                .size(11.0),
            );
        }
    }
    ui.add_space(4.0);
    ui.label(RichText::new("페이로드").color(COL_WEAK).size(11.0));
    let plabel = payload
        .and_then(|p| ctx.project.payloads.get(&p))
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "(모델 기본값)".into());
    egui::ComboBox::from_id_salt("pnode-payload")
        .selected_text(plabel)
        .show_ui(ui, |ui| {
            if ui.selectable_label(payload.is_none(), "(모델 기본값)").clicked() && payload.is_some() {
                *payload = None;
                changed = true;
            }
            for (id, p) in &ctx.project.payloads {
                if ui.selectable_label(*payload == Some(*id), &p.name).clicked() && *payload != Some(*id) {
                    *payload = Some(*id);
                    changed = true;
                }
            }
        });
    changed
}

// ── 로직 ────────────────────────────────────────────────────────────

fn logic_editor(ui: &mut egui::Ui, logic: &mut Logic, state: &mut PipelineViewState) -> bool {
    let mut changed = false;
    ui.label(RichText::new("로직 종류").color(COL_WEAK).size(11.5));
    egui::ComboBox::from_id_salt("logic-kind")
        .selected_text(logic_label(logic))
        .show_ui(ui, |ui| {
            for l in crate::pcanvas::logic_palette() {
                let same = std::mem::discriminant(logic) == std::mem::discriminant(&l);
                if ui.selectable_label(same, logic_label(&l)).clicked() && !same {
                    *logic = l;
                    changed = true;
                }
            }
        });
    ui.add_space(4.0);
    match logic {
        Logic::Threshold { value } => {
            ui.horizontal(|ui| {
                ui.label("임계값 ≥");
                changed |= ui.add(DragValue::new(value).speed(0.01)).changed();
            });
        }
        Logic::Debounce { ms } => {
            ui.horizontal(|ui| {
                ui.label("같은 값 무시");
                changed |= ui.add(DragValue::new(ms).range(1..=600_000).suffix(" ms")).changed();
            });
        }
        Logic::Select { index } => {
            ui.horizontal(|ui| {
                ui.label("벡터 인덱스");
                changed |= ui.add(DragValue::new(index).range(0..=100_000)).changed();
            });
        }
        Logic::Majority { window } => {
            ui.horizontal(|ui| {
                ui.label("최근 값 개수");
                changed |= ui.add(DragValue::new(window).range(1..=1000)).changed();
            });
        }
        Logic::Map { table } => changed |= map_table_editor(ui, table, state),
    }
    changed
}

pub fn logic_label(l: &Logic) -> &'static str {
    PNodeKind::Logic { logic: l.clone() }.label()
}

fn map_table_editor(ui: &mut egui::Ui, table: &mut BTreeMap<i64, i64>, state: &mut PipelineViewState) -> bool {
    let mut changed = false;
    ui.label(RichText::new("정수 → 정수 치환").color(COL_WEAK).size(11.0));
    let mut remove: Option<i64> = None;
    egui::Grid::new("logic-map")
        .num_columns(3)
        .spacing([8.0, 3.0])
        .show(ui, |ui| {
            for (k, v) in table.iter_mut() {
                ui.label(k.to_string());
                changed |= ui.add(DragValue::new(v).speed(1.0)).changed();
                if ui.small_button("✖").clicked() {
                    remove = Some(*k);
                }
                ui.end_row();
            }
        });
    if let Some(k) = remove {
        table.remove(&k);
        changed = true;
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.new_map.0)
                .desired_width(60.0)
                .hint_text("입력"),
        );
        ui.label("→");
        ui.add(
            egui::TextEdit::singleline(&mut state.new_map.1)
                .desired_width(60.0)
                .hint_text("출력"),
        );
        let parsed = state
            .new_map
            .0
            .trim()
            .parse::<i64>()
            .ok()
            .zip(state.new_map.1.trim().parse::<i64>().ok());
        ui.add_enabled_ui(parsed.is_some(), |ui| {
            if ui.small_button("＋").clicked() {
                if let Some((k, v)) = parsed {
                    table.insert(k, v);
                    state.new_map = (String::new(), String::new());
                    changed = true;
                }
            }
        });
    });
    changed
}

// ── 싱크 ────────────────────────────────────────────────────────────

fn sink_editor(
    ui: &mut egui::Ui,
    sink: &mut Sink,
    ctx: &ViewCtx,
    _actions: &mut [ViewAction],
    pid: PipelineId,
    state: &mut PipelineViewState,
) -> bool {
    let mut changed = false;
    ui.label(RichText::new("싱크 종류").color(COL_WEAK).size(11.5));
    egui::ComboBox::from_id_salt("sink-kind")
        .selected_text(sink_label(sink))
        .show_ui(ui, |ui| {
            for s in crate::pcanvas::sink_palette() {
                let same = std::mem::discriminant(sink) == std::mem::discriminant(&s);
                if ui.selectable_label(same, sink_label(&s)).clicked() && !same {
                    *sink = s;
                    changed = true;
                }
            }
        });
    ui.add_space(4.0);
    match sink {
        Sink::Log => {
            ui.label(
                RichText::new("값을 로그로 남깁니다 (하단 도크의 로그 탭).")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        }
        Sink::StdoutJson => {
            ui.label(
                RichText::new("표준 출력에 JSON 한 줄씩 씁니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        }
        Sink::GuiWidget { widget } => {
            changed |= widget_picker(ui, widget, ctx, "이 위젯에 값을 표시합니다", "sink-widget");
        }
        Sink::File { path, append } => {
            ui.label(RichText::new("파일 경로").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY))
                .changed();
            changed |= ui.checkbox(append, "덧붙이기").changed();
        }
        Sink::WebSocketSend { url } => {
            ui.label(RichText::new("주소").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY))
                .changed();
        }
        Sink::HttpCall {
            method,
            url,
            headers,
            body_template,
        } => {
            ui.horizontal(|ui| {
                ui.label("메서드");
                egui::ComboBox::from_id_salt("sink-method")
                    .selected_text(method.clone())
                    .show_ui(ui, |ui| {
                        for m in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                            if ui.selectable_label(method == m, m).clicked() && method != m {
                                *method = m.to_string();
                                changed = true;
                            }
                        }
                    });
            });
            ui.label(RichText::new("주소").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(egui::TextEdit::singleline(url).desired_width(f32::INFINITY))
                .changed();
            changed |= headers_editor(ui, headers, "sink");
            ui.label(
                RichText::new("본문 틀 ({{value}} 가 입력 값으로 바뀝니다)")
                    .color(COL_WEAK)
                    .size(11.0),
            );
            changed |= ui
                .add(
                    egui::TextEdit::multiline(body_template)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY),
                )
                .changed();
        }
        Sink::HttpReply { server } => {
            ui.label(RichText::new("응답할 서버 노드").color(COL_WEAK).size(11.0));
            let servers: Vec<(PNodeId, String)> = ctx
                .project
                .pipelines
                .get(&pid)
                .map(|pl| {
                    pl.nodes
                        .values()
                        .filter(|n| {
                            matches!(
                                n.kind,
                                PNodeKind::Source {
                                    source: Source::HttpServer { .. }
                                }
                            )
                        })
                        .map(|n| (n.id, crate::pcanvas::node_title(n)))
                        .collect()
                })
                .unwrap_or_default();
            let label = servers
                .iter()
                .find(|(id, _)| id == server)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| "(고르세요)".into());
            egui::ComboBox::from_id_salt("sink-http-server")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    if servers.is_empty() {
                        ui.label(RichText::new("같은 파이프라인에 HTTP 서버 소스가 없습니다").weak());
                    }
                    for (id, name) in &servers {
                        if ui.selectable_label(server == id, name).clicked() && server != id {
                            *server = *id;
                            changed = true;
                        }
                    }
                });
            if servers.iter().any(|(id, _)| id == server) {
                ui.label(RichText::new("✔ 서버 노드와 짝지어졌습니다").color(COL_OK).size(11.0));
            } else {
                ui.label(
                    RichText::new("✖ 짝이 없으면 요청이 타임아웃까지 기다립니다")
                        .color(COL_ERROR)
                        .size(11.0),
                );
            }
        }
        Sink::MouseKeyboard {
            actions: list,
            cooldown_ms,
        } => {
            ui.horizontal(|ui| {
                ui.label("쿨다운");
                changed |= ui
                    .add(DragValue::new(cooldown_ms).range(0..=600_000).suffix(" ms"))
                    .changed();
            });
            ui.label(
                RichText::new("입력 값(정수 인덱스)에 해당하는 액션을 실행합니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
            changed |= actions_editor(ui, list, state);
        }
    }
    changed
}

pub fn sink_label(s: &Sink) -> &'static str {
    PNodeKind::Sink { sink: s.clone() }.label()
}

fn headers_editor(ui: &mut egui::Ui, headers: &mut BTreeMap<String, String>, salt: &str) -> bool {
    let mut changed = false;
    ui.add_space(4.0);
    ui.label(RichText::new("헤더").color(COL_WEAK).size(11.0));
    let mut remove: Option<String> = None;
    egui::Grid::new(format!("{salt}-headers"))
        .num_columns(3)
        .spacing([6.0, 3.0])
        .show(ui, |ui| {
            for (k, v) in headers.iter_mut() {
                ui.label(RichText::new(k.as_str()).size(11.0));
                changed |= ui.add(egui::TextEdit::singleline(v).desired_width(140.0)).changed();
                if ui.small_button("✖").clicked() {
                    remove = Some(k.clone());
                }
                ui.end_row();
            }
        });
    if let Some(k) = remove {
        headers.remove(&k);
        changed = true;
    }
    // 새 헤더 줄은 egui 메모리에 두어 노드마다 따로 유지된다.
    let id = ui.id().with((salt, "new-header"));
    let mut draft: (String, String) = ui.ctx().data(|d| d.get_temp(id)).unwrap_or_default();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut draft.0)
                .desired_width(90.0)
                .hint_text("이름"),
        );
        ui.add(
            egui::TextEdit::singleline(&mut draft.1)
                .desired_width(110.0)
                .hint_text("값"),
        );
        let ok = !draft.0.trim().is_empty();
        ui.add_enabled_ui(ok, |ui| {
            if ui.small_button("＋").clicked() {
                headers.insert(draft.0.trim().to_string(), draft.1.clone());
                draft = (String::new(), String::new());
                changed = true;
            }
        });
    });
    ui.ctx().data_mut(|d| d.insert_temp(id, draft));
    changed
}

// ── 입력 액션 ───────────────────────────────────────────────────────

/// 액션 목록 편집기. 인덱스가 모델 출력(클래스 번호)에 대응한다.
fn actions_editor(ui: &mut egui::Ui, list: &mut Vec<InputAction>, state: &mut PipelineViewState) -> bool {
    let mut changed = false;
    let mut remove: Option<usize> = None;
    let mut swap: Option<(usize, usize)> = None;
    let len = list.len();
    for (i, action) in list.iter_mut().enumerate() {
        egui::Frame::NONE
            .fill(COL_SURFACE)
            .inner_margin(6)
            .corner_radius(3)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("[{i}]")).color(COL_WEAK).size(11.0));
                    ui.label(RichText::new(nl_io::input::describe(action)).size(11.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                        ui.add_enabled_ui(i + 1 < len, |ui| {
                            if ui.small_button("▼").clicked() {
                                swap = Some((i, i + 1));
                            }
                        });
                        ui.add_enabled_ui(i > 0, |ui| {
                            if ui.small_button("▲").clicked() {
                                swap = Some((i, i - 1));
                            }
                        });
                    });
                });
                changed |= action_editor(ui, action, 0, i);
            });
    }
    if let Some((a, b)) = swap {
        list.swap(a, b);
        changed = true;
    }
    if let Some(i) = remove {
        list.remove(i);
        changed = true;
    }
    if ui.button("＋ 액션").clicked() {
        list.push(InputAction::None);
        changed = true;
    }
    let _ = state;
    changed
}

/// 액션 하나. `Sequence` 는 중첩 목록이라 깊이를 제한한다.
fn action_editor(ui: &mut egui::Ui, action: &mut InputAction, depth: usize, salt: usize) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("종류").color(COL_WEAK).size(11.0));
        egui::ComboBox::from_id_salt(("act-kind", depth, salt))
            .selected_text(action_label(action))
            .show_ui(ui, |ui| {
                for a in action_palette() {
                    let same = std::mem::discriminant(action) == std::mem::discriminant(&a);
                    if ui.selectable_label(same, action_label(&a)).clicked() && !same {
                        *action = a;
                        changed = true;
                    }
                }
            });
    });
    match action {
        InputAction::None => {}
        InputAction::MoveTo { x, y } => {
            ui.horizontal(|ui| {
                changed |= ui.add(DragValue::new(x).prefix("x ").speed(2.0)).changed();
                changed |= ui.add(DragValue::new(y).prefix("y ").speed(2.0)).changed();
            });
        }
        InputAction::MoveBy { dx, dy } => {
            ui.horizontal(|ui| {
                changed |= ui.add(DragValue::new(dx).prefix("dx ").speed(1.0)).changed();
                changed |= ui.add(DragValue::new(dy).prefix("dy ").speed(1.0)).changed();
            });
        }
        InputAction::Scroll { dx, dy } => {
            ui.horizontal(|ui| {
                changed |= ui.add(DragValue::new(dx).prefix("dx ").speed(1.0)).changed();
                changed |= ui.add(DragValue::new(dy).prefix("dy ").speed(1.0)).changed();
            });
        }
        InputAction::Click { button } => {
            ui.horizontal(|ui| {
                ui.label("버튼");
                for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                    let text = match b {
                        MouseButton::Left => "왼쪽",
                        MouseButton::Right => "오른쪽",
                        MouseButton::Middle => "가운데",
                    };
                    if ui.selectable_label(*button == b, text).clicked() && *button != b {
                        *button = b;
                        changed = true;
                    }
                }
            });
        }
        InputAction::KeyTap { key } | InputAction::KeyDown { key } | InputAction::KeyUp { key } => {
            ui.horizontal(|ui| {
                ui.label("키");
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(key)
                            .desired_width(110.0)
                            .id_salt(("key", depth, salt)),
                    )
                    .changed();
                // 키 이름은 실행기가 쓰는 `parse_key` 로 즉시 검증한다 — 실행 때 처음 알면 늦다.
                match nl_io::parse_key(key) {
                    Ok(_) => {
                        ui.label(RichText::new("✔").color(COL_OK));
                    }
                    Err(e) => {
                        ui.label(RichText::new("✖").color(COL_ERROR))
                            .on_hover_text(format!("{e}"));
                    }
                }
            });
        }
        InputAction::TypeText { text } => {
            ui.label(RichText::new("문자열").color(COL_WEAK).size(11.0));
            changed |= ui
                .add(
                    egui::TextEdit::singleline(text)
                        .desired_width(f32::INFINITY)
                        .id_salt(("txt", depth, salt)),
                )
                .changed();
        }
        InputAction::Sequence { steps } => {
            if depth >= 2 {
                ui.label(
                    RichText::new("더 깊은 중첩은 편집기에서 지원하지 않습니다")
                        .color(COL_WARN)
                        .size(11.0),
                );
                return changed;
            }
            ui.indent(("seq", depth, salt), |ui| {
                let mut remove: Option<usize> = None;
                for (i, step) in steps.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}.", i + 1)).color(COL_WEAK).size(11.0));
                        if ui.small_button("✖").clicked() {
                            remove = Some(i);
                        }
                    });
                    changed |= action_editor(ui, step, depth + 1, salt * 100 + i);
                }
                if let Some(i) = remove {
                    steps.remove(i);
                    changed = true;
                }
                if ui.small_button("＋ 단계").clicked() {
                    steps.push(InputAction::None);
                    changed = true;
                }
            });
        }
    }
    changed
}

/// 액션 팔레트 (편집기 콤보 순서).
pub fn action_palette() -> Vec<InputAction> {
    vec![
        InputAction::None,
        InputAction::MoveTo { x: 0, y: 0 },
        InputAction::MoveBy { dx: 0, dy: 0 },
        InputAction::Click {
            button: MouseButton::Left,
        },
        InputAction::Scroll { dx: 0, dy: 0 },
        InputAction::KeyTap { key: "space".into() },
        InputAction::KeyDown { key: "shift".into() },
        InputAction::KeyUp { key: "shift".into() },
        InputAction::TypeText { text: String::new() },
        InputAction::Sequence { steps: vec![] },
    ]
}

pub fn action_label(a: &InputAction) -> &'static str {
    match a {
        InputAction::None => "없음",
        InputAction::MoveTo { .. } => "커서 이동",
        InputAction::MoveBy { .. } => "커서 상대 이동",
        InputAction::Click { .. } => "클릭",
        InputAction::Scroll { .. } => "스크롤",
        InputAction::KeyTap { .. } => "키 누르고 떼기",
        InputAction::KeyDown { .. } => "키 누르기",
        InputAction::KeyUp { .. } => "키 떼기",
        InputAction::TypeText { .. } => "텍스트 입력",
        InputAction::Sequence { .. } => "순서대로",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_number_accepts_one_or_many() {
        assert_eq!(parse_manual(ManualKind::Number, "1.5"), Ok(Value::Number(1.5)));
        assert_eq!(
            parse_manual(ManualKind::Number, "0, 1"),
            Ok(Value::Numbers(vec![0.0, 1.0]))
        );
        assert!(parse_manual(ManualKind::Number, "").is_err());
        assert!(parse_manual(ManualKind::Number, "a").is_err());
    }

    #[test]
    fn manual_text_and_json() {
        assert_eq!(parse_manual(ManualKind::Text, "안녕"), Ok(Value::Text("안녕".into())));
        let v = parse_manual(ManualKind::Json, r#"{"a":1}"#).unwrap();
        assert!(matches!(v, Value::Json(_)));
        assert!(parse_manual(ManualKind::Json, "{").is_err());
    }

    #[test]
    fn action_palette_covers_every_variant_once() {
        let p = action_palette();
        let mut labels: Vec<&str> = p.iter().map(action_label).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "같은 라벨이 두 번");
        assert_eq!(n, 10, "새 InputAction 변형을 팔레트에 추가할 것");
    }

    #[test]
    fn curl_example_points_at_something_reachable() {
        assert_eq!(
            curl_example("127.0.0.1:8787", "/infer", None),
            "curl -X POST http://127.0.0.1:8787/infer -d '[0,1]'"
        );
        // 0.0.0.0 에 묶었어도 부를 때는 루프백으로 부른다.
        assert!(curl_example("0.0.0.0:9000", "/x", None).contains("http://127.0.0.1:9000/x"));
    }

    /// 토큰이 있으면 예시에 헤더가 들어가야 한다 — 빠진 예시를 복사하면 401 만 돌아온다.
    #[test]
    fn curl_example_carries_the_token_header() {
        let cmd = curl_example("127.0.0.1:8787", "/infer", Some("abc123"));
        assert!(cmd.contains("-H 'X-NL-Token: abc123'"), "{cmd}");
        // 공백뿐인 토큰은 없는 것으로 본다 (편집기가 그렇게 저장한다).
        assert!(!curl_example("127.0.0.1:8787", "/infer", Some("  ")).contains("X-NL-Token"));
    }

    /// 팔레트로 만든 서버는 토큰을 이미 갖고 있어야 한다.
    #[test]
    fn a_new_http_server_comes_with_a_token() {
        let found = crate::pcanvas::source_palette()
            .into_iter()
            .find(|s| matches!(s, Source::HttpServer { .. }))
            .expect("팔레트에 HTTP 서버");
        let Source::HttpServer { token, .. } = found else {
            panic!("HTTP 서버")
        };
        let token = token.expect("토큰이 채워져 있어야 한다");
        assert_eq!(token.chars().count(), nl_core::pipeline::TOKEN_LEN);
    }

    /// 팔레트의 기본 HttpServer 는 루프백이어야 한다 — 새 노드가 바깥에 열려 있으면 안 된다.
    #[test]
    fn default_http_server_binds_to_loopback() {
        let bind: std::net::SocketAddr = crate::pcanvas::DEFAULT_HTTP_BIND.parse().expect("주소");
        assert!(bind.ip().is_loopback());
        assert!(crate::pcanvas::DEFAULT_HTTP_PATH.starts_with('/'));
    }

    /// 편집기가 만들어 내는 키 이름 기본값은 실행기가 반드시 해석할 수 있어야 한다.
    #[test]
    fn default_key_names_parse() {
        for a in action_palette() {
            let key = match &a {
                InputAction::KeyTap { key } | InputAction::KeyDown { key } | InputAction::KeyUp { key } => key,
                _ => continue,
            };
            assert!(nl_io::parse_key(key).is_ok(), "{key:?} 를 실행기가 못 읽는다");
        }
    }
}
