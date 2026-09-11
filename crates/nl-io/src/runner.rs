//! 파이프라인 실행기. 틱 루프가 별도 스레드에서 소스 → 모델/로직 → 싱크 순으로 값을 흘린다.
//! 빌더의 "시험 실행"과 배포 런타임이 같은 Runner 를 쓴다.
//!
//! ## 한 틱의 흐름
//! 1. `RunnerInput` 채널을 비워 `Source::Manual` / `Source::GuiEvent` 입력을 모은다(같은 노드에 여러 개가
//!    쌓였으면 마지막 것만 쓴다 — 틱 하나에 값 하나).
//! 2. WebSocket 연결 스레드가 남긴 상태(연결됨·끊김)를 이벤트로 옮긴다.
//! 3. 노드를 위상 순서로 훑는다. 소스는 자기 주기가 됐을 때만 값을 내고, 그 뒤 노드는 상류에 값이 있을 때만 돈다.
//! 4. 값을 낸 노드(소스·모델·로직)는 [`RunnerEvent::Value`] 로도 알린다. 싱크는 값을 소비만 하므로 제외한다.
//! 5. `tick_hz` 로 정해진 주기가 될 때까지 잔다. 잠은 10ms 씩 끊어 자므로 [`RunnerHandle::stop`] 은 곧바로 먹는다.
//!
//! ## 드롭 정책
//! 이벤트 채널은 unbounded 라서 소비자가 느려도 막히지 않지만, 그만큼 이미지가 쌓이면 메모리를 먹는다.
//! 그래서 [`Value::Image`] 는 [`RunnerEvent::Value`] 로 아예 보내지 않고 `Sink::GuiWidget` 이 있을 때만
//! [`RunnerEvent::Widget`] 으로 나간다. 그마저도 채널이 밀려 있으면(`IMAGE_BACKLOG_LIMIT` 이상) 새 프레임을 버린다.
//! GUI 는 최신 프레임만 그리면 되므로 버려도 되고, 버려야 지연이 쌓이지 않는다.
//!
//! ## 안전장치
//! `Sink::MouseKeyboard` 는 [`Runner::arm_input`] 이 켜져 있을 때만 실제 입력을 보낸다. 기본값은 꺼짐(로그만).

use crate::http::{self, HttpResponse};
use crate::input::{self, InputSim};
use crate::screen::Capturer;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use nl_core::payload::PayloadSpec;
use nl_core::{DevicePref, InputAction, Logic, PNodeId, PNodeKind, Pipeline, Project, Sink, Source, WidgetId};
use nl_engine::{HostTensor, Session, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 소스 폴링·싱크 호출에 쓰는 HTTP 타임아웃.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 틱 속도 상한. 이보다 빠르게 돌라고 해도 여기서 잘린다.
pub const MAX_TICK_HZ: f32 = 240.0;
/// 틱 속도 하한.
pub const MIN_TICK_HZ: f32 = 0.05;
/// 한 번에 자는 최대 시간. `stop()` 응답 지연의 상한이기도 하다.
const SLEEP_SLICE: Duration = Duration::from_millis(10);
/// 이벤트 채널에 이만큼 쌓여 있으면 새 이미지 프레임을 버린다.
const IMAGE_BACKLOG_LIMIT: usize = 4;
/// 같은 노드에서 반복되는 오류를 이 간격으로 묶는다 (30Hz 루프가 초당 30개씩 뱉는 것을 막는다).
const ERROR_THROTTLE: Duration = Duration::from_secs(1);
/// 주기 하한. 0ms 간격이 무한 루프가 되지 않게 한다.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

#[derive(Clone, Debug)]
pub enum RunnerEvent {
    Started,
    /// 노드가 값을 냈다 (GUI 위젯 바인딩·디버그 표시).
    Value { node: PNodeId, value: Value },
    /// `Sink::GuiWidget` 로 위젯에 표시할 값.
    Widget { widget: WidgetId, value: Value },
    Log(String),
    Error { node: Option<PNodeId>, message: String },
    Stopped,
}

/// GUI → 파이프라인 입력 (`Source::GuiEvent`, `Source::Manual`).
#[derive(Clone, Debug)]
pub enum RunnerInput {
    Widget { widget: WidgetId, value: Value },
    Manual { node: PNodeId, value: Value },
}

pub struct Runner {
    pub project: Project,
    pub pipeline: Pipeline,
    /// 가중치 상대 경로 기준.
    pub base_dir: PathBuf,
    pub device: DevicePref,
    /// `Sink::MouseKeyboard` 무장 스위치. 꺼져 있으면(기본) 액션을 로그로만 남기고 실제 입력은 보내지 않는다.
    pub arm_input: bool,
    /// `Source::HttpServer` 가 받은 요청을 포기하는 시간. 기본 [`HTTP_REPLY_TIMEOUT`].
    /// 모델 추론이 오래 걸리는 파이프라인은 늘리고, 빠른 실패를 원하면 줄인다.
    pub http_reply_timeout: Duration,
}

#[derive(Clone)]
pub struct RunnerHandle {
    pub events: Receiver<RunnerEvent>,
    pub inputs: Sender<RunnerInput>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl RunnerHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
    /// 멈출 때까지 기다린다. `timeout` 안에 안 끝나면 `false`.
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
}

impl Runner {
    /// `arm_input` 은 꺼진 채로 시작한다. 실제 마우스·키보드를 움직이려면 켜고 나서 [`Runner::start`] 를 부른다.
    pub fn new(project: Project, pipeline: Pipeline, base_dir: PathBuf, device: DevicePref) -> Self {
        Self { project, pipeline, base_dir, device, arm_input: false, http_reply_timeout: HTTP_REPLY_TIMEOUT }
    }

    /// 즉시 돌아온다. 준비 실패(모델 로드 등)도 `RunnerEvent::Error` + `Stopped` 로 온다.
    pub fn start(self) -> anyhow::Result<RunnerHandle> {
        let (etx, erx) = crossbeam_channel::unbounded();
        let (itx, irx) = crossbeam_channel::unbounded::<RunnerInput>();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (s2, d2) = (stop.clone(), done.clone());
        std::thread::Builder::new().name("nl-runner".into()).spawn(move || {
            // 어떤 노드가 패닉을 내도 이벤트 채널에는 Error + Stopped 가 반드시 나가야 한다.
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_loop(self, &etx, &irx, &s2);
            }));
            if let Err(p) = res {
                let message = format!("실행기가 패닉으로 멈췄다: {}", panic_message(&*p));
                let _ = etx.send(RunnerEvent::Error { node: None, message });
            }
            let _ = etx.send(RunnerEvent::Stopped);
            d2.store(true, Ordering::SeqCst);
        })?;
        Ok(RunnerHandle { events: erx, inputs: itx, stop, done })
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 패닉".to_owned()
    }
}

// ───────────────────────────── 인바운드 HTTP 서버 ─────────────────────────────

/// 인바운드 요청 본문 상한. 이보다 크면 읽지 않고 413 으로 끊는다.
pub const MAX_HTTP_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
/// 응답이 나오지 않은 요청을 포기하는 시간. 넘기면 504 를 돌려준다.
pub const HTTP_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// 수신 스레드가 `stop` 을 확인하는 주기. `stop()` 응답 지연의 상한이다.
const HTTP_SERVER_POLL: Duration = Duration::from_millis(50);

/// 수신 스레드 → 틱 루프. 값과 아직 응답하지 않은 요청을 함께 넘긴다.
struct HttpIncoming {
    value: Value,
    request: tiny_http::Request,
}

/// `Source::HttpServer` 노드 하나의 상태.
///
/// 응답 싱크가 이 큐를 봐야 해서 노드별 [`NodeState`] 가 아니라 따로 둔다.
///
/// ## 요청 하나씩 규칙
/// 응답할 요청을 고를 때 값에 딸린 식별자를 하류로 들고 다니지 않는다. 대신 **미응답 요청이 없을 때만**
/// 다음 요청을 꺼내 값으로 흘린다. 그래서 파이프라인 안을 도는 값은 언제나 하나뿐이고,
/// [`Sink::HttpReply`] 는 큐 맨 앞의 요청에 답하면 그게 반드시 그 값의 주인이다.
/// 중간 노드가 값을 버려도(디바운스·치환) 짝이 어긋나지 않는다 — 그 요청은 제자리에서 시간 초과로 끝난다.
/// 동시 요청은 도착 순서대로 한 틱에 하나씩 처리된다(`tick_hz` 가 초당 처리량의 상한).
struct HttpServerState {
    /// 수신 스레드가 넣는 요청.
    rx: Receiver<HttpIncoming>,
    /// 아직 응답하지 않은 요청 (FIFO). 위 규칙상 0개나 1개다.
    pending: VecDeque<(Instant, tiny_http::Request)>,
    /// 수신 스레드를 깨우기 위해 공유한다.
    server: Arc<tiny_http::Server>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpServerState {
    /// 대기 중인 요청에 모두 같은 상태로 답하고 수신 스레드를 접는다.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        while let Some((_, req)) = self.pending.pop_front() {
            let _ = respond_json(req, 503, "파이프라인이 멈춰 요청을 처리하지 못했다");
        }
        // 아직 채널에 있던 요청도 같이 정리한다.
        while let Ok(inc) = self.rx.try_recv() {
            let _ = respond_json(inc.request, 503, "파이프라인이 멈춰 요청을 처리하지 못했다");
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// JSON 본문으로 답한다. 2xx 는 값 그대로, 그 밖에는 `{"error": ...}` 로 감싼다.
fn respond_json(request: tiny_http::Request, status: u16, body: &str) -> Result<(), String> {
    let payload = if (200..300).contains(&status) {
        body.to_owned()
    } else {
        serde_json::json!({ "error": body, "status": status }).to_string()
    };
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .map_err(|_| "Content-Type 헤더를 만들지 못했다".to_string())?;
    let response = tiny_http::Response::from_string(payload)
        .with_status_code(status)
        .with_header(header);
    request.respond(response).map_err(|e| format!("HTTP 응답 전송 실패: {e}"))
}

/// 서버 소켓을 열고 수신 스레드를 띄운다.
fn start_http_server(bind: &str, path: &str) -> Result<HttpServerState, String> {
    let server = tiny_http::Server::http(bind)
        .map(Arc::new)
        .map_err(|e| format!("{bind} 을 열지 못했다: {e}"))?;
    let (tx, rx) = crossbeam_channel::unbounded();
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, srv2, want) = (stop.clone(), server.clone(), normalize_path(path));
    let handle = std::thread::Builder::new()
        .name("nl-http-server".into())
        .spawn(move || http_server_loop(&srv2, &want, &tx, &s2))
        .map_err(|e| format!("HTTP 수신 스레드 생성 실패: {e}"))?;
    Ok(HttpServerState { rx, pending: VecDeque::new(), server, stop, handle: Some(handle) })
}

/// 경로 비교를 위해 앞에 `/` 를 붙이고 뒤쪽 `/` 는 뗀다. 빈 값은 `/`.
fn normalize_path(p: &str) -> String {
    let t = p.trim();
    if t.is_empty() || t == "/" {
        return "/".into();
    }
    let with_slash = if t.starts_with('/') { t.to_owned() } else { format!("/{t}") };
    with_slash.trim_end_matches('/').to_owned()
}

fn http_server_loop(
    server: &tiny_http::Server,
    want_path: &str,
    tx: &Sender<HttpIncoming>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        let mut request = match server.recv_timeout(HTTP_SERVER_POLL) {
            Ok(Some(r)) => r,
            // 시간이 지났을 뿐이다. stop 을 다시 본다.
            Ok(None) => continue,
            Err(_) => break,
        };

        let url = request.url().to_owned();
        let (got_path, query) = match url.split_once('?') {
            Some((p, q)) => (normalize_path(p), q.to_owned()),
            None => (normalize_path(&url), String::new()),
        };
        if got_path != want_path {
            let _ = respond_json(request, 404, &format!("{got_path} 은 이 서버가 받는 경로가 아니다 (받는 경로: {want_path})"));
            continue;
        }

        let method = request.method().as_str().to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH") {
            let _ = respond_json(request, 405, &format!("{method} 은 지원하지 않는다 (GET, POST, PUT, PATCH 만)"));
            continue;
        }

        // Content-Length 가 있으면 먼저 걸러 큰 본문을 아예 읽지 않는다.
        if request.body_length().map(|n| n as u64 > MAX_HTTP_REQUEST_BYTES).unwrap_or(false) {
            let _ = respond_json(request, 413, &format!("본문이 너무 크다 (상한 {MAX_HTTP_REQUEST_BYTES} 바이트)"));
            continue;
        }
        // 길이를 모르는(청크) 본문도 상한에서 끊는다.
        let mut buf = Vec::new();
        // `as_reader()` 는 `&mut dyn Read` 다. 점 호출은 trait object 로 역참조되어 `take` 를 못 쓰므로 UFCS 로 부른다.
        let reader: &mut dyn Read = request.as_reader();
        let read = std::io::Read::take(reader, MAX_HTTP_REQUEST_BYTES + 1).read_to_end(&mut buf);
        if let Err(e) = read {
            let _ = respond_json(request, 400, &format!("본문을 읽지 못했다: {e}"));
            continue;
        }
        if buf.len() as u64 > MAX_HTTP_REQUEST_BYTES {
            let _ = respond_json(request, 413, &format!("본문이 너무 크다 (상한 {MAX_HTTP_REQUEST_BYTES} 바이트)"));
            continue;
        }
        let body = match String::from_utf8(buf) {
            Ok(b) => b,
            Err(_) => {
                let _ = respond_json(request, 400, "본문이 UTF-8 이 아니다 (이 파이프라인은 텍스트·JSON 만 받는다)");
                continue;
            }
        };

        // 본문이 있으면 그것을, 없으면 쿼리스트링을 값으로 삼는다.
        let value = if body.trim().is_empty() { query_to_value(&query) } else { text_to_value(&body) };
        if tx.send(HttpIncoming { value, request }).is_err() {
            // 틱 루프가 사라졌다. 더 받아도 답할 사람이 없다.
            break;
        }
    }
}

/// `a=1&b=hi` → `{"a":"1","b":"hi"}`. 값은 전부 문자열이다(타입을 알 방법이 없다).
fn query_to_value(query: &str) -> Value {
    let mut map = serde_json::Map::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(form_decode(k), serde_json::Value::String(form_decode(v)));
    }
    Value::Json(serde_json::Value::Object(map))
}

/// `application/x-www-form-urlencoded` 해독: `+` 는 공백, `%XX` 는 바이트.
fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

// ───────────────────────────── WebSocket 연결 풀 ─────────────────────────────

/// WebSocket 연결 하나가 바깥으로 알리는 상태.
#[derive(Clone, Debug)]
enum WsStatus {
    /// 연결됐다(재접속 포함).
    Connected,
    /// 연결 실패·끊김. 곧 재시도한다.
    Error(String),
    /// 알려 줄 만한 일(바이너리 프레임 무시 등).
    Log(String),
}

/// `url` 하나에 대한 연결. 같은 url 을 쓰는 소스·싱크 노드가 이 연결을 나눠 쓴다.
struct WsConn {
    /// 연결 상태. 틱마다 비워 이벤트로 바꾼다.
    /// (보내는 쪽 `Sender<String>` 은 싱크 노드들의 [`NodeState::ws_out`] 이 들고 있다.)
    status: Receiver<WsStatus>,
    /// 이 url 을 쓰는 노드들 (오류를 누구에게 붙일지).
    nodes: Vec<PNodeId>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// 파이프라인 하나가 쓰는 WebSocket 연결들. url 이 키다.
#[derive(Default)]
struct WsPool {
    conns: BTreeMap<String, WsConn>,
    stop: Arc<AtomicBool>,
}

impl WsPool {
    /// 모든 연결 스레드에 종료를 알리고 기다린다.
    /// 종료 플래그를 **먼저 전부** 세운 뒤 join 하므로 대기 시간은 연결 수에 비례하지 않는다.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for c in self.conns.values_mut() {
            if let Some(h) = c.handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// 읽기 타임아웃. `stop()` 응답 지연의 상한을 정한다 (읽기 한 번이 이만큼 걸릴 수 있다).
const WS_READ_TIMEOUT: Duration = Duration::from_millis(50);
/// 재접속 첫 대기.
const WS_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// 재접속 대기 상한.
const WS_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// 연결 스레드를 띄운다. 끊기면 지수 백오프로 다시 붙는다.
///
/// - 수신 텍스트 프레임은 `subscribers` 전부에게 복사해 보낸다(같은 url 소스 노드가 여럿일 수 있다).
/// - 바이너리 프레임은 버리고 **연결당 한 번만** 알린다. 매 프레임 알리면 로그가 넘친다.
/// - `out_rx` 로 들어온 문자열은 텍스트 프레임으로 보낸다.
fn spawn_ws(
    url: String,
    subscribers: Vec<Sender<Value>>,
    out_rx: Receiver<String>,
    status_tx: Sender<WsStatus>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    std::thread::Builder::new()
        .name("nl-websocket".into())
        .spawn(move || ws_loop(&url, &subscribers, &out_rx, &status_tx, &stop))
        .map_err(|e| format!("WebSocket 스레드 생성 실패: {e}"))
}

fn ws_loop(
    url: &str,
    subscribers: &[Sender<Value>],
    out_rx: &Receiver<String>,
    status_tx: &Sender<WsStatus>,
    stop: &AtomicBool,
) {
    let mut backoff = WS_BACKOFF_MIN;
    while !stop.load(Ordering::SeqCst) {
        let mut sock = match tungstenite::connect(url) {
            Ok((s, _resp)) => s,
            Err(e) => {
                let _ = status_tx.send(WsStatus::Error(format!("{url} 에 붙지 못했다: {e}")));
                sleep_interruptible(backoff, stop);
                backoff = (backoff * 2).min(WS_BACKOFF_MAX);
                continue;
            }
        };
        // 읽기를 타임아웃으로 끊어야 stop 을 제때 볼 수 있다. 설정에 실패해도 진행은 한다
        // (그 경우 읽기가 블로킹이라 종료가 늦어질 수 있다 — 상태로 알린다).
        if let Err(e) = set_read_timeout(&mut sock, WS_READ_TIMEOUT) {
            let _ = status_tx.send(WsStatus::Log(format!("{url} 읽기 타임아웃을 걸지 못했다: {e}")));
        }
        let _ = status_tx.send(WsStatus::Connected);
        backoff = WS_BACKOFF_MIN;
        let mut warned_binary = false;

        loop {
            if stop.load(Ordering::SeqCst) {
                let _ = sock.close(None);
                return;
            }
            // 보낼 것부터 비운다. 보내기는 블로킹이지만 소켓이 살아 있으면 금방 끝난다.
            let mut send_failed = None;
            while let Ok(text) = out_rx.try_recv() {
                if let Err(e) = sock.send(tungstenite::Message::Text(text.into())) {
                    send_failed = Some(format!("{url} 로 보내지 못했다: {e}"));
                    break;
                }
            }
            if let Some(msg) = send_failed {
                let _ = status_tx.send(WsStatus::Error(msg));
                break;
            }

            match sock.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let v = text_to_value(t.as_str());
                    for s in subscribers {
                        let _ = s.send(v.clone());
                    }
                }
                Ok(tungstenite::Message::Binary(b)) => {
                    if !warned_binary {
                        warned_binary = true;
                        let _ = status_tx.send(WsStatus::Log(format!(
                            "{url} 이 바이너리 프레임({}바이트)을 보냈다. 이 파이프라인은 텍스트만 다루므로 버린다",
                            b.len()
                        )));
                    }
                }
                Ok(tungstenite::Message::Close(_)) => {
                    let _ = status_tx.send(WsStatus::Error(format!("{url} 이 연결을 닫았다")));
                    break;
                }
                // Ping/Pong 은 tungstenite 가 알아서 답한다. Frame 은 읽기에서 나오지 않는다.
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
                Err(e) => {
                    let _ = status_tx.send(WsStatus::Error(format!("{url} 읽기 실패: {e}")));
                    break;
                }
            }
        }

        if stop.load(Ordering::SeqCst) {
            return;
        }
        sleep_interruptible(backoff, stop);
        backoff = (backoff * 2).min(WS_BACKOFF_MAX);
    }
}

/// 읽기 타임아웃이 만든 "지금은 읽을 게 없다" 신호인가.
fn would_block(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

fn set_read_timeout(
    sock: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    t: Duration,
) -> std::io::Result<()> {
    match sock.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(t)),
        tungstenite::stream::MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(Some(t)),
        // `MaybeTlsStream` 은 non_exhaustive 다. 모르는 변형이면 타임아웃 없이 간다.
        _ => Ok(()),
    }
}

/// `stop` 을 자주 보면서 잔다.
fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let mut left = total;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let s = left.min(SLEEP_SLICE);
        std::thread::sleep(s);
        left -= s;
    }
}

// ───────────────────────────── 노드별 상태 ─────────────────────────────

#[derive(Default)]
struct NodeState {
    /// 다음 발화 시각 (Timer / ScreenCapture / HttpPoll / File).
    next: Option<Instant>,
    /// Timer 가 낸 틱 수.
    count: u64,
    /// 화면 캡처 연결 (재사용).
    capturer: Option<Capturer>,
    /// 진행 중인 HTTP 호출. 틱 루프를 막지 않으려고 별도 스레드에서 돈다.
    pending_http: Option<Receiver<anyhow::Result<HttpResponse>>>,
    /// stdin 줄 수신.
    stdin: Option<Receiver<String>>,
    /// Debounce 가 마지막으로 통과시킨 값과 그 시각.
    debounce: Option<(Value, Instant)>,
    /// Majority 의 최근 값들.
    window: VecDeque<i64>,
    /// 싱크 쿨다운.
    last_fire: Option<Instant>,
    /// 마지막 오류 보고 시각 (폭주 방지).
    last_error: Option<Instant>,
    /// `Source::WebSocket` 가 받은 값.
    ws_in: Option<Receiver<Value>>,
    /// `Sink::WebSocketSend` 가 보낼 곳.
    ws_out: Option<Sender<String>>,
}

// ───────────────────────────── 틱 루프 ─────────────────────────────

fn run_loop(runner: Runner, etx: &Sender<RunnerEvent>, irx: &Receiver<RunnerInput>, stop: &AtomicBool) {
    let Runner { project, pipeline, base_dir, device, arm_input, http_reply_timeout } = runner;
    let _ = etx.send(RunnerEvent::Started);

    let (order, cyclic) = topo_order(&pipeline);
    if !cyclic.is_empty() {
        for id in &cyclic {
            let _ = etx.send(RunnerEvent::Error {
                node: Some(*id),
                message: "순환 연결에 걸려 있어 이 노드는 실행하지 않는다".into(),
            });
        }
    }
    if order.is_empty() {
        let _ = etx.send(RunnerEvent::Error { node: None, message: "실행할 노드가 없다".into() });
        return;
    }

    let mut states: HashMap<PNodeId, NodeState> = order.iter().map(|id| (*id, NodeState::default())).collect();
    // 라벨은 바뀌지 않으니 한 번만 만든다 (틱마다 String 을 새로 찍지 않게).
    let labels: HashMap<PNodeId, String> = order.iter().map(|id| (*id, node_label(&pipeline, *id))).collect();
    let mut sessions: HashMap<PNodeId, Result<Session, String>> = HashMap::new();

    // ── 준비: 모델 세션 로드. 실패해도 루프는 돈다(그 노드를 지날 때 오류 이벤트).
    for id in &order {
        let node = &pipeline.nodes[id];
        let PNodeKind::Model { model, .. } = &node.kind else { continue };
        let Some(def) = project.models.get(model) else {
            let _ = etx.send(RunnerEvent::Error {
                node: Some(*id),
                message: format!("프로젝트에 없는 모델 {}", model.short()),
            });
            sessions.insert(*id, Err(format!("프로젝트에 없는 모델 {}", model.short())));
            continue;
        };
        let weights = def.weights.as_ref().map(|w| base_dir.join(w));
        match Session::load(def, weights.as_deref(), device) {
            Ok(s) => {
                let _ = etx.send(RunnerEvent::Log(format!("모델 '{}' 준비 완료 ({})", def.name, s.device_name())));
                sessions.insert(*id, Ok(s));
            }
            Err(e) => {
                let message = format!("모델 '{}' 을 올리지 못했다: {e:#}", def.name);
                let _ = etx.send(RunnerEvent::Error { node: Some(*id), message: message.clone() });
                sessions.insert(*id, Err(message));
            }
        }
    }

    // ── 준비: stdin 읽기 스레드 (StdinJson 소스가 있을 때만).
    let stdin_nodes: Vec<PNodeId> = order
        .iter()
        .copied()
        .filter(|id| matches!(&pipeline.nodes[id].kind, PNodeKind::Source { source: Source::StdinJson }))
        .collect();
    if !stdin_nodes.is_empty() {
        let mut senders = Vec::with_capacity(stdin_nodes.len());
        for id in &stdin_nodes {
            let (tx, rx) = crossbeam_channel::unbounded();
            senders.push(tx);
            states.get_mut(id).expect("상태를 미리 만들어 뒀다").stdin = Some(rx);
        }
        // stdin 읽기는 블로킹이라 stop 으로 깨울 수 없다. 수신자가 모두 사라지면 다음 줄에서 스스로 끝난다.
        let spawned = std::thread::Builder::new().name("nl-stdin".into()).spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(l) = line else { break };
                if senders.iter().all(|s| s.send(l.clone()).is_err()) {
                    break;
                }
            }
        });
        if let Err(e) = spawned {
            let _ = etx.send(RunnerEvent::Error { node: None, message: format!("stdin 읽기 스레드 생성 실패: {e}") });
        }
    }

    // ── 준비: 인바운드 HTTP 서버. 노드마다 소켓 하나를 연다.
    let mut servers: HashMap<PNodeId, HttpServerState> = HashMap::new();
    for id in &order {
        let PNodeKind::Source { source: Source::HttpServer { bind, path } } = &pipeline.nodes[id].kind else {
            continue;
        };
        match start_http_server(bind, path) {
            Ok(srv) => {
                let _ = etx.send(RunnerEvent::Log(format!(
                    "HTTP 서버 http://{bind}{} 열림",
                    normalize_path(path)
                )));
                servers.insert(*id, srv);
            }
            Err(e) => {
                let _ = etx.send(RunnerEvent::Error { node: Some(*id), message: e });
            }
        }
    }

    // ── 준비: WebSocket 연결. 같은 url 을 쓰는 소스·싱크는 연결 하나를 나눠 쓴다.
    let mut ws_pool = WsPool::default();
    {
        // url → (소스 노드들, 싱크 노드들)
        let mut by_url: BTreeMap<String, (Vec<PNodeId>, Vec<PNodeId>)> = BTreeMap::new();
        for id in &order {
            match &pipeline.nodes[id].kind {
                PNodeKind::Source { source: Source::WebSocket { url } } => {
                    by_url.entry(url.clone()).or_default().0.push(*id)
                }
                PNodeKind::Sink { sink: Sink::WebSocketSend { url } } => {
                    by_url.entry(url.clone()).or_default().1.push(*id)
                }
                _ => {}
            }
        }
        for (url, (sources, sinks)) in by_url {
            // 소스 노드마다 자기 수신 채널을 준다 (하나의 채널을 나눠 가지면 프레임이 한 노드에게만 간다).
            let mut subscribers = Vec::with_capacity(sources.len());
            for id in &sources {
                let (tx, rx) = crossbeam_channel::unbounded();
                subscribers.push(tx);
                states.get_mut(id).expect("상태 미리 생성").ws_in = Some(rx);
            }
            // 보내는 쪽 핸들은 싱크 노드 상태가 들고 있다. 싱크가 없으면 out_tx 는 여기서 사라지고,
            // 연결 스레드의 `try_recv` 가 Disconnected 를 받아 조용히 지나간다(받기만 하는 연결).
            let (out_tx, out_rx) = crossbeam_channel::unbounded::<String>();
            for id in &sinks {
                states.get_mut(id).expect("상태 미리 생성").ws_out = Some(out_tx.clone());
            }
            drop(out_tx);
            let (status_tx, status_rx) = crossbeam_channel::unbounded();
            let nodes: Vec<PNodeId> = sources.iter().chain(sinks.iter()).copied().collect();
            match spawn_ws(url.clone(), subscribers, out_rx, status_tx, ws_pool.stop.clone()) {
                Ok(handle) => {
                    ws_pool.conns.insert(
                        url,
                        WsConn { status: status_rx, nodes, handle: Some(handle) },
                    );
                }
                Err(e) => {
                    for id in &nodes {
                        let _ = etx.send(RunnerEvent::Error { node: Some(*id), message: e.clone() });
                    }
                }
            }
        }
    }

    // ── 준비: 입력 시뮬레이터.
    let mut sim = match InputSim::with_armed(arm_input) {
        Ok(s) => s,
        Err(e) => {
            let _ = etx.send(RunnerEvent::Error { node: None, message: format!("입력 시뮬레이터 준비 실패: {e:#}") });
            return;
        }
    };
    if arm_input {
        let _ = etx.send(RunnerEvent::Log("마우스·키보드 싱크가 무장됐다 (실제 입력을 보낸다)".into()));
    }

    let hz = pipeline.tick_hz.clamp(MIN_TICK_HZ, MAX_TICK_HZ);
    let period = Duration::from_secs_f32(1.0 / hz);

    let mut widget_inputs: HashMap<WidgetId, Value> = HashMap::new();
    let mut manual_inputs: HashMap<PNodeId, Value> = HashMap::new();

    while !stop.load(Ordering::SeqCst) {
        let tick_start = Instant::now();

        // 1. 밖에서 들어온 입력을 모은다. 같은 대상에 여러 개면 마지막 것만 쓴다.
        loop {
            match irx.try_recv() {
                Ok(RunnerInput::Widget { widget, value }) => {
                    widget_inputs.insert(widget, value);
                }
                Ok(RunnerInput::Manual { node, value }) => {
                    manual_inputs.insert(node, value);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        // 2. 응답이 오지 않은 HTTP 요청을 시간 초과로 닫는다.
        for (id, srv) in servers.iter_mut() {
            while srv.pending.front().is_some_and(|(at, _)| at.elapsed() >= http_reply_timeout) {
                let (_, req) = srv.pending.pop_front().expect("바로 위에서 확인했다");
                let _ = respond_json(
                    req,
                    504,
                    "파이프라인이 제한 시간 안에 응답을 내지 않았다 (HTTP 응답 싱크가 연결돼 있는지 확인하라)",
                );
                if let Some(node_st) = states.get_mut(id) {
                    report(etx, node_st, Some(*id), "HTTP 요청이 제한 시간 안에 응답을 받지 못해 504 로 닫았다".into());
                }
            }
        }

        // 3. WebSocket 연결 상태를 이벤트로 옮긴다. 오류는 그 url 을 쓰는 노드들에 붙인다.
        for conn in ws_pool.conns.values() {
            while let Ok(st) = conn.status.try_recv() {
                match st {
                    WsStatus::Connected | WsStatus::Log(_) => {
                        let text = match st {
                            WsStatus::Connected => "WebSocket 연결됨".to_string(),
                            WsStatus::Log(m) => m,
                            WsStatus::Error(_) => unreachable!("바로 위에서 걸렀다"),
                        };
                        let _ = etx.send(RunnerEvent::Log(text));
                    }
                    WsStatus::Error(m) => {
                        for id in &conn.nodes {
                            if let Some(node_st) = states.get_mut(id) {
                                report(etx, node_st, Some(*id), m.clone());
                            }
                        }
                    }
                }
            }
        }

        // 4. 위상 순서로 평가.
        let mut values: HashMap<PNodeId, Value> = HashMap::new();
        for id in &order {
            let id = *id;
            let node = &pipeline.nodes[&id];
            let name = labels[&id].as_str();

            match &node.kind {
                PNodeKind::Source { source } => {
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    match eval_source(
                        source,
                        id,
                        st,
                        tick_start,
                        &base_dir,
                        &mut widget_inputs,
                        &mut manual_inputs,
                        &mut servers,
                    ) {
                        Ok(Some(v)) => {
                            emit_value(etx, id, &v);
                            values.insert(id, v);
                        }
                        Ok(None) => {}
                        Err(msg) => report(etx, st, Some(id), format!("{name}: {msg}")),
                    }
                }
                PNodeKind::Model { model, payload } => {
                    let Some(v) = upstream_value(&pipeline, &values, id) else { continue };
                    let spec = payload
                        .or_else(|| project.models.get(model).and_then(|m| m.payload))
                        .and_then(|pid| project.payloads.get(&pid));
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    match sessions.get_mut(&id) {
                        Some(Ok(sess)) => match run_model(sess, spec, &v) {
                            Ok(out) => {
                                emit_value(etx, id, &out);
                                values.insert(id, out);
                            }
                            Err(msg) => report(etx, st, Some(id), format!("{name}: {msg}")),
                        },
                        Some(Err(msg)) => {
                            let msg = msg.clone();
                            report(etx, st, Some(id), format!("{name}: {msg}"));
                        }
                        None => report(etx, st, Some(id), format!("{name}: 준비되지 않은 모델 노드")),
                    }
                }
                PNodeKind::Logic { logic } => {
                    let Some(v) = upstream_value(&pipeline, &values, id) else { continue };
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    match eval_logic(logic, &v, st, tick_start) {
                        Ok(Some(out)) => {
                            emit_value(etx, id, &out);
                            values.insert(id, out);
                        }
                        Ok(None) => {}
                        Err(msg) => report(etx, st, Some(id), format!("{name}: {msg}")),
                    }
                }
                PNodeKind::Sink { sink } => {
                    let v = upstream_value(&pipeline, &values, id);
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    // HTTP 싱크는 비동기라서 값이 없어도 지난 호출 결과를 거둬야 한다.
                    if let Some(msg) = take_finished_http_error(st) {
                        report(etx, st, Some(id), format!("{name}: {msg}"));
                    }
                    let Some(v) = v else { continue };
                    if let Err(msg) = eval_sink(sink, &v, st, tick_start, &base_dir, &mut sim, etx, name, &mut servers)
                    {
                        report(etx, st, Some(id), format!("{name}: {msg}"));
                    }
                }
            }
        }

        // 5. 남은 주기만큼 잔다. stop 을 자주 확인한다.
        let elapsed = tick_start.elapsed();
        if elapsed < period {
            let mut left = period - elapsed;
            while !left.is_zero() && !stop.load(Ordering::SeqCst) {
                let s = left.min(SLEEP_SLICE);
                std::thread::sleep(s);
                left -= s;
            }
        }
    }

    // WebSocket 스레드를 정리하고 나간다. 읽기 타임아웃이 WS_READ_TIMEOUT 이라 곧 끝난다.
    ws_pool.shutdown();
    // 대기 중인 HTTP 요청에 503 으로 답하고 소켓을 닫는다.
    for srv in servers.values_mut() {
        srv.shutdown();
    }
}

/// 노드 이름 (없으면 종류 라벨 + 짧은 id).
fn node_label(p: &Pipeline, id: PNodeId) -> String {
    let n = &p.nodes[&id];
    if n.name.is_empty() {
        format!("{}({})", n.kind.label(), id.short())
    } else {
        n.name.clone()
    }
}

/// 상류 중 이번 틱에 값을 낸 첫 노드의 값. 순서는 노드 id 순(결정적).
fn upstream_value(p: &Pipeline, values: &HashMap<PNodeId, Value>, id: PNodeId) -> Option<Value> {
    let mut ups = p.upstream(id);
    ups.sort_unstable();
    ups.into_iter().find_map(|u| values.get(&u).cloned())
}

/// 이미지는 크기 때문에 `Value` 이벤트로 내보내지 않는다 (위젯 경로로만 간다).
fn emit_value(etx: &Sender<RunnerEvent>, node: PNodeId, v: &Value) {
    if matches!(v, Value::Image { .. }) {
        return;
    }
    let _ = etx.send(RunnerEvent::Value { node, value: v.clone() });
}

/// 같은 노드의 오류를 `ERROR_THROTTLE` 간격으로 묶어 보낸다.
fn report(etx: &Sender<RunnerEvent>, st: &mut NodeState, node: Option<PNodeId>, message: String) {
    if let Some(at) = st.last_error {
        if at.elapsed() < ERROR_THROTTLE {
            return;
        }
    }
    st.last_error = Some(Instant::now());
    let _ = etx.send(RunnerEvent::Error { node, message });
}

/// Kahn 위상 정렬. 두 번째 반환값은 순환에 걸려 실행할 수 없는 노드들.
fn topo_order(p: &Pipeline) -> (Vec<PNodeId>, Vec<PNodeId>) {
    let mut indeg: BTreeMap<PNodeId, usize> = p.nodes.keys().map(|k| (*k, 0usize)).collect();
    for l in p.links.values() {
        if p.nodes.contains_key(&l.from) && p.nodes.contains_key(&l.to) {
            if let Some(d) = indeg.get_mut(&l.to) {
                *d += 1;
            }
        }
    }
    let mut queue: VecDeque<PNodeId> = indeg.iter().filter(|(_, d)| **d == 0).map(|(k, _)| *k).collect();
    let mut order = Vec::with_capacity(p.nodes.len());
    while let Some(id) = queue.pop_front() {
        order.push(id);
        let mut down = p.downstream(id);
        down.sort_unstable();
        for d in down {
            if let Some(v) = indeg.get_mut(&d) {
                *v -= 1;
                if *v == 0 {
                    queue.push_back(d);
                }
            }
        }
    }
    let cyclic: Vec<PNodeId> = p.nodes.keys().copied().filter(|k| !order.contains(k)).collect();
    (order, cyclic)
}

// ───────────────────────────── 소스 ─────────────────────────────

/// 주기가 됐는지 보고, 됐으면 다음 시각을 밀어 둔다. 밀린 주기는 건너뛴다(누적 지연 방지).
fn due(st: &mut NodeState, now: Instant, interval: Duration) -> bool {
    let interval = interval.max(MIN_INTERVAL);
    match st.next {
        None => {
            // 첫 틱에 바로 한 번 낸다.
            st.next = Some(now + interval);
            true
        }
        Some(t) if now >= t => {
            let mut n = t + interval;
            while n <= now {
                n += interval;
            }
            st.next = Some(n);
            true
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn eval_source(
    source: &Source,
    id: PNodeId,
    st: &mut NodeState,
    now: Instant,
    base_dir: &Path,
    widget_inputs: &mut HashMap<WidgetId, Value>,
    manual_inputs: &mut HashMap<PNodeId, Value>,
    servers: &mut HashMap<PNodeId, HttpServerState>,
) -> Result<Option<Value>, String> {
    match source {
        Source::Timer { interval_ms } => {
            if due(st, now, Duration::from_millis(*interval_ms)) {
                let v = Value::Number(st.count as f64);
                st.count += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }

        Source::ScreenCapture { region, fps } => {
            let fps = fps.clamp(0.01, MAX_TICK_HZ);
            if !due(st, now, Duration::from_secs_f32(1.0 / fps)) {
                return Ok(None);
            }
            if st.capturer.is_none() {
                st.capturer = Some(Capturer::new().map_err(|e| format!("화면 캡처를 열지 못했다: {e:#}"))?);
            }
            let cap = st.capturer.as_mut().expect("바로 위에서 만들었다");
            let f = cap.capture(region).map_err(|e| format!("화면 캡처 실패: {e:#}"))?;
            Ok(Some(Value::Image { width: f.width, height: f.height, rgba: f.rgba }))
        }

        Source::HttpPoll { url, interval_ms, headers } => {
            // 지난 호출이 끝났으면 결과를 꺼낸다.
            if let Some(rx) = &st.pending_http {
                match rx.try_recv() {
                    Ok(res) => {
                        st.pending_http = None;
                        let r = res.map_err(|e| format!("HTTP 폴링 실패: {e:#}"))?;
                        return Ok(Some(response_to_value(&r)));
                    }
                    Err(TryRecvError::Empty) => return Ok(None),
                    Err(TryRecvError::Disconnected) => {
                        st.pending_http = None;
                        return Err("HTTP 폴링 스레드가 사라졌다".into());
                    }
                }
            }
            if !due(st, now, Duration::from_millis(*interval_ms)) {
                return Ok(None);
            }
            st.pending_http = Some(spawn_http("GET", url, headers.clone(), None)?);
            Ok(None)
        }

        Source::File { path, interval_ms } => {
            if !due(st, now, Duration::from_millis(*interval_ms)) {
                return Ok(None);
            }
            read_file_value(&resolve(base_dir, path)).map(Some)
        }

        Source::StdinJson => {
            let Some(rx) = &st.stdin else { return Err("stdin 읽기 스레드가 없다".into()) };
            match rx.try_recv() {
                Ok(line) => Ok(Some(text_to_value(&line))),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Ok(None),
            }
        }

        // 위젯·수동 입력은 `RunnerInput` 채널로 들어와 여기서 소비된다(한 번 쓰면 사라진다).
        Source::GuiEvent { widget } => Ok(widget_inputs.remove(widget)),

        Source::Manual => Ok(manual_inputs.remove(&id)),

        // 미응답 요청이 없을 때만 다음 요청을 꺼낸다 ([`HttpServerState`] 의 "요청 하나씩 규칙" 참고).
        Source::HttpServer { bind, path } => {
            let Some(srv) = servers.get_mut(&id) else {
                return Err(format!("http://{bind}{} 서버가 열려 있지 않다", normalize_path(path)));
            };
            if !srv.pending.is_empty() {
                return Ok(None);
            }
            match srv.rx.try_recv() {
                Ok(inc) => {
                    srv.pending.push_back((now, inc.request));
                    Ok(Some(inc.value))
                }
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Err("HTTP 수신 스레드가 사라졌다".into()),
            }
        }

        // 연결 스레드가 채널에 넣어 둔 프레임을 가져온다. 한 틱에 하나씩 흘린다.
        Source::WebSocket { url } => {
            let Some(rx) = &st.ws_in else {
                return Err(format!("{url} 연결이 준비되지 않았다"));
            };
            match rx.try_recv() {
                Ok(v) => Ok(Some(v)),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Err(format!("{url} 연결 스레드가 사라졌다")),
            }
        }
    }
}

fn resolve(base_dir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// 확장자가 이미지면 RGBA 로, 아니면 JSON → 텍스트 순으로 읽는다.
fn read_file_value(path: &Path) -> Result<Value, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
        let img = image::open(path).map_err(|e| format!("이미지 {}: {e}", path.display()))?;
        let rgba = img.to_rgba8();
        return Ok(Value::Image { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() });
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("파일 {}: {e}", path.display()))?;
    Ok(text_to_value(&text))
}

fn text_to_value(s: &str) -> Value {
    match serde_json::from_str::<serde_json::Value>(s.trim()) {
        Ok(j) => Value::Json(j),
        Err(_) => Value::Text(s.to_owned()),
    }
}

fn response_to_value(r: &HttpResponse) -> Value {
    if r.content_type.contains("json") {
        if let Ok(j) = serde_json::from_str::<serde_json::Value>(&r.body) {
            return Value::Json(j);
        }
    }
    text_to_value(&r.body)
}

/// HTTP 호출을 별도 스레드에 던진다. 틱 루프가 네트워크 지연에 묶이지 않게 하려는 것이다.
fn spawn_http(
    method: &str,
    url: &str,
    headers: BTreeMap<String, String>,
    body: Option<String>,
) -> Result<Receiver<anyhow::Result<HttpResponse>>, String> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let (m, u) = (method.to_owned(), url.to_owned());
    std::thread::Builder::new()
        .name("nl-http".into())
        .spawn(move || {
            let r = http::call(&m, &u, &headers, body.as_deref(), HTTP_TIMEOUT);
            let _ = tx.send(r);
        })
        .map_err(|e| format!("HTTP 스레드 생성 실패: {e}"))?;
    Ok(rx)
}

/// 끝난 HTTP 싱크 호출의 오류만 꺼낸다.
fn take_finished_http_error(st: &mut NodeState) -> Option<String> {
    let rx = st.pending_http.as_ref()?;
    match rx.try_recv() {
        Ok(Ok(r)) if !r.is_success() => {
            st.pending_http = None;
            Some(format!("HTTP 응답 {} — {}", r.status, first_line(&r.body)))
        }
        Ok(Ok(_)) => {
            st.pending_http = None;
            None
        }
        Ok(Err(e)) => {
            st.pending_http = None;
            Some(format!("HTTP 호출 실패: {e:#}"))
        }
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            st.pending_http = None;
            Some("HTTP 호출 스레드가 사라졌다".into())
        }
    }
}

fn first_line(s: &str) -> String {
    let t = s.lines().next().unwrap_or_default();
    if t.chars().count() > 200 {
        format!("{}…", t.chars().take(200).collect::<String>())
    } else {
        t.to_owned()
    }
}

// ───────────────────────────── 모델 ─────────────────────────────

fn run_model(sess: &mut Session, spec: Option<&PayloadSpec>, v: &Value) -> Result<Value, String> {
    let input = encode_input(spec, v)?;
    let outs = sess.run(&[input]).map_err(|e| format!("추론 실패: {e:#}"))?;
    let first = outs.into_iter().next().ok_or_else(|| "모델이 출력을 내지 않았다".to_string())?;
    match spec.and_then(|s| s.outputs.first()) {
        Some(field) => nl_engine::decode(field, &first).map_err(|e| format!("출력 디코딩 실패: {e:#}")),
        None => Ok(Value::Tensor(first)),
    }
}

/// 페이로드가 있으면 `inputs[0]` 의 인코더를, 없으면 텐서/숫자 값을 배치 1 텐서로 그대로 쓴다.
fn encode_input(spec: Option<&PayloadSpec>, v: &Value) -> Result<HostTensor, String> {
    if let Some(field) = spec.and_then(|s| s.inputs.first()) {
        return nl_engine::encode(field, v).map_err(|e| format!("입력 인코딩 실패: {e:#}"));
    }
    match v {
        // 이미 배치 차원을 포함한 것으로 본다.
        Value::Tensor(t) => Ok(t.clone()),
        Value::Numbers(n) => Ok(HostTensor::new(vec![1, n.len()], n.clone())),
        Value::Number(x) => Ok(HostTensor::new(vec![1, 1], vec![*x as f32])),
        other => Err(format!(
            "페이로드가 없으면 텐서·숫자 값만 모델에 넣을 수 있다 (받은 값: {})",
            kind_name(other)
        )),
    }
}

// ───────────────────────────── 로직 ─────────────────────────────

fn eval_logic(logic: &Logic, v: &Value, st: &mut NodeState, now: Instant) -> Result<Option<Value>, String> {
    match logic {
        Logic::Threshold { value } => {
            let x = as_f64(v)?;
            Ok(Some(Value::Number(if x >= *value as f64 { 1.0 } else { 0.0 })))
        }

        Logic::Debounce { ms } => {
            let window = Duration::from_millis(*ms);
            // 이미지는 통째로 비교하면 비싸다. 항상 통과시킨다.
            let dup = !matches!(v, Value::Image { .. })
                && matches!(&st.debounce, Some((prev, at)) if prev == v && now.duration_since(*at) < window);
            if dup {
                return Ok(None);
            }
            st.debounce = Some((v.clone(), now));
            Ok(Some(v.clone()))
        }

        Logic::Select { index } => match v {
            Value::Numbers(n) => n
                .get(*index)
                .map(|x| Value::Number(*x as f64))
                .map(Some)
                .ok_or_else(|| format!("Select: 인덱스 {index} 가 길이 {} 를 넘는다", n.len())),
            Value::Tensor(t) => t
                .data
                .get(*index)
                .map(|x| Value::Number(*x as f64))
                .map(Some)
                .ok_or_else(|| format!("Select: 인덱스 {index} 가 원소 수 {} 를 넘는다", t.numel())),
            Value::Json(serde_json::Value::Array(a)) => a
                .get(*index)
                .map(|j| Value::Json(j.clone()))
                .map(Some)
                .ok_or_else(|| format!("Select: 인덱스 {index} 가 배열 길이 {} 를 넘는다", a.len())),
            other => Err(format!("Select 는 벡터·텐서·JSON 배열에만 쓸 수 있다 (받은 값: {})", kind_name(other))),
        },

        // 표에 없는 값은 흘리지 않는다 (치환표는 "이 값만 통과" 라는 뜻으로도 쓰인다).
        Logic::Map { table } => {
            let k = as_i64(v)?;
            Ok(table.get(&k).map(|out| Value::Number(*out as f64)))
        }

        Logic::Majority { window } => {
            let k = as_i64(v)?;
            let w = (*window).max(1);
            st.window.push_back(k);
            while st.window.len() > w {
                st.window.pop_front();
            }
            let mut counts: BTreeMap<i64, usize> = BTreeMap::new();
            for x in &st.window {
                *counts.entry(*x).or_default() += 1;
            }
            // 동률이면 작은 값 (BTreeMap 순회가 오름차순이라 `>` 비교로 자연히 그렇게 된다).
            let best = counts.into_iter().fold((0i64, 0usize), |acc, (k, c)| if c > acc.1 { (k, c) } else { acc });
            Ok(Some(Value::Number(best.0 as f64)))
        }
    }
}

// ───────────────────────────── 싱크 ─────────────────────────────

#[allow(clippy::too_many_arguments)]
fn eval_sink(
    sink: &Sink,
    v: &Value,
    st: &mut NodeState,
    now: Instant,
    base_dir: &Path,
    sim: &mut InputSim,
    etx: &Sender<RunnerEvent>,
    name: &str,
    servers: &mut HashMap<PNodeId, HttpServerState>,
) -> Result<(), String> {
    match sink {
        Sink::MouseKeyboard { actions, cooldown_ms } => {
            if actions.is_empty() {
                return Err("액션 목록이 비어 있다".into());
            }
            if let Some(at) = st.last_fire {
                if now.duration_since(at) < Duration::from_millis(*cooldown_ms) {
                    return Ok(());
                }
            }
            let idx = as_i64(v)?;
            let a = usize::try_from(idx)
                .ok()
                .and_then(|i| actions.get(i))
                .ok_or_else(|| format!("인덱스 {idx} 에 해당하는 액션이 없다 (액션 {}개)", actions.len()))?;
            if matches!(a, InputAction::None) {
                return Ok(());
            }
            st.last_fire = Some(now);
            sim.perform(a).map_err(|e| format!("입력 실행 실패({}): {e:#}", input::describe(a)))
        }

        Sink::HttpCall { method, url, headers, body_template } => {
            if st.pending_http.is_some() {
                // 앞 호출이 아직 안 끝났다. 겹쳐 쏘지 않는다.
                return Ok(());
            }
            let body = if body_template.is_empty() {
                None
            } else {
                Some(body_template.replace("{{value}}", &value_to_json(v).to_string()))
            };
            st.pending_http = Some(spawn_http(method, url, headers.clone(), body)?);
            Ok(())
        }

        Sink::StdoutJson => {
            let line = value_to_json(v).to_string();
            let out = std::io::stdout();
            let mut lock = out.lock();
            writeln!(lock, "{line}").map_err(|e| format!("stdout 쓰기 실패: {e}"))?;
            lock.flush().map_err(|e| format!("stdout flush 실패: {e}"))
        }

        Sink::File { path, append } => {
            let p = resolve(base_dir, path);
            if let Some(dir) = p.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir).map_err(|e| format!("폴더 {}: {e}", dir.display()))?;
                }
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(*append)
                .truncate(!*append)
                .open(&p)
                .map_err(|e| format!("파일 {}: {e}", p.display()))?;
            writeln!(f, "{}", value_to_json(v)).map_err(|e| format!("파일 {} 쓰기 실패: {e}", p.display()))
        }

        Sink::Log => {
            let _ = etx.send(RunnerEvent::Log(format!("{name}: {}", brief(v))));
            Ok(())
        }

        // 큐 맨 앞의 요청에 답한다. "요청 하나씩 규칙" 덕분에 그게 반드시 이 값의 주인이다.
        Sink::HttpReply { server } => {
            let Some(srv) = servers.get_mut(server) else {
                return Err("가리키는 HTTP 서버 노드가 열려 있지 않다".into());
            };
            let Some((_, request)) = srv.pending.pop_front() else {
                return Err(
                    "답할 HTTP 요청이 없다 (이미 시간 초과로 닫혔거나, HTTP 서버에서 온 값이 아니다)".into()
                );
            };
            respond_json(request, 200, &value_to_json(v).to_string())
        }

        Sink::GuiWidget { widget } => {
            if matches!(v, Value::Image { .. }) && etx.len() >= IMAGE_BACKLOG_LIMIT {
                // 소비자가 밀렸다. 새 프레임을 버려 지연이 쌓이지 않게 한다.
                return Ok(());
            }
            let _ = etx.send(RunnerEvent::Widget { widget: *widget, value: v.clone() });
            Ok(())
        }

        // 텍스트 값은 그대로, 나머지는 JSON 문자열로 보낸다.
        Sink::WebSocketSend { url } => {
            let Some(tx) = &st.ws_out else {
                return Err(format!("{url} 연결이 준비되지 않았다"));
            };
            let text = match v {
                Value::Text(t) => t.clone(),
                other => value_to_json(other).to_string(),
            };
            tx.send(text).map_err(|_| format!("{url} 연결 스레드가 사라져 보내지 못했다"))
        }
    }
}

// ───────────────────────────── 값 변환 ─────────────────────────────

pub(crate) fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Number(_) => "숫자",
        Value::Numbers(_) => "숫자 벡터",
        Value::Text(_) => "텍스트",
        Value::Json(_) => "JSON",
        Value::Image { .. } => "이미지",
        Value::Tensor(_) => "텐서",
    }
}

/// 로그 한 줄용 짧은 표현.
fn brief(v: &Value) -> String {
    match v {
        Value::Image { width, height, .. } => format!("이미지 {width}x{height}"),
        Value::Tensor(t) => format!("텐서 {:?}", t.shape),
        Value::Text(s) => format!("{:?}", first_line(s)),
        other => value_to_json(other).to_string(),
    }
}

/// 값을 숫자 하나로. 벡터·텐서는 최댓값(확률 벡터에서 "가장 확실한 정도")을 쓴다.
pub(crate) fn as_f64(v: &Value) -> Result<f64, String> {
    match v {
        Value::Number(n) => Ok(*n),
        Value::Numbers(n) => n
            .iter()
            .copied()
            .fold(None::<f32>, |m, x| Some(m.map_or(x, |a| a.max(x))))
            .map(|x| x as f64)
            .ok_or_else(|| "빈 벡터는 숫자로 볼 수 없다".to_string()),
        Value::Text(s) => s.trim().parse::<f64>().map_err(|_| format!("숫자로 읽을 수 없는 텍스트: {s:?}")),
        Value::Json(j) => json_as_f64(j),
        Value::Tensor(t) => t
            .data
            .iter()
            .copied()
            .fold(None::<f32>, |m, x| Some(m.map_or(x, |a| a.max(x))))
            .map(|x| x as f64)
            .ok_or_else(|| "빈 텐서는 숫자로 볼 수 없다".to_string()),
        Value::Image { .. } => Err("이미지는 숫자로 볼 수 없다".into()),
    }
}

fn json_as_f64(j: &serde_json::Value) -> Result<f64, String> {
    match j {
        serde_json::Value::Number(n) => n.as_f64().ok_or_else(|| format!("숫자로 읽을 수 없다: {n}")),
        serde_json::Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::String(s) => {
            s.trim().parse::<f64>().map_err(|_| format!("숫자로 읽을 수 없는 문자열: {s:?}"))
        }
        other => Err(format!("숫자로 볼 수 없는 JSON: {other}")),
    }
}

/// 값을 정수 인덱스로. 벡터·텐서는 argmax(가장 큰 원소의 위치)를 쓴다.
pub(crate) fn as_i64(v: &Value) -> Result<i64, String> {
    match v {
        Value::Number(n) => Ok(n.round() as i64),
        Value::Numbers(n) => argmax(n).ok_or_else(|| "빈 벡터에는 argmax 가 없다".to_string()),
        Value::Text(s) => {
            let t = s.trim();
            t.parse::<i64>().or_else(|_| t.parse::<f64>().map(|x| x.round() as i64)).map_err(|_| {
                format!("정수로 읽을 수 없는 텍스트: {s:?}")
            })
        }
        Value::Json(j) => json_as_f64(j).map(|x| x.round() as i64),
        Value::Tensor(t) => {
            if t.numel() == 0 {
                Err("빈 텐서에는 argmax 가 없다".into())
            } else if t.numel() == 1 {
                Ok(t.data[0].round() as i64)
            } else {
                argmax(&t.data).ok_or_else(|| "argmax 실패".to_string())
            }
        }
        Value::Image { .. } => Err("이미지는 정수로 볼 수 없다".into()),
    }
}

fn argmax(v: &[f32]) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    let mut best = (0usize, v[0]);
    for (i, x) in v.iter().enumerate().skip(1) {
        if *x > best.1 {
            best = (i, *x);
        }
    }
    Some(best.0 as i64)
}

/// `{{value}}` 치환·stdout·파일 출력에 쓰는 JSON 표현.
/// JSON 값은 그대로, 텍스트는 따옴표 붙은 JSON 문자열이 된다 (템플릿이 `{"m": {{value}}}` 형태여도 깨지지 않게).
pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Number(n) => json!(n),
        Value::Numbers(n) => json!(n),
        Value::Text(s) => json!(s),
        Value::Json(j) => j.clone(),
        Value::Image { width, height, rgba } => json!({ "image": { "width": width, "height": height, "bytes": rgba.len() } }),
        Value::Tensor(t) => json!({ "shape": t.shape, "data": t.data }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::pipeline::{PNode, PNodeKind};
    use nl_core::{ModelDef, Sink, Source};

    fn drain_for(h: &RunnerHandle, timeout: Duration) -> Vec<RunnerEvent> {
        let mut out = Vec::new();
        let start = Instant::now();
        while start.elapsed() < timeout {
            match h.events.recv_timeout(Duration::from_millis(20)) {
                Ok(e) => out.push(e),
                Err(_) => continue,
            }
        }
        out
    }

    /// `timeout` 안에 조건에 맞는 이벤트가 올 때까지 기다린다.
    fn wait_for<F: Fn(&RunnerEvent) -> bool>(h: &RunnerHandle, timeout: Duration, f: F) -> Option<RunnerEvent> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            match h.events.recv_timeout(Duration::from_millis(50)) {
                Ok(e) => {
                    if f(&e) {
                        return Some(e);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("nl-io-test-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).expect("임시 폴더 생성 실패");
        p
    }

    // ── 파이프라인 통합 ──

    #[test]
    fn timer_threshold_log_runs_and_stops() {
        let mut p = Pipeline::new("t");
        p.tick_hz = 60.0;
        let src = p.add_node(PNode::new(PNodeKind::Source { source: Source::Timer { interval_ms: 10 } }, [0.0, 0.0]));
        let thr = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Threshold { value: 0.5 } }, [1.0, 0.0]));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        assert!(p.add_link(src, thr).is_some());
        assert!(p.add_link(thr, log).is_some());

        let h = Runner::new(Project::new("p"), p, tmp_dir("timer"), DevicePref::Cpu).start().unwrap();
        assert!(wait_for(&h, Duration::from_secs(2), |e| matches!(e, RunnerEvent::Started)).is_some());
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Value { .. })).is_some(),
            "값 이벤트가 오지 않았다"
        );
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Log(_))).is_some(),
            "Log 싱크 이벤트가 오지 않았다"
        );

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)), "stop() 후에도 끝나지 않았다");
        assert!(t.elapsed() < Duration::from_millis(300), "stop 이 너무 느리다: {:?}", t.elapsed());
        assert!(
            wait_for(&h, Duration::from_secs(1), |e| matches!(e, RunnerEvent::Stopped)).is_some(),
            "Stopped 이벤트가 오지 않았다"
        );
    }

    #[test]
    fn manual_select_log_passes_the_chosen_element() {
        let mut p = Pipeline::new("m");
        p.tick_hz = 60.0;
        let src = p.add_node(PNode::new(PNodeKind::Source { source: Source::Manual }, [0.0, 0.0]));
        let sel = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Select { index: 1 } }, [1.0, 0.0]));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        p.add_link(src, sel).unwrap();
        p.add_link(sel, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("manual"), DevicePref::Cpu).start().unwrap();
        h.inputs.send(RunnerInput::Manual { node: src, value: Value::Numbers(vec![1.0, 5.0, 2.0]) }).unwrap();

        let ev = wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Value { node, .. } if *node == sel));
        match ev {
            Some(RunnerEvent::Value { value, .. }) => assert_eq!(value, Value::Number(5.0)),
            other => panic!("Select 결과가 오지 않았다: {other:?}"),
        }
        assert!(wait_for(&h, Duration::from_secs(2), |e| matches!(e, RunnerEvent::Log(_))).is_some());
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)));
    }

    #[test]
    fn model_node_reports_session_failure_and_keeps_looping() {
        let mut project = Project::new("p");
        let mut def = ModelDef::new("분류기");
        def.weights = None;
        let mid = def.id;
        project.models.insert(mid, def);

        let mut p = Pipeline::new("model");
        p.tick_hz = 60.0;
        let src = p.add_node(PNode::new(PNodeKind::Source { source: Source::Timer { interval_ms: 10 } }, [0.0, 0.0]));
        let m = p.add_node(PNode::new(PNodeKind::Model { model: mid, payload: None }, [1.0, 0.0]));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        p.add_link(src, m).unwrap();
        p.add_link(m, log).unwrap();

        let h = Runner::new(project, p, tmp_dir("model"), DevicePref::Cpu).start().unwrap();
        // nl-engine 이 아직 스텁이라 Session::load 가 실패한다. 그래도 루프는 계속 돌아야 한다.
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Error { node, .. } if *node == Some(m)))
                .is_some(),
            "모델 오류 이벤트가 오지 않았다"
        );
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Value { node, .. } if *node == src))
                .is_some(),
            "모델이 실패한 뒤 타이머가 멈췄다"
        );
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)));
        assert!(wait_for(&h, Duration::from_secs(1), |e| matches!(e, RunnerEvent::Stopped)).is_some());
    }

    #[test]
    fn cyclic_nodes_are_reported_not_executed() {
        let mut p = Pipeline::new("cycle");
        let a = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Select { index: 0 } }, [0.0, 0.0]));
        let b = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Select { index: 0 } }, [1.0, 0.0]));
        p.add_link(a, b).unwrap();
        p.add_link(b, a).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("cycle"), DevicePref::Cpu).start().unwrap();
        assert!(
            wait_for(&h, Duration::from_secs(2), |e| {
                matches!(e, RunnerEvent::Error { message, .. } if message.contains("순환"))
            })
            .is_some(),
            "순환 경고가 오지 않았다"
        );
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)));
    }

    #[test]
    fn empty_pipeline_stops_with_an_error() {
        let h = Runner::new(Project::new("p"), Pipeline::new("빈"), tmp_dir("empty"), DevicePref::Cpu)
            .start()
            .unwrap();
        let evs = drain_for(&h, Duration::from_millis(400));
        assert!(evs.iter().any(|e| matches!(e, RunnerEvent::Error { .. })), "{evs:?}");
        assert!(evs.iter().any(|e| matches!(e, RunnerEvent::Stopped)), "{evs:?}");
        assert!(h.is_done());
    }

    #[test]
    fn file_sink_writes_json_lines() {
        let dir = tmp_dir("filesink");
        let mut p = Pipeline::new("f");
        p.tick_hz = 60.0;
        let src = p.add_node(PNode::new(PNodeKind::Source { source: Source::Timer { interval_ms: 10 } }, [0.0, 0.0]));
        let sink = p.add_node(PNode::new(
            PNodeKind::Sink { sink: Sink::File { path: "out.jsonl".into(), append: true } },
            [1.0, 0.0],
        ));
        p.add_link(src, sink).unwrap();
        let h = Runner::new(Project::new("p"), p, dir.clone(), DevicePref::Cpu).start().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)));
        let text = std::fs::read_to_string(dir.join("out.jsonl")).expect("싱크가 파일을 만들지 않았다");
        assert!(!text.trim().is_empty(), "파일이 비어 있다");
        assert!(text.lines().next().unwrap().parse::<f64>().is_ok(), "JSON 숫자 줄이 아니다: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 로직 단위 ──

    fn st() -> NodeState {
        NodeState::default()
    }

    #[test]
    fn debounce_drops_repeats_inside_the_window() {
        let mut s = st();
        let logic = Logic::Debounce { ms: 1000 };
        let t0 = Instant::now();
        let v = Value::Number(3.0);
        assert_eq!(eval_logic(&logic, &v, &mut s, t0).unwrap(), Some(Value::Number(3.0)));
        // 같은 값, 창 안 → 버린다.
        assert_eq!(eval_logic(&logic, &v, &mut s, t0 + Duration::from_millis(100)).unwrap(), None);
        // 다른 값은 즉시 통과.
        let other = Value::Number(4.0);
        assert_eq!(eval_logic(&logic, &other, &mut s, t0 + Duration::from_millis(150)).unwrap(), Some(other.clone()));
        // 창을 지나면 같은 값도 다시 통과.
        assert_eq!(
            eval_logic(&logic, &other, &mut s, t0 + Duration::from_millis(1300)).unwrap(),
            Some(Value::Number(4.0))
        );
    }

    #[test]
    fn majority_returns_the_most_common_of_the_window() {
        let mut s = st();
        let logic = Logic::Majority { window: 3 };
        let now = Instant::now();
        let f = |s: &mut NodeState, x: f64| match eval_logic(&logic, &Value::Number(x), s, now).unwrap() {
            Some(Value::Number(n)) => n,
            other => panic!("숫자가 아니다: {other:?}"),
        };
        assert_eq!(f(&mut s, 1.0), 1.0);
        assert_eq!(f(&mut s, 1.0), 1.0);
        assert_eq!(f(&mut s, 0.0), 1.0); // [1,1,0] → 1
        assert_eq!(f(&mut s, 0.0), 0.0); // [1,0,0] → 0
        assert_eq!(f(&mut s, 0.0), 0.0); // [0,0,0] → 0
    }

    #[test]
    fn threshold_compares_against_the_value() {
        let mut s = st();
        let logic = Logic::Threshold { value: 0.5 };
        let now = Instant::now();
        assert_eq!(eval_logic(&logic, &Value::Number(0.4), &mut s, now).unwrap(), Some(Value::Number(0.0)));
        assert_eq!(eval_logic(&logic, &Value::Number(0.5), &mut s, now).unwrap(), Some(Value::Number(1.0)));
        // 벡터는 최댓값 기준.
        assert_eq!(
            eval_logic(&logic, &Value::Numbers(vec![0.1, 0.9]), &mut s, now).unwrap(),
            Some(Value::Number(1.0))
        );
    }

    #[test]
    fn map_only_passes_values_in_the_table() {
        let mut s = st();
        let mut table = BTreeMap::new();
        table.insert(1i64, 7i64);
        let logic = Logic::Map { table };
        let now = Instant::now();
        assert_eq!(eval_logic(&logic, &Value::Number(1.0), &mut s, now).unwrap(), Some(Value::Number(7.0)));
        assert_eq!(eval_logic(&logic, &Value::Number(2.0), &mut s, now).unwrap(), None);
    }

    #[test]
    fn select_reports_out_of_range() {
        let mut s = st();
        let logic = Logic::Select { index: 5 };
        let now = Instant::now();
        assert!(eval_logic(&logic, &Value::Numbers(vec![1.0]), &mut s, now).is_err());
        assert!(eval_logic(&logic, &Value::Text("x".into()), &mut s, now).is_err());
    }

    // ── 값 변환 단위 ──

    #[test]
    fn as_i64_uses_argmax_for_vectors() {
        assert_eq!(as_i64(&Value::Numbers(vec![0.1, 0.7, 0.2])).unwrap(), 1);
        assert_eq!(as_i64(&Value::Number(2.6)).unwrap(), 3);
        assert_eq!(as_i64(&Value::Text(" 4 ".into())).unwrap(), 4);
        assert_eq!(as_i64(&Value::Text("4.6".into())).unwrap(), 5);
        assert_eq!(as_i64(&Value::Json(serde_json::json!(3))).unwrap(), 3);
        assert_eq!(as_i64(&Value::Json(serde_json::json!(true))).unwrap(), 1);
        assert_eq!(as_i64(&Value::Tensor(HostTensor::scalar(2.0))).unwrap(), 2);
        assert_eq!(as_i64(&Value::Tensor(HostTensor::new(vec![1, 3], vec![0.0, 9.0, 1.0]))).unwrap(), 1);
        assert!(as_i64(&Value::Image { width: 1, height: 1, rgba: vec![0; 4] }).is_err());
        assert!(as_i64(&Value::Text("사과".into())).is_err());
    }

    #[test]
    fn as_f64_uses_max_for_vectors() {
        assert_eq!(as_f64(&Value::Numbers(vec![0.1, 0.7])).unwrap(), 0.7f32 as f64);
        assert_eq!(as_f64(&Value::Number(1.5)).unwrap(), 1.5);
        assert!(as_f64(&Value::Numbers(vec![])).is_err());
    }

    #[test]
    fn value_to_json_keeps_json_as_is_and_quotes_text() {
        assert_eq!(value_to_json(&Value::Json(serde_json::json!({"a":1}))), serde_json::json!({"a":1}));
        assert_eq!(value_to_json(&Value::Text("안녕".into())).to_string(), "\"안녕\"");
        assert_eq!(value_to_json(&Value::Number(2.0)).to_string(), "2.0");
        // 이미지는 바이트를 통째로 싣지 않는다.
        let j = value_to_json(&Value::Image { width: 2, height: 1, rgba: vec![0; 8] });
        assert_eq!(j["image"]["width"], serde_json::json!(2));
        assert_eq!(j["image"]["bytes"], serde_json::json!(8));
    }

    #[test]
    fn text_to_value_prefers_json() {
        assert_eq!(text_to_value("{\"a\":1}"), Value::Json(serde_json::json!({"a":1})));
        assert_eq!(text_to_value("그냥 글"), Value::Text("그냥 글".into()));
    }

    #[test]
    fn due_skips_missed_intervals() {
        let mut s = st();
        let t0 = Instant::now();
        assert!(due(&mut s, t0, Duration::from_millis(10)), "첫 호출은 바로 낸다");
        assert!(!due(&mut s, t0, Duration::from_millis(10)));
        // 100ms 뒤 = 10주기가 밀렸지만 한 번만 낸다.
        assert!(due(&mut s, t0 + Duration::from_millis(100), Duration::from_millis(10)));
        assert!(!due(&mut s, t0 + Duration::from_millis(100), Duration::from_millis(10)));
    }

    #[test]
    fn zero_interval_does_not_hang() {
        let mut s = st();
        let t0 = Instant::now();
        assert!(due(&mut s, t0, Duration::ZERO));
        assert!(due(&mut s, t0 + Duration::from_millis(5), Duration::ZERO));
    }

    #[test]
    fn encode_input_without_payload_accepts_numbers_only() {
        let t = encode_input(None, &Value::Numbers(vec![1.0, 2.0])).unwrap();
        assert_eq!(t.shape, vec![1, 2]);
        assert_eq!(encode_input(None, &Value::Number(3.0)).unwrap().shape, vec![1, 1]);
        assert!(encode_input(None, &Value::Text("x".into())).is_err());
    }

    #[test]
    fn manual_source_consumes_the_value_once() {
        let id = PNodeId::from_u128(7);
        let mut s = st();
        let mut manual = HashMap::new();
        let mut widgets = HashMap::new();
        manual.insert(id, Value::Number(1.0));
        let dir = std::env::temp_dir();
        let mut servers = HashMap::new();
        let mut call = |s: &mut NodeState, m: &mut HashMap<PNodeId, Value>, w: &mut HashMap<WidgetId, Value>| {
            eval_source(&Source::Manual, id, s, Instant::now(), &dir, w, m, &mut servers).unwrap()
        };
        assert_eq!(call(&mut s, &mut manual, &mut widgets), Some(Value::Number(1.0)));
        assert_eq!(call(&mut s, &mut manual, &mut widgets), None, "수동 입력은 한 번만 쓰인다");
    }

    #[test]
    fn websocket_source_without_a_connection_reports_it() {
        let mut s = st();
        let src = Source::WebSocket { url: "ws://x".into() };
        let dir = std::env::temp_dir();
        let (mut m, mut w) = (HashMap::new(), HashMap::new());
        let mut servers = HashMap::new();
        let e = eval_source(&src, PNodeId::from_u128(1), &mut s, Instant::now(), &dir, &mut w, &mut m, &mut servers)
            .unwrap_err();
        assert!(e.contains("ws://x"), "{e}");
    }

    #[test]
    fn websocket_sink_serializes_text_as_is_and_others_as_json() {
        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        let mut s = st();
        s.ws_out = Some(tx);
        let sink = Sink::WebSocketSend { url: "ws://x".into() };
        let (etx, _erx) = crossbeam_channel::unbounded();
        let mut sim = InputSim::new().unwrap();
        let dir = std::env::temp_dir();
        let now = Instant::now();

        let mut servers = HashMap::new();
        eval_sink(&sink, &Value::Text("그대로".into()), &mut s, now, &dir, &mut sim, &etx, "n", &mut servers).unwrap();
        assert_eq!(rx.try_recv().unwrap(), "그대로", "텍스트는 따옴표 없이 그대로 나가야 한다");

        eval_sink(&sink, &Value::Number(3.0), &mut s, now, &dir, &mut sim, &etx, "n", &mut servers).unwrap();
        assert_eq!(rx.try_recv().unwrap(), "3.0");

        eval_sink(&sink, &Value::Json(serde_json::json!({"a":1})), &mut s, now, &dir, &mut sim, &etx, "n", &mut servers)
            .unwrap();
        assert_eq!(rx.try_recv().unwrap(), r#"{"a":1}"#);
    }

    // ── 인바운드 HTTP 서버 ──

    /// 비어 있는 TCP 포트를 잡아 주소만 돌려준다 (리스너는 바로 닫는다).
    fn free_addr() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("포트를 잡지 못했다");
        let a = l.local_addr().expect("주소를 알 수 없다");
        drop(l);
        a.to_string()
    }

    fn http_server_node(p: &mut Pipeline, bind: &str, path: &str) -> PNodeId {
        p.add_node(PNode::new(
            PNodeKind::Source { source: Source::HttpServer { bind: bind.into(), path: path.into() } },
            [0.0, 0.0],
        ))
    }

    /// 서버가 실제로 뜰 때까지 기다린다 (Log 이벤트로 확인).
    fn wait_server_up(h: &RunnerHandle) {
        assert!(
            wait_for(h, Duration::from_secs(5), |e| matches!(e, RunnerEvent::Log(m) if m.contains("HTTP 서버")))
                .is_some(),
            "HTTP 서버가 열리지 않았다"
        );
    }

    #[test]
    fn http_server_select_reply_answers_a_post() {
        let addr = free_addr();
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, &addr, "/infer");
        // 본문 {"x":[1,9,2]} → Select 로 x 를 꺼낸다.
        let pick = p.add_node(PNode::new(PNodeKind::Logic { logic: Logic::Select { index: 1 } }, [1.0, 0.0]));
        let reply = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server } }, [2.0, 0.0]));
        p.add_link(server, pick).unwrap();
        p.add_link(pick, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpsrv"), DevicePref::Cpu).start().unwrap();
        wait_server_up(&h);

        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[1, 9, 2]"),
            Duration::from_secs(5),
        )
        .expect("요청이 실패했다");
        assert_eq!(res.status, 200, "본문: {}", res.body);
        assert!(res.content_type.contains("application/json"), "content-type: {}", res.content_type);
        // JSON 배열의 1번 원소. 정수는 정수 그대로 돌아온다 (JSON 값은 변환 없이 지나간다).
        assert_eq!(res.json().expect("JSON 이 아니다"), serde_json::json!(9));

        // 두 번째 요청도 같은 서버가 받는다 (요청 하나씩 규칙이 막히지 않는다).
        let res2 = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[5, 7, 3]"),
            Duration::from_secs(5),
        )
        .expect("두 번째 요청이 실패했다");
        assert_eq!(res2.json().unwrap(), serde_json::json!(7));

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    #[test]
    fn http_server_reads_the_query_string_when_the_body_is_empty() {
        let addr = free_addr();
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, &addr, "/q");
        let reply = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server } }, [1.0, 0.0]));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpq"), DevicePref::Cpu).start().unwrap();
        wait_server_up(&h);

        let res = crate::http::call(
            "GET",
            &format!("http://{addr}/q?name=%EA%B0%80+%EB%82%98&n=3"),
            &BTreeMap::new(),
            None,
            Duration::from_secs(5),
        )
        .expect("요청이 실패했다");
        assert_eq!(res.status, 200, "본문: {}", res.body);
        assert_eq!(res.json().unwrap(), serde_json::json!({"name": "가 나", "n": "3"}));

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    #[test]
    fn http_server_returns_404_for_another_path() {
        let addr = free_addr();
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, &addr, "/infer");
        let reply = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server } }, [1.0, 0.0]));
        p.add_link(server, reply).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("http404"), DevicePref::Cpu).start().unwrap();
        wait_server_up(&h);

        let res = crate::http::call("GET", &format!("http://{addr}/nope"), &BTreeMap::new(), None, Duration::from_secs(5))
            .expect("요청이 실패했다");
        assert_eq!(res.status, 404);
        assert!(res.json().unwrap()["error"].is_string(), "본문: {}", res.body);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    /// 응답 싱크가 없으면 제한 시간 뒤 504 가 나가야 한다.
    /// 기본 10초를 다 기다리지 않도록 `http_reply_timeout` 을 줄여 실제 응답을 받아 본다.
    #[test]
    fn http_server_without_a_reply_sink_answers_504() {
        let addr = free_addr();
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, &addr, "/infer");
        // 응답 싱크 대신 로그만 붙인다 — 값은 흐르지만 답하는 노드가 없다.
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(server, log).unwrap();

        let mut runner = Runner::new(Project::new("p"), p, tmp_dir("http504"), DevicePref::Cpu);
        assert_eq!(runner.http_reply_timeout, HTTP_REPLY_TIMEOUT, "기본값이 상수와 달라졌다");
        runner.http_reply_timeout = Duration::from_millis(300);
        let h = runner.start().unwrap();
        wait_server_up(&h);

        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("{}"),
            Duration::from_secs(5),
        )
        .expect("504 응답이 오지 않았다");
        assert_eq!(res.status, 504, "본문: {}", res.body);
        let body = res.json().expect("504 본문이 JSON 이 아니다");
        assert_eq!(body["status"], serde_json::json!(504));
        assert!(body["error"].as_str().unwrap().contains("응답"), "본문: {}", res.body);

        // 504 가 나왔다는 것 자체가 "요청은 받았고, 제한 시간 안에 답이 없었다" 는 증거다.
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    #[test]
    fn stopping_closes_the_socket_and_refuses_new_connections() {
        let addr = free_addr();
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, &addr, "/infer");
        let reply = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server } }, [1.0, 0.0]));
        p.add_link(server, reply).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("httpstop"), DevicePref::Cpu).start().unwrap();
        wait_server_up(&h);

        // 살아 있을 때는 답한다.
        let url = format!("http://{addr}/infer");
        let ok = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(ok.status, 200);

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)), "stop 후에도 끝나지 않았다");
        assert!(t.elapsed() < Duration::from_millis(400), "HTTP 서버 정리가 느리다: {:?}", t.elapsed());

        // 소켓이 닫혔으니 새 연결은 거부된다.
        let mut refused = false;
        for _ in 0..20 {
            match std::net::TcpStream::connect(&addr) {
                Ok(_) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "stop 뒤에도 {addr} 이 연결을 받는다");
    }

    #[test]
    fn normalize_path_is_forgiving() {
        assert_eq!(normalize_path("/infer"), "/infer");
        assert_eq!(normalize_path("infer"), "/infer");
        assert_eq!(normalize_path("/infer/"), "/infer");
        assert_eq!(normalize_path("  /a/b/  "), "/a/b");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn query_strings_become_json_objects() {
        assert_eq!(query_to_value("a=1&b=hi"), Value::Json(serde_json::json!({"a":"1","b":"hi"})));
        // `+` 는 공백, `%XX` 는 바이트.
        assert_eq!(query_to_value("s=a+b%21"), Value::Json(serde_json::json!({"s":"a b!"})));
        // 값 없는 키, 빈 쿼리.
        assert_eq!(query_to_value("flag"), Value::Json(serde_json::json!({"flag":""})));
        assert_eq!(query_to_value(""), Value::Json(serde_json::json!({})));
        // 깨진 이스케이프는 그대로 둔다.
        assert_eq!(form_decode("a%zz"), "a%zz");
        assert_eq!(form_decode("%ED%95%9C"), "한");
    }

    // ── WebSocket 통합 (로컬 에코 서버) ──

    /// 붙는 클라이언트마다 받은 텍스트를 그대로 돌려주는 시험용 서버.
    /// 드롭되면 리스너를 닫고 스레드를 접는다.
    struct EchoServer {
        url: String,
        stop: Arc<AtomicBool>,
        /// 서버가 실제로 받은 메시지 (싱크 검증용).
        seen: Receiver<String>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl EchoServer {
        /// `greet` 이 있으면 연결 직후 그 문자열을 먼저 보낸다.
        fn start(greet: Option<&str>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("시험 서버를 열지 못했다");
            let url = format!("ws://{}", listener.local_addr().expect("주소를 알 수 없다"));
            listener.set_nonblocking(true).expect("논블로킹 설정 실패");
            let stop = Arc::new(AtomicBool::new(false));
            let (seen_tx, seen) = crossbeam_channel::unbounded();
            let (s2, greet) = (stop.clone(), greet.map(|g| g.to_string()));
            let handle = std::thread::spawn(move || {
                while !s2.load(Ordering::SeqCst) {
                    let stream = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream.set_nonblocking(false).ok();
                    let Ok(mut ws) = tungstenite::accept(stream) else { continue };
                    ws.get_mut().set_read_timeout(Some(Duration::from_millis(20))).ok();
                    if let Some(g) = &greet {
                        let _ = ws.send(tungstenite::Message::Text(g.clone().into()));
                    }
                    while !s2.load(Ordering::SeqCst) {
                        match ws.read() {
                            Ok(tungstenite::Message::Text(t)) => {
                                let _ = seen_tx.send(t.to_string());
                                if ws.send(tungstenite::Message::Text(t)).is_err() {
                                    break;
                                }
                            }
                            Ok(tungstenite::Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
                            Err(_) => break,
                        }
                    }
                    let _ = ws.close(None);
                }
            });
            Self { url, stop, seen, handle: Some(handle) }
        }
    }

    impl Drop for EchoServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn websocket_source_receives_frames_into_the_log() {
        let server = EchoServer::start(Some(r#"{"hello":1}"#));
        let mut p = Pipeline::new("ws-in");
        p.tick_hz = 60.0;
        let src = p.add_node(PNode::new(
            PNodeKind::Source { source: Source::WebSocket { url: server.url.clone() } },
            [0.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(src, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsin"), DevicePref::Cpu).start().unwrap();
        let ev = wait_for(&h, Duration::from_secs(5), |e| matches!(e, RunnerEvent::Value { node, .. } if *node == src));
        match ev {
            Some(RunnerEvent::Value { value, .. }) => {
                // 서버가 JSON 텍스트를 보냈으니 Json 으로 들어와야 한다.
                assert_eq!(value, Value::Json(serde_json::json!({"hello":1})));
            }
            other => panic!("WebSocket 수신 값이 오지 않았다: {other:?}"),
        }
        assert!(wait_for(&h, Duration::from_secs(2), |e| matches!(e, RunnerEvent::Log(_))).is_some());

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)), "stop 후에도 끝나지 않았다");
        assert!(t.elapsed() < Duration::from_millis(400), "WebSocket 스레드 정리가 느리다: {:?}", t.elapsed());
    }

    #[test]
    fn websocket_sink_sends_and_shares_the_connection_with_the_source() {
        let server = EchoServer::start(None);
        let mut p = Pipeline::new("ws-roundtrip");
        p.tick_hz = 60.0;
        // 같은 url 의 소스와 싱크 → 연결 하나를 공유한다. 보낸 것이 에코로 되돌아온다.
        let manual = p.add_node(PNode::new(PNodeKind::Source { source: Source::Manual }, [0.0, 0.0]));
        let out = p.add_node(PNode::new(
            PNodeKind::Sink { sink: Sink::WebSocketSend { url: server.url.clone() } },
            [1.0, 0.0],
        ));
        let back = p.add_node(PNode::new(
            PNodeKind::Source { source: Source::WebSocket { url: server.url.clone() } },
            [0.0, 1.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 1.0]));
        p.add_link(manual, out).unwrap();
        p.add_link(back, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsout"), DevicePref::Cpu).start().unwrap();
        // 연결이 설 때까지 기다렸다가 보낸다.
        assert!(
            wait_for(&h, Duration::from_secs(5), |e| matches!(e, RunnerEvent::Log(m) if m.contains("연결됨")))
                .is_some(),
            "WebSocket 연결 로그가 오지 않았다"
        );
        h.inputs.send(RunnerInput::Manual { node: manual, value: Value::Text("핑".into()) }).unwrap();

        let got = server.seen.recv_timeout(Duration::from_secs(5)).expect("서버가 메시지를 받지 못했다");
        assert_eq!(got, "핑", "텍스트 값은 따옴표 없이 그대로 가야 한다");

        let ev = wait_for(&h, Duration::from_secs(5), |e| matches!(e, RunnerEvent::Value { node, .. } if *node == back));
        match ev {
            Some(RunnerEvent::Value { value, .. }) => assert_eq!(value, Value::Text("핑".into())),
            other => panic!("에코가 돌아오지 않았다: {other:?}"),
        }

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    #[test]
    fn websocket_failure_reports_and_keeps_the_loop_running() {
        // 아무도 듣지 않는 포트. 붙지 못하고 백오프로 재시도한다.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap();
            drop(l);
            format!("ws://{addr}")
        };
        let mut p = Pipeline::new("ws-dead");
        p.tick_hz = 60.0;
        let ws = p.add_node(PNode::new(PNodeKind::Source { source: Source::WebSocket { url: dead } }, [0.0, 0.0]));
        let timer = p.add_node(PNode::new(
            PNodeKind::Source { source: Source::Timer { interval_ms: 10 } },
            [0.0, 1.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(ws, log).unwrap();
        p.add_link(timer, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsdead"), DevicePref::Cpu).start().unwrap();
        assert!(
            wait_for(&h, Duration::from_secs(5), |e| matches!(e, RunnerEvent::Error { node, .. } if *node == Some(ws)))
                .is_some(),
            "연결 실패 오류가 오지 않았다"
        );
        // 루프는 계속 돈다.
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| matches!(e, RunnerEvent::Value { node, .. } if *node == timer))
                .is_some(),
            "WebSocket 실패 뒤 타이머가 멈췄다"
        );

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)), "stop 후에도 끝나지 않았다");
        // 백오프로 자고 있어도 곧바로 깨야 한다.
        assert!(t.elapsed() < Duration::from_millis(400), "백오프 중 stop 이 느리다: {:?}", t.elapsed());
        assert!(wait_for(&h, Duration::from_secs(1), |e| matches!(e, RunnerEvent::Stopped)).is_some());
    }
}
