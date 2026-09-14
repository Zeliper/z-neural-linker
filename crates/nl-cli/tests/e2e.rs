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
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).expect("워크스페이스 루트").to_path_buf()
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

/// 명령을 돌리고 실패하면 양쪽 출력을 붙여 패닉한다.
fn run(label: &str, cmd: &mut Command) -> Output {
    let out = cmd.env("NO_COLOR", "1").output().unwrap_or_else(|e| panic!("{label} 을 실행하지 못했다: {e}"));
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
        if std::net::TcpStream::connect_timeout(
            &addr.parse().expect("주소 형식"),
            Duration::from_millis(200),
        )
        .is_ok()
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

    let dir = std::env::temp_dir().join(format!("nl-e2e-{}-{:?}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    std::fs::create_dir_all(&dir).expect("임시 폴더");
    let proj = dir.join("xor.nlproj");

    // ── 1. 샘플 ──
    let out = run("nl sample", Command::new(NL).arg("sample").arg(&proj));
    assert!(proj.is_file(), "샘플 파일이 만들어지지 않았다: {}", stdout(&out));

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
    assert!(saved.contains("final.safetensors"), "학습 뒤 가중치 경로가 프로젝트에 없다");

    // ── 3. 추론 ──
    let out = run(
        "nl infer",
        Command::new(NL).args(["infer"]).arg(&proj).args(["--model", "XOR MLP", "--input", "[0.8,-0.8]"]),
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
            .args(["--name", "종단 데모", "--version", "1.0.0", "--pipeline", "추론 API", "--runtime"])
            .arg(&runtime),
    );
    let text = stdout(&out);
    assert!(text.contains("추론 API"), "진입 파이프라인이 잡히지 않았다:\n{text}");

    let tarball = std::fs::read_dir(&dist)
        .expect("dist 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().ends_with("-linux-x86_64.tar.gz"))
        .expect("tar.gz 산출물이 없다");
    assert!(std::fs::metadata(&tarball).unwrap().len() > 1_000_000, "아카이브가 너무 작다");

    // ── 5. 배포판을 풀어서 실제로 띄운다 ──
    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked).unwrap();
    let tar = run(
        "tar 풀기",
        Command::new("tar").arg("xzf").arg(&tarball).arg("-C").arg(&unpacked),
    );
    let _ = tar;
    let app = std::fs::read_dir(&unpacked)
        .expect("푼 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("앱 폴더가 없다");
    let exe = std::fs::read_dir(&app)
        .expect("앱 폴더")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_file() && p.extension().is_none())
        .expect("실행 파일이 없다");
    eprintln!("배포판 실행 파일: {}", exe.display());

    // 5a. 짧게 띄워 종료 코드 0 을 본다.
    let out = run(
        "배포판 헤드리스",
        Command::new(&exe).args(["--headless", "--run-for", "2", "--device", "cpu"]),
    );
    let text = stdout(&out);
    assert!(text.contains("종단 데모 1.0.0"), "앱 이름이 안 보인다:\n{text}");
    assert!(text.contains("HTTP 서버"), "HTTP 서버가 열리지 않았다:\n{text}");
    assert!(!text.contains("마우스·키보드를 실제로 조작"), "무장하지 않았는데 무장 문구가 있다:\n{text}");

    // 5b. 다시 띄워 두고 실제로 추론을 요청한다.
    let mut child = Command::new(&exe)
        .args(["--headless", "--run-for", "20", "--device", "cpu"])
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("배포판을 띄우지 못했다");

    let served = wait_until_serving(nl_core::sample::API_BIND, Duration::from_secs(15));
    let result = if served {
        let url = format!("http://{}{}", nl_core::sample::API_BIND, nl_core::sample::API_PATH);
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

    assert!(served, "배포판이 {} 에서 듣지 않았다", nl_core::sample::API_BIND);
    let res = result.expect("배포판이 추론 요청에 답하지 않았다");
    assert_eq!(res.status, 200, "본문: {}", res.body);
    let body: serde_json::Value = serde_json::from_str(&res.body).expect("응답이 JSON 이 아니다");
    let out_logits = body.as_array().expect("2 클래스 로짓 배열이어야 한다");
    assert_eq!(out_logits.len(), 2, "배포판 응답이 2개가 아니다: {body}");
    eprintln!("배포판 HTTP 추론 결과: {body}");

    std::fs::remove_dir_all(&dir).ok();
}
