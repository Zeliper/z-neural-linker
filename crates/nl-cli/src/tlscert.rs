//! `nl tls-cert` — `HttpServer` 노드를 https 로 열 때 쓸 자체 서명 인증서를 만든다.
//!
//! **자체 서명이다.** 공개 인터넷에 내놓을 것이 아니라, 사내망이나 시험처럼 인증서를 받는 쪽에
//! 이 인증서를 직접 심어 줄 수 있는 자리에서 쓴다. 브라우저와 `curl` 은 기본으로 거부하므로
//! `curl --cacert` 나 `-k` 가 필요하다. 공개 서비스라면 Let's Encrypt 같은 데서 받은 인증서를
//! 같은 자리에 놓으면 된다 — `nl run --tls-cert/--tls-key` 는 어느 쪽이든 똑같이 받는다.

use crate::common::*;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Args<'a> {
    /// 인증서를 놓을 폴더. 그 아래 `certs/` 를 만든다.
    pub out_dir: &'a Path,
    /// 인증서에 넣을 이름들. DNS 이름과 IP 를 섞어 적을 수 있다.
    pub hosts: &'a str,
    pub days: u32,
    /// 이미 있는 파일을 덮어쓴다.
    pub force: bool,
}

/// 만들어 둔 파일 위치. 다른 명령이 경로를 알려 줄 때도 쓴다.
#[derive(Debug)]
pub struct Made {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// 프로젝트 폴더 기준 상대 경로 (`tls` 에 그대로 적는 값).
    pub cert_rel: String,
    pub key_rel: String,
}

pub const CERT_DIR: &str = "certs";
pub const CERT_FILE: &str = "server.crt";
pub const KEY_FILE: &str = "server.key";

pub fn run(args: Args<'_>) -> Result<i32> {
    let made = generate(args.out_dir, args.hosts, args.days, args.force)?;

    println!("{} 자체 서명 인증서를 만들었다", green("완료"));
    println!("  {} {}", dim("인증서"), made.cert.display());
    println!("  {} {}   {}", dim("개인키"), made.key.display(), dim("(0600)"));
    println!();
    println!("{}", bold("파이프라인에 붙이기"));
    println!(
        "  nl run 프로젝트.nlproj --tls-cert {} --tls-key {}",
        made.cert_rel, made.key_rel
    );
    println!("  {}", dim("프로젝트 파일은 바뀌지 않는다 — 이번 실행에만 붙는다."));
    println!();
    println!("{}", bold("불러 보기"));
    println!(
        "  curl --cacert {} -d '[0,1]' https://127.0.0.1:8787/infer",
        made.cert_rel
    );
    println!("  {}", dim("자체 서명이라 --cacert 없이는 거부된다 (시험이면 -k)."));
    println!();

    // 개인키가 저장소나 배포물로 새는 것이 이 기능에서 가장 흔한 사고다.
    println!("{}", yellow("개인키를 다루는 법"));
    println!("  · {}/ 를 .gitignore 에 넣어라:", CERT_DIR);
    println!("      {}", dim(&format!("echo '{CERT_DIR}/' >> .gitignore")));
    println!("  · 배포물(.nlapp)에 담지 마라 — 개인키가 든 번들은 그 자체가 유출이다.");
    println!(
        "    {}",
        dim("nl build 는 경로만 적고 인증서 파일은 넣지 않는다. 설치한 기계의 실행 파일 옆에 두어라.")
    );
    println!("  · 이 인증서는 자체 서명이다. 공개 서비스에는 제대로 발급받은 것을 써라.");
    Ok(0)
}

/// 인증서와 키를 만들어 `<out_dir>/certs/` 에 쓴다.
pub fn generate(out_dir: &Path, hosts: &str, days: u32, force: bool) -> Result<Made> {
    anyhow::ensure!(days > 0, "--days 는 1 이상이어야 한다");
    let names = parse_hosts(hosts)?;

    let dir = out_dir.join(CERT_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("폴더를 만들지 못했다: {}", dir.display()))?;
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if !force {
        for p in [&cert_path, &key_path] {
            anyhow::ensure!(
                !p.exists(),
                "{} 가 이미 있다. 덮어쓰려면 --force (옛 인증서를 쓰던 쪽이 붙지 못하게 된다)",
                p.display()
            );
        }
    }

    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).context("인증서 매개변수를 만들지 못했다")?;
    params.subject_alt_names = names.sans;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, names.common_name.clone());
    params.distinguished_name = dn;
    // 유효 기간. 시계가 조금 어긋난 기계에서도 바로 쓸 수 있게 하루 앞에서 시작한다.
    // 날짜 계산은 이미 의존성인 chrono 로 한다 — rcgen 은 `time` 의 타입을 다시 내보내지 않아서,
    // 직접 쓰려면 `time` 을 새 의존성으로 들여야 한다.
    // 반환 타입(`time::OffsetDateTime`)의 이름은 쓰지 않는다 — rcgen 이 그 타입을 다시
    // 내보내지 않아서, 이름을 쓰려면 `time` 을 새 의존성으로 들여야 한다. 호출만 하면 된다.
    let ymd = |d: chrono::NaiveDate| {
        use chrono::Datelike as _;
        let month = u8::try_from(d.month()).expect("달은 1..=12");
        let day = u8::try_from(d.day()).expect("날은 1..=31");
        rcgen::date_time_ymd(d.year(), month, day)
    };
    let today = chrono::Utc::now().date_naive();
    params.not_before = ymd(today - chrono::Duration::days(1));
    params.not_after = ymd(today + chrono::Duration::days(i64::from(days)));

    let key = rcgen::KeyPair::generate().context("키를 만들지 못했다")?;
    let cert = params.self_signed(&key).context("자체 서명에 실패했다")?;

    write_private(&key_path, key.serialize_pem().as_bytes())
        .with_context(|| format!("개인키를 쓰지 못했다: {}", key_path.display()))?;
    std::fs::write(&cert_path, cert.pem()).with_context(|| format!("인증서를 쓰지 못했다: {}", cert_path.display()))?;

    Ok(Made {
        cert: cert_path,
        key: key_path,
        cert_rel: format!("{CERT_DIR}/{CERT_FILE}"),
        key_rel: format!("{CERT_DIR}/{KEY_FILE}"),
    })
}

struct Names {
    sans: Vec<rcgen::SanType>,
    common_name: String,
}

/// `localhost,127.0.0.1,api.example.com` 을 SAN 목록으로. IP 처럼 생긴 것은 IP SAN 으로 넣는다.
///
/// 요즘 클라이언트는 CN 을 보지 않고 SAN 만 본다. IP 로 붙는데 DNS SAN 만 있으면 이름 검증에서 떨어진다.
fn parse_hosts(hosts: &str) -> Result<Names> {
    let mut sans = Vec::new();
    let mut common_name = None;
    for raw in hosts.split(',') {
        let h = raw.trim();
        if h.is_empty() {
            continue;
        }
        if common_name.is_none() {
            common_name = Some(h.to_owned());
        }
        match h.parse::<std::net::IpAddr>() {
            Ok(ip) => sans.push(rcgen::SanType::IpAddress(ip)),
            Err(_) => sans.push(rcgen::SanType::DnsName(
                h.to_string()
                    .try_into()
                    .map_err(|e| anyhow::anyhow!("이름으로 쓸 수 없다: {h} ({e})"))?,
            )),
        }
    }
    anyhow::ensure!(!sans.is_empty(), "--hosts 가 비어 있다");
    Ok(Names {
        sans,
        common_name: common_name.expect("비어 있지 않다"),
    })
}

/// 개인키는 소유자만 읽게 쓴다. 먼저 만들고 권한을 주는 것이 아니라, **권한을 준 채로 만든다** —
/// 그 사이에 다른 사용자가 열어 볼 틈을 두지 않으려는 것이다.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.flush()
}

/// Windows 에는 유닉스 권한 비트가 없다. ACL 을 건드리는 대신, 파일이 사용자 폴더 안에 있다는
/// 전제에 기댄다 — 이 차이를 `nl tls-cert` 출력이 알리지는 않으므로 문서에 적어 둔다.
#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("nl-cli-tls-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn it_writes_a_usable_pair() {
        let dir = tmp("pair");
        let made = generate(&dir, "localhost,127.0.0.1", 30, false).expect("만들기");
        let cert = std::fs::read(&made.cert).unwrap();
        let key = std::fs::read(&made.key).unwrap();
        // nl-io 가 받아 주는 것이 곧 "쓸 수 있다" 는 뜻이다.
        nl_io::httpd::TlsAcceptor::from_pem(&cert, &key).expect("nl-io 가 거부했다");
        assert_eq!(made.cert_rel, "certs/server.crt");
        assert_eq!(made.key_rel, "certs/server.key");
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("mode");
        let made = generate(&dir, "localhost", 30, false).expect("만들기");
        let mode = std::fs::metadata(&made.key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "개인키 권한이 {mode:o} 다");
        // 인증서는 공개해도 되는 것이라 굳이 조이지 않는다.
        assert!(made.cert.exists());
    }

    #[test]
    fn it_refuses_to_overwrite_without_force() {
        let dir = tmp("overwrite");
        generate(&dir, "localhost", 30, false).expect("첫 번째");
        let err = generate(&dir, "localhost", 30, false).expect_err("덮어썼다");
        assert!(err.to_string().contains("--force"), "{err}");
        generate(&dir, "localhost", 30, true).expect("--force 면 덮어써야 한다");
    }

    #[test]
    fn ip_hosts_become_ip_sans() {
        let names = parse_hosts("example.com, 10.0.0.5 ,::1").expect("해석");
        assert_eq!(names.common_name, "example.com");
        let ips = names
            .sans
            .iter()
            .filter(|s| matches!(s, rcgen::SanType::IpAddress(_)))
            .count();
        assert_eq!(ips, 2, "IP 두 개가 IP SAN 이어야 한다");
    }

    #[test]
    fn junk_arguments_are_errors_not_panics() {
        let dir = tmp("junk");
        assert!(generate(&dir, "", 30, false).is_err(), "빈 hosts 가 통과했다");
        assert!(generate(&dir, "localhost", 0, false).is_err(), "days 0 이 통과했다");
    }
}
