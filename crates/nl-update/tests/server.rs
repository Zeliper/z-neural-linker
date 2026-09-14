//! 로컬 HTTP 서버(tiny_http)로 매니페스트와 자산을 서빙하며 확인·다운로드·검증 경로를 돈다.
//! 바깥 네트워크는 쓰지 않는다.

use nl_update::{check_signed, download, sha256_hex, Asset, AssetKind, Progress, State, Updater};
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
    /// 라우트를 만들면서 같이 나온 값(서명 공개키 따위).
    extra: String,
}

impl TestServer {
    fn start(routes: Vec<(&'static str, Route)>) -> Self {
        Self::start_late(|_| {
            (
                routes.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                String::new(),
            )
        })
    }

    /// 서버를 먼저 열고 그 주소(`http://호스트:포트`)를 넘겨 라우트를 만든다.
    /// 본문에 서버 주소가 들어가야 서명할 수 있는 경우에 쓴다 — `{BASE}` 치환은 서명을 깨뜨린다.
    fn start_late(make: impl FnOnce(&str) -> (Vec<(String, Route)>, String)) -> Self {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("서버를 열지 못했습니다"));
        let addr = server.server_addr().to_ip().expect("IP 주소").to_string();
        let base = format!("http://{addr}");

        let (routes, extra) = make(&base);
        let routes: HashMap<String, Route> = routes.into_iter().collect();

        let worker = server.clone();
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
        Self {
            server,
            addr,
            handle: Some(handle),
            stop,
            extra,
        }
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
    manifest_json_at(version, asset_url, sha, &now_rfc3339())
}

fn manifest_json_at(version: &str, asset_url: &str, sha: &str, published_at: &str) -> String {
    let target = nl_update::target_key();
    format!(
        r#"{{"version":"{version}","notes":"고친 것","published_at":"{published_at}","assets":{{"{target}":{{"url":"{asset_url}","sha256":"{sha}","kind":"binary","size":{}}}}}}}"#,
        ASSET_BODY.len()
    )
}

/// 지금 시각을 RFC 3339 로. 매니페스트가 신선해야 통과한다.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("1970 이후")
        .as_secs() as i64;
    // days_from_civil 의 역연산 — 시험 안에서만 쓰는 최소 구현.
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn v(s: &str) -> semver::Version {
    semver::Version::parse(s).unwrap()
}

/// 이 시험 서버는 평문 http 다. 라이브러리는 https 만 받으므로 루프백 탈출구를 켠다.
///
/// 환경 변수는 프로세스 전역이라 한 번만 세우고, 모든 시험이 **맨 처음** 이것을 부른다.
/// `Once` 가 첫 호출이 끝날 때까지 나머지를 막으므로 다른 스레드가 세우는 도중에 읽는 일은 없다.
fn allow_loopback_http() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var(nl_update::ALLOW_HTTP_ENV, "1"));
}

/// 서명 없이 매니페스트만 받아 비교한다. 라이브러리에는 이런 진입점이 더 이상 없고
/// (키 없는 확인은 막았다) 받기·파싱·비교 각 단계를 따로 시험하려고 여기서만 이어 붙인다.
fn check_unsigned(
    url: &str,
    current: &semver::Version,
    timeout: Duration,
) -> anyhow::Result<Option<nl_update::Available>> {
    let raw = nl_update::fetch_manifest(url, timeout)?;
    let manifest = nl_update::Manifest::parse(&raw)?;
    Ok(manifest.newer_for(current, &nl_update::target_key()))
}

/// 진짜 minisign 키 한 쌍을 만들어 `body` 에 서명한다. 반환값은 `(공개키 base64, .minisig 본문)`.
fn sign_for_test(body: &[u8]) -> (String, String) {
    let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("키 생성");
    let sig = minisign::sign(Some(&pair.pk), &pair.sk, body, None, None).expect("서명");
    (pair.pk.to_base64(), sig.into_string())
}

// ───────────────────────────── 확인 ─────────────────────────────

#[test]
fn check_finds_a_newer_version() {
    allow_loopback_http();
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::BodyWithBase(manifest_json("0.2.0", "{BASE}/app", &sha256_hex(ASSET_BODY))),
    )]);

    let found = check_unsigned(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap();
    let found = found.expect("새 버전을 찾아야 합니다");
    assert_eq!(found.version, v("0.2.0"));
    assert_eq!(found.notes, "고친 것");
    assert_eq!(found.target, nl_update::target_key());
    assert_eq!(found.asset.kind, AssetKind::Binary);
    assert_eq!(found.asset.size as usize, ASSET_BODY.len());
}

#[test]
fn check_says_nothing_when_up_to_date() {
    allow_loopback_http();
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::Body(manifest_json("0.1.0", "http://x/app", &sha256_hex(ASSET_BODY)).into_bytes()),
    )]);
    let found = check_unsigned(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5)).unwrap();
    assert!(found.is_none(), "같은 버전이면 새 것이 없다");

    // 더 낮은 버전도 마찬가지.
    let found = check_unsigned(&server.url("/latest.json"), &v("0.9.0"), Duration::from_secs(5)).unwrap();
    assert!(found.is_none());
}

#[test]
fn missing_manifest_is_an_error() {
    allow_loopback_http();
    let server = TestServer::start(vec![]);
    let err = check_unsigned(&server.url("/없음.json"), &v("0.1.0"), Duration::from_secs(5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("매니페스트"), "{err}");
}

#[test]
fn broken_manifest_is_an_error() {
    allow_loopback_http();
    let server = TestServer::start(vec![("/latest.json", Route::Body(b"{ not json".to_vec()))]);
    let err = check_unsigned(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("형식"), "{err}");
}

#[test]
fn a_slow_server_hits_the_timeout_instead_of_hanging() {
    allow_loopback_http();
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::Slow(
            Duration::from_secs(30),
            manifest_json("0.2.0", "http://x/a", "ab").into_bytes(),
        ),
    )]);

    let started = std::time::Instant::now();
    let result = check_unsigned(&server.url("/latest.json"), &v("0.1.0"), Duration::from_millis(400));
    let elapsed = started.elapsed();

    assert!(result.is_err(), "타임아웃은 오류여야 합니다");
    assert!(
        elapsed < Duration::from_secs(10),
        "타임아웃이 걸리지 않았습니다: {elapsed:?}"
    );
}

// ───────────────────────────── 다운로드 ─────────────────────────────

#[test]
fn download_writes_the_file_and_reports_progress() {
    allow_loopback_http();
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
        // 0700 — 적용 전 바이너리를 같은 호스트의 다른 사용자가 실행할 수 없어야 한다.
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
    }
}

#[test]
fn checksum_mismatch_is_rejected_and_leaves_nothing_behind() {
    allow_loopback_http();
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

    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "실패한 다운로드가 남았습니다: {left:?}");
}

#[test]
fn missing_asset_is_a_checksum_failure_not_a_partial_file() {
    allow_loopback_http();
    let server = TestServer::start(vec![]);
    let dir = tempfile::tempdir().unwrap();
    let (tx, _rx) = crossbeam_channel::unbounded();

    let asset = Asset {
        url: server.url("/없음"),
        sha256: sha256_hex(ASSET_BODY),
        kind: AssetKind::Binary,
        size: 0,
    };
    assert!(download(&asset, dir.path(), &tx).is_err());
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

// ───────────────────────────── 서명 ─────────────────────────────

/// 형식은 맞지만 아무것도 서명하지 않은 키. "키가 설정돼 있다" 만 나타내면 되는 자리에 쓴다.
const DUMMY_PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
/// 서명이 덮는 내용. 매니페스트로는 읽히지 않아서, 서명 검증이 파싱보다 먼저라는 것을 이 차이로 확인한다.
const SIGNED_BODY: &[u8] = b"not json";

#[test]
fn a_valid_signature_lets_the_manifest_through_to_parsing() {
    allow_loopback_http();
    let (pubkey, sig) = sign_for_test(SIGNED_BODY);
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(SIGNED_BODY.to_vec())),
        ("/latest.json.minisig", Route::Body(sig.into_bytes())),
    ]);

    let err = check_signed(
        &server.url("/latest.json"),
        &v("0.1.0"),
        Duration::from_secs(5),
        &pubkey,
    )
    .unwrap_err()
    .to_string();
    // 서명은 통과했고 그 다음 단계인 JSON 파싱에서 멈췄다.
    assert!(err.contains("형식"), "서명 검증을 통과하지 못했습니다: {err}");
    assert!(!err.contains("서명"), "{err}");
}

#[test]
fn a_tampered_manifest_is_rejected_before_parsing() {
    allow_loopback_http();
    let (pubkey, sig) = sign_for_test(SIGNED_BODY);
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(b"Not json".to_vec())), // 대문자 N — 서명과 어긋난다
        ("/latest.json.minisig", Route::Body(sig.into_bytes())),
    ]);

    let err = check_signed(
        &server.url("/latest.json"),
        &v("0.1.0"),
        Duration::from_secs(5),
        &pubkey,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("서명"), "{err}");
}

#[test]
fn a_missing_signature_file_fails_when_a_key_is_configured() {
    allow_loopback_http();
    let server = TestServer::start(vec![("/latest.json", Route::Body(SIGNED_BODY.to_vec()))]);
    let err = check_signed(
        &server.url("/latest.json"),
        &v("0.1.0"),
        Duration::from_secs(5),
        DUMMY_PUBKEY,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("서명"), "{err}");
}

// ───────────────────────────── 재생 · 오리진 ─────────────────────────────

#[test]
fn a_stale_but_correctly_signed_manifest_is_refused() {
    allow_loopback_http();
    let server = TestServer::start_late(|base| {
        // 서명은 진짜다. 발행 시각만 오래됐다 — 정품 옛 매니페스트를 다시 들려주는 공격.
        let body = manifest_json_at(
            "9.9.9",
            &format!("{base}/app"),
            &sha256_hex(ASSET_BODY),
            "2020-01-01T00:00:00Z",
        );
        let (pubkey, sig) = sign_for_test(body.as_bytes());
        (
            vec![
                ("/latest.json".to_string(), Route::Body(body.into_bytes())),
                ("/latest.json.minisig".to_string(), Route::Body(sig.into_bytes())),
            ],
            pubkey,
        )
    });
    let err = format!(
        "{:#}",
        check_signed(
            &server.url("/latest.json"),
            &v("0.1.0"),
            Duration::from_secs(5),
            &server.extra
        )
        .unwrap_err()
    );
    assert!(err.contains("너무 오래됐습니다"), "{err}");
}

#[test]
fn a_manifest_without_a_published_at_is_refused() {
    allow_loopback_http();
    let target = nl_update::target_key();
    let body = format!(
        r#"{{"version":"9.9.9","assets":{{"{target}":{{"url":"https://h/app","sha256":"ab","kind":"binary"}}}}}}"#
    );
    let (pubkey, sig) = sign_for_test(body.as_bytes());
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(body.into_bytes())),
        ("/latest.json.minisig", Route::Body(sig.into_bytes())),
    ]);
    let err = format!(
        "{:#}",
        check_signed(
            &server.url("/latest.json"),
            &v("0.1.0"),
            Duration::from_secs(5),
            &pubkey
        )
        .unwrap_err()
    );
    assert!(err.contains("published_at"), "{err}");
}

#[test]
fn an_asset_on_a_foreign_host_is_refused() {
    allow_loopback_http();
    // 서명은 진짜지만 자산이 내부망 주소를 가리킨다 — 응답 여부로 내부망을 훑는 통로.
    let body = manifest_json("9.9.9", "https://192.168.0.1/app", &sha256_hex(ASSET_BODY));
    let (pubkey, sig) = sign_for_test(body.as_bytes());
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(body.into_bytes())),
        ("/latest.json.minisig", Route::Body(sig.into_bytes())),
    ]);
    let err = format!(
        "{:#}",
        check_signed(
            &server.url("/latest.json"),
            &v("0.1.0"),
            Duration::from_secs(5),
            &pubkey
        )
        .unwrap_err()
    );
    assert!(err.contains("allowed_asset_hosts"), "{err}");
}

#[test]
fn an_empty_key_is_refused_rather_than_skipping_verification() {
    allow_loopback_http();
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::BodyWithBase(manifest_json("0.2.0", "{BASE}/app", &sha256_hex(ASSET_BODY))),
    )]);
    // 빈 키로 "검증 생략" 이 되면 안 된다 — 오류여야 한다.
    let err = check_signed(&server.url("/latest.json"), &v("0.1.0"), Duration::from_secs(5), "")
        .unwrap_err()
        .to_string();
    assert!(err.contains("공개키") || err.contains("서명"), "{err}");
}

#[test]
fn a_real_signature_carries_the_manifest_all_the_way_through() {
    allow_loopback_http();
    let server = TestServer::start_late(|base| {
        let body = manifest_json("9.9.9", &format!("{base}/app"), &sha256_hex(ASSET_BODY));
        let (pubkey, sig) = sign_for_test(body.as_bytes());
        (
            vec![
                ("/latest.json".to_string(), Route::Body(body.into_bytes())),
                ("/latest.json.minisig".to_string(), Route::Body(sig.into_bytes())),
            ],
            pubkey,
        )
    });

    let found = check_signed(
        &server.url("/latest.json"),
        &v("0.1.0"),
        Duration::from_secs(5),
        &server.extra,
    )
    .unwrap()
    .expect("새 버전");
    assert_eq!(found.version, v("9.9.9"));
    assert_eq!(found.asset.kind, AssetKind::Binary);
}

// ───────────────────────────── fail-closed ─────────────────────────────

#[test]
fn without_a_public_key_the_updater_is_disabled_and_never_checks() {
    allow_loopback_http();
    let server = TestServer::start(vec![(
        "/latest.json",
        Route::BodyWithBase(manifest_json("9.9.9", "{BASE}/app", &sha256_hex(ASSET_BODY))),
    )]);

    let mut updater = Updater::new(server.url("/latest.json"), v("0.1.0"));
    assert!(updater.state().is_disabled(), "{:?}", updater.state());

    updater.check();
    // 확인 스레드조차 뜨지 않는다 — 상태가 그대로다.
    assert!(updater.state().is_disabled(), "{:?}", updater.state());
    assert!(!updater.is_busy());
    assert!(updater.poll().is_empty());
    assert!(updater.available().is_none());
}

#[test]
fn a_plain_http_url_disables_the_updater_even_with_a_key() {
    allow_loopback_http(); // 탈출구가 켜져 있어도 루프백이 아니면 막혀야 한다
    let updater = Updater::new("http://example.com/latest.json", v("0.1.0")).with_public_key(Some(DUMMY_PUBKEY));
    assert!(updater.state().is_disabled(), "{:?}", updater.state());
    assert!(
        updater.state().message().contains("http"),
        "{}",
        updater.state().message()
    );
}

#[test]
fn a_key_turns_the_updater_on_and_removing_the_url_turns_it_back_off() {
    allow_loopback_http();
    let mut updater = Updater::new("https://example.com/latest.json", v("0.1.0")).with_public_key(Some(DUMMY_PUBKEY));
    assert_eq!(*updater.state(), State::Idle);

    updater.set_manifest_url("ftp://example.com/latest.json");
    assert!(updater.state().is_disabled(), "{:?}", updater.state());
}

// ───────────────────────────── 상태 기계 전체 ─────────────────────────────

#[test]
fn updater_walks_from_check_to_downloaded_against_a_real_server() {
    allow_loopback_http();
    // 매니페스트에 서버 주소가 들어가야 하므로 서버를 먼저 열고, 그 주소로 본문을 만든 뒤 서명한다.
    // 그래서 라우트를 나중에 채울 수 있는 서버가 필요하다.
    let server = TestServer::start_late(|base| {
        let body = manifest_json("9.9.9", &format!("{base}/app"), &sha256_hex(ASSET_BODY));
        let (pubkey, sig) = sign_for_test(body.as_bytes());
        let routes = vec![
            ("/latest.json".to_string(), Route::Body(body.into_bytes())),
            ("/latest.json.minisig".to_string(), Route::Body(sig.into_bytes())),
            ("/app".to_string(), Route::Body(ASSET_BODY.to_vec())),
        ];
        (routes, pubkey)
    });
    let pubkey = server.extra.clone();

    let dir = tempfile::tempdir().unwrap();
    let mut updater = Updater::new(server.url("/latest.json"), v("0.1.0"))
        .with_public_key(Some(pubkey))
        .with_timeout(Duration::from_secs(5));
    assert_eq!(*updater.state(), State::Idle);

    updater.check();
    wait_until(&mut updater, |s| matches!(s, State::Available(_) | State::Failed(_)));
    let available = updater.available().expect("새 버전을 찾아야 합니다").clone();
    assert_eq!(available.version, v("9.9.9"));

    updater.download(dir.path().to_path_buf());
    wait_until(&mut updater, |s| {
        matches!(s, State::Downloaded { .. } | State::Failed(_))
    });

    let path = updater.downloaded().expect("내려받은 파일").to_path_buf();
    assert_eq!(std::fs::read(&path).unwrap(), ASSET_BODY);
    assert!(matches!(
        updater.state(),
        State::Downloaded {
            kind: AssetKind::Binary,
            ..
        }
    ));
    assert!(!updater.is_busy());
}

#[test]
fn updater_reports_a_failure_instead_of_hanging() {
    allow_loopback_http();
    let server = TestServer::start(vec![]);
    let mut updater = Updater::new(server.url("/없음.json"), v("0.1.0"))
        .with_public_key(Some(DUMMY_PUBKEY))
        .with_timeout(Duration::from_secs(3));
    updater.check();
    wait_until(&mut updater, |s| {
        matches!(s, State::Failed(_) | State::UpToDate | State::Available(_))
    });
    assert!(matches!(updater.state(), State::Failed(_)), "{:?}", updater.state());
    assert!(updater.state().message().starts_with("실패"));
}

#[test]
fn events_from_poll_mirror_the_state() {
    allow_loopback_http();
    // 최신이라 자산까지 가지 않는다 — 자산 주소는 아무 https 나 괜찮다.
    let body = manifest_json("0.1.0", "https://example.invalid/a", &sha256_hex(ASSET_BODY));
    let (pubkey, sig) = sign_for_test(body.as_bytes());
    let server = TestServer::start(vec![
        ("/latest.json", Route::Body(body.into_bytes())),
        ("/latest.json.minisig", Route::Body(sig.into_bytes())),
    ]);
    let mut updater = Updater::new(server.url("/latest.json"), v("0.1.0"))
        .with_public_key(Some(pubkey))
        .with_timeout(Duration::from_secs(5));
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
