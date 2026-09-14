//! HTTP 호출 (ureq 3, 타임아웃 필수).
//!
//! - 타임아웃은 인자로 **반드시** 받는다. 무한 대기는 파이프라인 틱 루프를 통째로 멈춘다.
//! - 2xx 가 아니어도 본문을 그대로 돌려준다. `Err` 는 연결·DNS·타임아웃·본문 읽기 실패 같은
//!   전송 계층 문제일 때만 난다. 상태 코드 판단은 호출부의 몫이다.
//! - `Agent` 하나를 전역으로 재사용해 연결 풀을 살린다. 타임아웃만 요청마다 덮어쓴다.

use anyhow::{anyhow, bail, Context};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;
use ureq::http::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use ureq::http::{Method, Request};
use ureq::Agent;

/// 응답 본문 상한. 화면 캡처 결과 같은 큰 페이로드가 메모리를 다 먹는 일을 막는다.
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// 호출부가 따로 정하지 않을 때 쓰는 타임아웃.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
}

impl HttpResponse {
    /// 2xx 인가.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 본문을 JSON 으로 읽어 본다. 실패하면 `None`.
    pub fn json(&self) -> Option<serde_json::Value> {
        serde_json::from_str(&self.body).ok()
    }
}

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = Agent::config_builder()
            // 4xx/5xx 를 오류로 바꾸지 않는다. 본문을 봐야 하는 쪽이 많다.
            .http_status_as_error(false)
            .user_agent(concat!("neural-linker/", env!("CARGO_PKG_VERSION")))
            .build();
        Agent::new_with_config(config)
    })
}

/// `method` 는 대소문자를 가리지 않는다. `body` 가 `None` 이면 본문 없이 보낸다.
pub fn call(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<HttpResponse> {
    if timeout.is_zero() {
        bail!("HTTP 타임아웃은 0 일 수 없다");
    }
    let m = Method::from_bytes(method.trim().to_ascii_uppercase().as_bytes())
        .with_context(|| format!("모르는 HTTP 메서드: {method:?}"))?;

    let mut builder = Request::builder().method(m).uri(url);
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("헤더 이름이 잘못됐다: {k:?}"))?;
        let value = HeaderValue::from_str(v).with_context(|| format!("헤더 값이 잘못됐다: {k}: {v:?}"))?;
        builder = builder.header(name, value);
    }

    let a = agent();
    let res = match body {
        Some(b) => {
            let req = builder.body(b.to_owned()).with_context(|| format!("요청을 만들지 못했다: {url}"))?;
            let req = a.configure_request(req).timeout_global(Some(timeout)).build();
            a.run(req)
        }
        None => {
            let req = builder.body(()).with_context(|| format!("요청을 만들지 못했다: {url}"))?;
            let req = a.configure_request(req).timeout_global(Some(timeout)).build();
            a.run(req)
        }
    }
    .map_err(|e| anyhow!("{method} {url} 실패: {e}"))?;

    let status = res.status().as_u16();
    let content_type =
        res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();

    let mut b = res.into_body();
    let raw = b
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_vec()
        .map_err(|e| anyhow!("{method} {url} 본문을 읽지 못했다: {e}"))?;
    // 이진 응답이 와도 죽지 않게 손실 변환한다.
    let body = String::from_utf8_lossy(&raw).into_owned();

    Ok(HttpResponse { status, body, content_type })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    /// 시험용 로컬 서버. 드롭되면 스레드를 접는다.
    struct TestServer {
        addr: String,
        stop: Arc<AtomicBool>,
        server: Arc<tiny_http::Server>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        /// `handler` 는 요청 하나를 받아 응답 `(status, content_type, body)` 를 만든다.
        fn start<F>(handler: F) -> Self
        where
            F: Fn(&mut tiny_http::Request, &str) -> (u16, String, String) + Send + 'static,
        {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("시험 서버를 열지 못했다"));
            let addr = format!("http://{}", server.server_addr());
            let stop = Arc::new(AtomicBool::new(false));
            let (s2, st2) = (server.clone(), stop.clone());
            let handle = thread::spawn(move || {
                while !st2.load(Ordering::SeqCst) {
                    match s2.recv_timeout(Duration::from_millis(50)) {
                        Ok(Some(mut req)) => {
                            let mut body = String::new();
                            let _ = req.as_reader().read_to_string(&mut body);
                            let (status, ctype, out) = handler(&mut req, &body);
                            let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes())
                                .expect("Content-Type 헤더를 만들지 못했다");
                            let resp = tiny_http::Response::from_string(out)
                                .with_status_code(status)
                                .with_header(header);
                            let _ = req.respond(resp);
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            });
            Self { addr, stop, server, handle: Some(handle) }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.server.unblock();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    #[test]
    fn get_returns_status_body_and_content_type() {
        let s = TestServer::start(|req, _| {
            assert_eq!(req.method().as_str(), "GET");
            (200, "application/json".into(), r#"{"ok":true}"#.into())
        });
        let res = call("GET", &format!("{}/x", s.addr), &BTreeMap::new(), None, Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 200);
        assert_eq!(res.body, r#"{"ok":true}"#);
        assert!(res.content_type.starts_with("application/json"), "content-type: {}", res.content_type);
        assert!(res.is_success());
        assert_eq!(res.json().unwrap()["ok"], serde_json::json!(true));
    }

    #[test]
    fn post_sends_body_and_headers() {
        let s = TestServer::start(|req, body| {
            assert_eq!(req.method().as_str(), "POST");
            let has = req.headers().iter().any(|h| h.field.equiv("X-Token") && h.value.as_str() == "abc");
            assert!(has, "X-Token 헤더가 오지 않았다: {:?}", req.headers());
            (201, "text/plain".into(), format!("받음:{body}"))
        });
        let mut headers = BTreeMap::new();
        headers.insert("X-Token".to_string(), "abc".to_string());
        let res = call("post", &s.addr, &headers, Some("hello"), Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 201);
        assert_eq!(res.body, "받음:hello");
    }

    #[test]
    fn put_patch_delete_reach_the_server() {
        let s = TestServer::start(|req, _| (200, "text/plain".into(), req.method().as_str().to_string()));
        for m in ["PUT", "PATCH"] {
            let res = call(m, &s.addr, &BTreeMap::new(), Some("{}"), Duration::from_secs(5)).unwrap();
            assert_eq!(res.body, m);
        }
        let res = call("DELETE", &s.addr, &BTreeMap::new(), None, Duration::from_secs(5)).unwrap();
        assert_eq!(res.body, "DELETE");
    }

    #[test]
    fn non_2xx_still_returns_the_body() {
        let s = TestServer::start(|_, _| (404, "text/plain".into(), "없음".into()));
        let res = call("GET", &s.addr, &BTreeMap::new(), None, Duration::from_secs(5)).unwrap();
        assert_eq!(res.status, 404);
        assert_eq!(res.body, "없음");
        assert!(!res.is_success());
    }

    #[test]
    fn slow_response_hits_the_timeout() {
        let s = TestServer::start(|_, _| {
            thread::sleep(Duration::from_millis(1500));
            (200, "text/plain".into(), "늦음".into())
        });
        let start = std::time::Instant::now();
        let err = call("GET", &s.addr, &BTreeMap::new(), None, Duration::from_millis(200)).unwrap_err();
        // 서버는 1.5초를 잔다. 그보다 일찍 끝났으면 타임아웃이 먹은 것이다.
        assert!(start.elapsed() < Duration::from_millis(1200), "타임아웃이 걸리지 않았다: {:?}", start.elapsed());
        let msg = err.to_string();
        assert!(msg.contains("GET"), "오류 메시지에 메서드가 없다: {msg}");
    }

    #[test]
    fn zero_timeout_is_rejected() {
        assert!(call("GET", "http://127.0.0.1:1/", &BTreeMap::new(), None, Duration::ZERO).is_err());
    }

    #[test]
    fn unknown_method_is_rejected() {
        let err = call("몰라", "http://127.0.0.1:1/", &BTreeMap::new(), None, DEFAULT_TIMEOUT).unwrap_err();
        assert!(format!("{err:#}").contains("메서드"), "{err:#}");
    }

    #[test]
    fn connection_refused_is_an_error() {
        // 127.0.0.1:1 은 열려 있지 않다.
        assert!(call("GET", "http://127.0.0.1:1/", &BTreeMap::new(), None, Duration::from_millis(500)).is_err());
    }
}
