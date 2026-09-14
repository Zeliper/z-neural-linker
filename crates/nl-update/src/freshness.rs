//! 매니페스트 발행 시각. 재생(replay) 공격을 막는 데만 쓴다.
//!
//! 정품 서명이 붙은 **옛** 매니페스트를 다시 들려주면 서명 검증은 통과한다. 그래서 서명 대상 안에
//! 발행 시각을 넣고 너무 오래된 것을 거절한다. 이렇게 하면 공격자가 업데이트를 영원히 묶어 두는
//! freeze 공격의 창이 [`MAX_AGE`] 로 제한된다.
//!
//! 날짜 크레이트를 새로 들이지 않으려고 RFC 3339 중 우리가 발행하는 형태(UTC)만 읽는다.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 매니페스트를 믿어 주는 최대 나이. 배포 주기보다 넉넉하되 freeze 창을 한 달로 묶는다.
pub const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// 미래 방향 허용치. 발행 서버와 사용자 시계가 조금 어긋나도 거절하지 않는다.
pub const CLOCK_SKEW: Duration = Duration::from_secs(24 * 60 * 60);

/// `2026-09-14T08:51:00Z` 같은 UTC RFC 3339 문자열을 Unix 초로. 오프셋 표기(`+09:00`)도 받는다.
pub fn parse_rfc3339(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let bytes = s.as_bytes();
    anyhow::ensure!(bytes.len() >= 20, "발행 시각 형식이 아닙니다: {s}");

    let num = |from: usize, to: usize| -> anyhow::Result<i64> {
        s.get(from..to)
            .and_then(|v| v.parse::<i64>().ok())
            .ok_or_else(|| anyhow::anyhow!("발행 시각 형식이 아닙니다: {s}"))
    };
    anyhow::ensure!(bytes[4] == b'-' && bytes[7] == b'-', "발행 시각 형식이 아닙니다: {s}");
    anyhow::ensure!(
        bytes[10] == b'T' || bytes[10] == b't' || bytes[10] == b' ',
        "발행 시각 형식이 아닙니다: {s}"
    );
    anyhow::ensure!(bytes[13] == b':' && bytes[16] == b':', "발행 시각 형식이 아닙니다: {s}");

    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    anyhow::ensure!((1..=12).contains(&month), "달이 범위를 벗어났습니다: {s}");
    anyhow::ensure!((1..=31).contains(&day), "날이 범위를 벗어났습니다: {s}");
    anyhow::ensure!(hour < 24 && min < 60 && sec <= 60, "시각이 범위를 벗어났습니다: {s}");

    // 소수 초를 건너뛰고 오프셋을 읽는다.
    let tail = &s[19..];
    let tail = match tail.find(['Z', 'z', '+']) {
        // '-' 는 소수 초 뒤에만 올 수 있으므로 따로 찾는다.
        Some(i) => &tail[i..],
        None => match tail.rfind('-') {
            Some(i) => &tail[i..],
            None => anyhow::bail!("시간대가 없습니다 (Z 또는 ±hh:mm): {s}"),
        },
    };
    let offset = if tail.eq_ignore_ascii_case("Z") {
        0
    } else {
        let (sign, rest) = tail.split_at(1);
        let sign = match sign {
            "+" => 1,
            "-" => -1,
            _ => anyhow::bail!("시간대가 없습니다 (Z 또는 ±hh:mm): {s}"),
        };
        let rest = rest.replace(':', "");
        anyhow::ensure!(rest.len() == 4, "시간대 형식이 아닙니다: {s}");
        let oh: i64 = rest[..2]
            .parse()
            .map_err(|_| anyhow::anyhow!("시간대 형식이 아닙니다: {s}"))?;
        let om: i64 = rest[2..]
            .parse()
            .map_err(|_| anyhow::anyhow!("시간대 형식이 아닙니다: {s}"))?;
        sign * (oh * 3600 + om * 60)
    };

    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec - offset)
}

/// Howard Hinnant 의 `days_from_civil` — 1970-01-01 로부터의 날짜 수. 윤년 규칙을 그대로 담는다.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 지금을 Unix 초로. 시계가 1970 이전이면 0.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 발행 시각이 지금 기준으로 쓸 만한가. 너무 오래됐거나 너무 미래면 오류.
pub fn check_age(published_at: &str, now: i64) -> anyhow::Result<()> {
    let issued = parse_rfc3339(published_at)?;
    let age = now - issued;
    anyhow::ensure!(
        age <= MAX_AGE.as_secs() as i64,
        "매니페스트가 너무 오래됐습니다 ({}일 전 발행) — 옛 매니페스트를 다시 들려주는 공격일 수 있습니다",
        age / 86_400
    );
    anyhow::ensure!(
        -age <= CLOCK_SKEW.as_secs() as i64,
        "매니페스트 발행 시각이 미래입니다 ({published_at}) — 시계가 어긋났거나 조작된 매니페스트입니다"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_timestamps_round_trip() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z").unwrap(), 0);
        assert_eq!(parse_rfc3339("2001-09-09T01:46:40Z").unwrap(), 1_000_000_000);
        assert_eq!(parse_rfc3339("2026-09-14T08:51:00Z").unwrap(), 1_789_375_860);
        // 윤일.
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z").unwrap(), 1_709_164_800);
    }

    #[test]
    fn offsets_and_fractions_are_understood() {
        let utc = parse_rfc3339("2026-09-14T00:00:00Z").unwrap();
        assert_eq!(parse_rfc3339("2026-09-14T09:00:00+09:00").unwrap(), utc);
        assert_eq!(parse_rfc3339("2026-09-13T19:00:00-05:00").unwrap(), utc);
        assert_eq!(parse_rfc3339("2026-09-14T00:00:00.123456Z").unwrap(), utc);
        assert_eq!(parse_rfc3339("2026-09-14T00:00:00.5-00:00").unwrap(), utc);
    }

    #[test]
    fn junk_is_rejected_rather_than_guessed() {
        for bad in [
            "",
            "어제",
            "2026-09-14",
            "2026-09-14T08:51:00",
            "2026-13-01T00:00:00Z",
            "2026-09-32T00:00:00Z",
            "2026-09-14T25:00:00Z",
            "2026/09/14T00:00:00Z",
        ] {
            assert!(parse_rfc3339(bad).is_err(), "{bad:?} 는 거부해야 합니다");
        }
    }

    #[test]
    fn stale_manifests_are_refused_and_fresh_ones_pass() {
        let now = parse_rfc3339("2026-09-14T00:00:00Z").unwrap();
        check_age("2026-09-13T00:00:00Z", now).unwrap();
        check_age("2026-08-20T00:00:00Z", now).unwrap();

        let err = check_age("2026-01-01T00:00:00Z", now).unwrap_err().to_string();
        assert!(err.contains("너무 오래됐습니다"), "{err}");

        // 시계 어긋남 한 시간은 봐준다.
        check_age("2026-09-14T01:00:00Z", now).unwrap();
        let err = check_age("2027-01-01T00:00:00Z", now).unwrap_err().to_string();
        assert!(err.contains("미래"), "{err}");
    }
}
