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
//!
//! ## TLS
//! `Source::HttpServer` 에 인증서를 주면 [`TlsAcceptor`] 가 accept 직후 핸드셰이크를 끝내고
//! [`Conn::Tls`] 를 돌려준다. 그 위쪽 HTTP 처리는 평문과 **글자 하나 다르지 않다** — 전송 계층만
//! 갈아 끼운다. 유일하게 TLS 만 쓰는 것은 rustls 이고, 암호 제공자는 `ring` 이라 시스템
//! 개발 라이브러리가 필요 없다.
//!
//! TLS 는 도청과 중간자를 막을 뿐 **누가 부를 수 있는지는 정하지 않는다**. 토큰 규칙은
//! TLS 여부와 상관없이 그대로다.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// TLS 핸드셰이크를 끝내기까지 허용하는 총 시간.
///
/// 평문 쪽 [`HEADER_TIMEOUT`] 과 같은 이유로 둔다 — 핸드셰이크를 한 바이트씩 흘리면서
/// 스레드를 붙잡는 상대를 여기서 끊는다. 읽기 한 번의 타임아웃만으로는 부족하다.
pub const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// 평문이거나 TLS 인 연결. 이 위쪽 HTTP 코드는 어느 쪽인지 알 필요가 없다.
pub enum Conn {
    Plain(TcpStream),
    /// rustls 세션은 읽기와 쓰기가 한 덩어리라 `try_clone` 으로 나눌 수 없다.
    /// 그래서 평문 쪽도 핸들을 나누지 않고 이 하나로 읽고 쓴다.
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Conn {
    /// 밑에 깔린 TCP 소켓. 타임아웃과 셧다운은 TLS 여부와 무관하게 여기에 건다.
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &s.sock,
        }
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.tcp().set_read_timeout(d)
    }

    fn shutdown_write(&self) {
        let _ = self.tcp().shutdown(Shutdown::Write);
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

/// 인증서·키를 읽어 둔 TLS 받개. 연결마다 세션 하나를 만든다.
pub struct TlsAcceptor {
    config: Arc<rustls::ServerConfig>,
}

impl TlsAcceptor {
    /// PEM 바이트에서 만든다. **서버를 열기 전에** 불러서, 인증서가 잘못되면
    /// 소켓을 열기도 전에 실패하게 한다 — 반쯤 열린 채로 도는 상태를 만들지 않는다.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, String> {
        let certs = rustls_pemfile::certs(&mut std::io::Cursor::new(cert_pem))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("인증서 PEM 을 읽지 못했다: {e}"))?;
        if certs.is_empty() {
            return Err("인증서 PEM 에 인증서가 없다 (`-----BEGIN CERTIFICATE-----` 가 보이는지 확인하라)".into());
        }
        let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem))
            .map_err(|e| format!("개인키 PEM 을 읽지 못했다: {e}"))?
            .ok_or_else(|| "개인키 PEM 에 키가 없다 (PKCS#8·PKCS#1·SEC1 중 하나여야 한다)".to_string())?;

        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            // 키가 인증서와 맞지 않으면 여기서 걸린다. 실행 중이 아니라 준비 단계에서 알게 된다.
            .with_single_cert(certs, key)
            .map_err(|e| format!("인증서와 키가 맞지 않는다: {e}"))?;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    /// 핸드셰이크를 끝내고 연결을 돌려준다. [`TLS_HANDSHAKE_TIMEOUT`] 안에 못 끝내면 오류.
    ///
    /// 평문으로 말을 거는 상대는 첫 바이트가 TLS 레코드가 아니라서 여기서 바로 끊긴다.
    pub fn accept(&self, mut stream: TcpStream) -> Result<Conn, String> {
        let mut conn =
            rustls::ServerConnection::new(self.config.clone()).map_err(|e| format!("TLS 세션을 만들지 못했다: {e}"))?;
        let deadline = Instant::now() + TLS_HANDSHAKE_TIMEOUT;
        while conn.is_handshaking() {
            let mut io = DeadlineIo {
                inner: &mut stream,
                deadline,
            };
            // `complete_io` 는 막힐 때까지 안에서 돈다. 그래서 마감은 `DeadlineIo` 가
            // **읽기 한 번마다** 확인한다 — 그러지 않으면 1바이트씩 흘리는 상대에게 붙잡힌다.
            match conn.complete_io(&mut io) {
                Ok(_) => {}
                Err(e) => return Err(format!("TLS 핸드셰이크 실패: {e}")),
            }
        }
        // 핸드셰이크가 끝났으니 평문과 같은 타임아웃으로 돌려놓는다.
        stream
            .set_read_timeout(Some(HEADER_TIMEOUT))
            .map_err(|e| format!("읽기 타임아웃을 걸지 못했다: {e}"))?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .map_err(|e| format!("쓰기 타임아웃을 걸지 못했다: {e}"))?;
        Ok(Conn::Tls(Box::new(rustls::StreamOwned::new(conn, stream))))
    }
}

/// 마감을 **읽기·쓰기 한 번마다** 다시 거는 어댑터.
///
/// 소켓 타임아웃만 걸어 두면 상대가 조금씩 보내는 동안 매번 갱신돼 전체 시간이 무한해진다.
/// 남은 시간을 계산해 다시 걸어야 총 시간이 묶인다 — 평문 쪽 `read_line` 과 같은 수법이다.
struct DeadlineIo<'a> {
    inner: &'a mut TcpStream,
    deadline: Instant,
}

impl DeadlineIo<'_> {
    fn left(&self) -> std::io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "TLS 핸드셰이크 시간 초과",
            ));
        }
        Ok(left)
    }
}

impl Read for DeadlineIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.inner.set_read_timeout(Some(left))?;
        self.inner.read(buf)
    }
}

impl Write for DeadlineIo<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let left = self.left()?;
        self.inner.set_write_timeout(Some(left))?;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

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
    /// 읽기와 쓰기를 함께 하는 하나의 연결. TLS 세션은 둘로 쪼갤 수 없어서,
    /// 평문 쪽도 핸들을 나누지 않고 여기로 맞췄다. 쓰기는 `io.get_mut()` 으로 한다.
    io: BufReader<Conn>,
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
        Self {
            status,
            message: message.into(),
        }
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
        Self {
            status,
            message: message.into(),
        }
    }
}

impl Request {
    /// 연결에서 요청 라인과 헤더를 읽는다. 본문은 건드리지 않는다.
    ///
    /// 실패하면 연결을 **돌려준다** — 호출부가 그 위에 상태 코드를 적어 보낼 수 있게.
    pub fn read_head(conn: Conn) -> Result<Self, (Conn, HeadError)> {
        let mut reader = BufReader::new(conn);
        let mut budget = MAX_HEADER_BYTES;
        // 머리를 다 받기까지의 마감. 천천히 흘리는 상대를 여기서 끊는다.
        let deadline = std::time::Instant::now() + HEADER_TIMEOUT;

        let line = match read_line(&mut reader, &mut budget, deadline) {
            Ok(l) => l,
            Err(e) => return Err((reader.into_inner(), e)),
        };
        let Some((method, target, _version)) = parse_request_line(&line) else {
            return Err((
                reader.into_inner(),
                HeadError::new(400, format!("요청 줄을 읽을 수 없다: {line:?}")),
            ));
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
                return Err((
                    reader.into_inner(),
                    HeadError::new(400, format!("헤더 줄이 잘못됐다: {line:?}")),
                ));
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

        Ok(Self {
            method,
            path,
            query,
            headers,
            io: reader,
            body,
            answered: false,
        })
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
        let _ = self.io.get_ref().set_read_timeout(Some(BODY_TIMEOUT));
        let out = self.read_body_inner(limit);
        if out.is_ok() {
            // **다 읽었다고 표시한다.** 이걸 안 하면 `respond` 가 `drain` 으로 들어가,
            // 이미 끝난 본문을 200ms 동안 더 기다린다 — 요청마다 200ms 가 그냥 붙는다.
            // 실패했을 때는 그대로 둔다: 상대가 아직 보내는 중일 수 있어 조금은 읽어 줘야 한다.
            self.body = BodyKind::None;
        }
        out
    }

    fn read_body_inner(&mut self, limit: u64) -> Result<Vec<u8>, BodyError> {
        match self.body {
            BodyKind::None => Ok(Vec::new()),
            BodyKind::Sized(n) => {
                if n > limit {
                    return Err(BodyError::new(413, format!("본문이 너무 크다 (상한 {limit} 바이트)")));
                }
                let mut buf = Vec::new();
                // 선언 길이를 믿고 통째로 잡지 않는다 — 서버가 부는 대로 메모리를 내주지 않으려는 것이다.
                let read = Read::take(&mut self.io, n).read_to_end(&mut buf);
                match read {
                    Ok(got) if got as u64 == n => Ok(buf),
                    Ok(got) => Err(BodyError::new(400, format!("본문이 짧다 ({got}/{n} 바이트)"))),
                    Err(e) => Err(BodyError::new(
                        400,
                        format!("본문을 읽지 못했다: {}", describe_io_error(&e)),
                    )),
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
            self.io
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
                while self.io.read_line(&mut tail).is_ok() {
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
            self.io
                .read_exact(&mut out[start..])
                .map_err(|e| BodyError::new(400, format!("청크 데이터를 읽지 못했다: {e}")))?;
            // 데이터 뒤의 CRLF.
            let mut crlf = [0u8; 2];
            self.io
                .read_exact(&mut crlf)
                .map_err(|e| BodyError::new(400, format!("청크 끝을 읽지 못했다: {e}")))?;
        }
    }

    /// 남은 본문을 버린다. 응답만 하고 끝낼 때, 상대가 다 보내기 전에 닫으면
    /// 운영체제가 RST 를 보내 응답이 유실될 수 있어 조금은 읽어 준다.
    fn drain(&mut self, limit: u64) {
        let _ = self.io.get_ref().set_read_timeout(Some(Duration::from_millis(200)));
        let mut sink = std::io::sink();
        let _ = std::io::copy(&mut Read::take(&mut self.io, limit), &mut sink);
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
        self.io.get_mut().write_all(head.as_bytes())?;
        if !body.is_empty() {
            self.io.get_mut().write_all(body)?;
        }
        self.io.get_mut().flush()?;
        self.io.get_ref().shutdown_write();
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
            let _ = self.io.get_mut().write_all(head.as_bytes());
            let _ = self.io.get_mut().write_all(body);
            let _ = self.io.get_mut().flush();
            self.io.get_ref().shutdown_write();
        }
    }
}

/// 머리를 읽지 못한 소켓에 상태 코드만 적어 보낸다.
pub fn respond_raw(mut conn: Conn, status: u16, message: &str) {
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
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body.as_bytes());
    let _ = conn.flush();
    conn.shutdown_write();
}

/// CRLF 까지 한 줄. 남은 예산을 깎고, 다 쓰면 431. `deadline` 을 넘기면 408.
fn read_line(
    reader: &mut BufReader<Conn>,
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
            Err(e) => return Err(HeadError::new(400, format!("읽기 실패: {}", describe_io_error(&e)))),
        }
        if *budget == 0 {
            return Err(HeadError::new(
                431,
                format!("머리가 너무 크다 (상한 {MAX_HEADER_BYTES} 바이트)"),
            ));
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

/// 운영체제 소켓 오류를 사람이 읽을 수 있는 한국어로. 원본 메시지는 괄호에 남긴다.
///
/// 이유가 둘이다. 첫째, 같은 상황의 오류 번호가 플랫폼마다 다르다 — 주소가 이미 쓰이는 중이면
/// Linux 는 98, Windows 는 10048 이다. 둘째, **wine 에서는 원본 메시지가 사람이 못 읽는다**:
/// `FormatMessageW` 가 실패해 `OS Error 10048 (FormatMessageW() returned error 317)` 만 남는다.
/// 번호를 우리가 직접 풀어 주면 어느 쪽에서든 읽힌다.
///
/// 모르는 번호는 원본을 그대로 쓴다 — 틀린 설명을 붙이는 것보다 낫다.
pub fn describe_io_error(e: &std::io::Error) -> String {
    let Some(code) = e.raw_os_error() else {
        return e.to_string();
    };
    let why = match code {
        // ── Windows (winsock) ──
        10048 => Some("그 주소와 포트를 이미 다른 프로그램이 쓰고 있다"),
        10013 => Some("그 주소에 묶을 권한이 없다 (낮은 포트이거나 방화벽 정책)"),
        10060 => Some("상대가 제때 답하지 않았다 (시간 초과)"),
        10061 => Some("상대가 연결을 거부했다 (그 포트에서 듣는 프로그램이 없다)"),
        // ── Linux ──
        98 => Some("그 주소와 포트를 이미 다른 프로그램이 쓰고 있다"),
        13 => Some("그 주소에 묶을 권한이 없다 (1024 미만 포트는 관리자 권한이 필요하다)"),
        110 => Some("상대가 제때 답하지 않았다 (시간 초과)"),
        111 => Some("상대가 연결을 거부했다 (그 포트에서 듣는 프로그램이 없다)"),
        _ => None,
    };
    match why {
        Some(text) => format!("{text} ({e})"),
        None => e.to_string(),
    }
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
        assert_eq!(
            body_kind(&h(&[("content-length", "5")]), "POST").unwrap(),
            BodyKind::Sized(5)
        );
        assert_eq!(
            body_kind(&h(&[("content-length", "0")]), "POST").unwrap(),
            BodyKind::None
        );
        assert_eq!(
            body_kind(&h(&[("transfer-encoding", "chunked")]), "POST").unwrap(),
            BodyKind::Chunked
        );
        assert_eq!(body_kind(&h(&[]), "GET").unwrap(), BodyKind::None);

        // 본문이 있어야 할 메서드인데 길이가 없다 → 411.
        assert_eq!(body_kind(&h(&[]), "POST").unwrap_err().status, 411);
        assert_eq!(body_kind(&h(&[]), "PUT").unwrap_err().status, 411);
        // 수가 아닌 길이 → 400.
        assert_eq!(
            body_kind(&h(&[("content-length", "많이")]), "POST").unwrap_err().status,
            400
        );
        // 모르는 전송 인코딩 → 501.
        assert_eq!(
            body_kind(&h(&[("transfer-encoding", "gzip")]), "POST")
                .unwrap_err()
                .status,
            501
        );
    }

    /// 본문을 다 읽고 나면 `respond` 가 더 기다리지 않아야 한다.
    ///
    /// 예전에는 `read_body` 뒤에도 `BodyKind` 가 그대로라 `respond` 가 `drain` 으로 들어갔고,
    /// 상대는 더 보낼 것이 없는데 200ms 읽기 타임아웃을 꽉 채웠다. 요청마다 200ms 가 붙어
    /// 처리량이 초당 4건에 묶였다(30분 부하 점검에서 발견).
    #[test]
    fn a_fully_read_body_is_not_drained_again() {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_nodelay(true).ok();
            let body = b"[1,2,3]";
            let head = format!("POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n", body.len());
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(body).unwrap();
            // **쓰기 쪽을 닫지 않는다.** 진짜 클라이언트처럼 응답을 기다린다 —
            // 닫아 버리면 드레인이 EOF 로 곧장 끝나 이 시험이 의미가 없다.
            let start = Instant::now();
            let mut got = Vec::new();
            let _ = Read::read_to_end(&mut s, &mut got);
            (start.elapsed(), got)
        });

        let (stream, _) = listener.accept().expect("accept");
        let mut req = Request::read_head(Conn::Plain(stream))
            .map_err(|(_, e)| e)
            .expect("head");
        assert_eq!(req.read_body(1024).expect("body"), b"[1,2,3]");
        req.respond(200, "application/json", b"[]").expect("respond");

        let (elapsed, got) = client.join().expect("client");
        assert!(
            String::from_utf8_lossy(&got).starts_with("HTTP/1.1 200"),
            "{:?}",
            String::from_utf8_lossy(&got)
        );
        // 드레인이 남아 있으면 여기서 200ms 를 꽉 채운다. 넉넉히 잡아도 100ms 안에 끝나야 한다.
        assert!(
            elapsed < Duration::from_millis(100),
            "응답이 {elapsed:?} 걸렸다 — 본문을 또 기다린 것으로 보인다"
        );
    }

    /// 본문을 읽지 않고 답할 때는 여전히 조금 읽어 준다 — 안 그러면 RST 로 응답이 유실된다.
    #[test]
    fn an_unread_body_is_still_drained_before_answering() {
        use std::io::Write as _;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).expect("connect");
            s.set_nodelay(true).ok();
            let body = vec![b'a'; 4096];
            let head = format!("POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: {}\r\n\r\n", body.len());
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(&body).unwrap();
            let mut got = Vec::new();
            let _ = Read::read_to_end(&mut s, &mut got);
            got
        });

        let (stream, _) = listener.accept().expect("accept");
        let req = Request::read_head(Conn::Plain(stream))
            .map_err(|(_, e)| e)
            .expect("head");
        // 본문을 읽지 않고 곧바로 404 로 답한다 (경로가 틀렸을 때의 길).
        req.respond(404, "application/json", b"{}").expect("respond");

        let got = client.join().expect("client");
        assert!(
            String::from_utf8_lossy(&got).starts_with("HTTP/1.1 404"),
            "응답이 유실됐다: {:?}",
            String::from_utf8_lossy(&got)
        );
    }

    /// 코드 → 문구 매핑. 같은 상황의 번호가 플랫폼마다 달라서 둘 다 같은 말로 풀려야 한다.
    #[test]
    fn os_error_codes_map_to_the_same_korean_text() {
        use std::io::Error;
        let d = |code: i32| describe_io_error(&Error::from_raw_os_error(code));

        // 짝이 되는 번호끼리 같은 설명이어야 한다 (Windows, Linux).
        for (win, linux, needle) in [
            (10048, 98, "이미 다른 프로그램이"),
            (10013, 13, "권한이 없다"),
            (10060, 110, "시간 초과"),
            (10061, 111, "연결을 거부했다"),
        ] {
            assert!(d(win).contains(needle), "{win}: {}", d(win));
            assert!(d(linux).contains(needle), "{linux}: {}", d(linux));
        }

        // 원본 메시지를 괄호로 남긴다 — 번호를 잃으면 검색이 안 된다.
        let text = d(10048);
        assert!(text.contains("(") && text.contains(")"), "원본이 없다: {text}");
        assert!(text.contains("10048"), "번호가 사라졌다: {text}");

        // 모르는 번호는 원본 그대로. 틀린 설명을 붙이지 않는다.
        let unknown = Error::from_raw_os_error(999_999);
        assert_eq!(describe_io_error(&unknown), unknown.to_string());

        // OS 번호가 없는 오류(우리가 만든 것)도 그대로.
        let made = Error::other("우리가 만든 오류");
        assert_eq!(describe_io_error(&made), "우리가 만든 오류");
    }

    #[test]
    fn reasons_cover_the_codes_we_send() {
        for code in [200, 400, 401, 403, 404, 405, 408, 411, 413, 431, 500, 501, 503, 504] {
            assert_ne!(reason(code), "Unknown", "{code} 의 이유 문구가 없다");
        }
    }
}
