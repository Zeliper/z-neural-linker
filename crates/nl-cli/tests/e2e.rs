//! 종단 테스트: `nl sample → train → infer → build` → 만들어진 배포판을 실제로 띄워 HTTP 로 추론까지.
//!
//! 무겁고(학습·링크·20MB 아카이브) 런타임 바이너리를 먼저 빌드해야 해서 **`NL_E2E=1` 일 때만** 돈다.
//! 평소 `cargo test` 에서는 건너뛴다는 줄만 찍고 통과한다.
//!
//! ```sh
//! cargo build --release -p nl-runtime
//! NL_E2E=1 cargo test -p nl-cli --test e2e -- --nocapture
//! ```
//!
//! 런타임은 이 순서로 찾는다: `NL_RUNTIME` 환경 변수 → `CARGO_BIN_EXE_nl-runtime`(같은 워크스페이스를
//! 함께 테스트할 때 카고가 넣어 준다) → `target/release/nl-runtime` → `target/debug/nl-runtime`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// `nl` 실행 파일. 카고가 이 테스트를 위해 만들어 준다.
const NL: &str = env!("CARGO_BIN_EXE_nl");

fn enabled() -> bool {
    std::env::var("NL_E2E").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// 워크스페이스 루트(`crates/nl-cli` 의 두 단계 위).
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("워크스페이스 루트")
        .to_path_buf()
}

/// 배포판에 붙일 런타임 실행 파일.
fn find_runtime() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("NL_RUNTIME") {
        candidates.push(PathBuf::from(p));
    }
    if let Some(p) = option_env!("CARGO_BIN_EXE_nl-runtime") {
        candidates.push(PathBuf::from(p));
    }
    let root = workspace_root();
    candidates.push(root.join("target/release/nl-runtime"));
    candidates.push(root.join("target/debug/nl-runtime"));
    candidates.into_iter().find(|p| p.is_file())
}

/// 이 실행에만 쓰는 임시 폴더.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("nl-e2e-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("임시 폴더");
    dir
}

/// `dist` 의 tar.gz 를 풀어 배포판 실행 파일 경로를 돌려준다.
fn unpack_app(dist: &Path, dir: &Path) -> PathBuf {
    let tarball = std::fs::read_dir(dist)
        .expect("dist 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with("-linux-x86_64.tar.gz"))
        .expect("tar.gz 산출물이 없다");
    assert!(
        std::fs::metadata(&tarball).unwrap().len() > 1_000_000,
        "아카이브가 너무 작다"
    );

    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked).unwrap();
    run(
        "tar 풀기",
        Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&unpacked),
    );

    let app = std::fs::read_dir(&unpacked)
        .expect("푼 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("앱 폴더가 없다");
    std::fs::read_dir(&app)
        .expect("앱 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file() && p.extension().is_none())
        .expect("실행 파일이 없다")
}

/// 명령을 돌리고 실패하면 양쪽 출력을 붙여 패닉한다.
fn run(label: &str, cmd: &mut Command) -> Output {
    let out = cmd
        .env("NO_COLOR", "1")
        .output()
        .unwrap_or_else(|e| panic!("{label} 을 실행하지 못했다: {e}"));
    assert!(
        out.status.success(),
        "{label} 실패 (종료 코드 {:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// 비어 있는 TCP 포트를 잡아 열려 있는지 확인하는 데 쓴다.
fn wait_until_serving(addr: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect_timeout(&addr.parse().expect("주소 형식"), Duration::from_millis(200)).is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn sample_train_infer_build_and_run_the_deployed_app() {
    if !enabled() {
        eprintln!("NL_E2E 가 없어 종단 테스트를 건너뛴다 (켜려면 NL_E2E=1)");
        return;
    }
    let Some(runtime) = find_runtime() else {
        panic!(
            "런타임 실행 파일을 찾지 못했다. `cargo build --release -p nl-runtime` 을 먼저 돌리거나 \
             NL_RUNTIME 으로 경로를 주어라"
        );
    };
    eprintln!("런타임: {}", runtime.display());

    let dir = temp_dir("xor");
    let proj = dir.join("xor.nlproj");

    // ── 1. 샘플 ──
    let out = run("nl sample", Command::new(NL).arg("sample").arg(&proj));
    assert!(proj.is_file(), "샘플 파일이 만들어지지 않았다: {}", stdout(&out));

    // 샘플의 고정 포트(8799)는 옆에서 도는 다른 시험·다른 세션과 부딪힌다. 빈 포트로 옮긴다.
    let bind = rebind_http_server(&proj, nl_core::sample::API_BIND);
    eprintln!("이 시험의 추론 API 주소: {bind}");

    // ── 2. 학습 (짧게) ──
    let out = run(
        "nl train",
        Command::new(NL)
            .args(["train"])
            .arg(&proj)
            .args(["--model", "XOR MLP", "--device", "cpu", "--epochs", "3"]),
    );
    let text = stdout(&out);
    assert!(text.contains("상태"), "학습 요약이 없다:\n{text}");
    assert!(text.contains("가중치"), "가중치 줄이 없다:\n{text}");

    // 프로젝트에 가중치 경로가 적혔다.
    let saved = std::fs::read_to_string(&proj).expect("프로젝트 파일");
    assert!(
        saved.contains("final.safetensors"),
        "학습 뒤 가중치 경로가 프로젝트에 없다"
    );

    // ── 3. 추론 ──
    let out = run(
        "nl infer",
        Command::new(NL)
            .args(["infer"])
            .arg(&proj)
            .args(["--model", "XOR MLP", "--input", "[0.8,-0.8]"]),
    );
    let json: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("추론 결과가 JSON 이 아니다");
    let logits = json.as_array().expect("2 클래스 로짓 배열이어야 한다");
    assert_eq!(logits.len(), 2, "출력이 2개가 아니다: {json}");
    assert!(logits.iter().all(|v| v.is_number()), "로짓이 수가 아니다: {json}");

    // ── 4. 빌드 ──
    let dist = dir.join("dist");
    let out = run(
        "nl build",
        Command::new(NL)
            .args(["build"])
            .arg(&proj)
            .args(["--target", "linux", "--out"])
            .arg(&dist)
            .args([
                "--name",
                "종단 데모",
                "--version",
                "1.0.0",
                "--pipeline",
                "추론 API",
                "--runtime",
            ])
            .arg(&runtime),
    );
    let text = stdout(&out);
    assert!(text.contains("추론 API"), "진입 파이프라인이 잡히지 않았다:\n{text}");

    // ── 5. 배포판을 풀어서 실제로 띄운다 ──
    let exe = unpack_app(&dist, &dir);
    eprintln!("배포판 실행 파일: {}", exe.display());

    // 5a. 짧게 띄워 종료 코드 0 을 본다.
    let out = run(
        "배포판 헤드리스",
        Command::new(&exe).args(["--headless", "--run-for", "2", "--device", "cpu"]),
    );
    let text = stdout(&out);
    assert!(text.contains("종단 데모 1.0.0"), "앱 이름이 안 보인다:\n{text}");
    assert!(text.contains("HTTP 서버"), "HTTP 서버가 열리지 않았다:\n{text}");
    assert!(
        !text.contains("마우스·키보드를 실제로 조작"),
        "무장하지 않았는데 무장 문구가 있다:\n{text}"
    );

    // 5b. 다시 띄워 두고 실제로 추론을 요청한다.
    let mut child = Command::new(&exe)
        .args(["--headless", "--run-for", "20", "--device", "cpu"])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("배포판을 띄우지 못했다");

    let served = wait_until_serving(&bind, Duration::from_secs(15));
    let result = if served {
        let url = format!("http://{bind}{}", nl_core::sample::API_PATH);
        // 모델이 올라올 때까지 503 이 나올 수 있다 — 200 이 될 때까지 잠깐 다시 시도한다.
        let mut last = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(15) {
            match nl_io::http::call(
                "POST",
                &url,
                &std::collections::BTreeMap::new(),
                Some("[0.8,-0.8]"),
                Duration::from_secs(5),
            ) {
                Ok(r) if r.status == 200 => {
                    last = Some(r);
                    break;
                }
                Ok(r) => last = Some(r),
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        last
    } else {
        None
    };

    let _ = child.kill();
    let _ = child.wait();

    assert!(served, "배포판이 {bind} 에서 듣지 않았다");
    let res = result.expect("배포판이 추론 요청에 답하지 않았다");
    assert_eq!(res.status, 200, "본문: {}", res.body);
    let body: serde_json::Value = serde_json::from_str(&res.body).expect("응답이 JSON 이 아니다");
    let out_logits = body.as_array().expect("2 클래스 로짓 배열이어야 한다");
    assert_eq!(out_logits.len(), 2, "배포판 응답이 2개가 아니다: {body}");
    eprintln!("배포판 HTTP 추론 결과: {body}");

    std::fs::remove_dir_all(&dir).ok();
}

/// TLS 시나리오: `nl tls-cert` 로 인증서를 만들고 `nl run --tls-cert/--tls-key` 로 띄워
/// **진짜 https** 로 한 바퀴 돈다. 배포판이 아니라 `nl run` 을 쓰는 이유는, 여기서 보려는 것이
/// "인증서 만들기 → 주입 → 핸드셰이크" 사슬이지 번들 경로가 아니기 때문이다.
#[test]
fn tls_cert_then_run_serves_https() {
    if !enabled() {
        eprintln!("NL_E2E 가 없어 TLS 종단 테스트를 건너뛴다 (켜려면 NL_E2E=1)");
        return;
    }
    if which_curl().is_none() {
        panic!("curl 이 없어 https 왕복을 확인할 수 없다 — curl 을 설치하라");
    }

    let dir = temp_dir("tls");
    let proj = dir.join("xor.nlproj");

    // ── 1. 샘플 + 짧은 학습 (모델이 있어야 답한다) ──
    run("nl sample", Command::new(NL).arg("sample").arg(&proj));

    // 샘플의 고정 포트(8799)를 그대로 쓰면 다른 종단 시험과 **동시에** 돌 때 서로 포트를 뺏는다.
    // 실제로 그렇게 깨졌다 — 여기서만 빈 포트로 바꿔 둔다.
    let addr = rebind_http_server(&proj, nl_core::sample::API_BIND);
    eprintln!("이 시험의 https 주소: {addr}");
    run(
        "nl train",
        Command::new(NL)
            .args(["train"])
            .arg(&proj)
            .args(["--model", "XOR MLP", "--device", "cpu", "--epochs", "3"]),
    );

    // ── 2. 인증서 만들기 ──
    let out = run(
        "nl tls-cert",
        Command::new(NL)
            .arg("tls-cert")
            .arg(&dir)
            .args(["--hosts", "localhost,127.0.0.1", "--days", "30"]),
    );
    let text = stdout(&out);
    assert!(text.contains("certs/server.crt"), "인증서 경로 안내가 없다:\n{text}");
    assert!(text.contains(".gitignore"), ".gitignore 안내가 없다:\n{text}");
    assert!(text.contains("배포물"), "배포물 경고가 없다:\n{text}");
    let cert = dir.join("certs/server.crt");
    let key = dir.join("certs/server.key");
    assert!(cert.is_file() && key.is_file(), "인증서 파일이 없다");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "개인키 권한이 {mode:o} 다");
    }

    // ── 3. https 로 띄운다 ──
    let mut child = Command::new(NL)
        .args(["run"])
        .arg(&proj)
        .args([
            "--pipeline",
            "추론 API",
            "--for",
            "25",
            "--device",
            "cpu",
            "--tls-cert",
            "certs/server.crt",
            "--tls-key",
            "certs/server.key",
        ])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("nl run 을 띄우지 못했다");

    let served = wait_until_serving(&addr, Duration::from_secs(20));

    // ── 4. curl -k 로 https 요청 ──
    let result = if served {
        let url = format!("https://{addr}{}", nl_core::sample::API_PATH);
        let mut last: Option<(String, String)> = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            let out = Command::new("curl")
                .args(["-sS", "-k", "-o", "-", "-w", "\n%{http_code}", "-X", "POST"])
                .args(["-H", "Content-Type: application/json", "-d", "[0.8,-0.8]"])
                .arg(&url)
                .output();
            if let Ok(o) = out {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
                last = Some((body.to_string(), code.trim().to_string()));
                if code.trim() == "200" {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        last
    } else {
        None
    };

    // ── 5. 평문으로 같은 포트를 두드리면 답하지 않는다 ──
    let plaintext = if served {
        Command::new("curl")
            .args(["-sS", "--max-time", "5", "-o", "-", "-w", "%{http_code}", "-d", "[0,1]"])
            .arg(format!("http://{addr}{}", nl_core::sample::API_PATH))
            .output()
            .ok()
    } else {
        None
    };

    let _ = child.kill();
    let _ = child.wait();

    assert!(served, "nl run 이 {addr} 에서 듣지 않았다");
    let (body, code) = result.expect("https 요청에 답하지 않았다");
    assert_eq!(code, "200", "본문: {body}");
    let json: serde_json::Value = serde_json::from_str(&body).expect("응답이 JSON 이 아니다");
    let logits = json.as_array().expect("2 클래스 로짓 배열이어야 한다");
    assert_eq!(logits.len(), 2, "응답이 2개가 아니다: {json}");
    eprintln!("https 추론 결과: {json}");

    if let Some(o) = plaintext {
        let text = String::from_utf8_lossy(&o.stdout);
        assert!(
            !text.trim().ends_with("200"),
            "평문 요청이 200 을 받았다 — TLS 포트가 평문에도 답한다: {text}"
        );
        eprintln!("평문 요청은 거부됐다 (curl 종료 {:?})", o.status.code());
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// 샘플의 고정 포트를 빈 포트로 바꾸고 새 주소를 돌려준다.
///
/// 샘플은 문서에 적어 둘 수 있도록 고정 포트를 쓴다(XOR 8799, CNN 8800). 시험은 그 고정 포트를
/// **절대 그대로 쓰지 않는다.** 같은 기계에서 다른 시험·다른 세션이 같은 포트를 잡으면 서로를
/// 막기 때문이다. 실제로 그렇게 깨졌다 — 옆 세션의 부하 시험이 8799 를 쥔 채였다.
///
/// 운영체제에 빈 포트를 물어본 뒤 곧바로 놓아 주므로 그 사이에 남이 채 갈 틈이 이론상 있지만,
/// 높은 임의 포트라 실제로 부딪히지 않는다.
///
///   let addr = rebind_http_server(&proj, nl_core::sample::API_BIND);
fn rebind_http_server(proj: &Path, from: &str) -> String {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("빈 포트");
        l.local_addr().expect("주소").port()
    };
    let addr = format!("127.0.0.1:{port}");
    let text = std::fs::read_to_string(proj).expect("프로젝트 파일");
    let replaced = text.replace(from, &addr);
    assert_ne!(replaced, text, "샘플에 {from} 가 없다");
    std::fs::write(proj, replaced).expect("프로젝트 파일 쓰기");
    addr
}

/// 다입력·다출력 시나리오: Input 2개(`[2]`,`[3]`) · Output 2개(`a`,`b`) 인 모델을
/// **테스트가 직접 만든 프로젝트**로 학습·빌드해 배포판에 두 형식으로 POST 한다.
///
/// 샘플에는 넣지 않는다 — 샘플은 처음 쓰는 사람이 여는 것이라 단순해야 한다.
#[test]
fn a_multi_input_multi_output_model_answers_through_the_deployed_app() {
    if !enabled() {
        eprintln!("NL_E2E 가 없어 다입출력 종단 테스트를 건너뛴다 (켜려면 NL_E2E=1)");
        return;
    }
    let Some(runtime) = find_runtime() else {
        panic!("런타임 실행 파일을 찾지 못했다. `cargo build --release -p nl-runtime` 을 먼저 돌려라");
    };

    let dir = temp_dir("multi");
    let proj = dir.join("multi.nlproj");
    let bind = {
        // 다른 종단 시험과 포트를 다투지 않게 빈 포트를 받아 둔다.
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("빈 포트");
        format!("127.0.0.1:{}", l.local_addr().expect("주소").port())
    };
    write_multi_project(&proj, &dir, &bind);

    // ── 1. 검사: 순서 계약이 표로 보이는지 ──
    let out = run("nl inspect", Command::new(NL).arg("inspect").arg(&proj));
    let text = stdout(&out);
    assert!(text.contains("입출력 순서"), "순서 표가 없다:\n{text}");
    assert!(text.contains("x1") && text.contains("x2"), "입력 순서가 없다:\n{text}");
    assert!(
        text.contains("벡터[2]") && text.contains("벡터[3]"),
        "필드 종류가 없다:\n{text}"
    );
    assert!(
        text.contains("첫 출력") || text.contains("손실"),
        "다출력 학습 규칙 안내가 없다:\n{text}"
    );

    // ── 2. 학습 (3 에포크) ──
    let out = run(
        "nl train",
        Command::new(NL)
            .args(["train"])
            .arg(&proj)
            .args(["--model", "두 갈래", "--device", "cpu", "--epochs", "3"]),
    );
    let text = stdout(&out);
    assert!(text.contains("가중치"), "학습이 가중치를 남기지 않았다:\n{text}");

    // ── 3. 빌드 ──
    let dist = dir.join("dist");
    run(
        "nl build",
        Command::new(NL)
            .args(["build"])
            .arg(&proj)
            .args(["--target", "linux", "--out"])
            .arg(&dist)
            .args([
                "--name",
                "다입출력 데모",
                "--version",
                "1.0.0",
                "--pipeline",
                "추론",
                "--runtime",
            ])
            .arg(&runtime),
    );

    // ── 4. 배포판에 두 형식으로 POST ──
    let exe = unpack_app(&dist, &dir);
    let mut child = Command::new(&exe)
        .args(["--headless", "--run-for", "30", "--device", "cpu"])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("배포판을 띄우지 못했다");

    let served = wait_until_serving(&bind, Duration::from_secs(20));
    let url = format!("http://{bind}/infer");
    let mut object_reply = None;
    let mut array_reply = None;
    if served {
        // 객체 형식: 키가 곧 입력 필드 이름이다.
        object_reply = post_json_until_ok(&url, br#"{"x1":[0.5,-0.5],"x2":[1.0,0.0,-1.0]}"#);
        // 배열 형식: 입력 필드 **순서**대로 채운다.
        array_reply = post_json_until_ok(&url, br#"[[0.5,-0.5],[1.0,0.0,-1.0]]"#);
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(served, "배포판이 {bind} 에서 듣지 않았다");
    for (label, reply) in [("객체", object_reply), ("배열", array_reply)] {
        let r = reply.unwrap_or_else(|| panic!("{label} 형식이 답을 받지 못했다"));
        assert_eq!(r.status, 200, "{label}: {}", r.body);
        let v: serde_json::Value = serde_json::from_str(&r.body).expect("JSON 이 아니다");
        let obj = v
            .as_object()
            .unwrap_or_else(|| panic!("{label} 응답이 객체가 아니다: {v}"));
        assert_eq!(obj.len(), 2, "{label}: 키가 둘이어야 한다 — {v}");
        let a = obj.get("a").unwrap_or_else(|| panic!("{label}: a 키가 없다 — {v}"));
        let b = obj.get("b").unwrap_or_else(|| panic!("{label}: b 키가 없다 — {v}"));
        assert_eq!(a.as_array().expect("a 는 배열").len(), 2, "{label}: a 길이");
        assert_eq!(b.as_array().expect("b 는 배열").len(), 3, "{label}: b 길이");
        eprintln!("{label} 형식 응답: {v}");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// 모델이 올라올 때까지 503 을 넘기며 다시 보낸다.
fn post_json_until_ok(url: &str, body: &[u8]) -> Option<Reply> {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    let mut last = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        match post_bytes(url, &headers, body) {
            Ok(r) if r.status == 200 => return Some(r),
            Ok(r) => last = Some(r),
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    last
}

/// Input 2개·Output 2개짜리 프로젝트와 그 학습용 CSV 를 만든다.
///
/// **이름이 순서를 정한다**: `Graph::input_nodes()`/`output_nodes()` 가 노드 이름순으로 정렬하고,
/// 페이로드 필드도 같은 순서여야 한다. 그래서 노드에 `x1`·`x2`·`a`·`b` 를 준다.
fn write_multi_project(path: &Path, dir: &Path, bind: &str) {
    use nl_core::dataset::{DataSource, DatasetSpec};
    use nl_core::model::{Node, Port};
    use nl_core::payload::{Field, FieldKind, PayloadSpec};
    use nl_core::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
    use nl_core::train::Loss;
    use nl_core::{Act, LayerKind, ModelDef, Project, ProjectFile};

    // CSV: x1 두 열 + x2 세 열 = 입력 다섯 열, 타깃 두 열.
    // 엔진은 `input_cols` 를 Input 노드 순서대로 2개·3개씩 앞에서부터 잘라 넣는다.
    let csv = dir.join("train.csv");
    let mut text = String::from("i1,i2,i3,i4,i5,t1,t2\n");
    for n in 0..200 {
        let f = n as f32;
        let (a, b, c, d, e) = (f * 0.01, f * -0.02, f * 0.03, f * 0.005, f * -0.01);
        // 타깃은 입력에서 결정되는 값이라 3 에포크로도 손실이 내려간다.
        let (t1, t2) = (a + c, b + e);
        text.push_str(&format!("{a},{b},{c},{d},{e},{t1},{t2}\n"));
    }
    std::fs::write(&csv, text).expect("CSV 쓰기");

    let mut payload = PayloadSpec::new("둘입출력");
    payload.inputs.push(Field::new("x1", FieldKind::Vector { len: 2 }));
    payload.inputs.push(Field::new("x2", FieldKind::Vector { len: 3 }));
    payload.outputs.push(Field::new("a", FieldKind::Vector { len: 2 }));
    payload.outputs.push(Field::new("b", FieldKind::Vector { len: 3 }));

    let mut dataset = DatasetSpec::new(
        "다입력 CSV",
        DataSource::Csv {
            path: "train.csv".into(),
            input_cols: ["i1", "i2", "i3", "i4", "i5"].iter().map(|s| s.to_string()).collect(),
            target_cols: ["t1", "t2"].iter().map(|s| s.to_string()).collect(),
            header: true,
        },
    );
    dataset.payload = None;

    let mut def = ModelDef::new("두 갈래");
    let named = |kind: LayerKind, name: &str, pos: [f32; 2]| {
        let mut n = Node::new(kind, pos);
        n.name = name.into();
        n
    };
    let in1 = def
        .graph
        .add_node(named(LayerKind::Input { shape: vec![2] }, "x1", [0.0, -1.0]));
    let in2 = def
        .graph
        .add_node(named(LayerKind::Input { shape: vec![3] }, "x2", [0.0, 1.0]));
    let l1 = def.graph.add_node(Node::new(
        LayerKind::Linear {
            out_features: 4,
            bias: true,
        },
        [1.0, -1.0],
    ));
    let l2 = def.graph.add_node(Node::new(
        LayerKind::Linear {
            out_features: 4,
            bias: true,
        },
        [1.0, 1.0],
    ));
    let cat = def.graph.add_node(Node::new(LayerKind::Concat { dim: 0 }, [2.0, 0.0]));
    let act = def
        .graph
        .add_node(Node::new(LayerKind::Activation { act: Act::Relu }, [3.0, 0.0]));
    let head_a = def.graph.add_node(Node::new(
        LayerKind::Linear {
            out_features: 2,
            bias: true,
        },
        [4.0, -1.0],
    ));
    let head_b = def.graph.add_node(Node::new(
        LayerKind::Linear {
            out_features: 3,
            bias: true,
        },
        [4.0, 1.0],
    ));
    let out_a = def.graph.add_node(named(LayerKind::Output, "a", [5.0, -1.0]));
    let out_b = def.graph.add_node(named(LayerKind::Output, "b", [5.0, 1.0]));

    for (from, to, slot) in [
        (in1, l1, 0),
        (in2, l2, 0),
        (l1, cat, 0),
        (l2, cat, 1),
        (cat, act, 0),
        (act, head_a, 0),
        (head_a, out_a, 0),
        (act, head_b, 0),
        (head_b, out_b, 0),
    ] {
        def.graph.add_edge(from, Port::new(to, slot)).expect("연결");
    }
    def.payload = Some(payload.id);
    // 회귀라 MSE. 기본값인 CrossEntropy 는 타깃을 클래스 인덱스로 본다.
    def.train.loss = Loss::Mse;
    def.train.dataset = Some(dataset.id);
    def.train.epochs = 3;
    def.train.batch_size = 32;

    let mut pl = Pipeline::new("추론");
    pl.tick_hz = 60.0;
    let server = pl.add_node(PNode::new(
        PNodeKind::Source {
            source: Source::HttpServer {
                bind: bind.to_string(),
                path: "/infer".into(),
                token: None,
                tls: None,
            },
        },
        [0.0, 0.0],
    ));
    let model_node = pl.add_node(PNode::new(
        PNodeKind::Model {
            model: def.id,
            payload: Some(payload.id),
        },
        [1.0, 0.0],
    ));
    let reply = pl.add_node(PNode::new(
        PNodeKind::Sink {
            sink: Sink::HttpReply { server },
        },
        [2.0, 0.0],
    ));
    pl.add_link(server, model_node).unwrap();
    pl.add_link(model_node, reply).unwrap();

    let mut project = Project::new("다입출력");
    project.payloads.insert(payload.id, payload);
    project.datasets.insert(dataset.id, dataset);
    project.models.insert(def.id, def);
    project.pipelines.insert(pl.id, pl);

    std::fs::write(path, ProjectFile::new(project).to_json()).expect("프로젝트 쓰기");
}

fn which_curl() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("curl"))
        .find(|p| p.is_file())
}

/// 이미지 모델 시나리오: 샘플 → 학습 → 빌드 → 배포판에 PNG 를 POST 해서 분류 결과를 받는다.
///
/// XOR 쪽이 숫자 벡터를 다룬다면 이쪽은 **이진 이미지 본문**과 `MapLabel` 디코드 체인을 확인한다.
#[test]
fn the_cnn_sample_classifies_a_png_through_the_deployed_app() {
    if !enabled() {
        eprintln!("NL_E2E 가 없어 CNN 종단 테스트를 건너뛴다 (켜려면 NL_E2E=1)");
        return;
    }
    let Some(runtime) = find_runtime() else {
        panic!("런타임 실행 파일을 찾지 못했다. `cargo build --release -p nl-runtime` 을 먼저 돌려라");
    };

    let dir = temp_dir("cnn");
    let proj = dir.join("cnn.nlproj");
    let out = run(
        "nl sample --kind cnn",
        Command::new(NL).args(["sample"]).arg(&proj).args(["--kind", "cnn"]),
    );
    assert!(proj.is_file(), "CNN 샘플 파일이 만들어지지 않았다: {}", stdout(&out));
    assert!(
        stdout(&out).contains("사분면 CNN"),
        "모델 이름이 안 보인다:\n{}",
        stdout(&out)
    );

    // 샘플의 고정 포트(8800)는 옆에서 도는 다른 시험·다른 세션과 부딪힌다. 빈 포트로 옮긴다.
    let bind = rebind_http_server(&proj, nl_core::sample::API_BIND_CNN);
    eprintln!("이 시험의 추론 API 주소: {bind}");

    // ── 학습 (짧게) ──
    let out = run(
        "nl train (cnn)",
        Command::new(NL)
            .args(["train"])
            .arg(&proj)
            .args(["--model", "사분면 CNN", "--device", "cpu", "--epochs", "3"]),
    );
    assert!(
        stdout(&out).contains("가중치"),
        "학습 결과에 가중치가 없다:\n{}",
        stdout(&out)
    );

    // ── 빌드 ──
    let dist = dir.join("dist");
    run(
        "nl build (cnn)",
        Command::new(NL)
            .args(["build"])
            .arg(&proj)
            .args(["--target", "linux", "--out"])
            .arg(&dist)
            .args([
                "--name",
                "사분면 데모",
                "--version",
                "1.0.0",
                "--pipeline",
                "추론 API",
                "--runtime",
            ])
            .arg(&runtime),
    );
    let exe = unpack_app(&dist, &dir);
    eprintln!("배포판 실행 파일: {}", exe.display());

    // ── 띄우고 PNG 를 보낸다 ──
    let mut child = Command::new(&exe)
        .args(["--headless", "--run-for", "8", "--device", "cpu"])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("배포판을 띄우지 못했다");

    let served = wait_until_serving(&bind, Duration::from_secs(15));
    let url = format!("http://{bind}{}", nl_core::sample::API_PATH);
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("Content-Type".to_string(), "image/png".to_string());

    // 8×8 원본 크기와, 리샘플이 필요한 32×32 를 둘 다 보낸다.
    let mut answers = Vec::new();
    if served {
        for (label, w, h) in [("8x8", 8u32, 8u32), ("32x32", 32, 32)] {
            let png = quadrant_png(w, h, 3); // 우하 사분면을 밝게
            let mut last = None;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(15) {
                match post_bytes(&url, &headers, &png) {
                    Ok(r) if r.status == 200 => {
                        last = Some(r);
                        break;
                    }
                    Ok(r) => last = Some(r),
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            answers.push((label, last));
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(served, "배포판이 {bind} 에서 듣지 않았다");
    for (label, answer) in answers {
        let res = answer.unwrap_or_else(|| panic!("{label}: 배포판이 답하지 않았다"));
        assert_eq!(res.status, 200, "{label} 본문: {}", res.body);
        let body: serde_json::Value =
            serde_json::from_str(&res.body).unwrap_or_else(|e| panic!("{label}: JSON 이 아니다 ({e}): {}", res.body));
        // 페이로드의 decode 체인이 `MapLabel` 이면 라벨 문자열이, 아니면 인덱스가 온다.
        let ok = match &body {
            serde_json::Value::String(s) => ["좌상", "우상", "좌하", "우하"].contains(&s.as_str()),
            serde_json::Value::Number(n) => n.as_f64().is_some_and(|v| (0.0..4.0).contains(&v)),
            _ => false,
        };
        assert!(ok, "{label}: 클래스 라벨도 인덱스도 아니다: {body}");
        eprintln!("CNN 배포판 응답 ({label}): {body}");
    }

    std::fs::remove_dir_all(&dir).ok();
}

/// 한 사분면만 밝은 회색 PNG. `quadrant` 는 0=좌상, 1=우상, 2=좌하, 3=우하.
///
/// 합성 데이터(`SyntheticKind::Quadrants`)가 만드는 그림과 같은 모양이다 — 어두운 바탕에 밝은 사각형.
fn quadrant_png(w: u32, h: u32, quadrant: u32) -> Vec<u8> {
    let (ox, oy) = ((quadrant % 2) * w / 2, (quadrant / 2) * h / 2);
    let img = image::GrayImage::from_fn(w, h, |x, y| {
        let inside = x >= ox + w / 8 && x < ox + w / 2 - w / 8 && y >= oy + h / 8 && y < oy + h / 2 - h / 8;
        image::Luma([if inside { 255u8 } else { 0 }])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("PNG 인코딩");
    out.into_inner()
}

/// 이진 본문을 POST 한다. `nl_io::http::call` 은 텍스트 본문만 다뤄 소켓으로 직접 보낸다.
fn post_bytes(url: &str, headers: &std::collections::BTreeMap<String, String>, body: &[u8]) -> std::io::Result<Reply> {
    use std::io::{Read, Write};
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (hostport, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));

    let mut sock = std::net::TcpStream::connect(hostport)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    sock.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    sock.write_all(head.as_bytes())?;
    sock.write_all(body)?;
    sock.flush()?;

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("응답 머리와 몸을 가를 수 없다"))?;
    let head_text = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head_text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("상태 코드를 읽을 수 없다"))?;
    Ok(Reply {
        status,
        body: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
    })
}

struct Reply {
    status: u16,
    body: String,
}
