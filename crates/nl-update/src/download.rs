//! 자산 내려받기. 파일로 흘려 쓰면서 sha256 을 같이 계산한다 —
//! 설치 프로그램은 수십 MB 라 메모리에 통째로 올리지 않는다.

use crate::{agent, require_https, Asset};
use anyhow::Context;
use crossbeam_channel::Sender;
use sha2::{Digest, Sha256};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 큰 자산이라 전체 시간은 넉넉히 잡는다.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// 본문이 이 시간 안에 다 오지 않으면 끊는다.
const RECV_BODY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 자산 크기 상한. 이보다 큰 것은 우리 배포물이 아니다.
const MAX_ASSET_BYTES: u64 = 2_000_000_000;
/// 다 받기 전까지 쓰는 확장자. 중간에 끊긴 파일을 완성본으로 착각하지 않게 한다.
const PART_EXT: &str = "part";
/// 매니페스트가 말한 크기에 얹어 주는 여유. 압축 전송이나 헤더 차이를 흡수할 만큼만 둔다.
const SIZE_SLACK_NUM: u64 = 11;
const SIZE_SLACK_DEN: u64 = 10;

/// 이 자산에 허용할 최대 바이트 수. 매니페스트에 크기가 있으면 그쪽을 따른다 —
/// 서명된 값이라 서버가 부풀릴 수 없고, 2 GB 전역 상한보다 훨씬 촘촘하다.
fn byte_budget(asset: &Asset) -> u64 {
    if asset.size == 0 {
        return MAX_ASSET_BYTES;
    }
    asset
        .size
        .saturating_mul(SIZE_SLACK_NUM)
        .saturating_div(SIZE_SLACK_DEN)
        .min(MAX_ASSET_BYTES)
}

/// 길이가 같은 두 해시를 상수 시간에 비교한다. 공개 해시라 실질 위험은 없지만,
/// 비교 시간으로 정보를 흘리지 않는 편이 습관으로 낫다.
fn hash_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 소유자만 읽고 쓰는 폴더를 만든다. 이미 있으면 그대로 쓴다.
///
/// 공용 `/tmp` 에 선점된 폴더를 물려받지 않게 하려면 **호출자가 사용자 전용 경로를 줘야 한다** —
/// 여기서 소유권까지 확인하지는 않는다.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// `.part` 를 **새로 만들어서만** 연다. 이미 뭔가 있으면(심볼릭 링크 포함) 실패한다 —
/// 공격자가 미리 링크를 걸어 두고 남의 파일을 덮어쓰게 하는 길을 막는다.
fn create_part_file(path: &Path) -> std::io::Result<std::fs::File> {
    // 우리가 만든 찌꺼기는 치우고 시작한다. 링크라면 링크만 지워진다.
    let _ = std::fs::remove_file(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// 내려받기 진행 상황.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub received: u64,
    /// 서버가 `Content-Length` 를 주거나 매니페스트에 크기가 있을 때만 채워진다.
    pub total: Option<u64>,
}

impl Progress {
    /// 0.0~1.0. 전체 크기를 모르면 `None`.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.filter(|t| *t > 0)?;
        Some((self.received as f64 / total as f64).min(1.0) as f32)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// 자산을 `dest_dir` 에 내려받고 sha256 을 검증한다. 검증에 실패하면 받은 파일을 지우고 오류를 낸다.
///
/// 파일 이름은 자산 URL 의 마지막 조각에서 가져온다. `dest_dir` 은 이 업데이트 전용 폴더를 주는 것이 좋다 —
/// [`prune_downloads`] 로 옛 파일을 치울 수 있다.
pub fn download(asset: &Asset, dest_dir: &Path, progress: &Sender<Progress>) -> anyhow::Result<PathBuf> {
    let expected = asset.sha256.trim().to_ascii_lowercase();
    if expected.is_empty() {
        anyhow::bail!("자산에 sha256 이 없습니다: {}", asset.url);
    }
    require_https(&asset.url).context("자산 주소")?;

    create_private_dir(dest_dir).with_context(|| format!("폴더를 만들지 못했습니다: {}", dest_dir.display()))?;

    let final_path = dest_dir.join(asset_file_name(asset));
    let part_path = final_path.with_extension(PART_EXT);
    let budget = byte_budget(asset);

    let agent = agent(DOWNLOAD_TIMEOUT, RECV_BODY_TIMEOUT);
    let mut res = agent
        .get(&asset.url)
        .call()
        .with_context(|| format!("자산을 받지 못했습니다: {}", asset.url))?;

    let total = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or((asset.size > 0).then_some(asset.size));

    let mut reader = res.body_mut().with_config().limit(budget).reader();
    let mut file = BufWriter::new(
        create_part_file(&part_path).with_context(|| format!("파일을 만들지 못했습니다: {}", part_path.display()))?,
    );

    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut chunk = vec![0u8; 64 * 1024];
    let outcome = (|| -> anyhow::Result<()> {
        loop {
            let n = reader.read(&mut chunk).context("내려받는 중 끊겼습니다")?;
            if n == 0 {
                break;
            }
            let bytes = &chunk[..n];
            // 상한 검사를 **쓰기 전에** 한다 — 뒤에 두면 상한을 넘긴 마지막 덩어리가 이미 디스크에 닿는다.
            received += n as u64;
            if received > budget {
                anyhow::bail!("자산이 상한({budget} 바이트)을 넘었습니다: {}", asset.url);
            }
            hasher.update(bytes);
            file.write_all(bytes).context("내려받은 내용을 쓰지 못했습니다")?;
            let _ = progress.send(Progress { received, total });
        }
        file.flush().context("파일을 비우지 못했습니다")?;
        Ok(())
    })();

    drop(file);
    if let Err(e) = outcome {
        let _ = std::fs::remove_file(&part_path);
        return Err(e);
    }

    let got = format!("{:x}", hasher.finalize());
    if !hash_eq(&got, &expected) {
        let _ = std::fs::remove_file(&part_path);
        anyhow::bail!("체크섬이 다릅니다 (기대 {expected}, 받은 것 {got})");
    }
    if received == 0 {
        let _ = std::fs::remove_file(&part_path);
        anyhow::bail!("빈 자산을 받았습니다: {}", asset.url);
    }

    std::fs::rename(&part_path, &final_path)
        .with_context(|| format!("이름을 바꾸지 못했습니다: {}", final_path.display()))?;
    make_executable(&final_path);
    log::info!("자산을 받았습니다: {} ({received} 바이트)", final_path.display());
    Ok(final_path)
}

/// 소유자만 실행할 수 있게 한다. 0755 로 두면 같은 호스트의 다른 사용자가 아직 적용 전인
/// 업데이트 바이너리를 실행할 수 있다.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)) {
        log::warn!("실행 권한을 주지 못했습니다 ({}): {e}", path.display());
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// 자산 URL 의 마지막 조각을 파일 이름으로. 경로 구분자나 `..` 는 걸러 낸다.
fn asset_file_name(asset: &Asset) -> String {
    let tail = asset
        .url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    // `:` 는 NTFS 대체 데이터 스트림(`app.exe:evil`)을 여는 구분자라 통째로 뺀다.
    let cleaned: String = tail.chars().filter(|c| !matches!(c, '/' | '\\' | ':')).collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    // Windows 예약 장치명은 파일이 아니라 장치를 연다. 확장자를 붙여도 마찬가지다.
    let stem = cleaned.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let cleaned = if reserved { String::new() } else { cleaned };
    if cleaned.is_empty() {
        match asset.kind {
            crate::AssetKind::Installer => "nl-update-download.exe".into(),
            crate::AssetKind::Binary => "nl-update-download".into(),
        }
    } else {
        cleaned
    }
}

/// `dir` 안에서 `keep` 말고 다 지운다. 버전마다 수십 MB 가 쌓이지 않게 호출자가 직접 부른다 —
/// `download` 가 몰래 지우지 않는 이유는 호출자가 공용 폴더를 줬을 수 있어서다.
pub fn prune_downloads(dir: &Path, keep: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path != keep && entry.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AssetKind;

    fn asset(url: &str, kind: AssetKind) -> Asset {
        Asset {
            url: url.into(),
            sha256: "ab".into(),
            kind,
            size: 0,
        }
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_name_comes_from_url_tail() {
        assert_eq!(
            asset_file_name(&asset("https://h/a/app-1.2.3.tar.gz", AssetKind::Binary)),
            "app-1.2.3.tar.gz"
        );
        assert_eq!(
            asset_file_name(&asset("https://h/s.exe?token=1", AssetKind::Installer)),
            "s.exe"
        );
        assert_eq!(asset_file_name(&asset("https://h/x#frag", AssetKind::Binary)), "x");
        // 경로 탈출 시도는 이름만 남는다.
        assert_eq!(
            asset_file_name(&asset("https://h/../../etc/passwd", AssetKind::Binary)),
            "passwd"
        );
        assert_eq!(
            asset_file_name(&asset("https://h/", AssetKind::Binary)),
            "nl-update-download"
        );
        assert_eq!(
            asset_file_name(&asset("https://h/..", AssetKind::Installer)),
            "nl-update-download.exe"
        );
    }

    #[test]
    fn reserved_windows_device_names_never_become_file_names() {
        for url in [
            "https://h/CON",
            "https://h/nul.txt",
            "https://h/COM1.exe",
            "https://h/LPT9",
        ] {
            let name = asset_file_name(&asset(url, AssetKind::Binary));
            assert_eq!(name, "nl-update-download", "{url} → {name}");
        }
        // 대체 데이터 스트림 구분자는 사라진다.
        assert_eq!(
            asset_file_name(&asset("https://h/app.exe:evil", AssetKind::Binary)),
            "app.exeevil"
        );
        // 비슷하지만 예약어가 아닌 이름은 그대로 둔다.
        assert_eq!(
            asset_file_name(&asset("https://h/console.bin", AssetKind::Binary)),
            "console.bin"
        );
        assert_eq!(asset_file_name(&asset("https://h/COM10", AssetKind::Binary)), "COM10");
    }

    #[test]
    fn the_budget_follows_the_signed_size() {
        let mut a = asset("https://h/x", AssetKind::Binary);
        a.size = 1000;
        assert_eq!(byte_budget(&a), 1100, "서명된 크기의 1.1 배");
        a.size = 0;
        assert_eq!(byte_budget(&a), MAX_ASSET_BYTES, "크기를 모르면 전역 상한");
        a.size = u64::MAX;
        assert_eq!(byte_budget(&a), MAX_ASSET_BYTES, "전역 상한을 넘지 않는다");
    }

    #[test]
    fn hashes_compare_by_value_not_by_length_alone() {
        assert!(hash_eq("abcd", "abcd"));
        assert!(!hash_eq("abcd", "abce"));
        assert!(!hash_eq("abcd", "abc"));
        assert!(hash_eq("", ""));
    }

    #[test]
    fn a_part_file_is_never_opened_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("소중한파일");
        std::fs::write(&target, "건드리지 마시오".as_bytes()).unwrap();
        let part = dir.path().join("app.part");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &part).unwrap();
        #[cfg(not(unix))]
        std::fs::write(&part, b"").unwrap();

        // 링크를 따라가지 않고 링크 자체를 지운 뒤 새 파일을 만든다.
        let f = create_part_file(&part).unwrap();
        drop(f);
        assert_eq!(std::fs::read(&target).unwrap(), "건드리지 마시오".as_bytes());
        assert_eq!(std::fs::read(&part).unwrap(), b"");
    }

    #[cfg(unix)]
    #[test]
    fn the_download_folder_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        create_private_dir(&nested).unwrap();
        assert_eq!(std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn a_plain_http_asset_is_refused_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut a = asset("http://evil.example/app", AssetKind::Binary);
        a.sha256 = "ab".into();
        let err = download(&a, dir.path(), &tx).unwrap_err().to_string();
        assert!(err.contains("자산 주소"), "{err}");
    }

    #[test]
    fn progress_fraction_needs_a_total() {
        assert_eq!(
            Progress {
                received: 5,
                total: Some(10)
            }
            .fraction(),
            Some(0.5)
        );
        assert_eq!(
            Progress {
                received: 5,
                total: None
            }
            .fraction(),
            None
        );
        assert_eq!(
            Progress {
                received: 5,
                total: Some(0)
            }
            .fraction(),
            None
        );
        // 서버가 거짓말해도 1.0 을 넘지 않는다.
        assert_eq!(
            Progress {
                received: 20,
                total: Some(10)
            }
            .fraction(),
            Some(1.0)
        );
    }

    #[test]
    fn prune_keeps_only_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.bin");
        std::fs::write(&keep, b"a").unwrap();
        std::fs::write(dir.path().join("old-1.bin"), b"b").unwrap();
        std::fs::write(dir.path().join("old-2.bin"), b"c").unwrap();

        prune_downloads(dir.path(), &keep).unwrap();
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert_eq!(left, vec!["keep.bin".to_string()]);
    }

    #[test]
    fn empty_checksum_is_rejected_before_any_request() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        let mut a = asset("https://127.0.0.1:1/never", AssetKind::Binary);
        a.sha256 = "  ".into();
        let err = download(&a, dir.path(), &tx).unwrap_err().to_string();
        assert!(err.contains("sha256"), "{err}");
    }
}
