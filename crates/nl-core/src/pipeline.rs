//! 파이프라인 = 소스 → (모델 | 로직) → 싱크 데이터 흐름. 실행은 `nl-engine::pipeline::Runner`.

use crate::ids::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 화면 영역. `monitor` 는 `nl_io::screen::monitors()` 순서의 번호, `x`/`y` 는 **그 모니터의 왼쪽 위 기준 상대 좌표(물리 픽셀)**.
/// `width == 0` 이면 모니터 전체.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Region {
    pub monitor: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Source {
    ScreenCapture { region: Region, fps: f32 },
    HttpPoll { url: String, interval_ms: u64, #[serde(default)] headers: BTreeMap<String, String> },
    WebSocket { url: String },
    /// 표준 입력 한 줄 = JSON 하나.
    StdinJson,
    /// 파일 내용(이미지/CSV 행)을 주기적으로 다시 읽는다.
    File { path: String, interval_ms: u64 },
    Timer { interval_ms: u64 },
    /// GUI 위젯 이벤트(버튼 클릭, 슬라이더 값, 텍스트).
    GuiEvent { widget: WidgetId },
    /// 빌더에서 사용자가 값을 직접 넣는 시험용 소스.
    Manual,
    /// **인바운드** HTTP 서버. 배포된 앱을 바깥 프로그램이 호출할 수 있게 연다.
    /// 들어온 요청 본문이 값이 되고, 짝이 되는 [`Sink::HttpReply`] 가 응답을 돌려준다.
    /// `bind` 는 `"127.0.0.1:8787"` 처럼 주소:포트, `path` 는 `"/infer"` 처럼 받을 경로다.
    HttpServer { bind: String, path: String },
}

/// 마우스·키보드 액션. 모델 출력(클래스 인덱스)에 대응시킨다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputAction {
    None,
    MoveTo { x: i32, y: i32 },
    Click { button: MouseButton },
    /// 현재 커서 기준 상대 이동.
    MoveBy { dx: i32, dy: i32 },
    KeyTap { key: String },
    KeyDown { key: String },
    KeyUp { key: String },
    TypeText { text: String },
    Scroll { dx: i32, dy: i32 },
    /// 여러 액션 순서대로.
    Sequence { steps: Vec<InputAction> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Sink {
    /// 입력값(정수 인덱스) → `actions[index]`.
    MouseKeyboard { actions: Vec<InputAction>, #[serde(default)] cooldown_ms: u64 },
    /// `body_template` 안의 `{{value}}` 를 입력 JSON 으로 치환.
    HttpCall { method: String, url: String, #[serde(default)] headers: BTreeMap<String, String>, #[serde(default)] body_template: String },
    WebSocketSend { url: String },
    StdoutJson,
    GuiWidget { widget: WidgetId },
    File { path: String, #[serde(default)] append: bool },
    Log,
    /// [`Source::HttpServer`] 노드가 받은 요청에 값을 JSON 으로 돌려준다.
    /// `server` 는 그 서버 노드의 id 여야 한다 (같은 파이프라인 안).
    HttpReply { server: PNodeId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Logic {
    /// 값 ≥ threshold → 1 아니면 0.
    Threshold { value: f32 },
    /// 같은 값이 `ms` 안에 반복되면 버린다.
    Debounce { ms: u64 },
    /// 벡터의 index 번째.
    Select { index: usize },
    /// 정수 → 정수 치환표.
    Map { table: BTreeMap<i64, i64> },
    /// 마지막 N 개 값의 다수결.
    Majority { window: usize },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PNodeKind {
    Source { source: Source },
    /// 모델 추론. `payload` 가 인코더/디코더를 정한다(없으면 모델의 payload).
    Model { model: ModelId, #[serde(default)] payload: Option<PayloadId> },
    Logic { logic: Logic },
    Sink { sink: Sink },
}

impl PNodeKind {
    pub fn label(&self) -> &'static str {
        match self {
            PNodeKind::Source { source } => match source {
                Source::ScreenCapture { .. } => "화면 캡처",
                Source::HttpPoll { .. } => "HTTP 폴링",
                Source::WebSocket { .. } => "WebSocket 수신",
                Source::StdinJson => "stdin JSON",
                Source::File { .. } => "파일",
                Source::Timer { .. } => "타이머",
                Source::GuiEvent { .. } => "GUI 이벤트",
                Source::Manual => "수동 입력",
                Source::HttpServer { .. } => "HTTP 서버",
            },
            PNodeKind::Model { .. } => "모델",
            PNodeKind::Logic { logic } => match logic {
                Logic::Threshold { .. } => "임계값",
                Logic::Debounce { .. } => "디바운스",
                Logic::Select { .. } => "선택",
                Logic::Map { .. } => "치환",
                Logic::Majority { .. } => "다수결",
            },
            PNodeKind::Sink { sink } => match sink {
                Sink::MouseKeyboard { .. } => "마우스/키보드",
                Sink::HttpCall { .. } => "HTTP 호출",
                Sink::WebSocketSend { .. } => "WebSocket 송신",
                Sink::StdoutJson => "stdout JSON",
                Sink::GuiWidget { .. } => "GUI 위젯",
                Sink::File { .. } => "파일 쓰기",
                Sink::Log => "로그",
                Sink::HttpReply { .. } => "HTTP 응답",
            },
        }
    }
    pub fn is_source(&self) -> bool {
        matches!(self, PNodeKind::Source { .. })
    }
    pub fn is_sink(&self) -> bool {
        matches!(self, PNodeKind::Sink { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PNode {
    pub id: PNodeId,
    #[serde(default)]
    pub name: String,
    pub kind: PNodeKind,
    #[serde(default)]
    pub pos: [f32; 2],
}

impl PNode {
    pub fn new(kind: PNodeKind, pos: [f32; 2]) -> Self {
        Self { id: PNodeId::new(), name: String::new(), kind, pos }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub id: LinkId,
    pub from: PNodeId,
    pub to: PNodeId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: PipelineId,
    pub name: String,
    #[serde(default)]
    pub nodes: BTreeMap<PNodeId, PNode>,
    #[serde(default)]
    pub links: BTreeMap<LinkId, Link>,
    /// 소스가 시간 기반이 아닐 때의 최대 틱 속도.
    #[serde(default = "d_hz")]
    pub tick_hz: f32,
}

fn d_hz() -> f32 {
    30.0
}

impl Pipeline {
    pub fn new(name: impl Into<String>) -> Self {
        Self { id: PipelineId::new(), name: name.into(), nodes: BTreeMap::new(), links: BTreeMap::new(), tick_hz: d_hz() }
    }

    pub fn add_node(&mut self, node: PNode) -> PNodeId {
        let id = node.id;
        self.nodes.insert(id, node);
        id
    }

    /// 자기참조·중복·소스로 들어가는 링크·싱크에서 나가는 링크는 거부.
    pub fn add_link(&mut self, from: PNodeId, to: PNodeId) -> Option<LinkId> {
        if from == to {
            return None;
        }
        let (f, t) = (self.nodes.get(&from)?, self.nodes.get(&to)?);
        if f.kind.is_sink() || t.kind.is_source() {
            return None;
        }
        if self.links.values().any(|l| l.from == from && l.to == to) {
            return None;
        }
        let l = Link { id: LinkId::new(), from, to };
        let id = l.id;
        self.links.insert(id, l);
        Some(id)
    }

    pub fn remove_node(&mut self, id: PNodeId) -> (Option<PNode>, Vec<Link>) {
        let node = self.nodes.remove(&id);
        let gone: Vec<LinkId> = self.links.values().filter(|l| l.from == id || l.to == id).map(|l| l.id).collect();
        let links = gone.iter().filter_map(|k| self.links.remove(k)).collect();
        (node, links)
    }

    pub fn upstream(&self, id: PNodeId) -> Vec<PNodeId> {
        self.links.values().filter(|l| l.to == id).map(|l| l.from).collect()
    }

    pub fn downstream(&self, id: PNodeId) -> Vec<PNodeId> {
        self.links.values().filter(|l| l.from == id).map(|l| l.to).collect()
    }
}
