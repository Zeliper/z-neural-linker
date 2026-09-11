//! 매니페스트 서명 검증 (minisign).
//!
//! 배포 절차에서 `minisign -Sm latest.json` 으로 `latest.json.minisig` 를 만들고, 앱은 매니페스트와 함께
//! 그 파일을 받아 여기서 검증한다. 공개키를 상수로 박아 두면 매니페스트가 바꿔치기돼도 자산을 내려받지 않는다.
//!
//! 공개키가 없으면(`None`) 검증을 건너뛰고 경고만 남긴다 — 서명 인프라가 아직 없는 배포를 막지 않기 위해서다.

use anyhow::Context;
use minisign_verify::{PublicKey, Signature};

/// 매니페스트 옆의 서명 파일 주소. `https://h/latest.json?x=1` → `https://h/latest.json.minisig?x=1`.
pub fn signature_url(manifest_url: &str) -> String {
    match manifest_url.find(['?', '#']) {
        Some(at) => format!("{}.minisig{}", &manifest_url[..at], &manifest_url[at..]),
        None => format!("{manifest_url}.minisig"),
    }
}

/// `json` 바이트가 `sig`(detached `.minisig` 내용)로 서명됐는지 확인한다.
///
/// `public_key` 는 minisign 공개키다. 한 줄 base64(`RWQf6…`)와 `minisign.pub` 두 줄 형식 모두 받는다.
/// `None` 이면 검증하지 않고 `Ok(())` 를 돌려주며 경고 로그를 남긴다.
///
/// 옛 minisign 이 만든 비프리해시 서명도 받아들인다. 둘 다 Ed25519 라 안전성 차이는 없고,
/// 서명 인프라 세대가 섞여도 배포가 멈추지 않는다.
pub fn verify_manifest(json: &[u8], sig: &str, public_key: Option<&str>) -> anyhow::Result<()> {
    let Some(key) = public_key else {
        log::warn!("공개키가 없어 매니페스트 서명을 검증하지 않았습니다");
        return Ok(());
    };
    let key = parse_public_key(key)?;
    let signature = Signature::decode(sig.trim()).map_err(|e| anyhow::anyhow!("서명을 읽지 못했습니다: {e}"))?;
    key.verify(json, &signature, true).map_err(|e| anyhow::anyhow!("서명이 맞지 않습니다: {e}"))?;
    Ok(())
}

/// 두 줄짜리 `minisign.pub` 이든 base64 한 줄이든 받는다.
fn parse_public_key(key: &str) -> anyhow::Result<PublicKey> {
    let key = key.trim();
    if let Ok(pk) = PublicKey::decode(key) {
        return Ok(pk);
    }
    PublicKey::from_base64(key.lines().last().unwrap_or(key).trim())
        .map_err(|e| anyhow::anyhow!("공개키를 읽지 못했습니다: {e}"))
        .context("minisign 공개키 형식이 아닙니다")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// minisign-verify 크레이트 문서의 시험 벡터.
    const PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const PUBKEY_TWO_LINES: &str = "untrusted comment: minisign public key E7620F1842B4E81F
RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIG: &str = "untrusted comment: signature from minisign secret key
RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
    /// 위 서명이 덮는 내용.
    const SIGNED: &[u8] = b"test";

    #[test]
    fn valid_signature_passes() {
        verify_manifest(SIGNED, SIG, Some(PUBKEY)).unwrap();
    }

    #[test]
    fn two_line_public_key_is_accepted() {
        verify_manifest(SIGNED, SIG, Some(PUBKEY_TWO_LINES)).unwrap();
    }

    #[test]
    fn tampered_content_is_rejected() {
        let err = verify_manifest(b"Test", SIG, Some(PUBKEY)).unwrap_err().to_string();
        assert!(err.contains("서명이 맞지 않습니다"), "{err}");
    }

    #[test]
    fn wrong_key_is_rejected() {
        // 마지막 글자를 바꾼 다른 키 — 키 id 가 달라 거부된다.
        let other = "RWSf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        assert!(verify_manifest(SIGNED, SIG, Some(other)).is_err());
    }

    #[test]
    fn broken_inputs_are_errors_not_panics() {
        assert!(verify_manifest(SIGNED, "쓰레기", Some(PUBKEY)).is_err());
        assert!(verify_manifest(SIGNED, SIG, Some("쓰레기")).is_err());
        assert!(verify_manifest(SIGNED, "", Some(PUBKEY)).is_err());
    }

    #[test]
    fn no_public_key_skips_verification() {
        verify_manifest("아무 내용".as_bytes(), "아무 서명", None).unwrap();
    }

    #[test]
    fn signature_url_appends_before_query() {
        assert_eq!(signature_url("https://h/latest.json"), "https://h/latest.json.minisig");
        assert_eq!(signature_url("https://h/latest.json?v=2"), "https://h/latest.json.minisig?v=2");
        assert_eq!(signature_url("https://h/latest.json#a"), "https://h/latest.json.minisig#a");
    }
}
