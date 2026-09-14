//! 업데이트가 다루는 주소의 규칙. **평문 http 는 받지 않는다.**
//!
//! 매니페스트와 자산은 실행 파일을 바꿔치우는 통로다. 평문으로 받으면 중간자가 내용을 갈아치울 수 있고,
//! 서명을 붙여도 서명 파일까지 함께 갈아치우면 그만이다. 그래서 전송 계층에서 먼저 막는다.
//!
//! 시험용 탈출구는 **두 조건을 모두** 만족할 때만 열린다: `NL_ALLOW_HTTP=1` 환경 변수가 있고,
//! 호스트가 루프백(`localhost`·`127.0.0.0/8`·`[::1]`)일 것. 로컬 tiny_http 서버로 도는 통합 테스트를
//! 위한 것이고, 바깥 주소에는 어떤 경우에도 열리지 않는다.

/// 평문 http 를 허용하는 환경 변수. 루프백 호스트에만 먹는다.
pub const ALLOW_HTTP_ENV: &str = "NL_ALLOW_HTTP";

/// https 이거나, (탈출구가 켜져 있고) 루프백 http 이면 통과.
pub fn require_https(url: &str) -> anyhow::Result<()> {
    let url = url.trim();
    if url.is_empty() {
        anyhow::bail!("주소가 비어 있습니다");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        anyhow::bail!("주소에 스킴이 없습니다 (https:// 로 시작해야 합니다): {url}");
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "https" {
        return Ok(());
    }
    if scheme != "http" {
        anyhow::bail!("https 만 받습니다 (받은 스킴: {scheme}): {url}");
    }
    if !http_allowed() {
        anyhow::bail!("평문 http 는 받지 않습니다 — 실행 파일을 바꿔치우는 통로라 중간자를 막아야 합니다: {url}");
    }
    if !is_loopback(rest) {
        anyhow::bail!("{ALLOW_HTTP_ENV} 는 루프백 주소에만 먹습니다 (받은 주소: {url})");
    }
    Ok(())
}

/// 루프백 http 시험용 탈출구가 켜져 있는가.
///
/// ureq 에이전트의 `https_only` 를 끌지 정하는 데도 쓴다 — 켜져 있어도 [`require_https`] 가
/// 호스트를 따로 검사하므로 바깥 주소는 여전히 막힌다.
pub fn plain_http_allowed() -> bool {
    std::env::var(ALLOW_HTTP_ENV).is_ok_and(|v| v == "1")
}

fn http_allowed() -> bool {
    plain_http_allowed()
}

/// `://` 뒤에서 호스트와 포트를 떼어 낸다. 사용자 정보(`user@`)·경로·질의는 걷어낸다.
fn split_authority(rest: &str) -> (String, Option<String>) {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    // IPv6 리터럴: [::1]:8080
    if let Some(rest) = host_port.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = rest[..end].to_ascii_lowercase();
            let port = rest[end + 1..].strip_prefix(':').map(|p| p.to_string());
            return (host, port);
        }
    }
    let mut parts = host_port.splitn(2, ':');
    let host = parts.next().unwrap_or("").to_ascii_lowercase();
    (host, parts.next().map(|p| p.to_string()))
}

/// `rest` 가 루프백 호스트를 가리키는가.
fn is_loopback(rest: &str) -> bool {
    let (host, _) = split_authority(rest);
    if host == "localhost" || host == "::1" {
        return true;
    }
    // 127.0.0.0/8 전체가 루프백이다.
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// 주소의 오리진 — `스킴://호스트:포트`. 기본 포트를 채워 두어 `https://h` 와 `https://h:443` 이 같아진다.
pub fn origin_of(url: &str) -> anyhow::Result<String> {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        anyhow::bail!("주소에 스킴이 없습니다: {url}");
    };
    let scheme = scheme.to_ascii_lowercase();
    let (host, port) = split_authority(rest);
    if host.is_empty() {
        anyhow::bail!("주소에 호스트가 없습니다: {url}");
    }
    let port = match port {
        Some(p) if !p.is_empty() => p,
        _ => match scheme.as_str() {
            "https" => "443".to_string(),
            "http" => "80".to_string(),
            _ => anyhow::bail!("포트를 알 수 없는 스킴입니다: {scheme}"),
        },
    };
    Ok(format!("{scheme}://{host}:{port}"))
}

/// 주소의 호스트만. 허용 호스트 목록과 맞춰 볼 때 쓴다.
pub fn host_of(url: &str) -> anyhow::Result<String> {
    let Some((_, rest)) = url.trim().split_once("://") else {
        anyhow::bail!("주소에 스킴이 없습니다: {url}");
    };
    let (host, _) = split_authority(rest);
    if host.is_empty() {
        anyhow::bail!("주소에 호스트가 없습니다: {url}");
    }
    Ok(host)
}

/// 두 주소가 같은 오리진인가. 어느 쪽이든 파싱에 실패하면 `false` — 모르면 다른 것으로 본다.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (origin_of(a), origin_of(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 환경 변수를 건드리는 시험은 프로세스 전역이라 하나로 묶는다.
    #[test]
    fn https_only_unless_loopback_escape_hatch() {
        // https 는 언제나 통과.
        require_https("https://updates.example/latest.json").unwrap();
        require_https("HTTPS://UPDATES.EXAMPLE/x").unwrap();

        // 탈출구가 꺼져 있으면 루프백이라도 막는다.
        std::env::remove_var(ALLOW_HTTP_ENV);
        let err = require_https("http://127.0.0.1:8787/latest.json")
            .unwrap_err()
            .to_string();
        assert!(err.contains("평문 http"), "{err}");

        // 탈출구를 켜면 루프백만 통과.
        std::env::set_var(ALLOW_HTTP_ENV, "1");
        for ok in [
            "http://127.0.0.1:8787/latest.json",
            "http://localhost/x",
            "http://[::1]:9000/x",
            "http://127.5.5.5/x",
            "http://user@127.0.0.1:1/x",
        ] {
            require_https(ok).unwrap_or_else(|e| panic!("{ok} 는 통과해야 합니다: {e}"));
        }
        for bad in [
            "http://updates.example/x",
            "http://127.0.0.1.evil.com/x",
            "http://[::2]/x",
        ] {
            let err = require_https(bad).unwrap_err().to_string();
            assert!(err.contains("루프백"), "{bad} → {err}");
        }
        std::env::remove_var(ALLOW_HTTP_ENV);
    }

    #[test]
    fn origins_fill_in_the_default_port() {
        assert_eq!(origin_of("https://h/x").unwrap(), "https://h:443");
        assert_eq!(origin_of("https://h:443/x").unwrap(), "https://h:443");
        assert_eq!(origin_of("https://H/x").unwrap(), "https://h:443");
        assert_eq!(origin_of("http://h/x").unwrap(), "http://h:80");
        assert_eq!(origin_of("https://u:p@h:8443/x?q#f").unwrap(), "https://h:8443");
        assert_eq!(origin_of("https://[::1]:9/x").unwrap(), "https://::1:9");
        assert_eq!(origin_of("https://[::1]/x").unwrap(), "https://::1:443");
        assert!(origin_of("https:///x").is_err());
        assert!(origin_of("h/x").is_err());
    }

    #[test]
    fn same_origin_needs_scheme_host_and_port_to_match() {
        assert!(same_origin("https://h/latest.json", "https://h/files/app"));
        assert!(same_origin("https://h:443/a", "https://h/b"));
        assert!(
            !same_origin("https://h/a", "https://evil.h/a"),
            "하위 도메인은 다른 오리진"
        );
        assert!(!same_origin("https://h/a", "http://h/a"), "스킴이 다르면 다른 오리진");
        assert!(!same_origin("https://h/a", "https://h:8443/a"));
        assert!(!same_origin("쓰레기", "https://h/a"));
    }

    #[test]
    fn hosts_drop_user_info_and_port() {
        assert_eq!(host_of("https://u@h:8443/x").unwrap(), "h");
        assert_eq!(host_of("https://[::1]:9/x").unwrap(), "::1");
        assert!(host_of("없음").is_err());
    }

    #[test]
    fn other_schemes_and_junk_are_rejected() {
        for bad in [
            "ftp://h/x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "그냥문자열",
            "",
        ] {
            assert!(require_https(bad).is_err(), "{bad} 는 거부해야 합니다");
        }
    }
}
