//! 로컬 HTTP 서버(tiny_http)로 매니페스트와 자산을 서빙하며 확인·다운로드·검증 경로를 돈다.
//! 바깥 네트워크는 쓰지 않는다.

use nl_update::{check, check_signed, download, sha256_hex, Asset, AssetKind, Progress, State, Updater};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ───────────────────────────── 시험용 서버 ─────────────────────────────

#[derive(Clone)]
enum Route {
    Body(Vec<u8>),
    /// 본문 안의 `{BASE}` 를 서버 자신의 `http://호스트:포트` 로 바꿔 응답한다.
    /// 매니페스트가 같은 서버의 자산을 가리켜야 해서 필요하다.
    BodyWithBase(String),
    /// 응답하기 전에 이만큼 잔다 — 타임아웃 시험용.
    Slow(Duration, Vec<u8>),
}

struct TestServer {
    server: Arc<tiny_http::Server>,
    addr: String,
    handle: Option<JoinHandle<()>>,
    /// 느린 응답을 자고 있는 핸들러를 깨워 끝내기 위한 깃발.
    stop: Arc<AtomicBool>,
}

impl TestServer {
    fn start(routes: Vec<(&'static str, Route)>) -> Self {
        let routes: HashMap<String, Route> = routes.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("서버를 열지 못했습니다"));
        let addr = server.server_addr().to_ip().expect("IP 주소").to_string();

        let worker = server.clone();
        let base = format!("http://{addr}");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            for req in worker.incoming_requests() {
                let path = req.url().split(['?', '#']).next().unwrap_or("").to_string();
                match routes.get(&path) {
                    Some(Route::Body(body)) => {
                        let _ = req.respond(tiny_http::Response::from_data(body.clone()));
                    }
                    Some(Route::BodyWithBase(template)) => {
                        let body = template.replace("{BASE}", &base);
                        let _ = req.respond(tiny_http::Response::from_data(body.into_bytes()));
                    }
                    Some(Route::Slow(delay, body)) => {
                        // 잘게 쪼개 자면서 종료 깃발을 본다 — 안 그러면 테스트 정리가 delay 만큼 걸린다.
                        let deadline = std::time::Instant::now() + *delay;
                        while std::time::Instant::now() < deadline && !worker_stop.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        let _ = req.respond(tiny_http::Response::from_data(body.clone()));
                    }
                    None => {
                        let _ = req.respond(tiny_http::Response::empty(404));
                    }
                }
            }
        });
        Self { server, addr, handle: Some(handle), stop }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
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

// ───────────────────────────── 도우미 ─────────────────────────────

const ASSET_BODY: &[u8] = "\u{7f}ELF 새 버전 실행 파일 바이트".as_bytes();

fn manifest_json(version: &str, asset_url: &str, sha: &str) -> String {
    let target = nl_update::target_key();
    format!(
        r#"{{"version":"{version}","notes":"고친 것","assets":{{"{target}":{{"url":"{asset_url}","sha256":"{sha}","kind":"binary","size":{}}}}}}}"#,
        ASSET_BODY.len()
    )
}

fn v(s: &str) -> semver::Version {
    semver::Version::parse(s).unwrap()
}

// ───────────────────────────── 확인 ─────────────────────────────

#[test]
fn check_finds_a_newer_version() {
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::BodyWithBase(manifest_json("0.2.0", "{BASE}/app", &sha256_hex(ASSET_BODY))),
    )]);

    let found = check(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap();
    let found = found.expect("새 버전을 찾아야 합니다");
    assert_eq!(found.version, v("0.2.0"));
    assert_eq!(found.notes, "고친 것");
    assert_eq!(found.target, nl_update::target_key());
    assert_eq!(found.asset.kind, AssetKind::Binary);
    assert_eq!(found.asset.size as usize, ASSET_BODY.len());
}

#[test]
fn check_says_nothing_when_up_to_date() {
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::Body(manifest_json("0.1.0", "http://x/app", &sha256_hex(ASSET_BODY)).into_bytes()),
    )]);
    let found = check(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap();
    assert!(found.is_none(), "같은 버전이면 새 것이 없다");

    // 더 낮은 버전도 마찬가지.
    let found = check(&server.url("/latest.json"), &v("0.9.0"), Duration::from_secs(5)).unwrap();
    assert!(found.is_none());
}

#[test]
fn missing_manifest_is_an_error() {
    let server = TestServer::start(vec![]);
    let err = check(&server.url("/없음.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap_err().to_string();
    assert!(err.contains("매니페스트"), "{err}");
}

#[test]
fn broken_manifest_is_an_error() {
    let server = TestServer::start(vec![("/latest.json", Route::Body(b"{ not json".to_vec()))]);
    let err = check(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap_err().to_string();
    assert!(err.contains("형식"), "{err}");
}

#[test]
fn a_slow_server_hits_the_timeout_instead_of_hanging() {
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::Slow(Duration::from_secs(30), manifest_json("0.2.0", "http://x/a", "ab").into_bytes()),
    )]);

    let started = std::time::Instant::now();
    let result = check(&server.url("/latest.json"), &v("0.1.0"), Duration::from_millis(400));
    let elapsed = started.elapsed();

    assert!(result.is_err(), "타임아웃은 오류여야 합니다");
    assert!(elapsed < Duration::from_secs(10), "타임아웃이 걸리지 않았습니다: {elapsed:?}");
}

// ───────────────────────────── 다운로드 ─────────────────────────────

#[test]
fn download_writes_the_file_and_reports_progress() {
    let server = TestServer::start(vec![("/app", Route::Body(ASSET_BODY.to_vec()))]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, rx) = crossbeam_channel::unbounded::<Progress>();

    let asset = Asset {
        url: server.url("/app"),
        sha256: sha256_hex(ASSET_BODY).to_uppercase(), // 대문자도 받아야 한다
        kind: AssetKind::Binary,
        size: ASSET_BODY.len() as u64,
    };
    let path = download(&asset, dir.path(), &tx).unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), ASSET_BODY);
    assert_eq!(path.file_name().unwrap(), "app");
    assert!(!path.with_extension("part").exists(), ".part 파일이 남았습니다");

    drop(tx);
    let events: Vec<Progress> = rx.into_iter().collect();
    assert!(!events.is_empty(), "진행률 이벤트가 없습니다");
    let last = events.last().unwrap();
    assert_eq!(last.received, ASSET_BODY.len() as u64);
    assert_eq!(last.fraction(), Some(1.0));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0o111);
    }
}

#[test]
fn checksum_mismatch_is_rejected_and_leaves_nothing_behind() {
    let server = TestServer::start(vec![("/app", Route::Body("엉뚱한 내용".as_bytes().to_vec()))]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = crossbeam_channel::unbounded();

    let asset = Asset {
        url: server.url("/app"),
        sha256: sha256_hex(ASSET_BODY),
        kind: AssetKind::Binary,
        size: 0,
    };
    let err = download(&asset, dir.path(), &tx).unwrap_err().to_string();
    assert!(err.contains("체크섬"), "{err}");

    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "실패한 다운로드가 남았습니다: {left:?}");
}

#[test]
fn missing_asset_is_a_checksum_failure_not_a_partial_file() {
    let server = TestServer::start(vec![]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = crossbeam_channel::unbounded();

    let asset =
        Asset { url: server.url("/없음"), sha256: sha256_hex(ASSET_BODY), kind: AssetKind::Binary, size: 0 };
    assert!(download(&asset, dir.path(), &tx).is_err());
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "{left:?}");
}

// ───────────────────────────── 서명 ─────────────────────────────

/// minisign-verify 문서의 시험 벡터. 서명이 덮는 내용은 `test` 라 매니페스트로는 읽히지 않는다 —
/// 서명 검증이 파싱보다 먼저 일어난다는 것을 이 차이로 확인한다.
const VECTOR_PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
const VECTOR_SIG: &str = "untrusted comment: signature from minisign secret key
RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
const VECTOR_BODY: &[u8] = b"test";

#[test]
fn a_valid_signature_lets_the_manifest_through_to_parsing() {
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(VECTOR_BODY.to_vec())),
        ("/latest.json.minisig", Route::Body(VECTOR_SIG.as_bytes().to_vec())),
    ]);

    let err = check_signed(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5), Some(VECTOR_PUBKEY))
        .unwrap_err()
        .to_string();
    // 서명은 통과했고 그 다음 단계인 JSON 파싱에서 멈췄다.
    assert!(err.contains("형식"), "서명 검증을 통과하지 못했습니다: {err}");
    assert!(!err.contains("서명"), "{err}");
}

#[test]
fn a_tampered_manifest_is_rejected_before_parsing() {
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(b"Test".to_vec())), // 대문자 T — 서명과 어긋난다
        ("/latest.json.minisig", Route::Body(VECTOR_SIG.as_bytes().to_vec())),
    ]);

    let err = check_signed(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5), Some(VECTOR_PUBKEY))
        .unwrap_err()
        .to_string();
    assert!(err.contains("서명"), "{err}");
}

#[test]
fn a_missing_signature_file_fails_when_a_key_is_configured() {
    let server = TestServer::start(vec![("/latest.json", Route::Body(VECTOR_BODY.to_vec()))]);
    let err = check_signed(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5), Some(VECTOR_PUBKEY))
        .unwrap_err()
        .to_string();
    assert!(err.contains("서명"), "{err}");
}

#[test]
fn without_a_key_the_signature_is_not_even_fetched() {
    // `.minisig` 라우트가 없어도 확인이 성공한다.
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::BodyWithBase(manifest_json("0.2.0", "{BASE}/app", &sha256_hex(ASSET_BODY))),
    )]);
    let found = check_signed(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5), None).unwrap();
    assert!(found.is_some());
}

// ───────────────────────────── 상태 기계 전체 ─────────────────────────────

#[test]
fn updater_walks_from_check_to_downloaded_against_a_real_server() {
    let server = TestServer::start(vec![
        (
            "/latest.json",
            Route::BodyWithBase(manifest_json("9.9.9", "{BASE}/app", &sha256_hex(ASSET_BODY))),
        ),
        ("/app", Route::Body(ASSET_BODY.to_vec())),
    ]);

    let dir = tempfile::tempdir().unwrap();
    let mut updater = Updater::new(server.url("/latest.json"), v("0.1.0")).with_timeout(Duration::from_secs(5));
    assert_eq!(*updater.state(), State::Idle);

    updater.check();
    wait_until(&mut updater, |s| matches!(s, State::Available(_) | State::Failed(_)));
    let available = updater.available().expect("새 버전을 찾아야 합니다").clone();
    assert_eq!(available.version, v("9.9.9"));

    updater.download(dir.path().to_path_buf());
    wait_until(&mut updater, |s| matches!(s, State::Downloaded { .. } | State::Failed(_)));

    let path = updater.downloaded().expect("내려받은 파일").to_path_buf();
    assert_eq!(std::fs::read(&path).unwrap(), ASSET_BODY);
    assert!(matches!(updater.state(), State::Downloaded { kind: AssetKind::Binary, .. }));
    assert!(!updater.is_busy());
}

#[test]
fn updater_reports_a_failure_instead_of_hanging() {
    let server = TestServer::start(vec![]);
    let mut updater = Updater::new(server.url("/없음.json"), v("0.1.0")).with_timeout(Duration::from_secs(3));
    updater.check();
    wait_until(&mut updater, |s| matches!(s, State::Failed(_) | State::UpToDate | State::Available(_)));
    assert!(matches!(updater.state(), State::Failed(_)), "{:?}", updater.state());
    assert!(updater.state().message().starts_with("실패"));
}

#[test]
fn events_from_poll_mirror_the_state() {
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::Body(manifest_json("0.1.0", "http://x/a", &sha256_hex(ASSET_BODY)).into_bytes()),
    )]);
    let mut updater = Updater::new(server.url("/latest.json"), v("0.1.0")).with_timeout(Duration::from_secs(5));
    updater.check();

    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        seen.extend(updater.poll().into_iter().map(|e| format!("{e:?}")));
        if !updater.is_busy() && !seen.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.iter().any(|e| e.starts_with("Checking")), "{seen:?}");
    assert!(seen.iter().any(|e| e.starts_with("UpToDate")), "{seen:?}");
    assert_eq!(*updater.state(), State::UpToDate);
    // 작업이 끝난 뒤의 poll 은 빈 목록.
    assert!(updater.poll().is_empty());
}

/// 상태가 조건을 만족할 때까지 `poll` 을 돌린다.
fn wait_until(updater: &mut Updater, done: impl Fn(&State) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        updater.poll();
        if done(updater.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("시간 안에 상태가 바뀌지 않았습니다: {:?}", updater.state());
}
