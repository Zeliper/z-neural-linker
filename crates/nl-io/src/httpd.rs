//! 파이프라인 `Source::HttpServer` 가 쓰는 최소 HTTP/1.1 서버. **표준 라이브러리만** 쓴다.
//!
//! 범용 서버가 아니다. 파이프라인 하나가 요청 하나를 받아 값 하나로 답하는, 딱 그만큼만 한다.
//! 그 대신 **바깥에 노출되는 입구**라서 자원 한계를 전부 우리가 쥔다 — 이것이 직접 쓴 이유다.
//!
//! ## 무엇을 막는가
//! | 공격 | 대응 |
//! |---|---|
//! | slowloris (헤더를 1바이트씩 천천히) | 소켓 읽기 타임아웃 [`HEADER_TIMEOUT`] |
//! | 본문을 천천히 | 본문 단계 타임아웃 [`BODY_TIMEOUT`] |
//! | 헤더 폭탄 (무한 헤더·초장문 한 줄) | 총 [`MAX_HEADER_BYTES`] · 개수 [`MAX_HEADERS`] → 431 |
//! | 연결 폭주 | 동시 연결 상한 (호출부가 센다) → 503 |
//! | 거대 본문 | 호출부가 정한 상한 → 413 |
//! | 응답을 안 읽는 클라이언트 | 쓰기 타임아웃 [`WRITE_TIMEOUT`] |
//!
//! ## 하지 않는 것
//! keep-alive 를 하지 않는다. 응답마다 `Connection: close` 를 붙이고 소켓을 닫는다. 연결 하나가
//! 요청 하나다 — 상태가 없어 동시 연결 수 세기가 정확해지고, 파이프라인의 "요청 하나씩" 규칙과도 맞는다.
//! HTTP/1.0 요청도 받는다(응답은 언제나 `HTTP/1.1`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// 요청 라인 + 헤더를 **다 받기까지** 허용하는 총 시간.
///
/// 읽기 한 번의 타임아웃만으로는 slowloris 를 막지 못한다 — 400ms 마다 1바이트씩 흘리면
/// 읽기는 매번 제때 끝나지만 머리는 영원히 안 끝난다. 그래서 총 마감을 따로 둔다.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// 본문을 읽는 동안의 소켓 읽기 타임아웃.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// 응답을 쓰는 동안의 타임아웃.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// 요청 라인 + 헤더를 합친 바이트 상한. 넘으면 431.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// 헤더 줄 수 상한. 넘으면 431.
pub const MAX_HEADERS: usize = 64;

/// 읽기·쓰기 준비가 끝난 연결을 넘겨주는 리스너.
pub struct Server {
    listener: TcpListener,
    addr: SocketAddr,
}

impl Server {
    /// 주소에 묶고 논블로킹으로 둔다. 논블로킹이라야 `stop` 플래그를 제때 본다.
    pub fn bind(addr: &str) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        Ok(Self { listener, addr })
    }

    /// 실제로 묶인 주소. `:0` 으로 열었을 때 포트를 알아내는 데 쓴다.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// 연결 하나를 받는다. 없으면 `Ok(None)` (논블로킹).
    ///
    /// 받자마자 타임아웃을 걸어 둔다 — 그래야 느린 상대가 스레드를 붙잡지 못한다.
    pub fn accept(&self) -> std::io::Result<Option<TcpStream>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(HEADER_TIMEOUT))?;
                stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
                // 작은 응답을 곧바로 내보낸다.
                let _ = stream.set_nodelay(true);
                Ok(Some(stream))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// 머리까지 읽은 요청. 본문은 아직 소켓에 남아 있다.
pub struct Request {
    method: String,
    /// 쿼리를 뗀 경로. 퍼센트 디코드는 하지 않는다(호출부 규칙에 맡긴다).
    path: String,
    query: String,
    /// 이름은 소문자로 눕혀 둔다.
    headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    /// 응답을 쓰는 쪽. 같은 소켓의 두 번째 핸들이다.
    writer: TcpStream,
    body: BodyKind,
    /// 응답을 이미 보냈는가. 안 보냈으면 드롭될 때 500 을 보낸다.
    answered: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BodyKind {
    None,
    Sized(u64),
    Chunked,
}

/// 머리를 읽다가 생긴 문제. 어떤 상태 코드로 답할지까지 담는다.
#[derive(Debug)]
pub struct HeadError {
    pub status: u16,
    pub message: String,
}

impl HeadError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

/// 본문을 읽다가 생긴 문제.
#[derive(Debug)]
pub struct BodyError {
    pub status: u16,
    pub message: String,
}

impl BodyError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

impl Request {
    /// 소켓에서 요청 라인과 헤더를 읽는다. 본문은 건드리지 않는다.
    pub fn read_head(stream: TcpStream) -> Result<Self, (TcpStream, HeadError)> {
        let writer = match stream.try_clone() {
            Ok(w) => w,
            Err(e) => return Err((stream, HeadError::new(500, format!("소켓을 복제하지 못했다: {e}")))),
        };
        let mut reader = BufReader::new(stream);
        let mut budget = MAX_HEADER_BYTES;
        // 머리를 다 받기까지의 마감. 천천히 흘리는 상대를 여기서 끊는다.
        let deadline = std::time::Instant::now() + HEADER_TIMEOUT;

        let line = match read_line(&mut reader, &mut budget, deadline) {
            Ok(l) => l,
            Err(e) => return Err((reader.into_inner(), e)),
        };
        let Some((method, target, _version)) = parse_request_line(&line) else {
            return Err((reader.into_inner(), HeadError::new(400, format!("요청 줄을 읽을 수 없다: {line:?}"))));
        };

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let line = match read_line(&mut reader, &mut budget, deadline) {
                Ok(l) => l,
                Err(e) => return Err((reader.into_inner(), e)),
            };
            if line.is_empty() {
                break;
            }
            if headers.len() >= MAX_HEADERS {
                return Err((
                    reader.into_inner(),
                    HeadError::new(431, format!("헤더가 너무 많다 (상한 {MAX_HEADERS}개)")),
                ));
            }
            let Some((name, value)) = line.split_once(':') else {
                return Err((reader.into_inner(), HeadError::new(400, format!("헤더 줄이 잘못됐다: {line:?}"))));
            };
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }

        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), q.to_owned()),
            None => (target.to_owned(), String::new()),
        };

        let body = match body_kind(&headers, &method) {
            Ok(b) => b,
            Err(e) => return Err((reader.into_inner(), e)),
        };

        Ok(Self { method, path, query, headers, reader, writer, body, answered: false })
    }

    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    /// 헤더 하나 (이름은 대소문자를 가리지 않는다).
    pub fn header(&self, name: &str) -> Option<&str> {
        let want = name.to_ascii_lowercase();
        self.headers.iter().find(|(n, _)| *n == want).map(|(_, v)| v.as_str())
    }
    /// `Content-Length` 가 알려 준 길이. 청크 전송이면 `None`.
    pub fn declared_len(&self) -> Option<u64> {
        match self.body {
            BodyKind::Sized(n) => Some(n),
            _ => None,
        }
    }

    /// 본문을 읽는다. `limit` 을 넘으면 413.
    ///
    /// 여기서부터는 읽기 타임아웃이 [`BODY_TIMEOUT`] 으로 늘어난다 — 큰 본문은 시간이 걸려도 정상이다.
    pub fn read_body(&mut self, limit: u64) -> Result<Vec<u8>, BodyError> {
        let _ = self.reader.get_ref().set_read_timeout(Some(BODY_TIMEOUT));
        match self.body {
            BodyKind::None => Ok(Vec::new()),
            BodyKind::Sized(n) => {
                if n > limit {
                    return Err(BodyError::new(413, format!("본문이 너무 크다 (상한 {limit} 바이트)")));
                }
                let mut buf = Vec::new();
                // 선언 길이를 믿고 통째로 잡지 않는다 — 서버가 부는 대로 메모리를 내주지 않으려는 것이다.
                let read = Read::take(&mut self.reader, n).read_to_end(&mut buf);
                match read {
                    Ok(got) if got as u64 == n => Ok(buf),
                    Ok(got) => Err(BodyError::new(400, format!("본문이 짧다 ({got}/{n} 바이트)"))),
                    Err(e) => Err(BodyError::new(400, format!("본문을 읽지 못했다: {e}"))),
                }
            }
            BodyKind::Chunked => self.read_chunked(limit),
        }
    }

    /// `<크기(16진)>CRLF<데이터>CRLF` 를 0 크기가 올 때까지. 누적이 `limit` 을 넘으면 413.
    fn read_chunked(&mut self, limit: u64) -> Result<Vec<u8>, BodyError> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .map_err(|e| BodyError::new(400, format!("청크 길이를 읽지 못했다: {e}")))?;
            let head = line.trim_end_matches(['\r', '\n']);
            // 청크 확장(`;name=value`)은 무시한다.
            let size_text = head.split(';').next().unwrap_or(head).trim();
            if size_text.is_empty() {
                return Err(BodyError::new(400, "청크 길이가 비어 있다".to_string()));
            }
            let size = u64::from_str_radix(size_text, 16)
                .map_err(|_| BodyError::new(400, format!("청크 길이가 16진수가 아니다: {size_text:?}")))?;
            if size == 0 {
                // 마지막 청크 뒤의 트레일러와 빈 줄을 흘려보낸다.
                let mut tail = String::new();
                while self.reader.read_line(&mut tail).is_ok() {
                    let t = tail.trim_end_matches(['\r', '\n']).to_owned();
                    tail.clear();
                    if t.is_empty() {
                        break;
                    }
                }
                return Ok(out);
            }
            if out.len() as u64 + size > limit {
                return Err(BodyError::new(413, format!("본문이 너무 크다 (상한 {limit} 바이트)")));
            }
            let start = out.len();
            out.resize(start + size as usize, 0);
            self.reader
                .read_exact(&mut out[start..])
                .map_err(|e| BodyError::new(400, format!("청크 데이터를 읽지 못했다: {e}")))?;
            // 데이터 뒤의 CRLF.
            let mut crlf = [0u8; 2];
            self.reader
                .read_exact(&mut crlf)
                .map_err(|e| BodyError::new(400, format!("청크 끝을 읽지 못했다: {e}")))?;
        }
    }

    /// 남은 본문을 버린다. 응답만 하고 끝낼 때, 상대가 다 보내기 전에 닫으면
    /// 운영체제가 RST 를 보내 응답이 유실될 수 있어 조금은 읽어 준다.
    fn drain(&mut self, limit: u64) {
        let _ = self.reader.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
        let mut sink = std::io::sink();
        let _ = std::io::copy(&mut Read::take(&mut self.reader, limit), &mut sink);
    }

    /// 응답을 보내고 연결을 닫는다. 소비하므로 한 번만 답할 수 있다.
    pub fn respond(mut self, status: u16, content_type: &str, body: &[u8]) -> std::io::Result<()> {
        self.answered = true;
        // 상대가 아직 본문을 보내는 중일 수 있다. 조금 읽어 주고 답해야 응답이 제대로 닿는다.
        if self.body != BodyKind::None {
            self.drain(64 * 1024);
        }
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {len}\r\n\
             X-Content-Type-Options: nosniff\r\n\
             Connection: close\r\n\
             \r\n",
            reason = reason(status),
            len = body.len()
        );
        self.writer.write_all(head.as_bytes())?;
        if !body.is_empty() {
            self.writer.write_all(body)?;
        }
        self.writer.flush()?;
        let _ = self.writer.shutdown(Shutdown::Write);
        Ok(())
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // 답하지 않고 버려지는 요청이 없게 한다. 클라이언트가 영원히 기다리는 것보다 500 이 낫다.
        if !self.answered {
            let body = b"{\"error\":\"no response\",\"status\":500}";
            let head = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 Content-Type: application/json; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 X-Content-Type-Options: nosniff\r\n\
                 Connection: close\r\n\
                 \r\n",
                body.len()
            );
            let _ = self.writer.write_all(head.as_bytes());
            let _ = self.writer.write_all(body);
            let _ = self.writer.flush();
            let _ = self.writer.shutdown(Shutdown::Write);
        }
    }
}

/// 머리를 읽지 못한 소켓에 상태 코드만 적어 보낸다.
pub fn respond_raw(mut stream: TcpStream, status: u16, message: &str) {
    let body = format!(
        "{{\"error\":{},\"status\":{status}}}",
        serde_json::Value::String(message.to_owned())
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\
         \r\n",
        reason = reason(status),
        len = body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
}

/// CRLF 까지 한 줄. 남은 예산을 깎고, 다 쓰면 431. `deadline` 을 넘기면 408.
fn read_line(
    reader: &mut BufReader<TcpStream>,
    budget: &mut usize,
    deadline: std::time::Instant,
) -> Result<String, HeadError> {
    let mut raw = Vec::new();
    loop {
        // 한 바이트씩 천천히 보내도 여기서 끊긴다 (slowloris).
        if std::time::Instant::now() >= deadline {
            return Err(HeadError::new(408, "머리를 보내는 데 너무 오래 걸린다"));
        }
        let mut byte = [0u8; 1];
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(HeadError::new(400, "요청이 중간에 끊겼다"));
            }
            Err(e) if is_timeout(&e) => {
                return Err(HeadError::new(408, "머리를 보내는 데 너무 오래 걸린다"));
            }
            Err(e) => return Err(HeadError::new(400, format!("읽기 실패: {e}"))),
        }
        if *budget == 0 {
            return Err(HeadError::new(431, format!("머리가 너무 크다 (상한 {MAX_HEADER_BYTES} 바이트)")));
        }
        *budget -= 1;
        if byte[0] == b'\n' {
            break;
        }
        raw.push(byte[0]);
    }
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    String::from_utf8(raw).map_err(|_| HeadError::new(400, "머리에 UTF-8 이 아닌 바이트가 있다"))
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
}

/// `POST /x?a=1 HTTP/1.1` → `("POST", "/x?a=1", "HTTP/1.1")`.
fn parse_request_line(line: &str) -> Option<(String, &str, &str)> {
    let mut parts = line.split(' ').filter(|s| !s.is_empty());
    let method = parts.next()?;
    let target = parts.next()?;
    // HTTP/0.9 는 버전이 없다. 없으면 1.0 으로 본다.
    let version = parts.next().unwrap_or("HTTP/1.0");
    if !version.starts_with("HTTP/") {
        return None;
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    Some((method.to_ascii_uppercase(), target, version))
}

/// 본문이 어떻게 오는지. 본문이 있어야 할 메서드인데 길이를 안 알려 주면 411.
fn body_kind(headers: &[(String, String)], method: &str) -> Result<BodyKind, HeadError> {
    let get = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    if let Some(te) = get("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            return Ok(BodyKind::Chunked);
        }
        return Err(HeadError::new(501, format!("모르는 Transfer-Encoding: {te}")));
    }
    if let Some(len) = get("content-length") {
        let n = len
            .trim()
            .parse::<u64>()
            .map_err(|_| HeadError::new(400, format!("Content-Length 가 수가 아니다: {len:?}")))?;
        return Ok(if n == 0 { BodyKind::None } else { BodyKind::Sized(n) });
    }
    if matches!(method, "POST" | "PUT" | "PATCH") {
        return Err(HeadError::new(411, "본문이 있는 요청에는 Content-Length 가 필요하다"));
    }
    Ok(BodyKind::None)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_lines_parse() {
        let (m, t, v) = parse_request_line("POST /infer?a=1 HTTP/1.1").unwrap();
        assert_eq!((m.as_str(), t, v), ("POST", "/infer?a=1", "HTTP/1.1"));
        // 소문자 메서드는 대문자로.
        assert_eq!(parse_request_line("get / HTTP/1.0").unwrap().0, "GET");
        // 버전이 없으면 1.0 으로 본다.
        assert_eq!(parse_request_line("GET /").unwrap().2, "HTTP/1.0");
        // 망가진 줄.
        assert!(parse_request_line("").is_none());
        assert!(parse_request_line("GET").is_none());
        assert!(parse_request_line("GET / FTP/1.0").is_none());
        assert!(parse_request_line("G3T / HTTP/1.1").is_none());
    }

    #[test]
    fn body_kind_follows_the_headers() {
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        assert_eq!(body_kind(&h(&[("content-length", "5")]), "POST").unwrap(), BodyKind::Sized(5));
        assert_eq!(body_kind(&h(&[("content-length", "0")]), "POST").unwrap(), BodyKind::None);
        assert_eq!(body_kind(&h(&[("transfer-encoding", "chunked")]), "POST").unwrap(), BodyKind::Chunked);
        assert_eq!(body_kind(&h(&[]), "GET").unwrap(), BodyKind::None);

        // 본문이 있어야 할 메서드인데 길이가 없다 → 411.
        assert_eq!(body_kind(&h(&[]), "POST").unwrap_err().status, 411);
        assert_eq!(body_kind(&h(&[]), "PUT").unwrap_err().status, 411);
        // 수가 아닌 길이 → 400.
        assert_eq!(body_kind(&h(&[("content-length", "많이")]), "POST").unwrap_err().status, 400);
        // 모르는 전송 인코딩 → 501.
        assert_eq!(body_kind(&h(&[("transfer-encoding", "gzip")]), "POST").unwrap_err().status, 501);
    }

    #[test]
    fn reasons_cover_the_codes_we_send() {
        for code in [200, 400, 401, 403, 404, 405, 408, 411, 413, 431, 500, 501, 503, 504] {
            assert_ne!(reason(code), "Unknown", "{code} 의 이유 문구가 없다");
        }
    }
}
