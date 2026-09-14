//! 매니페스트 서명 검증 (minisign).
//!
//! 배포 절차에서 `minisign -Sm latest.json` 으로 `latest.json.minisig` 를 만들고, 앱은 매니페스트와 함께
//! 그 파일을 받아 여기서 검증한다. 공개키를 상수로 박아 두면 매니페스트가 바꿔치기돼도 자산을 내려받지 않는다.
//!
//! **공개키 없이 검증하는 길은 없다.** 예전에는 키가 없으면 경고만 남기고 지나갔는데, 그러면 서명을
//! 붙여 둔 배포에서도 키를 지우기만 하면 검증이 사라진다. 지금은 키를 받지 못하면 아예 이 함수를 부를 수 없고,
//! 호출자(`Updater`)는 키가 없을 때 업데이트 기능 자체를 끈다.

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
///
/// 옛 minisign 이 만든 비프리해시 서명도 받아들인다. 둘 다 Ed25519 라 안전성 차이는 없고,
/// 서명 인프라 세대가 섞여도 배포가 멈추지 않는다.
pub fn verify_manifest(json: &[u8], sig: &str, public_key: &str) -> anyhow::Result<()> {
    let key = parse_public_key(public_key)?;
    let signature = Signature::decode(sig.trim()).map_err(|e| anyhow::anyhow!("서명을 읽지 못했습니다: {e}"))?;
    // `allow_legacy = false` — 옛 `Ed` 형식(원문 전체를 그대로 서명)은 받지 않는다.
    // 지금 서명 도구가 내는 것은 전부 prehashed `ED` 라 좁혀도 잃는 것이 없다.
    key.verify(json, &signature, false)
        .map_err(|e| anyhow::anyhow!("서명이 맞지 않습니다: {e}"))?;
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

    const SIGNED: &[u8] = b"test";

    /// 진짜 키 한 쌍을 만들어 서명한다. 벡터를 박아 두는 대신 매번 만드는 이유는
    /// 옛 `Ed` 형식 벡터를 더는 받지 않기 때문이다 — 지금 도구가 내는 prehashed `ED` 로만 시험한다.
    fn signed(body: &[u8]) -> (String, String) {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().expect("키 생성");
        let sig = minisign::sign(Some(&pair.pk), &pair.sk, body, None, None).expect("서명");
        (pair.pk.to_base64(), sig.into_string())
    }

    #[test]
    fn valid_signature_passes() {
        let (pubkey, sig) = signed(SIGNED);
        verify_manifest(SIGNED, &sig, &pubkey).unwrap();
    }

    #[test]
    fn two_line_public_key_is_accepted() {
        let (pubkey, sig) = signed(SIGNED);
        let two_lines = format!("untrusted comment: minisign public key\n{pubkey}");
        verify_manifest(SIGNED, &sig, &two_lines).unwrap();
    }

    #[test]
    fn tampered_content_is_rejected() {
        let (pubkey, sig) = signed(SIGNED);
        let err = verify_manifest(b"Test", &sig, &pubkey).unwrap_err().to_string();
        assert!(err.contains("서명이 맞지 않습니다"), "{err}");
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (_, sig) = signed(SIGNED);
        let (other, _) = signed(SIGNED);
        assert!(
            verify_manifest(SIGNED, &sig, &other).is_err(),
            "다른 키로는 통과하면 안 됩니다"
        );
    }

    /// 옛 `Ed`(비-prehashed) 형식은 받지 않는다. minisign-verify 문서의 legacy 벡터로 확인한다.
    #[test]
    fn a_legacy_signature_is_no_longer_accepted() {
        const LEGACY_PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
        const LEGACY_SIG: &str = "untrusted comment: signature from minisign secret key
RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=
trusted comment: timestamp:1555779966\tfile:test
QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
        let err = verify_manifest(SIGNED, LEGACY_SIG, LEGACY_PUBKEY)
            .unwrap_err()
            .to_string();
        assert!(err.contains("서명이 맞지 않습니다"), "{err}");
    }

    #[test]
    fn broken_inputs_are_errors_not_panics() {
        let (pubkey, sig) = signed(SIGNED);
        assert!(verify_manifest(SIGNED, "쓰레기", &pubkey).is_err());
        assert!(verify_manifest(SIGNED, &sig, "쓰레기").is_err());
        assert!(verify_manifest(SIGNED, "", &pubkey).is_err());
    }

    #[test]
    fn an_empty_key_is_an_error_not_a_skip() {
        // 예전에는 키가 없으면 조용히 통과했다. 이제는 부를 방법 자체가 없고, 빈 문자열은 오류다.
        let (_, sig) = signed(SIGNED);
        assert!(verify_manifest(SIGNED, &sig, "").is_err());
        assert!(verify_manifest(SIGNED, &sig, "   ").is_err());
    }

    #[test]
    fn signature_url_appends_before_query() {
        assert_eq!(signature_url("https://h/latest.json"), "https://h/latest.json.minisig");
        assert_eq!(
            signature_url("https://h/latest.json?v=2"),
            "https://h/latest.json.minisig?v=2"
        );
        assert_eq!(
            signature_url("https://h/latest.json#a"),
            "https://h/latest.json.minisig#a"
        );
    }
}
