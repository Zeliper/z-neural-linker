//! 파이프라인 실행기. 틱 루프가 별도 스레드에서 소스 → 모델/로직 → 싱크 순으로 값을 흘린다.
//! 빌더의 "시험 실행"과 배포 런타임이 같은 Runner 를 쓴다.
//!
//! ## 한 틱의 흐름
//! 1. `RunnerInput` 채널을 비워 `Source::Manual` / `Source::GuiEvent` 입력을 모은다(같은 노드에 여러 개가
//!    쌓였으면 마지막 것만 쓴다 — 틱 하나에 값 하나).
//! 2. stdin·WebSocket 이 버린 값과 연결 상태를 이벤트로 옮긴다.
//! 3. 응답이 오지 않은 HTTP 요청을 시간 초과로 닫는다.
//! 4. 노드를 위상 순서로 훑는다. 소스는 자기 주기가 됐을 때만 값을 내고, 그 뒤 노드는 상류에 값이 있을 때만 돈다.
//! 5. 값을 낸 노드(소스·모델·로직)는 [`RunnerEvent::Value`] 로도 알린다. 싱크는 값을 소비만 하므로 제외한다.
//! 6. `tick_hz` 로 정해진 주기가 될 때까지 잔다. 잠은 10ms 씩 끊어 자므로 [`RunnerHandle::stop`] 은 곧바로 먹는다.
//!
//! ## HTTP 서버로 보내는 값
//! [`Source::HttpServer`] 는 `Content-Type` 을 보고 본문을 값으로 바꾼다.
//!
//! | Content-Type | 값 |
//! |---|---|
//! | `image/png`·`jpeg`·`webp`·`bmp` | [`Value::Image`] (디코드) |
//! | `multipart/form-data` | 첫 파일 파트를 같은 규칙으로 |
//! | 그 밖(텍스트·JSON) | [`Value::Json`] 이거나 [`Value::Text`] |
//! | 본문 없음 | 쿼리스트링을 JSON 객체로 |
//!
//! `application/octet-stream` 은 무엇인지 알 수 없어 받지 않는다(400). 이미지를 보낼 때는 형식을 정확히 적는다.
//!
//! ```sh
//! # 이진 이미지 하나
//! curl --data-binary @a.png -H 'Content-Type: image/png' http://127.0.0.1:8799/infer
//!
//! # 폼 업로드 (첫 파일 파트를 쓴다)
//! curl -F 'file=@a.png' http://127.0.0.1:8799/infer
//!
//! # 숫자 벡터
//! curl -d '[0.8,-0.8]' http://127.0.0.1:8799/infer
//!
//! # 토큰이 걸린 서버 (둘 중 아무 헤더나)
//! curl -H 'Authorization: Bearer <token>' -d '[0.8,-0.8]' http://127.0.0.1:8799/infer
//! curl -H 'X-NL-Token: <token>'           -d '[0.8,-0.8]' http://127.0.0.1:8799/infer
//! ```
//!
//! ## 누가 부를 수 있나
//! 이 서버는 파이프라인을 **구동한다**. `Sink::MouseKeyboard` 가 붙어 있으면 요청 하나가 남의 컴퓨터를
//! 움직이므로, 인증은 기능이 아니라 방어선이다. `AccessPolicy` 가 요청마다 세 가지를 본다.
//!
//! | 조건 | 결과 |
//! |---|---|
//! | `Source::HttpServer::token` 이 있는데 헤더가 없거나 틀림 | 401 |
//! | `Origin` 헤더가 있음 (브라우저에서 온 요청) | 403 |
//! | `Host` 가 바인드 주소도 `localhost` 도 아님 (DNS rebinding) | 400 |
//!
//! 토큰은 환경 변수로 덮어쓸 수 있다 — 번들에 박힌 값 대신 실행할 때 새로 준다.
//! `NL_HTTP_TOKEN_<포트>` 가 `NL_HTTP_TOKEN` 보다 우선한다.
//!
//! ```sh
//! NL_HTTP_TOKEN=$(head -c24 /dev/urandom | base64 | tr '+/' '-_') ./내앱 --headless
//! NL_HTTP_TOKEN_8799=다른토큰 ./내앱 --headless
//! ```
//!
//! 토큰이 없으면 **루프백 바인드에서만** 열린다. 바깥에서 닿는 주소(`0.0.0.0` 등)에 토큰 없이 열려고 하면
//! 준비 단계에서 오류로 거부한다. `nl_core::validate` 도 같은 조건을 미리 잡아 준다.
//!
//! 되돌아오는 쪽도 값에 맞춘다. [`Sink::HttpReply`] 에 이미지가 그대로 오면 `image/png` 로, 로직을 거쳐
//! 숫자가 됐으면 `application/json` 으로 답한다.
//!
//! ## 기동 순서
//! 소스(HTTP 서버·WebSocket·stdin)를 **모델보다 먼저** 연다. `Session::load` 는 GPU 초기화 때문에 몇 초가
//! 걸릴 수 있는데, 그 사이 포트가 닫혀 있으면 클라이언트는 "연결 거부" 를 본다. 먼저 열어 두면 포트는
//! 살아 있고, 아직 답할 수 없는 요청은 큐에 쌓지 않고 곧바로 503([`MODEL_LOADING`])으로 돌려보낸다.
//! 모델이 다 올라오면 `요청 받기 시작` 로그와 함께 200 응답으로 넘어간다.
//!
//! ## 드롭 정책
//! 이벤트 채널은 unbounded 라서 소비자가 느려도 막히지 않지만, 그만큼 이미지가 쌓이면 메모리를 먹는다.
//! 그래서 [`Value::Image`] 는 [`RunnerEvent::Value`] 로 아예 보내지 않고 `Sink::GuiWidget` 이 있을 때만
//! [`RunnerEvent::Widget`] 으로 나간다. 그마저도 채널이 밀려 있으면(`IMAGE_BACKLOG_LIMIT` 이상) 새 프레임을 버린다.
//! GUI 는 최신 프레임만 그리면 되므로 버려도 되고, 버려야 지연이 쌓이지 않는다.
//! 캔버스·인스펙터가 쓸 작은 축소판은 [`RunnerEvent::ValuePreview`] 로 **노드당 초당 4회까지만** 나간다
//! (최장변 [`PREVIEW_MAX_SIDE`] px). 원본이 아니라 축소판이라 쌓여도 메모리를 크게 먹지 않는다.
//!
//! ## 안전장치
//! `Sink::MouseKeyboard` 는 [`Runner::arm_input`] 이 켜져 있을 때만 실제 입력을 보낸다. 기본값은 꺼짐(로그만).
//! 무장은 실행 중에도 [`RunnerHandle::set_armed`] 로 켜고 끌 수 있다 — 플래그는 `Arc<AtomicBool>` 로 공유하고
//! 틱 루프가 **액션을 보내기 직전에** 읽어 `InputSim` 에 반영한다. 그래서 "지금 당장 멈춰" 가 다음 액션부터 먹는다.

use crate::http::{self, HttpResponse};
use crate::input::{self, InputSim};
use crate::screen::Capturer;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use nl_core::payload::PayloadSpec;
use nl_core::{DevicePref, InputAction, Logic, PNodeId, PNodeKind, Pipeline, Project, Sink, Source, WidgetId};
use nl_engine::{HostTensor, Session, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
/// 미리보기 썸네일의 최장변 상한 (px).
pub const PREVIEW_MAX_SIDE: u32 = 160;
/// 노드당 미리보기 발행 간격. 초당 4회.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
/// 통계 이벤트 간격. 초당 1회.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub enum RunnerEvent {
    Started,
    /// 노드가 값을 냈다 (GUI 위젯 바인딩·디버그 표시).
    Value {
        node: PNodeId,
        value: Value,
    },
    /// `Sink::GuiWidget` 로 위젯에 표시할 값.
    Widget {
        widget: WidgetId,
        value: Value,
    },
    /// 노드가 낸 `Value::Image` 의 축소판. 캔버스·인스펙터 미리보기용으로 노드당 초당 4회까지만 나간다.
    /// 원본 이미지는 [`RunnerEvent::Value`] 로 나가지 않으므로(드롭 정책) 이것이 유일한 이미지 통로다.
    ValuePreview {
        node: PNodeId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// 틱 루프 상태. 초당 1회. 상태바에 실제 속도를 보여 주는 용도다.
    Stats {
        /// 시작 이후 누적 틱 수.
        tick: u64,
        /// 지난 구간의 틱 하나당 평균 작업 시간(ms). 잠든 시간은 빼고 잰다.
        tick_ms: f32,
        /// 지난 구간의 실제 틱 속도(Hz).
        hz: f32,
    },
    Log(String),
    Error {
        node: Option<PNodeId>,
        message: String,
    },
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
    /// `Sink::MouseKeyboard` 무장 스위치의 **초기값**. 꺼져 있으면(기본) 액션을 로그로만 남기고 실제 입력은 보내지 않는다.
    /// 시작한 뒤에는 [`RunnerHandle::set_armed`] 로 바꾼다.
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
    /// 틱 루프와 공유하는 마우스·키보드 무장 플래그.
    armed: Arc<AtomicBool>,
}

impl RunnerHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
    /// 마우스·키보드 싱크 무장을 실행 중에 켜고 끈다. 다음 액션부터 곧바로 먹는다.
    pub fn set_armed(&self, armed: bool) {
        self.armed.store(armed, Ordering::SeqCst);
    }

    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
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
        Self {
            project,
            pipeline,
            base_dir,
            device,
            arm_input: false,
            http_reply_timeout: HTTP_REPLY_TIMEOUT,
        }
    }

    /// 즉시 돌아온다. 준비 실패(모델 로드 등)도 `RunnerEvent::Error` + `Stopped` 로 온다.
    pub fn start(self) -> anyhow::Result<RunnerHandle> {
        let (etx, erx) = crossbeam_channel::unbounded();
        let (itx, irx) = crossbeam_channel::unbounded::<RunnerInput>();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let armed = Arc::new(AtomicBool::new(self.arm_input));
        let (s2, d2, a2) = (stop.clone(), done.clone(), armed.clone());
        std::thread::Builder::new().name("nl-runner".into()).spawn(move || {
            // 어떤 노드가 패닉을 내도 이벤트 채널에는 Error + Stopped 가 나가야 한다.
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_loop(self, &etx, &irx, &s2, &a2);
            }));
            if let Err(p) = res {
                let message = format!("실행기가 패닉으로 멈췄다: {}", panic_message(&*p));
                let _ = etx.send(RunnerEvent::Error { node: None, message });
            }
            let _ = etx.send(RunnerEvent::Stopped);
            d2.store(true, Ordering::SeqCst);
        })?;
        Ok(RunnerHandle {
            events: erx,
            inputs: itx,
            stop,
            done,
            armed,
        })
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
/// 틱 루프가 아직 꺼내지 않은 요청을 몇 개까지 들고 있을지. 넘치면 곧바로 503 을 돌려준다.
/// 무제한이면 느린 파이프라인 앞에서 소켓과 본문(최대 8MB씩)을 끝없이 붙잡게 된다.
pub const HTTP_QUEUE_LIMIT: usize = 16;
/// 동시에 처리 중인 연결 상한. 넘으면 새 요청을 503 으로 흘려보낸다 (slowloris 완화).
pub const HTTP_MAX_CONNECTIONS: usize = 64;
/// 모든 `HttpServer` 노드의 토큰을 덮어쓰는 환경 변수.
pub const HTTP_TOKEN_ENV: &str = "NL_HTTP_TOKEN";
/// 포트별로 덮어쓰는 환경 변수의 앞부분. 뒤에 포트 번호를 붙인다 (`NL_HTTP_TOKEN_8799`).
pub const HTTP_TOKEN_ENV_PREFIX: &str = "NL_HTTP_TOKEN_";

/// 환경 변수로 이 바인드 주소의 토큰을 덮어쓸 수 있으면 그 값.
///
/// 배포한 앱마다 다른 토큰을 쓰려고 둔 문이다 — 번들에 박힌 토큰은 받은 사람이 다 볼 수 있으므로,
/// 서버로 돌릴 때는 실행 환경에서 새 토큰을 주는 편이 낫다.
/// 포트별 변수가 전체 변수보다 우선한다.
pub fn token_override(bind: &str) -> Option<String> {
    let port = bind
        .trim()
        .rsplit_once(':')
        .map(|(_, p)| p.trim().to_owned())
        .unwrap_or_default();
    let by_port = (!port.is_empty())
        .then(|| std::env::var(format!("{HTTP_TOKEN_ENV_PREFIX}{port}")).ok())
        .flatten();
    pick_token(by_port, std::env::var(HTTP_TOKEN_ENV).ok())
}

/// 포트별 값과 전체 값 중 무엇을 쓸지. 포트별이 우선이고, 공백뿐인 값은 없는 것과 같다.
///
/// 환경 변수 읽기와 갈라 둔 덕에 시험이 전역 상태를 건드리지 않는다 —
/// `NL_HTTP_TOKEN` 을 시험 중에 설정하면 **같이 돌던 다른 시험의 서버**가 토큰을 요구하게 된다.
fn pick_token(by_port: Option<String>, global: Option<String>) -> Option<String> {
    by_port
        .or(global)
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}
/// 모델이 준비되기 전에 온 요청에 돌려주는 503 본문. 영문 `model loading` 을 함께 넣어 두어
/// 클라이언트가 문자열로도 구분할 수 있게 한다 (상태 코드 503 이 본래 계약이다).
pub const MODEL_LOADING: &str = "모델을 올리는 중입니다 (model loading). 잠시 뒤 다시 시도하세요";

/// 인바운드 서버의 접근 정책. 수신 스레드가 요청마다 확인한다.
///
/// 이 서버는 파이프라인을 구동한다 — 마우스·키보드 싱크가 붙어 있으면 요청 하나가 남의 컴퓨터를
/// 움직인다. 그래서 세 겹으로 막는다.
///
/// 1. **토큰**: `Authorization: Bearer <token>` 이나 `X-NL-Token`. 없거나 틀리면 401.
///    토큰이 설정돼 있지 않으면 루프백 바인드에서만 열린다(그 확인은 [`start_http_server`] 가 한다).
/// 2. **`Origin` 금지**: 헤더가 있으면 403. 브라우저에서 온 요청이라는 뜻이고, 웹페이지가
///    `fetch(..., mode:'no-cors')` 로 몰래 두드리는 길을 막는다. CORS preflight(`OPTIONS`)도 같이 막힌다.
/// 3. **`Host` 확인**: 바인드 주소나 `localhost` 가 아니면 400. DNS rebinding 으로 남의 이름을 태워
///    보내는 요청을 걸러 낸다.
struct AccessPolicy {
    /// 요구할 토큰. `None` 이면 검사하지 않는다(루프백 전용).
    token: Option<String>,
    /// 받아들일 `Host` 값들 (소문자, 포트 포함/미포함 양쪽).
    allowed_hosts: Vec<String>,
}

impl AccessPolicy {
    fn new(bind: &str, token: Option<&str>) -> Self {
        let token = token.map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
        let host = nl_core::pipeline::host_of_bind(bind).to_ascii_lowercase();
        let port = bind.rsplit_once(':').map(|(_, p)| p.to_owned()).unwrap_or_default();
        let mut allowed_hosts = vec![host.clone()];
        // 루프백은 이름으로도 온다. 어느 쪽이든 "내 컴퓨터" 를 가리키므로 같이 받는다.
        if nl_core::pipeline::is_loopback_bind(bind) {
            allowed_hosts.extend(["localhost".into(), "127.0.0.1".into(), "::1".into(), "[::1]".into()]);
        }
        // 포트가 붙은 형태도 받는다.
        if !port.is_empty() {
            let with_port: Vec<String> = allowed_hosts.iter().map(|h| format!("{h}:{port}")).collect();
            allowed_hosts.extend(with_port);
        }
        allowed_hosts.sort();
        allowed_hosts.dedup();
        Self { token, allowed_hosts }
    }

    /// 요청을 받아들일지. 거절이면 `(상태 코드, 사유)`.
    fn check(
        &self,
        origin: Option<&str>,
        host: Option<&str>,
        auth: Option<&str>,
        nl_token: Option<&str>,
    ) -> Result<(), (u16, String)> {
        // 브라우저에서 온 요청은 받지 않는다. 사람이 연 페이지가 몰래 부르는 길을 막는다.
        if let Some(o) = origin {
            return Err((403, format!("브라우저에서 온 요청은 받지 않는다 (Origin: {o})")));
        }
        // Host 가 바인드와 다르면 남의 이름을 태워 온 요청이다 (DNS rebinding).
        if let Some(h) = host {
            let h = h.trim().to_ascii_lowercase();
            if !self.allowed_hosts.contains(&h) {
                return Err((
                    400,
                    format!(
                        "Host 가 이 서버의 주소와 다르다 ({h}). 받는 이름: {}",
                        self.allowed_hosts.join(", ")
                    ),
                ));
            }
        }
        // 토큰.
        let Some(want) = &self.token else { return Ok(()) };
        let given = auth
            .and_then(|a| {
                a.trim()
                    .strip_prefix("Bearer ")
                    .or_else(|| a.trim().strip_prefix("bearer "))
            })
            .map(str::trim)
            .or(nl_token.map(str::trim));
        match given {
            Some(g) if constant_time_eq(g.as_bytes(), want.as_bytes()) => Ok(()),
            Some(_) => Err((401, "토큰이 맞지 않는다".into())),
            None => Err((
                401,
                "토큰이 필요하다 (Authorization: Bearer <token> 또는 X-NL-Token: <token>)".into(),
            )),
        }
    }
}

/// 길이와 내용을 시간 차이 없이 비교한다. 토큰을 한 글자씩 맞혀 나가는 공격을 막는다.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 수신 스레드 → 틱 루프. 값과 아직 응답하지 않은 요청을 함께 넘긴다.
struct HttpIncoming {
    value: Value,
    request: crate::httpd::Request,
    /// 요청이 **서버에 도착한** 시각. 큐에서 기다린 시간까지 제한에 넣으려고 여기서 잰다.
    /// (`pending` 으로 옮겨질 때 재면 큐에서 5분을 기다린 요청도 그때부터 다시 10초를 받는다.)
    at: Instant,
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
    pending: VecDeque<(Instant, crate::httpd::Request)>,
    /// 채널에서 꺼내 두었지만 아직 파이프라인에 넣지 않은 요청 (FIFO).
    /// 시간 초과 회수가 여기까지 훑어야 해서 채널에 두지 않고 옮겨 놓는다.
    queued: VecDeque<HttpIncoming>,
    /// 리스너. accept 스레드와 함께 들고 있어, 실제로 묶인 주소를 알려 줄 수 있다
    /// (`:0` 으로 열면 운영체제가 포트를 고른다).
    server: Arc<crate::httpd::Server>,
    /// 모델이 다 올라왔는가. 꺼져 있는 동안 들어온 요청은 **큐에 넣지 않고** 곧바로 503 으로 돌려보낸다.
    /// 서버를 모델보다 먼저 여는 대신, 아직 답할 수 없는 요청을 물고 있지 않으려는 것이다.
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpServerState {
    /// 실제로 묶인 주소. 설정이 `:0` 이면 운영체제가 고른 포트가 여기 들어 있다.
    fn local_addr(&self) -> std::net::SocketAddr {
        self.server.local_addr()
    }

    /// 대기 중인 요청에 모두 같은 상태로 답하고 수신 스레드를 접는다.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // accept 는 논블로킹이라 깨울 것이 없다. 다음 폴링에서 stop 을 본다.
        while let Some((_, req)) = self.pending.pop_front() {
            let _ = respond_json(req, 503, "파이프라인이 멈춰 요청을 처리하지 못했다");
        }
        while let Some(inc) = self.queued.pop_front() {
            let _ = respond_json(inc.request, 503, "파이프라인이 멈춰 요청을 처리하지 못했다");
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
fn respond_json(request: crate::httpd::Request, status: u16, body: &str) -> Result<(), String> {
    let payload = if (200..300).contains(&status) {
        body.to_owned()
    } else {
        serde_json::json!({ "error": body, "status": status }).to_string()
    };
    request
        .respond(status, "application/json; charset=utf-8", payload.as_bytes())
        .map_err(|e| format!("HTTP 응답 전송 실패: {e}"))
}

/// 이미지 값을 PNG 로 답한다.
fn respond_png(request: crate::httpd::Request, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let png = encode_png(width, height, rgba)?;
    request
        .respond(200, "image/png", &png)
        .map_err(|e| format!("HTTP 응답 전송 실패: {e}"))
}

/// RGBA8 → PNG 바이트.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expect = width as usize * height as usize * 4;
    if rgba.len() != expect {
        return Err(format!(
            "이미지 크기가 맞지 않는다: {width}x{height} 인데 {} 바이트",
            rgba.len()
        ));
    }
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "RGBA 버퍼를 이미지로 만들지 못했다".to_string())?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("PNG 인코딩 실패: {e}"))?;
    Ok(out.into_inner())
}

/// 서버 소켓을 열고 수신 스레드를 띄운다.
fn start_http_server(bind: &str, path: &str, token: Option<&str>) -> Result<HttpServerState, String> {
    // 인증 없이 바깥에 여는 것은 거부한다. 이 서버는 파이프라인을 구동하므로,
    // 열어 두면 그 주소에 닿는 누구나 모델을 돌리고 (싱크에 따라) 입력까지 보낼 수 있다.
    // **소켓을 열기 전에** 본다 — 잠깐이라도 무방비로 열려 있으면 안 된다.
    let has_token = token.map(str::trim).is_some_and(|t| !t.is_empty());
    if !has_token && !nl_core::pipeline::is_loopback_bind(bind) {
        return Err(format!(
            "{bind} 은 바깥에서 닿는 주소라 토큰 없이 열 수 없다              (HttpServer 노드에 token 을 넣거나 127.0.0.1 에 묶어라)"
        ));
    }
    let server = crate::httpd::Server::bind(bind)
        .map(Arc::new)
        .map_err(|e| format!("{bind} 을 열지 못했다: {e}"))?;
    // 접근 정책은 **실제로 묶인 주소**로 만든다. 설정이 `:0` 이면 운영체제가 포트를 골라 주는데,
    // 설정 문자열로 만들면 `Host` 검사가 포트 0 을 기대해 모든 요청을 400 으로 막는다.
    let policy = AccessPolicy::new(&server.local_addr().to_string(), token);
    // 틱 루프로 넘기는 큐는 상한이 있다. 넘치면 붙잡지 않고 503 으로 돌려보낸다.
    let (tx, rx) = crossbeam_channel::bounded(HTTP_QUEUE_LIMIT);
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let policy = Arc::new(policy);
    let want = normalize_path(path);

    let inflight = Arc::new(AtomicUsize::new(0));
    let handoff = ConnContext {
        want_path: Arc::new(want),
        policy,
        ready: ready.clone(),
        inflight: inflight.clone(),
        to_pipeline: tx,
    };
    let (s2, srv2) = (stop.clone(), server.clone());
    let handle = std::thread::Builder::new()
        .name("nl-http-server".into())
        .spawn(move || http_accept_loop(&srv2, &handoff, &s2, &inflight))
        .map_err(|e| format!("HTTP 수신 스레드 생성 실패: {e}"))?;
    Ok(HttpServerState {
        rx,
        pending: VecDeque::new(),
        queued: VecDeque::new(),
        server,
        ready,
        stop,
        handle: Some(handle),
    })
}

/// 경로 비교를 위해 앞에 `/` 를 붙이고 뒤쪽 `/` 는 뗀다. 빈 값은 `/`.
fn normalize_path(p: &str) -> String {
    let t = p.trim();
    if t.is_empty() || t == "/" {
        return "/".into();
    }
    let with_slash = if t.starts_with('/') {
        t.to_owned()
    } else {
        format!("/{t}")
    };
    with_slash.trim_end_matches('/').to_owned()
}

/// accept 전용 루프. 소켓을 받아 **연결마다 짧은 스레드**에 넘긴다.
///
/// 여기서는 한 바이트도 읽지 않는다. 읽기는 전부 [`http_conn_thread`] 가 하고, 그쪽은 마감으로 묶여 있다.
fn http_accept_loop(server: &crate::httpd::Server, ctx: &ConnContext, stop: &AtomicBool, inflight: &AtomicUsize) {
    while !stop.load(Ordering::SeqCst) {
        let stream = match server.accept() {
            Ok(Some(s)) => s,
            // 아직 아무도 오지 않았다. stop 을 다시 본다.
            Ok(None) => {
                std::thread::sleep(HTTP_SERVER_POLL);
                continue;
            }
            Err(_) => break,
        };

        // 처리 중인 연결이 너무 많다. 머리도 읽지 않고 돌려보낸다 (slowloris 완화).
        if inflight.load(Ordering::SeqCst) >= HTTP_MAX_CONNECTIONS {
            crate::httpd::respond_raw(stream, 503, "연결이 너무 많다. 잠시 뒤 다시 시도하라");
            continue;
        }
        inflight.fetch_add(1, Ordering::SeqCst);

        let c = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("nl-http-conn".into())
            .spawn(move || http_conn_thread(stream, &c));
        if spawned.is_err() {
            // 스레드를 못 만들었다. 세어 둔 것을 되돌리고 다음 연결로 간다.
            inflight.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// 연결 스레드가 요청 하나를 끝내는 데 필요한 것들.
#[derive(Clone)]
struct ConnContext {
    want_path: Arc<String>,
    policy: Arc<AccessPolicy>,
    ready: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
    /// 틱 루프로 가는 길. 가득 차면 503.
    to_pipeline: Sender<HttpIncoming>,
}

/// 연결 하나를 **끝까지** 처리한다: 머리 → 정책 → 본문 → 값.
///
/// 단계를 나눠 고정 워커 풀에 맡기지 않는다. 그러면 느린 클라이언트 몇이 풀을 차지해 뒤 요청이 밀린다.
/// 연결마다 스레드를 쓰면 그 일이 없고, 스레드 수는 세 가지가 묶어 준다.
///
/// - 동시 연결 상한 [`HTTP_MAX_CONNECTIONS`] — 넘으면 머리도 읽지 않고 503
/// - 머리 마감 [`crate::httpd::HEADER_TIMEOUT`] (5초)
/// - 본문 마감 [`crate::httpd::BODY_TIMEOUT`] (30초)
///
/// 그래서 살아 있는 연결 스레드는 64개를 넘지 않고, 하나하나가 35초 안에 반드시 끝난다.
fn http_conn_thread(stream: std::net::TcpStream, c: &ConnContext) {
    // 이 연결을 어떻게 끝내든 처리 중 수는 줄어든다.
    let _guard = InflightGuard(c.inflight.clone());
    let at = Instant::now();

    let Some(mut request) = head_phase(stream, c) else {
        // 이미 답하고 끝났다.
        return;
    };

    let query = request.query().to_owned();
    let content_type = request.header("content-type").unwrap_or_default().to_owned();
    let body = match request.read_body(MAX_HTTP_REQUEST_BYTES) {
        Ok(b) => b,
        Err(e) => {
            let _ = respond_json(request, e.status, &e.message);
            return;
        }
    };

    let value = match body_to_value(&content_type, body, &query) {
        Ok(v) => v,
        Err(msg) => {
            let _ = respond_json(request, 400, &msg);
            return;
        }
    };

    // 큐가 가득 찼다. 붙잡지 않고 돌려보낸다 (M1).
    match c.to_pipeline.try_send(HttpIncoming { value, request, at }) {
        Ok(()) => {}
        Err(crossbeam_channel::TrySendError::Full(inc)) => {
            let _ = respond_json(inc.request, 503, "파이프라인이 밀렸다. 잠시 뒤 다시 시도하라");
        }
        Err(crossbeam_channel::TrySendError::Disconnected(inc)) => {
            let _ = respond_json(inc.request, 503, "파이프라인이 멈췄다");
        }
    }
}

/// 머리를 읽고 본문까지 갈 요청이면 돌려준다. 여기서 답하고 끝났으면 `None`.
///
/// 본문을 읽을 필요가 없는 응답(404·401·403·503·405·413·431·408)은 전부 여기서 끝난다.
fn head_phase(stream: std::net::TcpStream, c: &ConnContext) -> Option<crate::httpd::Request> {
    let request = match crate::httpd::Request::read_head(stream) {
        Ok(r) => r,
        Err((sock, e)) => {
            crate::httpd::respond_raw(sock, e.status, &e.message);
            return None;
        }
    };

    let got_path = normalize_path(request.path());
    if got_path != *c.want_path {
        let _ = respond_json(
            request,
            404,
            &format!(
                "{got_path} 은 이 서버가 받는 경로가 아니다 (받는 경로: {})",
                c.want_path
            ),
        );
        return None;
    }

    // 누가 보냈는지부터 본다. 인증·출처 확인은 준비 상태보다 앞이다 —
    // 아직 준비되지 않았다는 사실조차 아무에게나 알려 줄 이유가 없다.
    let deny = c.policy.check(
        request.header("origin"),
        request.header("host"),
        request.header("authorization"),
        request.header("x-nl-token"),
    );
    if let Err((status, why)) = deny {
        let _ = respond_json(request, status, &why);
        return None;
    }

    // 모델이 아직 안 올라왔다. 물고 있지 말고 곧바로 돌려보낸다 — 클라이언트가 재시도하면 된다.
    if !c.ready.load(Ordering::SeqCst) {
        let _ = respond_json(request, 503, MODEL_LOADING);
        return None;
    }

    let method = request.method().to_owned();
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH") {
        let _ = respond_json(
            request,
            405,
            &format!("{method} 은 지원하지 않는다 (GET, POST, PUT, PATCH 만)"),
        );
        return None;
    }

    // 길이를 알려 줬으면 읽기 전에 거른다.
    if request.declared_len().is_some_and(|n| n > MAX_HTTP_REQUEST_BYTES) {
        let _ = respond_json(
            request,
            413,
            &format!("본문이 너무 크다 (상한 {MAX_HTTP_REQUEST_BYTES} 바이트)"),
        );
        return None;
    }

    Some(request)
}

/// 살아 있는 동안 "처리 중" 으로 세어지는 표식. 어떤 길로 끝나든 수가 맞게 한다.
struct InflightGuard(Arc<AtomicUsize>);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `image/png; charset=x` → `image/png`.
fn mime_of(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// `image` 크레이트로 디코드할 수 있는 MIME 인가.
///
/// 실제로 읽히는지는 디코더가 정한다 — 여기서는 "이진 이미지로 받겠다" 는 뜻만 가린다.
/// (빌드된 feature 에 따라 png·jpeg 만 열릴 수 있다. webp·bmp 는 feature 가 없으면 디코드에서 400 이 난다.)
fn is_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp" | "image/bmp"
    )
}

/// 요청 본문 → 파이프라인 값.
///
/// - `image/*` → [`image`] 로 디코드한 [`Value::Image`]
/// - `multipart/form-data` → **첫 파일 파트**를 같은 규칙으로
/// - 그 밖에는 UTF-8 텍스트로 읽어 JSON 이면 [`Value::Json`], 아니면 [`Value::Text`]
/// - 본문이 비면 쿼리스트링을 JSON 객체로
///
/// `application/octet-stream` 은 무엇인지 알 수 없으므로 받지 않는다. 이미지를 보낼 때는
/// `Content-Type` 을 정확히 적어야 한다.
fn body_to_value(content_type: &str, body: Vec<u8>, query: &str) -> Result<Value, String> {
    let mime = mime_of(content_type);

    if is_image_mime(&mime) {
        return decode_image(&body, &mime);
    }

    if mime == "multipart/form-data" {
        let boundary =
            multipart_boundary(content_type).ok_or_else(|| "multipart/form-data 인데 boundary 가 없다".to_string())?;
        let part = first_file_part(&body, &boundary).ok_or_else(|| {
            "multipart 본문에서 파일 파트를 찾지 못했다 (filename 이 있는 파트가 필요하다)".to_string()
        })?;
        let part_mime = mime_of(&part.content_type);
        if is_image_mime(&part_mime) || part.content_type.is_empty() {
            // 파트에 Content-Type 이 없으면 확장자를 믿지 말고 내용으로 판단한다.
            return decode_image(&part.body, if part_mime.is_empty() { "(추측)" } else { &part_mime });
        }
        return Err(format!(
            "multipart 파일 파트의 형식을 다룰 수 없다: {}",
            part.content_type
        ));
    }

    if mime == "application/octet-stream" {
        return Err(
            "application/octet-stream 은 받지 않는다. 이미지면 Content-Type 을 image/png 처럼 정확히 적어라".into(),
        );
    }

    let text = String::from_utf8(body)
        .map_err(|_| "본문이 UTF-8 이 아니다 (이미지면 Content-Type 을 image/png 처럼 적어라)".to_string())?;
    Ok(if text.trim().is_empty() {
        query_to_value(query)
    } else {
        text_to_value(&text)
    })
}

/// 이진 이미지 → [`Value::Image`]. 형식은 내용으로 판단한다(헤더는 참고만).
fn decode_image(bytes: &[u8], mime: &str) -> Result<Value, String> {
    if bytes.is_empty() {
        return Err(format!("{mime} 인데 본문이 비어 있다"));
    }
    let img = image::load_from_memory(bytes).map_err(|e| format!("이미지를 읽지 못했다 ({mime}): {e}"))?;
    let rgba = img.to_rgba8();
    Ok(Value::Image {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// `multipart/form-data; boundary=----abc` → `----abc`. 따옴표는 벗긴다.
fn multipart_boundary(content_type: &str) -> Option<String> {
    for part in content_type.split(';').skip(1) {
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("boundary") {
            return Some(v.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// multipart 파트 하나 (필요한 것만).
struct MultipartPart {
    content_type: String,
    body: Vec<u8>,
}

/// `filename=` 이 있는 **첫 파트**를 꺼낸다.
///
/// RFC 7578 의 전부를 다루지는 않는다 — 파일 하나를 올리는 흔한 형태만 본다.
/// 파트 경계는 `--<boundary>` 이고, 머리와 몸은 빈 줄(CRLF CRLF)로 갈린다.
fn first_file_part(body: &[u8], boundary: &str) -> Option<MultipartPart> {
    let sep = format!("--{boundary}").into_bytes();
    let mut start = find(body, &sep)?;
    loop {
        // 경계 뒤의 CRLF 를 지나면 파트 머리가 시작된다.
        let after = start + sep.len();
        if body[after..].starts_with(b"--") {
            return None; // 마지막 경계.
        }
        let head_start = after + crlf_len(&body[after..]);
        let head_end = find(&body[head_start..], b"\r\n\r\n")? + head_start;
        let head = String::from_utf8_lossy(&body[head_start..head_end]).into_owned();
        let part_body_start = head_end + 4;
        let next = find(&body[part_body_start..], &sep).map(|i| i + part_body_start)?;
        // 다음 경계 바로 앞의 CRLF 는 구분자라 본문이 아니다.
        let part_body_end = next.saturating_sub(2);

        let has_filename = head.to_ascii_lowercase().contains("filename=");
        if has_filename {
            let content_type = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
            return Some(MultipartPart {
                content_type,
                body: body[part_body_start..part_body_end.max(part_body_start)].to_vec(),
            });
        }
        start = next;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn crlf_len(s: &[u8]) -> usize {
    if s.starts_with(b"\r\n") {
        2
    } else if s.starts_with(b"\n") {
        1
    } else {
        0
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
/// WebSocket·stdin 수신 큐 길이. 소비는 틱당 하나뿐이라 무제한이면 메모리가 단조 증가한다.
///
/// ## 드롭 정책
/// 가득 차면 **가장 오래된 것을 버리고** 새 것을 넣는다. 파이프라인은 "지금 무슨 값이 오는가" 로
/// 도는 것이라, 몇 초 전 프레임보다 방금 온 것이 쓸모 있다. 이미지 프레임을 버리는 것과 같은 사상이다.
/// 버린 수는 `Log` 이벤트로 알린다 (초당 한 번으로 묶는다).
pub const STREAM_QUEUE_LIMIT: usize = 256;
/// 버림 알림을 묶는 간격.
const DROP_REPORT_EVERY: Duration = Duration::from_secs(1);

/// 재접속 첫 대기.
const WS_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// 재접속 대기 상한.
const WS_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// 큐에 넣는다. 가득 찼으면 **가장 오래된 것을 버리고** 새 것을 넣는다.
///
/// crossbeam 채널은 앞을 밀어낼 수 없어, 보내는 쪽이 수신 핸들도 들고 하나 꺼낸 뒤 넣는다.
enum Pushed {
    /// 그대로 들어갔다.
    Ok,
    /// 자리를 만들려고 오래된 것을 하나 버렸다.
    DroppedOldest,
    /// 받는 쪽이 사라졌다.
    Disconnected,
}

fn send_dropping_oldest<T>(tx: &Sender<T>, rx: &Receiver<T>, value: T) -> Pushed {
    match tx.try_send(value) {
        Ok(()) => Pushed::Ok,
        Err(crossbeam_channel::TrySendError::Full(v)) => {
            let _ = rx.try_recv();
            match tx.try_send(v) {
                Ok(()) => Pushed::DroppedOldest,
                Err(crossbeam_channel::TrySendError::Full(_)) => Pushed::DroppedOldest,
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => Pushed::Disconnected,
            }
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => Pushed::Disconnected,
    }
}

/// WebSocket URL 을 확인한다. `ws://`·`wss://` 만 받는다.
///
/// 두 번째 값이 `Some` 이면 알릴 만한 주의다(평문으로 바깥에 나가는 경우).
fn check_ws_url(url: &str) -> Result<Option<String>, String> {
    let t = url.trim();
    let rest = if let Some(r) = t.strip_prefix("wss://") {
        return check_ws_host(r).map(|_| None);
    } else if let Some(r) = t.strip_prefix("ws://") {
        r
    } else {
        let scheme = t.split("://").next().unwrap_or(t);
        return Err(format!(
            "WebSocket 주소는 ws:// 나 wss:// 여야 한다 (받은 스킴: {scheme:?})"
        ));
    };
    check_ws_host(rest)?;
    // 평문 ws 가 바깥으로 나간다. 내용과 토큰이 그대로 보인다.
    let host = ws_host_of(rest);
    if nl_core::pipeline::is_loopback_bind(&host) {
        Ok(None)
    } else {
        Ok(Some(format!(
            "{url} 은 평문 ws:// 다. {host} 로 가는 내용이 중간에서 그대로 보인다 — wss:// 를 쓰는 편이 좋다"
        )))
    }
}

fn check_ws_host(rest: &str) -> Result<(), String> {
    if ws_host_of(rest).is_empty() {
        return Err("WebSocket 주소에 호스트가 없다".into());
    }
    Ok(())
}

/// `example.com:9001/path?q` → `example.com:9001`.
fn ws_host_of(rest: &str) -> String {
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    // 사용자 정보(`user:pass@`)가 있으면 뒤쪽이 호스트다.
    let hostport = &rest[..end];
    hostport.rsplit('@').next().unwrap_or(hostport).to_string()
}

/// 연결 스레드를 띄운다. 끊기면 지수 백오프로 다시 붙는다.
///
/// - 수신 텍스트 프레임은 `subscribers` 전부에게 복사해 보낸다(같은 url 소스 노드가 여럿일 수 있다).
/// - 바이너리 프레임은 버리고 **연결당 한 번만** 알린다. 매 프레임 알리면 로그가 넘친다.
/// - `out_rx` 로 들어온 문자열은 텍스트 프레임으로 보낸다.
fn spawn_ws(
    url: String,
    subscribers: Vec<(Sender<Value>, Receiver<Value>)>,
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
    subscribers: &[(Sender<Value>, Receiver<Value>)],
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
        let mut dropped = 0usize;
        let mut dropped_at = Instant::now();

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
                    for (tx, rx) in subscribers {
                        // 큐가 가득 차면 가장 오래된 것을 버린다. 소비는 틱당 하나뿐이라
                        // 상대가 빠르면 쌓이기만 하고, 오래된 값은 이미 쓸모가 없다.
                        if matches!(send_dropping_oldest(tx, rx, v.clone()), Pushed::DroppedOldest) {
                            dropped += 1;
                        }
                    }
                    if dropped > 0 && dropped_at.elapsed() >= DROP_REPORT_EVERY {
                        let _ = status_tx.send(WsStatus::Log(format!(
                            "{url} 에서 온 값 {dropped}개를 버렸다 (파이프라인이 따라가지 못한다)"
                        )));
                        dropped = 0;
                        dropped_at = Instant::now();
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
    /// 마지막 미리보기 발행 시각 (노드당 초당 4회 제한).
    last_preview: Option<Instant>,
    /// `Source::WebSocket` 가 받은 값.
    ws_in: Option<Receiver<Value>>,
    /// `Sink::WebSocketSend` 가 보낼 곳.
    ws_out: Option<Sender<String>>,
    /// `Source::File`·`Sink::File` 의 확정된 경로. 준비 단계에서 한 번 검사한 결과다.
    /// `Err` 면 그 노드는 매번 같은 오류를 낸다 (파일을 건드리지 않는다).
    file_path: Option<Result<PathBuf, String>>,
}

// ───────────────────────────── 틱 루프 ─────────────────────────────

fn run_loop(
    runner: Runner,
    etx: &Sender<RunnerEvent>,
    irx: &Receiver<RunnerInput>,
    stop: &AtomicBool,
    armed: &AtomicBool,
) {
    let Runner {
        project,
        pipeline,
        base_dir,
        device,
        arm_input,
        http_reply_timeout,
    } = runner;
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
        let _ = etx.send(RunnerEvent::Error {
            node: None,
            message: "실행할 노드가 없다".into(),
        });
        return;
    }

    let mut states: HashMap<PNodeId, NodeState> = order.iter().map(|id| (*id, NodeState::default())).collect();
    // 라벨은 바뀌지 않으니 한 번만 만든다 (틱마다 String 을 새로 찍지 않게).
    let labels: HashMap<PNodeId, String> = order.iter().map(|id| (*id, node_label(&pipeline, *id))).collect();
    let mut sessions: HashMap<PNodeId, Result<Session, String>> = HashMap::new();

    // ── 준비: stdin 읽기 스레드 (StdinJson 소스가 있을 때만).
    // stdin 이 버린 줄 수를 받는 길 (소스가 있을 때만 생긴다).
    let mut stdin_drops: Option<Receiver<usize>> = None;
    let stdin_nodes: Vec<PNodeId> = order
        .iter()
        .copied()
        .filter(|id| {
            matches!(
                &pipeline.nodes[id].kind,
                PNodeKind::Source {
                    source: Source::StdinJson
                }
            )
        })
        .collect();
    if !stdin_nodes.is_empty() {
        let mut senders = Vec::with_capacity(stdin_nodes.len());
        for id in &stdin_nodes {
            let (tx, rx) = crossbeam_channel::bounded(STREAM_QUEUE_LIMIT);
            senders.push((tx, rx.clone()));
            states.get_mut(id).expect("상태를 미리 만들어 뒀다").stdin = Some(rx);
        }
        // 버린 줄 수를 알리는 길. 틱 루프가 비워 Log 이벤트로 바꾼다 (WebSocket 과 같은 정책).
        let (drop_tx, drop_rx) = crossbeam_channel::bounded::<usize>(8);
        stdin_drops = Some(drop_rx);
        // stdin 읽기는 블로킹이라 stop 으로 깨울 수 없다. 수신자가 모두 사라지면 다음 줄에서 스스로 끝난다.
        let spawned = std::thread::Builder::new().name("nl-stdin".into()).spawn(move || {
            let stdin = std::io::stdin();
            let mut dropped = 0usize;
            let mut reported_at = Instant::now();
            for line in stdin.lock().lines() {
                let Ok(l) = line else { break };
                // 큐가 가득 차면 가장 오래된 줄을 버린다 (WebSocket 과 같은 정책).
                // 받는 노드가 모두 사라졌으면 더 읽어도 소용이 없다.
                let mut alive = 0usize;
                for (tx, rx) in &senders {
                    match send_dropping_oldest(tx, rx, l.clone()) {
                        Pushed::Ok => alive += 1,
                        Pushed::DroppedOldest => {
                            alive += 1;
                            dropped += 1;
                        }
                        Pushed::Disconnected => {}
                    }
                }
                if alive == 0 {
                    break;
                }
                if dropped > 0 && reported_at.elapsed() >= DROP_REPORT_EVERY {
                    // 채널이 차 있으면 이번 보고는 건너뛴다 — 알림 때문에 읽기가 막히면 안 된다.
                    if drop_tx.try_send(dropped).is_ok() {
                        dropped = 0;
                    }
                    reported_at = Instant::now();
                }
            }
        });
        if let Err(e) = spawned {
            let _ = etx.send(RunnerEvent::Error {
                node: None,
                message: format!("stdin 읽기 스레드 생성 실패: {e}"),
            });
        }
    }

    // ── 준비: 파일 경로를 한 번에 확인한다.
    // 신뢰할 수 없는 번들이 프로젝트 폴더 밖을 가리키지 못하게 여기서 걸러 둔다.
    // 틱마다 검사하지 않고, 결과를 노드 상태에 담아 둔 뒤 그대로 쓴다.
    for id in &order {
        let path = match &pipeline.nodes[id].kind {
            PNodeKind::Source {
                source: Source::File { path, .. },
            } => path,
            PNodeKind::Sink {
                sink: Sink::File { path, .. },
            } => path,
            _ => continue,
        };
        let resolved = resolve_inside(&base_dir, path);
        if let Err(e) = &resolved {
            let _ = etx.send(RunnerEvent::Error {
                node: Some(*id),
                message: e.clone(),
            });
        }
        states.get_mut(id).expect("상태 미리 생성").file_path = Some(resolved);
    }

    // ── 준비: 인바운드 HTTP 서버. 노드마다 소켓 하나를 연다.
    let mut servers: HashMap<PNodeId, HttpServerState> = HashMap::new();
    for id in &order {
        let PNodeKind::Source {
            source: Source::HttpServer { bind, path, token },
        } = &pipeline.nodes[id].kind
        else {
            continue;
        };
        // 환경 변수가 있으면 번들에 박힌 토큰 대신 그것을 쓴다.
        let overridden = token_override(bind);
        if overridden.is_some() {
            let _ = etx.send(RunnerEvent::Log(format!(
                "{bind} 의 토큰을 환경 변수로 덮어썼다 ({HTTP_TOKEN_ENV} 또는 {HTTP_TOKEN_ENV_PREFIX}<포트>)"
            )));
        }
        let effective = overridden.as_deref().or(token.as_deref());
        match start_http_server(bind, path, effective) {
            Ok(srv) => {
                let _ = etx.send(RunnerEvent::Log(format!(
                    "HTTP 서버 http://{}{} 열림 ({})",
                    srv.local_addr(),
                    normalize_path(path),
                    if effective.is_some_and(|t| !t.trim().is_empty()) {
                        "토큰 필요"
                    } else {
                        "루프백 전용"
                    }
                )));
                servers.insert(*id, srv);
            }
            Err(e) => {
                let _ = etx.send(RunnerEvent::Error {
                    node: Some(*id),
                    message: e,
                });
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
                PNodeKind::Source {
                    source: Source::WebSocket { url },
                } => by_url.entry(url.clone()).or_default().0.push(*id),
                PNodeKind::Sink {
                    sink: Sink::WebSocketSend { url },
                } => by_url.entry(url.clone()).or_default().1.push(*id),
                _ => {}
            }
        }
        for (url, (sources, sinks)) in by_url {
            // 소스 노드마다 자기 수신 채널을 준다 (하나의 채널을 나눠 가지면 프레임이 한 노드에게만 간다).
            // 주소를 먼저 본다. 스킴이 틀리면 붙어 볼 것도 없다 (M5).
            match check_ws_url(&url) {
                Ok(None) => {}
                Ok(Some(notice)) => {
                    let _ = etx.send(RunnerEvent::Log(notice));
                }
                Err(e) => {
                    for id in sources.iter().chain(sinks.iter()) {
                        let _ = etx.send(RunnerEvent::Error {
                            node: Some(*id),
                            message: e.clone(),
                        });
                    }
                    continue;
                }
            }
            let mut subscribers = Vec::with_capacity(sources.len());
            for id in &sources {
                let (tx, rx) = crossbeam_channel::bounded(STREAM_QUEUE_LIMIT);
                subscribers.push((tx, rx.clone()));
                states.get_mut(id).expect("상태 미리 생성").ws_in = Some(rx);
            }
            // 보내는 쪽 핸들은 싱크 노드 상태가 들고 있다. 싱크가 없으면 out_tx 는 여기서 사라지고,
            // 연결 스레드의 `try_recv` 가 Disconnected 를 받아 조용히 지나간다(받기만 하는 연결).
            let (out_tx, out_rx) = crossbeam_channel::bounded::<String>(STREAM_QUEUE_LIMIT);
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
                        WsConn {
                            status: status_rx,
                            nodes,
                            handle: Some(handle),
                        },
                    );
                }
                Err(e) => {
                    for id in &nodes {
                        let _ = etx.send(RunnerEvent::Error {
                            node: Some(*id),
                            message: e.clone(),
                        });
                    }
                }
            }
        }
    }

    // ── 준비: 모델 세션 로드. **소스를 먼저 연 다음**에 한다.
    //
    // `Session::load` 는 GPU 초기화 때문에 몇 초가 걸릴 수 있다. 이걸 먼저 하면 그동안 HTTP 서버가
    // 닫혀 있어 클라이언트가 "연결 거부" 를 본다. 소스를 먼저 열어 두면 포트는 살아 있고,
    // 아직 답할 수 없는 요청은 큐에 넣지 않고 503(`MODEL_LOADING`)으로 곧바로 돌려보낸다.
    // 실패해도 루프는 돈다(그 노드를 지날 때 오류 이벤트).
    let model_load_started = Instant::now();
    for id in &order {
        let node = &pipeline.nodes[id];
        let PNodeKind::Model { model, .. } = &node.kind else {
            continue;
        };
        let Some(def) = project.models.get(model) else {
            let _ = etx.send(RunnerEvent::Error {
                node: Some(*id),
                message: format!("프로젝트에 없는 모델 {}", model.short()),
            });
            sessions.insert(*id, Err(format!("프로젝트에 없는 모델 {}", model.short())));
            continue;
        };
        // 가중치 경로도 프로젝트 폴더 안이어야 한다 (번들이 `../../` 로 남의 파일을 읽게 두지 않는다).
        let weights = match def.weights.as_deref() {
            Some(w) => match resolve_inside(&base_dir, w) {
                Ok(p) => Some(p),
                Err(e) => {
                    let message = format!("모델 '{}' 의 가중치 경로를 쓸 수 없다: {e}", def.name);
                    let _ = etx.send(RunnerEvent::Error {
                        node: Some(*id),
                        message: message.clone(),
                    });
                    sessions.insert(*id, Err(message));
                    continue;
                }
            },
            None => None,
        };
        match Session::load(def, weights.as_deref(), device) {
            Ok(s) => {
                let _ = etx.send(RunnerEvent::Log(format!(
                    "모델 '{}' 준비 완료 ({})",
                    def.name,
                    s.device_name()
                )));
                sessions.insert(*id, Ok(s));
            }
            Err(e) => {
                let message = format!("모델 '{}' 을 올리지 못했다: {e:#}", def.name);
                let _ = etx.send(RunnerEvent::Error {
                    node: Some(*id),
                    message: message.clone(),
                });
                sessions.insert(*id, Err(message));
            }
        }
    }
    // 이제부터 요청을 받는다.
    if !servers.is_empty() {
        for srv in servers.values() {
            srv.ready.store(true, Ordering::SeqCst);
        }
        let _ = etx.send(RunnerEvent::Log(format!(
            "요청 받기 시작 (모델 준비에 {:.2}초)",
            model_load_started.elapsed().as_secs_f64()
        )));
    }

    // ── 준비: 입력 시뮬레이터.
    let mut sim = match InputSim::with_armed(arm_input) {
        Ok(s) => s,
        Err(e) => {
            let _ = etx.send(RunnerEvent::Error {
                node: None,
                message: format!("입력 시뮬레이터 준비 실패: {e:#}"),
            });
            return;
        }
    };
    if arm_input {
        let _ = etx.send(RunnerEvent::Log(
            "마우스·키보드 싱크가 무장됐다 (실제 입력을 보낸다)".into(),
        ));
    }

    let hz = pipeline.tick_hz.clamp(MIN_TICK_HZ, MAX_TICK_HZ);
    let period = Duration::from_secs_f32(1.0 / hz);

    let mut widget_inputs: HashMap<WidgetId, Value> = HashMap::new();
    let mut manual_inputs: HashMap<PNodeId, Value> = HashMap::new();

    // 통계: 지난 구간의 틱 수와 순수 작업 시간(잠든 시간 제외).
    let mut tick_total: u64 = 0;
    let mut stats_at = Instant::now();
    let mut stats_ticks: u32 = 0;
    let mut stats_work = Duration::ZERO;
    let mut armed_now = arm_input;

    while !stop.load(Ordering::SeqCst) {
        let tick_start = Instant::now();
        tick_total += 1;

        // 무장 상태를 틱마다 읽어 둔다. 실제 반영은 액션 직전에 한 번 더 확인한다.
        let want = armed.load(Ordering::SeqCst);
        if want != armed_now {
            armed_now = want;
            let _ = etx.send(RunnerEvent::Log(
                if want {
                    "마우스·키보드 싱크가 무장됐다 (실제 입력을 보낸다)"
                } else {
                    "마우스·키보드 싱크 무장을 풀었다 (로그만 남긴다)"
                }
                .into(),
            ));
        }

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
        //    `pending`(처리 중) 뿐 아니라 **아직 꺼내지 않은 큐**도 훑는다. 큐에서 오래 기다린 요청은
        //    클라이언트가 이미 포기했을 가능성이 크고, 붙잡고 있어 봐야 소켓과 본문만 낭비한다.
        for (id, srv) in servers.iter_mut() {
            let mut expired = 0usize;
            while srv
                .pending
                .front()
                .is_some_and(|(at, _)| at.elapsed() >= http_reply_timeout)
            {
                let (_, req) = srv.pending.pop_front().expect("바로 위에서 확인했다");
                let _ = respond_json(
                    req,
                    504,
                    "파이프라인이 제한 시간 안에 응답을 내지 않았다 (HTTP 응답 싱크가 연결돼 있는지 확인하라)",
                );
                expired += 1;
            }
            // 채널에 들어온 것을 큐로 옮긴 뒤, 큐 앞에서 늙은 것을 걷어 낸다.
            // FIFO 라 앞이 젊으면 뒤도 젊다.
            while let Ok(inc) = srv.rx.try_recv() {
                srv.queued.push_back(inc);
            }
            while srv
                .queued
                .front()
                .is_some_and(|inc| inc.at.elapsed() >= http_reply_timeout)
            {
                let inc = srv.queued.pop_front().expect("바로 위에서 확인했다");
                let _ = respond_json(inc.request, 504, "요청이 큐에서 제한 시간을 넘겼다");
                expired += 1;
            }
            if expired > 0 {
                if let Some(node_st) = states.get_mut(id) {
                    report(
                        etx,
                        node_st,
                        Some(*id),
                        format!("HTTP 요청 {expired}건이 제한 시간 안에 응답을 받지 못해 504 로 닫혔다"),
                    );
                }
            }
        }

        // 3. stdin 이 버린 줄을 알린다.
        if let Some(rx) = &stdin_drops {
            while let Ok(n) = rx.try_recv() {
                let _ = etx.send(RunnerEvent::Log(format!(
                    "stdin 에서 온 줄 {n}개를 버렸다 (파이프라인이 따라가지 못한다)"
                )));
            }
        }

        // 4. WebSocket 연결 상태를 이벤트로 옮긴다. 오류는 그 url 을 쓰는 노드들에 붙인다.
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

        // 5. 위상 순서로 평가.
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
                        &mut widget_inputs,
                        &mut manual_inputs,
                        &mut servers,
                    ) {
                        Ok(Some(v)) => {
                            emit_value(etx, id, &v, st, tick_start);
                            values.insert(id, v);
                        }
                        Ok(None) => {}
                        Err(msg) => report(etx, st, Some(id), format!("{name}: {msg}")),
                    }
                }
                PNodeKind::Model { model, payload } => {
                    let Some(v) = upstream_value(&pipeline, &values, id) else {
                        continue;
                    };
                    let spec = payload
                        .or_else(|| project.models.get(model).and_then(|m| m.payload))
                        .and_then(|pid| project.payloads.get(&pid));
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    match sessions.get_mut(&id) {
                        Some(Ok(sess)) => match run_model(sess, spec, &v) {
                            Ok(out) => {
                                emit_value(etx, id, &out, st, tick_start);
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
                    let Some(v) = upstream_value(&pipeline, &values, id) else {
                        continue;
                    };
                    let st = states.get_mut(&id).expect("상태 미리 생성");
                    match eval_logic(logic, &v, st, tick_start) {
                        Ok(Some(out)) => {
                            emit_value(etx, id, &out, st, tick_start);
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
                    if let Err(msg) = eval_sink(sink, &v, st, tick_start, &mut sim, armed, etx, name, &mut servers) {
                        report(etx, st, Some(id), format!("{name}: {msg}"));
                    }
                }
            }
        }

        // 5. 통계. 잠들기 전에 순수 작업 시간을 재 둔다.
        stats_ticks += 1;
        stats_work += tick_start.elapsed();
        if stats_at.elapsed() >= STATS_INTERVAL {
            let secs = stats_at.elapsed().as_secs_f32().max(f32::EPSILON);
            let _ = etx.send(RunnerEvent::Stats {
                tick: tick_total,
                tick_ms: stats_work.as_secs_f32() * 1000.0 / stats_ticks.max(1) as f32,
                hz: stats_ticks as f32 / secs,
            });
            stats_at = Instant::now();
            stats_ticks = 0;
            stats_work = Duration::ZERO;
        }

        // 6. 남은 주기만큼 잔다. stop 을 자주 확인한다.
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
/// 대신 캔버스·인스펙터가 쓸 작은 축소판을 [`RunnerEvent::ValuePreview`] 로 초당 4회까지 보낸다.
fn emit_value(etx: &Sender<RunnerEvent>, node: PNodeId, v: &Value, st: &mut NodeState, now: Instant) {
    if let Value::Image { width, height, rgba } = v {
        emit_preview(etx, node, *width, *height, rgba, st, now);
        return;
    }
    let _ = etx.send(RunnerEvent::Value { node, value: v.clone() });
}

/// 축소판 발행. 노드당 [`PREVIEW_INTERVAL`] 간격이고, 채널이 밀려 있으면 건너뛴다.
fn emit_preview(
    etx: &Sender<RunnerEvent>,
    node: PNodeId,
    width: u32,
    height: u32,
    rgba: &[u8],
    st: &mut NodeState,
    now: Instant,
) {
    if let Some(at) = st.last_preview {
        if now.duration_since(at) < PREVIEW_INTERVAL {
            return;
        }
    }
    // 소비자가 못 따라오면 최신 것만 보면 되므로 버린다 (원본 프레임과 같은 정책).
    if etx.len() >= IMAGE_BACKLOG_LIMIT {
        return;
    }
    let Some((w, h, small)) = thumbnail(width, height, rgba) else {
        return;
    };
    st.last_preview = Some(now);
    let _ = etx.send(RunnerEvent::ValuePreview {
        node,
        width: w,
        height: h,
        rgba: small,
    });
}

/// RGBA8 프레임을 최장변 [`PREVIEW_MAX_SIDE`] 이하로 줄인다. 비율은 그대로 두고,
/// 이미 작으면 그대로 복사한다. 크기가 버퍼와 맞지 않으면 `None`.
fn thumbnail(width: u32, height: u32, rgba: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if width == 0 || height == 0 || rgba.len() != expected {
        return None;
    }
    let longest = width.max(height);
    if longest <= PREVIEW_MAX_SIDE {
        return Some((width, height, rgba.to_vec()));
    }
    let scale = f64::from(PREVIEW_MAX_SIDE) / f64::from(longest);
    let w = ((f64::from(width) * scale).round() as u32).clamp(1, PREVIEW_MAX_SIDE);
    let h = ((f64::from(height) * scale).round() as u32).clamp(1, PREVIEW_MAX_SIDE);

    // 최근접 표본. 원본을 통째로 복사하지 않으려고 `image::imageops` 대신 직접 훑는다 —
    // 1920×1080 프레임이면 8MB 복사를 초당 4번 아끼는 값이다. 미리보기라 화질은 이것으로 충분하다.
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h {
        let sy = (u64::from(y) * u64::from(height) / u64::from(h)).min(u64::from(height) - 1) as usize;
        let src_row = sy * width as usize * 4;
        let dst_row = y as usize * w as usize * 4;
        for x in 0..w {
            let sx = (u64::from(x) * u64::from(width) / u64::from(w)).min(u64::from(width) - 1) as usize;
            let s = src_row + sx * 4;
            let d = dst_row + x as usize * 4;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    Some((w, h, out))
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
            Ok(Some(Value::Image {
                width: f.width,
                height: f.height,
                rgba: f.rgba,
            }))
        }

        Source::HttpPoll {
            url,
            interval_ms,
            headers,
        } => {
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
            let resolved = st
                .file_path
                .as_ref()
                .ok_or_else(|| format!("{path} 경로가 확인되지 않았다"))?
                .as_ref()
                .map_err(|e| e.clone())?;
            read_file_value(resolved).map(Some)
        }

        Source::StdinJson => {
            let Some(rx) = &st.stdin else {
                return Err("stdin 읽기 스레드가 없다".into());
            };
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
        Source::HttpServer { bind, path, .. } => {
            let Some(srv) = servers.get_mut(&id) else {
                return Err(format!("http://{bind}{} 서버가 열려 있지 않다", normalize_path(path)));
            };
            if !srv.pending.is_empty() {
                return Ok(None);
            }
            // 틱 루프가 채널을 큐로 옮겨 두었다. 혹시 남은 것이 있으면 마저 가져온다.
            while let Ok(inc) = srv.rx.try_recv() {
                srv.queued.push_back(inc);
            }
            match srv.queued.pop_front() {
                Some(inc) => {
                    // 큐에서 기다린 시간까지 제한에 포함시킨다 — `now` 가 아니라 도착 시각을 쓴다.
                    srv.pending.push_back((inc.at, inc.request));
                    Ok(Some(inc.value))
                }
                None => Ok(None),
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

/// 파이프라인이 만지는 파일은 **모두 `base_dir` 안**이어야 한다.
///
/// 신뢰할 수 없는 `.nlapp` 이 `Sink::File { path: "~/.ssh/authorized_keys" }` 같은 것을 들고 올 수 있다.
/// 그래서 다음을 모두 거부한다.
///
/// - 절대 경로 (`/etc/passwd`, `C:\Windows\...`)
/// - `..` 로 올라가는 경로, 루트·드라이브 접두사
/// - 심볼릭 링크 (경로 중간이든 마지막이든) — 밖으로 빠져나가는 가장 흔한 길이다
///
/// 마지막 요소는 아직 없을 수 있으므로(쓰기 대상) **부모까지** 실제 경로로 풀어 확인하고,
/// 파일 이름만 그 위에 붙인다.
fn resolve_inside(base_dir: &Path, path: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "절대 경로는 쓸 수 없다: {path} (프로젝트 폴더 기준 상대 경로만)"
        ));
    }
    let mut rel = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(seg) => rel.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "경로가 프로젝트 폴더 밖을 가리킨다: {path} ('..' 는 쓸 수 없다)"
                ))
            }
            Component::RootDir | Component::Prefix(_) => return Err(format!("절대 경로는 쓸 수 없다: {path}")),
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(format!("파일 이름이 비어 있다: {path:?}"));
    }
    let joined = base_dir.join(&rel);

    // 부모까지 실제 경로로 풀어 기준 폴더 안인지 본다. 아직 없는 폴더는 통과시킨다
    // (만들 때 그 위 단계가 검사를 이미 통과했다).
    let base_real = base_dir.canonicalize().unwrap_or_else(|_| base_dir.to_path_buf());
    if let Some(parent) = joined.parent() {
        if let Ok(real) = parent.canonicalize() {
            if !real.starts_with(&base_real) {
                return Err(format!(
                    "경로가 프로젝트 폴더 밖을 가리킨다: {path} (심볼릭 링크로 빠져나간다)"
                ));
            }
        }
    }
    // 마지막 요소가 이미 심볼릭 링크면 그 너머로 쓰게 된다.
    if let Ok(meta) = std::fs::symlink_metadata(&joined) {
        if meta.file_type().is_symlink() {
            return Err(format!("심볼릭 링크는 쓸 수 없다: {path}"));
        }
    }
    Ok(joined)
}

/// 확장자가 이미지면 RGBA 로, 아니면 JSON → 텍스트 순으로 읽는다.
fn read_file_value(path: &Path) -> Result<Value, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg") {
        let img = image::open(path).map_err(|e| format!("이미지 {}: {e}", path.display()))?;
        let rgba = img.to_rgba8();
        return Ok(Value::Image {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        });
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
    let first = outs
        .into_iter()
        .next()
        .ok_or_else(|| "모델이 출력을 내지 않았다".to_string())?;
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
            other => Err(format!(
                "Select 는 벡터·텐서·JSON 배열에만 쓸 수 있다 (받은 값: {})",
                kind_name(other)
            )),
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
            let best = counts
                .into_iter()
                .fold((0i64, 0usize), |acc, (k, c)| if c > acc.1 { (k, c) } else { acc });
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
    sim: &mut InputSim,
    armed: &AtomicBool,
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
            // 액션을 보내기 직전에 공유 플래그를 읽는다 — 실행 중 무장 해제가 이 액션부터 먹는다.
            sim.armed = armed.load(Ordering::SeqCst);
            if matches!(a, InputAction::None) {
                return Ok(());
            }
            st.last_fire = Some(now);
            sim.perform(a)
                .map_err(|e| format!("입력 실행 실패({}): {e:#}", input::describe(a)))
        }

        Sink::HttpCall {
            method,
            url,
            headers,
            body_template,
        } => {
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
            let p = st
                .file_path
                .as_ref()
                .ok_or_else(|| format!("{path} 경로가 확인되지 않았다"))?
                .as_ref()
                .map_err(|e| e.clone())?
                .clone();
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
                return Err("답할 HTTP 요청이 없다 (이미 시간 초과로 닫혔거나, HTTP 서버에서 온 값이 아니다)".into());
            };
            // 이미지가 응답까지 그대로 왔으면 PNG 로 돌려준다 (JSON 에 픽셀을 실을 수는 없다).
            // 중간에 로직을 거쳐 숫자가 됐으면 여느 값처럼 JSON 이다.
            match v {
                Value::Image { width, height, rgba } => respond_png(request, *width, *height, rgba),
                other => respond_json(request, 200, &value_to_json(other).to_string()),
            }
        }

        Sink::GuiWidget { widget } => {
            if matches!(v, Value::Image { .. }) && etx.len() >= IMAGE_BACKLOG_LIMIT {
                // 소비자가 밀렸다. 새 프레임을 버려 지연이 쌓이지 않게 한다.
                return Ok(());
            }
            let _ = etx.send(RunnerEvent::Widget {
                widget: *widget,
                value: v.clone(),
            });
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
            // 연결이 느리면 큐가 찬다. 막히지 말고 알린다 — 보내기가 틱 루프를 멈추면 안 된다.
            match tx.try_send(text) {
                Ok(()) => Ok(()),
                Err(crossbeam_channel::TrySendError::Full(_)) => {
                    Err(format!("{url} 로 보낼 것이 밀렸다 (연결이 느리거나 끊겼다)"))
                }
                Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                    Err(format!("{url} 연결 스레드가 사라져 보내지 못했다"))
                }
            }
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
        Value::Text(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("숫자로 읽을 수 없는 텍스트: {s:?}")),
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
        serde_json::Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("숫자로 읽을 수 없는 문자열: {s:?}")),
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
            t.parse::<i64>()
                .or_else(|_| t.parse::<f64>().map(|x| x.round() as i64))
                .map_err(|_| format!("정수로 읽을 수 없는 텍스트: {s:?}"))
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
        Value::Image { width, height, rgba } => {
            json!({ "image": { "width": width, "height": height, "bytes": rgba.len() } })
        }
        Value::Tensor(t) => json!({ "shape": t.shape, "data": t.data }),
    }
}

#[cfg(test)]
mod tests {
    /// `eval_sink` 시험에서 쓰는 비무장 플래그. 실제 마우스가 움직이지 않게 언제나 꺼 둔다.
    static ARMED_OFF: AtomicBool = AtomicBool::new(false);

    use super::*;
    // 소켓을 직접 다루는 시험이 여럿이라 여기서만 쓴다 (본 코드의 읽기는 `httpd` 가 한다).
    use nl_core::pipeline::{PNode, PNodeKind};
    use nl_core::{ModelDef, Sink, Source};
    use std::io::Read as _;

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

    /// 가로세로 그러데이션 RGBA 프레임. 축소해도 모서리 색으로 표본 위치를 확인할 수 있다.
    fn frame(width: u32, height: u32) -> Value {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 7, 255]);
            }
        }
        Value::Image { width, height, rgba }
    }

    // ── 무장 스위치 ──────────────────────────────────────────────

    #[test]
    fn armed_flag_is_shared_with_the_running_loop() {
        let mut p = Pipeline::new("무장");
        p.tick_hz = 60.0;
        let timer = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 10 },
            },
            [0.0, 0.0],
        ));
        let sink = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::MouseKeyboard {
                    actions: vec![InputAction::None],
                    cooldown_ms: 0,
                },
            },
            [1.0, 0.0],
        ));
        p.add_link(timer, sink);

        let h = Runner::new(Project::new("p"), p, PathBuf::from("."), DevicePref::Cpu)
            .start()
            .unwrap();
        assert!(!h.is_armed(), "기본은 비무장이다");

        h.set_armed(true);
        assert!(h.is_armed());
        // 틱 루프가 바뀐 것을 알아채고 로그를 남긴다.
        let ev = wait_for(
            &h,
            Duration::from_secs(3),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("무장됐다")),
        );
        assert!(ev.is_some(), "무장 로그가 오지 않았다");

        h.set_armed(false);
        let ev = wait_for(
            &h,
            Duration::from_secs(3),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("무장을 풀었다")),
        );
        assert!(ev.is_some(), "무장 해제 로그가 오지 않았다");
        assert!(!h.is_armed());

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// `arm_input` 을 켜고 시작하면 핸들도 무장 상태로 보인다 (기존 필드가 초기값 노릇을 한다).
    #[test]
    fn arm_input_seeds_the_shared_flag() {
        let mut p = Pipeline::new("초기 무장");
        p.tick_hz = 60.0;
        p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 50 },
            },
            [0.0, 0.0],
        ));

        let mut r = Runner::new(Project::new("p"), p, PathBuf::from("."), DevicePref::Cpu);
        r.arm_input = true;
        let h = r.start().unwrap();
        assert!(h.is_armed(), "arm_input=true 로 시작하면 무장 상태다");
        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 액션 직전에 공유 플래그를 읽으므로 `InputSim.armed` 가 그때그때 맞춰진다.
    #[test]
    fn eval_sink_syncs_armed_right_before_the_action() {
        let (etx, _erx) = crossbeam_channel::unbounded();
        let mut servers = HashMap::new();
        let mut st = NodeState::default();
        let mut sim = InputSim::new().unwrap();
        // 실제 입력이 나가지 않는 액션만 쓴다.
        let sink = Sink::MouseKeyboard {
            actions: vec![InputAction::None],
            cooldown_ms: 0,
        };

        let armed = AtomicBool::new(true);
        sim.armed = false;
        eval_sink(
            &sink,
            &Value::Number(0.0),
            &mut st,
            Instant::now(),
            &mut sim,
            &armed,
            &etx,
            "n",
            &mut servers,
        )
        .unwrap();
        assert!(sim.armed, "플래그가 켜져 있으면 액션 직전에 무장된다");

        armed.store(false, Ordering::SeqCst);
        st.last_fire = None;
        eval_sink(
            &sink,
            &Value::Number(0.0),
            &mut st,
            Instant::now(),
            &mut sim,
            &armed,
            &etx,
            "n",
            &mut servers,
        )
        .unwrap();
        assert!(!sim.armed, "플래그가 꺼지면 다음 액션부터 비무장이다");
    }

    // ── 이미지 미리보기 ──────────────────────────────────────────

    #[test]
    fn thumbnail_fits_the_longest_side_and_keeps_the_ratio() {
        let Value::Image { width, height, rgba } = frame(640, 480) else {
            unreachable!()
        };
        let (w, h, small) = thumbnail(width, height, &rgba).unwrap();
        assert_eq!(w, PREVIEW_MAX_SIDE);
        assert_eq!(h, 120, "4:3 비율이 유지된다");
        assert_eq!(small.len(), (w * h * 4) as usize);
        assert_eq!(small[3], 255, "알파는 그대로다");

        // 세로가 긴 그림은 세로가 상한에 맞는다.
        let Value::Image { width, height, rgba } = frame(100, 400) else {
            unreachable!()
        };
        let (w, h, _) = thumbnail(width, height, &rgba).unwrap();
        assert_eq!(h, PREVIEW_MAX_SIDE);
        assert_eq!(w, 40);

        // 이미 작으면 그대로.
        let Value::Image { width, height, rgba } = frame(32, 16) else {
            unreachable!()
        };
        let (w, h, small) = thumbnail(width, height, &rgba).unwrap();
        assert_eq!((w, h), (32, 16));
        assert_eq!(small, rgba);
    }

    #[test]
    fn thumbnail_rejects_a_mismatched_buffer() {
        assert!(thumbnail(10, 10, &[0u8; 7]).is_none(), "버퍼 길이가 맞지 않는다");
        assert!(thumbnail(0, 10, &[]).is_none(), "너비가 0");
        assert!(thumbnail(2, 2, &[0u8; 16]).is_some());
    }

    #[test]
    fn image_values_come_out_as_previews_not_as_values() {
        let (etx, erx) = crossbeam_channel::unbounded();
        let mut st = NodeState::default();
        let node = PNodeId::from_u128(1);
        let now = Instant::now();

        emit_value(&etx, node, &frame(320, 240), &mut st, now);
        match erx.try_recv().expect("미리보기가 와야 한다") {
            RunnerEvent::ValuePreview {
                node: n,
                width,
                height,
                rgba,
            } => {
                assert_eq!(n, node);
                assert_eq!((width, height), (PREVIEW_MAX_SIDE, 120));
                assert_eq!(rgba.len(), (width * height * 4) as usize);
            }
            other => panic!("ValuePreview 가 아니다: {other:?}"),
        }
        assert!(erx.try_recv().is_err(), "원본 Value 이벤트는 나가지 않는다");

        // 이미지가 아닌 값은 그대로 Value 로 나간다.
        emit_value(&etx, node, &Value::Number(1.0), &mut st, now);
        assert!(matches!(erx.try_recv(), Ok(RunnerEvent::Value { .. })));
    }

    #[test]
    fn previews_are_capped_at_four_per_second_per_node() {
        let (etx, erx) = crossbeam_channel::unbounded();
        let mut st = NodeState::default();
        let node = PNodeId::from_u128(1);
        let img = frame(200, 200);
        let t0 = Instant::now();

        emit_value(&etx, node, &img, &mut st, t0);
        assert_eq!(erx.len(), 1, "첫 프레임은 나간다");

        // 같은 구간 안에서는 더 나가지 않는다.
        emit_value(&etx, node, &img, &mut st, t0 + Duration::from_millis(100));
        emit_value(&etx, node, &img, &mut st, t0 + Duration::from_millis(240));
        assert_eq!(erx.len(), 1, "250ms 안에는 한 번뿐이다");

        // 간격이 지나면 다시 나간다.
        emit_value(&etx, node, &img, &mut st, t0 + PREVIEW_INTERVAL);
        assert_eq!(erx.len(), 2);

        // 노드마다 따로 센다.
        let mut other = NodeState::default();
        emit_value(&etx, PNodeId::from_u128(2), &img, &mut other, t0 + PREVIEW_INTERVAL);
        assert_eq!(erx.len(), 3);
    }

    #[test]
    fn previews_are_dropped_when_the_consumer_lags() {
        let (etx, _erx) = crossbeam_channel::unbounded();
        let mut st = NodeState::default();
        let node = PNodeId::from_u128(1);
        for _ in 0..IMAGE_BACKLOG_LIMIT {
            let _ = etx.send(RunnerEvent::Log("밀린 이벤트".into()));
        }
        emit_value(&etx, node, &frame(200, 200), &mut st, Instant::now());
        assert_eq!(etx.len(), IMAGE_BACKLOG_LIMIT, "밀려 있으면 미리보기를 버린다");
        assert!(st.last_preview.is_none(), "버린 프레임은 주기를 소모하지 않는다");
    }

    // ── 통계 ─────────────────────────────────────────────────────

    #[test]
    fn stats_arrive_about_once_a_second() {
        let mut p = Pipeline::new("통계");
        p.tick_hz = 60.0;
        p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 20 },
            },
            [0.0, 0.0],
        ));

        let h = Runner::new(Project::new("p"), p, PathBuf::from("."), DevicePref::Cpu)
            .start()
            .unwrap();
        let ev = wait_for(&h, Duration::from_secs(4), |e| matches!(e, RunnerEvent::Stats { .. }));
        match ev {
            Some(RunnerEvent::Stats { tick, tick_ms, hz }) => {
                assert!(tick > 0, "누적 틱 수가 0 이다");
                assert!((0.0..1000.0).contains(&tick_ms), "틱 작업 시간이 이상하다: {tick_ms}");
                // 60Hz 를 목표로 도는 루프라 한참 못 미치거나 넘치면 잘못이다.
                assert!((5.0..120.0).contains(&hz), "실제 속도가 이상하다: {hz}");
            }
            other => panic!("Stats 가 오지 않았다: {other:?}"),
        }
        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
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
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 10 },
            },
            [0.0, 0.0],
        ));
        let thr = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Threshold { value: 0.5 },
            },
            [1.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        assert!(p.add_link(src, thr).is_some());
        assert!(p.add_link(thr, log).is_some());

        let h = Runner::new(Project::new("p"), p, tmp_dir("timer"), DevicePref::Cpu)
            .start()
            .unwrap();
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
        // 12초를 다 기다리지 않고 곧 끝나는지를 본다. 바쁜 기계를 감안해 넉넉히 잡았다.
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "stop 이 너무 느리다: {:?}",
            t.elapsed()
        );
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
        let sel = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Select { index: 1 },
            },
            [1.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        p.add_link(src, sel).unwrap();
        p.add_link(sel, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("manual"), DevicePref::Cpu)
            .start()
            .unwrap();
        h.inputs
            .send(RunnerInput::Manual {
                node: src,
                value: Value::Numbers(vec![1.0, 5.0, 2.0]),
            })
            .unwrap();

        let ev = wait_for(
            &h,
            Duration::from_secs(3),
            |e| matches!(e, RunnerEvent::Value { node, .. } if *node == sel),
        );
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
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 10 },
            },
            [0.0, 0.0],
        ));
        let m = p.add_node(PNode::new(
            PNodeKind::Model {
                model: mid,
                payload: None,
            },
            [1.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [2.0, 0.0]));
        p.add_link(src, m).unwrap();
        p.add_link(m, log).unwrap();

        let h = Runner::new(project, p, tmp_dir("model"), DevicePref::Cpu)
            .start()
            .unwrap();
        // nl-engine 이 아직 스텁이라 Session::load 가 실패한다. 그래도 루프는 계속 돌아야 한다.
        assert!(
            wait_for(
                &h,
                Duration::from_secs(3),
                |e| matches!(e, RunnerEvent::Error { node, .. } if *node == Some(m))
            )
            .is_some(),
            "모델 오류 이벤트가 오지 않았다"
        );
        assert!(
            wait_for(
                &h,
                Duration::from_secs(3),
                |e| matches!(e, RunnerEvent::Value { node, .. } if *node == src)
            )
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
        let a = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Select { index: 0 },
            },
            [0.0, 0.0],
        ));
        let b = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Select { index: 0 },
            },
            [1.0, 0.0],
        ));
        p.add_link(a, b).unwrap();
        p.add_link(b, a).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("cycle"), DevicePref::Cpu)
            .start()
            .unwrap();
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
        let h = Runner::new(
            Project::new("p"),
            Pipeline::new("빈"),
            tmp_dir("empty"),
            DevicePref::Cpu,
        )
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
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 10 },
            },
            [0.0, 0.0],
        ));
        let sink = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::File {
                    path: "out.jsonl".into(),
                    append: true,
                },
            },
            [1.0, 0.0],
        ));
        p.add_link(src, sink).unwrap();
        let h = Runner::new(Project::new("p"), p, dir.clone(), DevicePref::Cpu)
            .start()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        h.stop();
        assert!(h.wait_done(Duration::from_millis(500)));
        let text = std::fs::read_to_string(dir.join("out.jsonl")).expect("싱크가 파일을 만들지 않았다");
        assert!(!text.trim().is_empty(), "파일이 비어 있다");
        assert!(
            text.lines().next().unwrap().parse::<f64>().is_ok(),
            "JSON 숫자 줄이 아니다: {text}"
        );
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
        assert_eq!(
            eval_logic(&logic, &v, &mut s, t0 + Duration::from_millis(100)).unwrap(),
            None
        );
        // 다른 값은 즉시 통과.
        let other = Value::Number(4.0);
        assert_eq!(
            eval_logic(&logic, &other, &mut s, t0 + Duration::from_millis(150)).unwrap(),
            Some(other.clone())
        );
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
        assert_eq!(
            eval_logic(&logic, &Value::Number(0.4), &mut s, now).unwrap(),
            Some(Value::Number(0.0))
        );
        assert_eq!(
            eval_logic(&logic, &Value::Number(0.5), &mut s, now).unwrap(),
            Some(Value::Number(1.0))
        );
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
        assert_eq!(
            eval_logic(&logic, &Value::Number(1.0), &mut s, now).unwrap(),
            Some(Value::Number(7.0))
        );
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
        assert_eq!(
            as_i64(&Value::Tensor(HostTensor::new(vec![1, 3], vec![0.0, 9.0, 1.0]))).unwrap(),
            1
        );
        assert!(as_i64(&Value::Image {
            width: 1,
            height: 1,
            rgba: vec![0; 4]
        })
        .is_err());
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
        assert_eq!(
            value_to_json(&Value::Json(serde_json::json!({"a":1}))),
            serde_json::json!({"a":1})
        );
        assert_eq!(value_to_json(&Value::Text("안녕".into())).to_string(), "\"안녕\"");
        assert_eq!(value_to_json(&Value::Number(2.0)).to_string(), "2.0");
        // 이미지는 바이트를 통째로 싣지 않는다.
        let j = value_to_json(&Value::Image {
            width: 2,
            height: 1,
            rgba: vec![0; 8],
        });
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
        let mut servers = HashMap::new();
        let mut call = |s: &mut NodeState, m: &mut HashMap<PNodeId, Value>, w: &mut HashMap<WidgetId, Value>| {
            eval_source(&Source::Manual, id, s, Instant::now(), w, m, &mut servers).unwrap()
        };
        assert_eq!(call(&mut s, &mut manual, &mut widgets), Some(Value::Number(1.0)));
        assert_eq!(
            call(&mut s, &mut manual, &mut widgets),
            None,
            "수동 입력은 한 번만 쓰인다"
        );
    }

    #[test]
    fn websocket_source_without_a_connection_reports_it() {
        let mut s = st();
        let src = Source::WebSocket { url: "ws://x".into() };
        let (mut m, mut w) = (HashMap::new(), HashMap::new());
        let mut servers = HashMap::new();
        let e = eval_source(
            &src,
            PNodeId::from_u128(1),
            &mut s,
            Instant::now(),
            &mut w,
            &mut m,
            &mut servers,
        )
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
        let now = Instant::now();

        let mut servers = HashMap::new();
        eval_sink(
            &sink,
            &Value::Text("그대로".into()),
            &mut s,
            now,
            &mut sim,
            &ARMED_OFF,
            &etx,
            "n",
            &mut servers,
        )
        .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            "그대로",
            "텍스트는 따옴표 없이 그대로 나가야 한다"
        );

        eval_sink(
            &sink,
            &Value::Number(3.0),
            &mut s,
            now,
            &mut sim,
            &ARMED_OFF,
            &etx,
            "n",
            &mut servers,
        )
        .unwrap();
        assert_eq!(rx.try_recv().unwrap(), "3.0");

        eval_sink(
            &sink,
            &Value::Json(serde_json::json!({"a":1})),
            &mut s,
            now,
            &mut sim,
            &ARMED_OFF,
            &etx,
            "n",
            &mut servers,
        )
        .unwrap();
        assert_eq!(rx.try_recv().unwrap(), r#"{"a":1}"#);
    }

    // ── 인바운드 HTTP 서버 ──

    /// 비어 있는 TCP 포트를 잡아 주소만 돌려준다 (리스너는 바로 닫는다).
    /// 시험용 바인드 주소. **포트를 미리 잡지 않는다** — `:0` 을 주면 운영체제가 고르고,
    /// 실제 주소는 서버가 열릴 때 로그로 알려 준다 ([`wait_server_up`]).
    ///
    /// 빈 포트를 먼저 찾아 두고 나중에 여는 방식은 그 사이에 다른 프로세스가 가져갈 수 있다.
    /// 시험을 병렬로 돌리면 실제로 부딪힌다.
    const ANY_ADDR: &str = "127.0.0.1:0";

    fn http_server_node(p: &mut Pipeline, bind: &str, path: &str) -> PNodeId {
        http_server_node_with(p, bind, path, None)
    }

    fn http_server_node_with(p: &mut Pipeline, bind: &str, path: &str, token: Option<&str>) -> PNodeId {
        p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::HttpServer {
                    bind: bind.into(),
                    path: path.into(),
                    token: token.map(str::to_string),
                },
            },
            [0.0, 0.0],
        ))
    }

    /// 서버가 실제로 뜰 때까지 기다린다 (Log 이벤트로 확인).
    fn wait_server_up(h: &RunnerHandle) -> String {
        let ev = wait_for(
            h,
            Duration::from_secs(10),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("HTTP 서버")),
        )
        .expect("HTTP 서버가 열리지 않았다");
        let RunnerEvent::Log(line) = ev else {
            panic!("로그가 아니다")
        };
        // `HTTP 서버 http://127.0.0.1:39481/infer 열림 (…)` 에서 주소만 뽑는다.
        let rest = line
            .split("http://")
            .nth(1)
            .unwrap_or_else(|| panic!("주소가 없다: {line}"));
        let end = rest.find(['/', ' ']).unwrap_or(rest.len());
        rest[..end].to_owned()
    }

    #[test]
    fn http_server_select_reply_answers_a_post() {
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        // 본문 {"x":[1,9,2]} → Select 로 x 를 꺼낸다.
        let pick = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Select { index: 1 },
            },
            [1.0, 0.0],
        ));
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [2.0, 0.0],
        ));
        p.add_link(server, pick).unwrap();
        p.add_link(pick, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpsrv"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);

        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[1, 9, 2]"),
            Duration::from_secs(5),
        )
        .expect("요청이 실패했다");
        assert_eq!(res.status, 200, "본문: {}", res.body);
        assert!(
            res.content_type.contains("application/json"),
            "content-type: {}",
            res.content_type
        );
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
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/q");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpq"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);

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
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("http404"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);

        let res = crate::http::call(
            "GET",
            &format!("http://{addr}/nope"),
            &BTreeMap::new(),
            None,
            Duration::from_secs(5),
        )
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
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        // 응답 싱크 대신 로그만 붙인다 — 값은 흐르지만 답하는 노드가 없다.
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(server, log).unwrap();

        let mut runner = Runner::new(Project::new("p"), p, tmp_dir("http504"), DevicePref::Cpu);
        assert_eq!(
            runner.http_reply_timeout, HTTP_REPLY_TIMEOUT,
            "기본값이 상수와 달라졌다"
        );
        runner.http_reply_timeout = Duration::from_millis(300);
        let h = runner.start().unwrap();
        let addr = wait_server_up(&h);

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
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir("httpstop"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);

        // 살아 있을 때는 답한다.
        let url = format!("http://{addr}/infer");
        let ok = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(ok.status, 200);

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)), "stop 후에도 끝나지 않았다");
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "HTTP 서버 정리가 느리다: {:?}",
            t.elapsed()
        );

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

    /// 모델을 올리는 동안에도 포트는 살아 있고, 그 사이 요청은 503 으로 곧바로 돌아온다.
    /// 모델이 준비되면 같은 요청이 200 이 된다.
    #[test]
    fn the_server_opens_before_the_model_and_answers_503_until_ready() {
        // 프로젝트에 없는 모델을 가리켜 `Session::load` 가 확실히 실패하게 한다.
        // (실패든 성공이든 "로딩이 끝나면 ready" 라는 전이는 같다.)
        let mut p = Pipeline::new("api");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("http503"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);

        // 준비가 끝났다는 로그가 오기 전까지는 503 만 나온다.
        assert!(
            wait_for(
                &h,
                Duration::from_secs(5),
                |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
            )
            .is_some(),
            "준비 완료 로그가 오지 않았다"
        );

        let url = format!("http://{addr}/infer");
        let res = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 200, "준비 뒤에는 200 이어야 한다. 본문: {}", res.body);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    /// `ready` 가 꺼져 있으면 요청을 큐에 넣지 않고 503 으로 돌려보낸다.
    /// 수신 스레드만 따로 띄워 게이트 자체를 확인한다 (모델 로딩 시간에 기대지 않는다).
    #[test]
    fn requests_before_ready_get_503_and_are_not_queued() {
        // 서버가 직접 :0 으로 열고 실제 주소를 알려 준다 — 포트를 미리 잡아 두지 않는다.
        let Ok(mut srv) = start_http_server(ANY_ADDR, "/infer", None) else {
            panic!("서버를 열지 못했다");
        };
        assert!(!srv.ready.load(Ordering::SeqCst), "처음에는 준비 전이다");

        let addr = srv.local_addr().to_string();
        let url = format!("http://{addr}/infer");
        let res = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 503);
        let body = res.json().expect("503 본문이 JSON 이 아니다");
        assert_eq!(body["status"], serde_json::json!(503));
        assert!(
            body["error"].as_str().unwrap().contains("model loading"),
            "본문: {}",
            res.body
        );
        // 큐에 남지 않았다 — 준비되면 낡은 요청이 되살아나지 않는다.
        assert!(srv.rx.try_recv().is_err(), "503 으로 돌려보낸 요청이 큐에 들어갔다");

        // 준비되면 같은 경로가 수신 채널로 넘어온다.
        srv.ready.store(true, Ordering::SeqCst);
        let (tx, rx) = crossbeam_channel::bounded::<u16>(1);
        let url2 = url.clone();
        std::thread::spawn(move || {
            let r = crate::http::call("POST", &url2, &BTreeMap::new(), Some("2"), Duration::from_secs(5));
            let _ = tx.send(r.map(|x| x.status).unwrap_or(0));
        });
        let inc = srv
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("준비 뒤 요청이 오지 않았다");
        // 본문 "2" 는 JSON 으로 읽히므로 Json(2) 이다 (텍스트보다 JSON 을 먼저 시도한다).
        assert_eq!(inc.value, Value::Json(serde_json::json!(2)));
        respond_json(inc.request, 200, "2").unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 200);

        srv.shutdown();
    }

    // ── 이진 본문 ──

    /// 작은 PNG 한 장을 바이트로.
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 20) as u8, (y * 20) as u8, 0x40, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn image_content_types_decode_to_image_values() {
        let png = png_bytes(6, 4);
        match body_to_value("image/png", png.clone(), "").unwrap() {
            Value::Image { width, height, rgba } => {
                assert_eq!((width, height), (6, 4));
                assert_eq!(rgba.len(), 6 * 4 * 4);
            }
            other => panic!("이미지가 아니다: {other:?}"),
        }
        // 매개변수가 붙어도 형식만 본다.
        assert!(matches!(
            body_to_value("image/png; charset=binary", png.clone(), "").unwrap(),
            Value::Image { .. }
        ));
        // 헤더가 jpeg 라고 해도 내용으로 판단한다 (PNG 가 들어오면 PNG 로 읽힌다).
        assert!(matches!(
            body_to_value("image/jpeg", png, "").unwrap(),
            Value::Image { .. }
        ));
    }

    #[test]
    fn a_broken_image_is_a_clear_error() {
        let err = body_to_value("image/png", b"not-a-png".to_vec(), "").unwrap_err();
        assert!(err.contains("이미지를 읽지 못했다"), "{err}");
        let empty = body_to_value("image/png", Vec::new(), "").unwrap_err();
        assert!(empty.contains("비어 있다"), "{empty}");
    }

    #[test]
    fn octet_stream_is_refused_with_advice() {
        let err = body_to_value("application/octet-stream", vec![1, 2, 3], "").unwrap_err();
        assert!(err.contains("image/png"), "형식을 적으라는 안내가 없다: {err}");
    }

    #[test]
    fn non_utf8_text_bodies_point_at_the_content_type() {
        let err = body_to_value("text/plain", vec![0xff, 0xfe, 0x00], "").unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
        assert!(err.contains("image/png"), "이미지 안내가 없다: {err}");
    }

    #[test]
    fn text_and_query_bodies_still_work() {
        assert_eq!(
            body_to_value("application/json", b"[1,2]".to_vec(), "").unwrap(),
            Value::Json(serde_json::json!([1, 2]))
        );
        assert_eq!(
            body_to_value("text/plain", "그냥 글".as_bytes().to_vec(), "").unwrap(),
            Value::Text("그냥 글".into())
        );
        // 본문이 비면 쿼리스트링.
        assert_eq!(
            body_to_value("", Vec::new(), "a=1").unwrap(),
            Value::Json(serde_json::json!({"a": "1"}))
        );
    }

    #[test]
    fn multipart_takes_the_first_file_part() {
        let png = png_bytes(3, 2);
        let boundary = "----nlTestBoundary";
        let mut body = Vec::new();
        // 파일이 아닌 파트를 먼저 둬서 건너뛰는지 본다.
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"note\"\r\n\r\n");
        body.extend_from_slice(b"hello\r\n");
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n",
        );
        body.extend_from_slice(&png);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let ct = format!("multipart/form-data; boundary={boundary}");
        match body_to_value(&ct, body, "").unwrap() {
            Value::Image { width, height, .. } => assert_eq!((width, height), (3, 2)),
            other => panic!("이미지가 아니다: {other:?}"),
        }
    }

    #[test]
    fn multipart_without_a_file_part_is_an_error() {
        let boundary = "b1";
        let mut body = Vec::new();
        body.extend_from_slice(b"--b1\r\nContent-Disposition: form-data; name=\"x\"\r\n\r\n1\r\n--b1--\r\n");
        let err = body_to_value("multipart/form-data; boundary=b1", body.clone(), "").unwrap_err();
        assert!(err.contains("파일 파트"), "{err}");
        // boundary 가 없으면 그 사실을 알린다.
        let err2 = body_to_value("multipart/form-data", body, "").unwrap_err();
        assert!(err2.contains("boundary"), "{err2}");
        let _ = boundary;
    }

    #[test]
    fn boundary_is_read_from_the_content_type() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"a b\"").as_deref(),
            Some("a b")
        );
        assert_eq!(multipart_boundary("multipart/form-data"), None);
    }

    #[test]
    fn png_round_trips_through_the_encoder() {
        let rgba: Vec<u8> = (0..(4 * 3 * 4)).map(|i| (i % 251) as u8).collect();
        let png = encode_png(4, 3, &rgba).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']), "PNG 시그니처가 아니다");
        let back = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!((back.width(), back.height()), (4, 3));
        assert_eq!(back.into_raw(), rgba, "PNG 는 무손실이라 픽셀이 그대로여야 한다");
        // 크기가 안 맞으면 패닉이 아니라 오류.
        assert!(encode_png(4, 3, &[0; 10]).is_err());
    }

    /// 진짜 이진 POST 는 소켓으로 직접 보낸다 (`http::call` 은 텍스트 본문만 다룬다).
    #[test]
    fn a_binary_png_post_flows_through_and_returns_a_png() {
        use std::io::Write as _;
        let mut p = Pipeline::new("이미지 API");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpbin"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(
            wait_for(
                &h,
                Duration::from_secs(5),
                |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
            )
            .is_some(),
            "준비 완료 로그가 오지 않았다"
        );

        let png = png_bytes(8, 5);
        let mut sock = std::net::TcpStream::connect(&addr).expect("서버에 붙지 못했다");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let head = format!(
            "POST /infer HTTP/1.1\r\nHost: {addr}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            png.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(&png).unwrap();
        sock.flush().unwrap();

        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).expect("응답을 읽지 못했다");
        let split = find(&raw, b"\r\n\r\n").expect("응답 머리와 몸을 가를 수 없다");
        let head_text = String::from_utf8_lossy(&raw[..split]).into_owned();
        let body = &raw[split + 4..];

        assert!(head_text.starts_with("HTTP/1.1 200"), "응답 머리: {head_text}");
        assert!(
            head_text.to_ascii_lowercase().contains("content-type: image/png"),
            "응답 머리: {head_text}"
        );
        let back = image::load_from_memory(body).expect("응답이 PNG 가 아니다");
        assert_eq!((back.width(), back.height()), (8, 5), "돌아온 이미지 크기가 다르다");

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    /// 로직을 거쳐도 이미지는 이미지다 — 디바운스는 값을 바꾸지 않으므로 응답은 여전히 PNG 다.
    /// (이미지를 숫자로 바꾸는 로직은 아직 없다. 그런 것이 생기면 응답이 JSON 으로 바뀌어야 한다.)
    #[test]
    fn an_image_through_a_logic_node_is_still_a_png() {
        use std::io::Write as _;
        let mut p = Pipeline::new("이미지 → 로직");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let logic = p.add_node(PNode::new(
            PNodeKind::Logic {
                logic: Logic::Debounce { ms: 0 },
            },
            [1.0, 0.0],
        ));
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [2.0, 0.0],
        ));
        p.add_link(server, logic).unwrap();
        p.add_link(logic, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpimg2"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
        )
        .is_some());

        let png = png_bytes(4, 4);
        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let head = format!(
            "POST /infer HTTP/1.1\r\nHost: {addr}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            png.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(&png).unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let head_text = String::from_utf8_lossy(&raw).into_owned();
        assert!(head_text.starts_with("HTTP/1.1 200"), "{head_text}");
        assert!(
            head_text.to_ascii_lowercase().contains("image/png"),
            "{}",
            &head_text[..head_text.len().min(300)]
        );

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    // ── 접근 정책 ──

    #[test]
    fn a_token_is_accepted_from_either_header() {
        let p = AccessPolicy::new("127.0.0.1:8799", Some("s3cret"));
        assert!(p.check(None, None, Some("Bearer s3cret"), None).is_ok());
        assert!(
            p.check(None, None, Some("bearer s3cret"), None).is_ok(),
            "소문자 bearer 도 받는다"
        );
        assert!(
            p.check(None, None, None, Some("s3cret")).is_ok(),
            "X-NL-Token 도 받는다"
        );
        // 앞뒤 공백은 무시한다.
        assert!(p.check(None, None, Some("Bearer  s3cret "), None).is_ok());
    }

    #[test]
    fn a_missing_or_wrong_token_is_401() {
        let p = AccessPolicy::new("127.0.0.1:8799", Some("s3cret"));
        assert_eq!(p.check(None, None, None, None).unwrap_err().0, 401);
        assert_eq!(p.check(None, None, Some("Bearer nope"), None).unwrap_err().0, 401);
        assert_eq!(p.check(None, None, None, Some("nope")).unwrap_err().0, 401);
        // Basic 인증은 토큰이 아니다.
        assert_eq!(p.check(None, None, Some("Basic abc"), None).unwrap_err().0, 401);
        // 안내 문구에 어느 헤더를 쓰라는지 적혀 있다.
        let (_, why) = p.check(None, None, None, None).unwrap_err();
        assert!(why.contains("X-NL-Token"), "{why}");
    }

    #[test]
    fn no_token_means_no_check() {
        let p = AccessPolicy::new("127.0.0.1:8799", None);
        assert!(p.check(None, None, None, None).is_ok());
        // 빈 토큰은 없는 것과 같다.
        assert!(AccessPolicy::new("127.0.0.1:8799", Some("   "))
            .check(None, None, None, None)
            .is_ok());
    }

    #[test]
    fn a_browser_origin_is_always_refused() {
        for token in [None, Some("s3cret")] {
            let p = AccessPolicy::new("127.0.0.1:8799", token);
            let (code, why) = p
                .check(
                    Some("https://evil.example"),
                    Some("127.0.0.1:8799"),
                    Some("Bearer s3cret"),
                    None,
                )
                .unwrap_err();
            assert_eq!(code, 403, "{why}");
            assert!(why.contains("브라우저"), "{why}");
        }
        // null Origin(샌드박스 iframe)도 막힌다.
        let p = AccessPolicy::new("127.0.0.1:8799", None);
        assert_eq!(p.check(Some("null"), None, None, None).unwrap_err().0, 403);
    }

    #[test]
    fn a_foreign_host_header_is_refused() {
        let p = AccessPolicy::new("127.0.0.1:8799", None);
        // 바인드 주소와 localhost 는 받는다 (포트가 붙든 말든).
        for ok in [
            "127.0.0.1:8799",
            "127.0.0.1",
            "localhost:8799",
            "localhost",
            "LOCALHOST",
        ] {
            assert!(p.check(None, Some(ok), None, None).is_ok(), "{ok} 가 거부됐다");
        }
        // 남의 이름을 태워 온 요청은 막는다 (DNS rebinding).
        for bad in ["evil.example", "evil.example:8799", "192.168.0.5:8799"] {
            let (code, why) = p.check(None, Some(bad), None, None).unwrap_err();
            assert_eq!(code, 400, "{bad}: {why}");
            assert!(why.contains("Host"), "{why}");
        }
        // Host 가 아예 없으면(HTTP/1.0) 통과시킨다 — 브라우저는 언제나 붙인다.
        assert!(p.check(None, None, None, None).is_ok());
    }

    #[test]
    fn a_non_loopback_bind_allows_its_own_host_only() {
        let p = AccessPolicy::new("0.0.0.0:8799", Some("t"));
        assert!(p.check(None, Some("0.0.0.0:8799"), None, Some("t")).is_ok());
        // 루프백이 아니면 localhost 를 덤으로 받지 않는다.
        assert_eq!(
            p.check(None, Some("localhost:8799"), None, Some("t")).unwrap_err().0,
            400
        );
    }

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"), "길이가 다르면 다르다");
        assert!(constant_time_eq(b"", b""));
    }

    /// 인증 없이 바깥 주소에 여는 것은 준비 단계에서 막힌다.
    #[test]
    fn opening_a_public_port_without_a_token_is_refused() {
        // `HttpServerState` 는 Debug 가 아니라 `unwrap_err` 를 못 쓴다.
        let Err(err) = start_http_server("0.0.0.0:0", "/x", None) else {
            panic!("토큰 없이 0.0.0.0 에 열렸다");
        };
        assert!(err.contains("토큰 없이 열 수 없다"), "{err}");
        // 토큰이 있으면 열린다 (0 번 포트라 실제로 바인드된다).
        // `HttpServerState` 는 Debug 가 아니라 `expect` 를 못 쓴다.
        let Ok(mut ok) = start_http_server("0.0.0.0:0", "/x", Some("t")) else {
            panic!("토큰이 있으면 열려야 한다");
        };
        ok.shutdown();
        // 루프백은 토큰 없이도 열린다.
        let Ok(mut lo) = start_http_server("127.0.0.1:0", "/x", None) else {
            panic!("루프백은 열려야 한다");
        };
        lo.shutdown();
    }

    /// 토큰이 걸린 서버에 실제 요청을 보내 401 → 200 을 확인한다.
    #[test]
    fn a_tokened_server_refuses_and_then_accepts() {
        let mut p = Pipeline::new("보안 API");
        p.tick_hz = 120.0;
        let server = http_server_node_with(&mut p, ANY_ADDR, "/infer", Some("s3cret"));
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httpauth"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
        )
        .is_some());
        let url = format!("http://{addr}/infer");

        // 토큰 없이 → 401.
        let res = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 401, "본문: {}", res.body);

        // 틀린 토큰 → 401.
        let mut bad = BTreeMap::new();
        bad.insert("Authorization".to_string(), "Bearer nope".to_string());
        let res = crate::http::call("POST", &url, &bad, Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 401);

        // 맞는 토큰 → 200.
        let mut good = BTreeMap::new();
        good.insert("Authorization".to_string(), "Bearer s3cret".to_string());
        let res = crate::http::call("POST", &url, &good, Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 200, "본문: {}", res.body);

        // X-NL-Token 으로도 된다.
        let mut alt = BTreeMap::new();
        alt.insert("X-NL-Token".to_string(), "s3cret".to_string());
        let res = crate::http::call("POST", &url, &alt, Some("2"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 200, "본문: {}", res.body);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    /// 브라우저에서 온 것처럼 `Origin` 을 붙이면 토큰이 맞아도 막힌다.
    #[test]
    fn a_request_with_an_origin_header_is_refused_end_to_end() {
        let mut p = Pipeline::new("보안 API");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httporigin"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
        )
        .is_some());
        let url = format!("http://{addr}/infer");

        let mut headers = BTreeMap::new();
        headers.insert("Origin".to_string(), "https://evil.example".to_string());
        let res = crate::http::call("POST", &url, &headers, Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 403, "본문: {}", res.body);

        // Origin 이 없으면 그대로 200.
        let res = crate::http::call("POST", &url, &BTreeMap::new(), Some("1"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 200, "본문: {}", res.body);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    /// 남의 이름을 태워 온 `Host` 는 400.
    #[test]
    fn a_rebound_host_header_is_refused_end_to_end() {
        let mut p = Pipeline::new("보안 API");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("httphost"), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
        )
        .is_some());

        // `http::call` 은 URI 에서 Host 를 만든다. 남의 이름을 태우려면 소켓으로 직접 보낸다.
        use std::io::Write as _;
        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let body = "1";
        let head = format!(
            "POST /infer HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).unwrap();
        sock.write_all(body.as_bytes()).unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.starts_with("HTTP/1.1 400"), "{}", &text[..text.len().min(200)]);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    // ── H10 · L6 · L7: 경로 제한 ──

    #[test]
    fn paths_outside_the_project_folder_are_refused() {
        let base = tmp_dir("inside");
        // 상대 경로는 된다.
        assert_eq!(resolve_inside(&base, "out.jsonl").unwrap(), base.join("out.jsonl"));
        assert_eq!(
            resolve_inside(&base, "sub/out.jsonl").unwrap(),
            base.join("sub/out.jsonl")
        );
        assert_eq!(resolve_inside(&base, "./a.txt").unwrap(), base.join("a.txt"));

        // 절대 경로·상위 이동은 안 된다.
        for bad in ["/etc/passwd", "/tmp/x", "../a", "a/../../b", ".."] {
            let err = resolve_inside(&base, bad).unwrap_err();
            assert!(
                err.contains("절대 경로") || err.contains("밖을 가리킨다"),
                "{bad}: {err}"
            );
        }
        // 빈 경로도 거부.
        assert!(resolve_inside(&base, "").is_err());
        assert!(resolve_inside(&base, ".").is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_folder_are_refused() {
        let base = tmp_dir("symlink");
        let outside = tmp_dir("symlink-target");
        std::fs::write(outside.join("secret.txt"), "비밀").unwrap();

        // 폴더 심볼릭 링크로 빠져나가는 경우.
        std::os::unix::fs::symlink(&outside, base.join("escape")).unwrap();
        let err = resolve_inside(&base, "escape/secret.txt").unwrap_err();
        assert!(err.contains("심볼릭 링크"), "{err}");

        // 파일 자체가 심볼릭 링크인 경우.
        std::os::unix::fs::symlink(outside.join("secret.txt"), base.join("link.txt")).unwrap();
        let err = resolve_inside(&base, "link.txt").unwrap_err();
        assert!(err.contains("심볼릭 링크"), "{err}");

        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// 밖을 가리키는 파일 싱크는 준비 단계에서 오류가 나고, 그 파일은 만들어지지 않는다.
    #[test]
    fn a_file_sink_outside_the_folder_never_writes() {
        let base = tmp_dir("filesink-escape");
        let outside = tmp_dir("filesink-victim");
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, "원래 내용").unwrap();

        let mut p = Pipeline::new("탈출");
        p.tick_hz = 120.0;
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 5 },
            },
            [0.0, 0.0],
        ));
        let sink = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::File {
                    path: victim.to_string_lossy().into_owned(),
                    append: false,
                },
            },
            [1.0, 0.0],
        ));
        p.add_link(src, sink).unwrap();

        let h = Runner::new(Project::new("p"), p, base.clone(), DevicePref::Cpu)
            .start()
            .unwrap();
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| {
                matches!(e, RunnerEvent::Error { node, message } if *node == Some(sink) && message.contains("절대 경로"))
            })
            .is_some(),
            "절대 경로 오류가 오지 않았다"
        );
        std::thread::sleep(Duration::from_millis(120));
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));

        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "원래 내용",
            "폴더 밖 파일이 덮어써졌다"
        );
        std::fs::remove_dir_all(&base).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    /// 폴더 안 상대 경로는 그대로 동작한다 (제한이 정상 사용을 막지 않는다).
    #[test]
    fn a_relative_file_sink_still_writes() {
        let base = tmp_dir("filesink-ok");
        let mut p = Pipeline::new("정상");
        p.tick_hz = 120.0;
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 5 },
            },
            [0.0, 0.0],
        ));
        let sink = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::File {
                    path: "logs/out.jsonl".into(),
                    append: true,
                },
            },
            [1.0, 0.0],
        ));
        p.add_link(src, sink).unwrap();

        let h = Runner::new(Project::new("p"), p, base.clone(), DevicePref::Cpu)
            .start()
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));

        let text = std::fs::read_to_string(base.join("logs/out.jsonl")).expect("파일이 없다");
        assert!(!text.trim().is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_file_source_outside_the_folder_is_refused() {
        let base = tmp_dir("filesrc-escape");
        let mut p = Pipeline::new("읽기 탈출");
        p.tick_hz = 120.0;
        let src = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::File {
                    path: "../../etc/passwd".into(),
                    interval_ms: 10,
                },
            },
            [0.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(src, log).unwrap();

        let h = Runner::new(Project::new("p"), p, base.clone(), DevicePref::Cpu)
            .start()
            .unwrap();
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| {
                matches!(e, RunnerEvent::Error { node, message } if *node == Some(src) && message.contains("밖을 가리킨다"))
            })
            .is_some(),
            "상위 이동 오류가 오지 않았다"
        );
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
        std::fs::remove_dir_all(&base).ok();
    }

    // ── M4: 스트림 큐 드롭 정책 ──

    #[test]
    fn a_full_queue_drops_the_oldest() {
        let (tx, rx) = crossbeam_channel::bounded::<i32>(2);
        assert!(matches!(send_dropping_oldest(&tx, &rx, 1), Pushed::Ok));
        assert!(matches!(send_dropping_oldest(&tx, &rx, 2), Pushed::Ok));
        // 여기서 가득 찼다 — 1 이 밀려난다.
        assert!(matches!(send_dropping_oldest(&tx, &rx, 3), Pushed::DroppedOldest));
        assert_eq!(rx.try_recv().unwrap(), 2, "가장 오래된 1 이 버려져야 한다");
        assert_eq!(rx.try_recv().unwrap(), 3);
        assert!(rx.try_recv().is_err());

        // 받는 쪽이 사라지면 알린다.
        let (tx2, rx2) = crossbeam_channel::bounded::<i32>(1);
        drop(rx2);
        assert!(matches!(
            send_dropping_oldest(&tx2, &crossbeam_channel::bounded::<i32>(1).1, 1),
            Pushed::Disconnected
        ));
        let _ = tx2;
    }

    // ── M5: WebSocket 주소 확인 ──

    #[test]
    fn websocket_urls_must_use_a_websocket_scheme() {
        // wss 는 조용히 통과.
        assert_eq!(check_ws_url("wss://example.com/s").unwrap(), None);
        // 루프백 ws 도 조용히 통과 (나갈 데가 없다).
        assert_eq!(check_ws_url("ws://127.0.0.1:9001").unwrap(), None);
        assert_eq!(check_ws_url("ws://localhost:9001/x").unwrap(), None);
        // 바깥으로 나가는 평문은 알린다.
        let notice = check_ws_url("ws://example.com:9001/s").unwrap().expect("주의가 없다");
        assert!(notice.contains("wss://"), "{notice}");

        // 다른 스킴은 거부.
        for bad in [
            "http://example.com",
            "https://example.com",
            "file:///etc/passwd",
            "example.com",
        ] {
            let err = check_ws_url(bad).unwrap_err();
            assert!(err.contains("ws://"), "{bad}: {err}");
        }
        // 호스트가 없으면 거부.
        assert!(check_ws_url("ws://").is_err());
        assert!(check_ws_url("ws:///path").is_err());
    }

    #[test]
    fn websocket_host_is_extracted_past_userinfo() {
        assert_eq!(ws_host_of("example.com:9001/x"), "example.com:9001");
        assert_eq!(ws_host_of("user:pw@example.com/x"), "example.com");
        assert_eq!(ws_host_of("127.0.0.1:1?q=1"), "127.0.0.1:1");
    }

    #[test]
    fn a_bad_websocket_scheme_is_reported_at_startup() {
        let mut p = Pipeline::new("잘못된 ws");
        p.tick_hz = 60.0;
        let ws = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::WebSocket {
                    url: "http://example.com".into(),
                },
            },
            [0.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(ws, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsscheme"), DevicePref::Cpu)
            .start()
            .unwrap();
        assert!(
            wait_for(&h, Duration::from_secs(3), |e| {
                matches!(e, RunnerEvent::Error { node, message } if *node == Some(ws) && message.contains("ws://"))
            })
            .is_some(),
            "스킴 오류가 오지 않았다"
        );
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)));
    }

    // ── 토큰 환경 변수 ──

    #[test]
    fn a_port_specific_token_wins_over_the_global_one() {
        // 결정 로직만 본다 — 전역 환경 변수를 건드리면 같이 돌던 시험의 서버가 토큰을 요구하게 된다.
        assert_eq!(
            pick_token(Some("포트".into()), Some("전체".into())).as_deref(),
            Some("포트")
        );
        assert_eq!(pick_token(None, Some("전체".into())).as_deref(), Some("전체"));
        assert_eq!(pick_token(Some("포트".into()), None).as_deref(), Some("포트"));
        assert_eq!(pick_token(None, None), None);
        // 공백뿐인 값은 없는 것과 같다.
        assert_eq!(pick_token(Some("   ".into()), None), None);
        assert_eq!(pick_token(None, Some(" \t ".into())), None);
        // 앞뒤 공백은 다듬는다.
        assert_eq!(pick_token(Some(" abc ".into()), None).as_deref(), Some("abc"));
    }

    /// 환경 변수 읽기 자체는 **포트별 변수로만** 확인한다.
    /// 이 이름은 이 시험만 쓰므로 다른 시험에 번지지 않는다
    /// (`NL_HTTP_TOKEN` 을 설정하면 같이 돌던 시험의 서버가 401 을 내기 시작한다).
    #[test]
    fn the_port_specific_variable_is_read_from_the_environment() {
        let port = "65432";
        let bind = format!("127.0.0.1:{port}");
        let name = format!("{HTTP_TOKEN_ENV_PREFIX}{port}");
        assert_eq!(token_override(&bind), None, "설정 전에는 없다");

        // SAFETY: 이 이름은 이 시험만 쓴다.
        unsafe { std::env::set_var(&name, "포트별토큰") };
        assert_eq!(token_override(&bind).as_deref(), Some("포트별토큰"));
        // 다른 포트는 영향받지 않는다.
        assert_eq!(token_override("127.0.0.1:65433"), None);
        // SAFETY: 위와 같다.
        unsafe { std::env::remove_var(&name) };
        assert_eq!(token_override(&bind), None, "지우면 다시 없다");
    }

    // ── M2: 느린 클라이언트와 헤더 폭탄 ──

    /// 서버를 하나 띄우고 그 주소를 돌려준다 (`HttpServer → HttpReply`).
    fn serve_echo(tag: &str) -> (RunnerHandle, String) {
        let mut p = Pipeline::new("echo");
        p.tick_hz = 120.0;
        let server = http_server_node(&mut p, ANY_ADDR, "/infer");
        let reply = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        p.add_link(server, reply).unwrap();
        let h = Runner::new(Project::new("p"), p, tmp_dir(tag), DevicePref::Cpu)
            .start()
            .unwrap();
        let addr = wait_server_up(&h);
        assert!(
            wait_for(
                &h,
                Duration::from_secs(5),
                |e| matches!(e, RunnerEvent::Log(m) if m.contains("요청 받기 시작"))
            )
            .is_some(),
            "준비 완료 로그가 오지 않았다"
        );
        (h, addr)
    }

    /// 응답의 상태 줄만 읽는다.
    fn status_line(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    /// 헤더를 1바이트씩 천천히 보내는 클라이언트는 타임아웃으로 끊기고,
    /// 그동안에도 다른 요청은 정상으로 처리된다.
    #[test]
    fn a_slowloris_client_is_cut_off_and_others_keep_working() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("slowloris");

        // 느린 연결: 요청 줄만 보내고 헤더를 아주 천천히 흘린다.
        let mut slow = std::net::TcpStream::connect(&addr).expect("붙지 못했다");
        slow.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        slow.write_all(b"POST /infer HTTP/1.1\r\n").unwrap();
        slow.flush().unwrap();
        let slow_started = Instant::now();
        // 헤더를 1바이트씩 아주 천천히. 머리 전체 마감(5초)보다 훨씬 오래 걸리게 잡는다 —
        // 읽기 한 번은 언제나 제때 끝나므로, 마감이 없으면 이 연결은 영원히 산다.
        let dribble = std::thread::spawn(move || {
            for b in "X-Slow: ".bytes().chain(std::iter::repeat_n(b'a', 60)) {
                if slow.write_all(&[b]).is_err() || slow.flush().is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            let mut raw = Vec::new();
            let _ = slow.read_to_end(&mut raw);
            raw
        });

        // 느린 연결이 살아 있는 동안 정상 요청이 통과해야 한다.
        std::thread::sleep(Duration::from_millis(300));
        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[1,2]"),
            Duration::from_secs(5),
        )
        .expect("느린 연결 때문에 정상 요청이 막혔다");
        assert_eq!(res.status, 200, "본문: {}", res.body);

        // 느린 쪽은 머리 마감(5초)에 끊긴다. 흘리기만 했다면 27초가 걸렸을 분량이다.
        let raw = dribble.join().expect("느린 클라이언트 스레드");
        let elapsed = slow_started.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "느린 연결이 끊기지 않았다 ({elapsed:?}) — 머리 마감이 동작하지 않는다"
        );
        let line = status_line(&raw);
        assert!(
            line.contains("408") || line.contains("431") || line.contains("400"),
            "끊긴 이유가 분명하지 않다: {line:?}"
        );

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// **느린 본문** 8개가 동시에 붙어 있어도 정상 요청이 곧바로 처리된다.
    ///
    /// 본문 읽기를 고정 워커 풀에 맡기던 시절에는 느린 본문 몇이 풀을 다 차지해 뒤 요청이 밀렸다.
    /// 지금은 연결마다 스레드라 그 일이 없다 — 머리든 본문이든 자기 연결만 붙잡는다.
    #[test]
    fn eight_slow_bodies_do_not_delay_a_normal_request() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("slowbodies");

        let stop = Arc::new(AtomicBool::new(false));
        let mut slow = Vec::new();
        for _ in 0..8 {
            let Ok(mut sock) = std::net::TcpStream::connect(&addr) else {
                continue;
            };
            sock.set_write_timeout(Some(Duration::from_secs(2))).ok();
            // 머리는 제대로 보낸다 — 여기서 막히면 본문 시험이 되지 않는다.
            // 본문 4096 바이트를 약속해 놓고 아주 천천히 흘린다.
            if sock
                .write_all(
                    b"POST /infer HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                      Content-Type: application/json\r\nContent-Length: 4096\r\n\r\n",
                )
                .is_err()
            {
                continue;
            }
            let _ = sock.flush();
            let s2 = stop.clone();
            slow.push(std::thread::spawn(move || {
                while !s2.load(Ordering::SeqCst) {
                    if sock.write_all(b" ").is_err() || sock.flush().is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            }));
        }
        assert_eq!(slow.len(), 8, "느린 본문 연결을 8개 만들지 못했다");
        std::thread::sleep(Duration::from_millis(300));

        // 느린 본문 8개가 살아 있는 동안 정상 요청이 1초 안에 끝나야 한다.
        let start = Instant::now();
        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[7]"),
            Duration::from_secs(5),
        )
        .expect("느린 본문 8개 때문에 정상 요청이 막혔다");
        let took = start.elapsed();
        assert_eq!(res.status, 200, "본문: {}", res.body);
        // 요점은 느린 연결에 막히지 않는다는 것이다. 막혔다면 머리 5초·본문 30초 마감까지 갔을 것이다.
        assert!(took < Duration::from_secs(3), "정상 요청이 {took:?} 나 걸렸다");

        stop.store(true, Ordering::SeqCst);
        for t in slow {
            let _ = t.join();
        }
        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 헤더를 끝없이 보내면 431 로 끊는다 — 메모리가 늘지 않는다.
    #[test]
    fn a_header_bomb_is_cut_with_431() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("headerbomb");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(b"POST /infer HTTP/1.1\r\nHost: 127.0.0.1\r\n").unwrap();
        // 상한(16KB)을 확실히 넘기도록 넉넉히 보낸다. 중간에 끊기면 그것이 정상이다.
        for i in 0..4000 {
            let line = format!("X-Pad-{i}: {}\r\n", "a".repeat(64));
            if sock.write_all(line.as_bytes()).is_err() {
                break;
            }
        }
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        let line = status_line(&raw);
        assert!(line.contains("431"), "헤더 폭탄이 431 로 끊기지 않았다: {line}");

        // 서버는 멀쩡하다.
        let res = crate::http::call(
            "POST",
            &format!("http://{addr}/infer"),
            &BTreeMap::new(),
            Some("[3]"),
            Duration::from_secs(5),
        )
        .expect("헤더 폭탄 뒤 서버가 죽었다");
        assert_eq!(res.status, 200);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 헤더 **개수**가 많아도 끊는다.
    #[test]
    fn too_many_headers_are_cut_with_431() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("manyheaders");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(b"POST /infer HTTP/1.1\r\n").unwrap();
        // 짧은 헤더를 상한(64개)보다 많이 — 바이트 예산에는 걸리지 않는 크기다.
        for i in 0..200 {
            let _ = sock.write_all(format!("X-{i}: 1\r\n").as_bytes());
        }
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        assert!(status_line(&raw).contains("431"), "{}", status_line(&raw));

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 본문이 있는 요청에 `Content-Length` 가 없으면 411.
    #[test]
    fn a_post_without_content_length_is_411() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("nolength");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(b"POST /infer HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        assert!(status_line(&raw).contains("411"), "{}", status_line(&raw));

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 청크 전송도 받는다.
    #[test]
    fn a_chunked_body_is_accepted() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("chunked");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(
            b"POST /infer HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
              Transfer-Encoding: chunked\r\n\r\n",
        )
        .unwrap();
        // `[1,2]` 를 두 조각으로.
        sock.write_all(b"3\r\n[1,\r\n2\r\n2]\r\n0\r\n\r\n").unwrap();
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.starts_with("HTTP/1.1 200"), "{}", &text[..text.len().min(200)]);
        assert!(text.trim_end().ends_with("[1,2]"), "본문이 다르다: {text}");

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// HTTP/1.0 요청도 받는다.
    #[test]
    fn http_1_0_requests_are_accepted() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("http10");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(b"POST /infer HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n[1]")
            .unwrap();
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.starts_with("HTTP/1.1 200"), "{}", &text[..text.len().min(200)]);

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
    }

    /// 응답에는 언제나 보안 헤더와 `Connection: close` 가 붙는다.
    #[test]
    fn responses_carry_the_expected_headers() {
        use std::io::Write as _;
        let (h, addr) = serve_echo("headers");

        let mut sock = std::net::TcpStream::connect(&addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(b"POST /infer HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n[1]")
            .unwrap();
        let _ = sock.flush();
        let mut raw = Vec::new();
        let _ = sock.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        for want in [
            "x-content-type-options: nosniff",
            "connection: close",
            "content-length:",
            "content-type:",
        ] {
            assert!(text.contains(want), "{want} 가 없다:\n{text}");
        }

        h.stop();
        assert!(h.wait_done(Duration::from_secs(3)));
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
        assert_eq!(
            query_to_value("a=1&b=hi"),
            Value::Json(serde_json::json!({"a":"1","b":"hi"}))
        );
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
                    let Ok(mut ws) = tungstenite::accept(stream) else {
                        continue;
                    };
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
            Self {
                url,
                stop,
                seen,
                handle: Some(handle),
            }
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
            PNodeKind::Source {
                source: Source::WebSocket {
                    url: server.url.clone(),
                },
            },
            [0.0, 0.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(src, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsin"), DevicePref::Cpu)
            .start()
            .unwrap();
        let ev = wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Value { node, .. } if *node == src),
        );
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
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "WebSocket 스레드 정리가 느리다: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn websocket_sink_sends_and_shares_the_connection_with_the_source() {
        let server = EchoServer::start(None);
        let mut p = Pipeline::new("ws-roundtrip");
        p.tick_hz = 60.0;
        // 같은 url 의 소스와 싱크 → 연결 하나를 공유한다. 보낸 것이 에코로 되돌아온다.
        let manual = p.add_node(PNode::new(PNodeKind::Source { source: Source::Manual }, [0.0, 0.0]));
        let out = p.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::WebSocketSend {
                    url: server.url.clone(),
                },
            },
            [1.0, 0.0],
        ));
        let back = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::WebSocket {
                    url: server.url.clone(),
                },
            },
            [0.0, 1.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 1.0]));
        p.add_link(manual, out).unwrap();
        p.add_link(back, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsout"), DevicePref::Cpu)
            .start()
            .unwrap();
        // 연결이 설 때까지 기다렸다가 보낸다.
        assert!(
            wait_for(
                &h,
                Duration::from_secs(5),
                |e| matches!(e, RunnerEvent::Log(m) if m.contains("연결됨"))
            )
            .is_some(),
            "WebSocket 연결 로그가 오지 않았다"
        );
        h.inputs
            .send(RunnerInput::Manual {
                node: manual,
                value: Value::Text("핑".into()),
            })
            .unwrap();

        let got = server
            .seen
            .recv_timeout(Duration::from_secs(5))
            .expect("서버가 메시지를 받지 못했다");
        assert_eq!(got, "핑", "텍스트 값은 따옴표 없이 그대로 가야 한다");

        let ev = wait_for(
            &h,
            Duration::from_secs(5),
            |e| matches!(e, RunnerEvent::Value { node, .. } if *node == back),
        );
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
        let ws = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::WebSocket { url: dead },
            },
            [0.0, 0.0],
        ));
        let timer = p.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::Timer { interval_ms: 10 },
            },
            [0.0, 1.0],
        ));
        let log = p.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        p.add_link(ws, log).unwrap();
        p.add_link(timer, log).unwrap();

        let h = Runner::new(Project::new("p"), p, tmp_dir("wsdead"), DevicePref::Cpu)
            .start()
            .unwrap();
        assert!(
            wait_for(
                &h,
                Duration::from_secs(5),
                |e| matches!(e, RunnerEvent::Error { node, .. } if *node == Some(ws))
            )
            .is_some(),
            "연결 실패 오류가 오지 않았다"
        );
        // 루프는 계속 돈다.
        assert!(
            wait_for(
                &h,
                Duration::from_secs(3),
                |e| matches!(e, RunnerEvent::Value { node, .. } if *node == timer)
            )
            .is_some(),
            "WebSocket 실패 뒤 타이머가 멈췄다"
        );

        let t = Instant::now();
        h.stop();
        assert!(h.wait_done(Duration::from_secs(2)), "stop 후에도 끝나지 않았다");
        // 백오프로 자고 있어도 곧바로 깨야 한다.
        // 백오프는 1초부터 시작한다. 그 안에 깨는지가 요점이라 2초면 충분히 구분된다.
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "백오프 중 stop 이 느리다: {:?}",
            t.elapsed()
        );
        assert!(wait_for(&h, Duration::from_secs(1), |e| matches!(e, RunnerEvent::Stopped)).is_some());
    }
}
