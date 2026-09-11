//! 배포 산출물에서 업데이트 매니페스트(`latest.json`)를 만든다.
//!
//! 형식은 `nl_update::Manifest` 그대로다 — 빌더가 여기서 만든 파일을 배포 앱이 그대로 읽는다.
//! `packaging/make-manifest.sh` 가 셸로 하는 일과 같은 결과를 내며, 빌더의 "빌드" 뷰가 이 함수를 부른다.
//!
//! 의존 방향은 `nl-bundle → nl-update` 한쪽뿐이다. `nl-update` 는 배포 앱에서도 쓰이므로
//! 번들 포맷을 알 필요가 없고, 그래야 순환이 생기지 않는다.

use crate::Artifact;
use anyhow::Context;
use nl_update::{Asset, AssetKind, Manifest};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 업데이트 매니페스트 파일 이름.
pub const MANIFEST_FILE: &str = "latest.json";

/// 대상 키별 산출물로 `latest.json` 을 만들어 `out_dir` 에 쓴다. 쓴 경로를 돌려준다.
///
/// `artifacts` 는 `(대상 키, 산출물, 자산 종류)` 목록이다. 대상 키는 배포 앱이 `nl_update::target_key()`
/// 로 만드는 값과 같아야 한다 (`linux-x86_64`, `windows-x86_64`).
/// 자산 주소는 `base_url` 뒤에 산출물 파일 이름을 붙여 만든다.
pub fn write_manifest(
    version: &str,
    notes: &str,
    artifacts: &[(String, Artifact, AssetKind)],
    base_url: &str,
    out_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let manifest = build_manifest(version, notes, artifacts, base_url)?;
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("폴더를 만들지 못했습니다: {}", out_dir.display()))?;
    let path = out_dir.join(MANIFEST_FILE);
    let json = serde_json::to_string_pretty(&manifest).context("매니페스트를 직렬화하지 못했습니다")?;
    std::fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("매니페스트를 쓰지 못했습니다: {}", path.display()))?;
    Ok(path)
}

/// 파일로 쓰지 않고 매니페스트 값만 만든다. 미리보기와 테스트가 쓴다.
pub fn build_manifest(
    version: &str,
    notes: &str,
    artifacts: &[(String, Artifact, AssetKind)],
    base_url: &str,
) -> anyhow::Result<Manifest> {
    if artifacts.is_empty() {
        anyhow::bail!("매니페스트에 넣을 산출물이 없습니다");
    }
    semver::Version::parse(version.trim())
        .with_context(|| format!("버전이 semver 가 아닙니다: {version}"))?;

    let mut assets: BTreeMap<String, Asset> = BTreeMap::new();
    for (target, artifact, kind) in artifacts {
        let name = artifact
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("산출물 파일 이름을 읽을 수 없습니다: {}", artifact.path.display()))?;
        let asset = Asset {
            url: join_url(base_url, name),
            sha256: artifact.sha256.trim().to_ascii_lowercase(),
            kind: *kind,
            size: artifact.size,
        };
        if let Some(old) = assets.insert(target.clone(), asset) {
            anyhow::bail!("대상 키가 겹칩니다: {target} (이미 {} 가 있습니다)", old.url);
        }
    }
    Ok(Manifest { version: version.trim().to_string(), notes: notes.to_string(), assets })
}

/// `https://h/app/0.2.0` + `app.tar.gz` → `https://h/app/0.2.0/app.tar.gz`.
fn join_url(base: &str, name: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), name.trim_start_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(name: &str, sha: &str, size: u64) -> Artifact {
        Artifact { path: PathBuf::from("/out").join(name), sha256: sha.into(), size }
    }

    fn entries() -> Vec<(String, Artifact, AssetKind)> {
        vec![
            (
                "linux-x86_64".to_string(),
                artifact("app-0.2.0-linux-x86_64.tar.gz", "AA11", 1234),
                AssetKind::Binary,
            ),
            (
                "windows-x86_64".to_string(),
                artifact("app-setup-0.2.0.exe", "bb22", 5678),
                AssetKind::Installer,
            ),
        ]
    }

    #[test]
    fn written_manifest_is_readable_by_nl_update() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_manifest("0.2.0", "고친 것", &entries(), "https://updates.example/app/0.2.0/", dir.path())
                .unwrap();
        assert_eq!(path.file_name().unwrap(), MANIFEST_FILE);

        let raw = std::fs::read(&path).unwrap();
        let parsed = nl_update::Manifest::parse(&raw).expect("nl-update 가 읽을 수 있어야 합니다");
        assert_eq!(parsed.version, "0.2.0");
        assert_eq!(parsed.notes, "고친 것");
        assert_eq!(parsed.assets.len(), 2);

        let linux = &parsed.assets["linux-x86_64"];
        assert_eq!(linux.url, "https://updates.example/app/0.2.0/app-0.2.0-linux-x86_64.tar.gz");
        assert_eq!(linux.sha256, "aa11", "sha256 은 소문자로 정규화한다");
        assert_eq!(linux.kind, AssetKind::Binary);
        assert_eq!(linux.size, 1234);

        let win = &parsed.assets["windows-x86_64"];
        assert_eq!(win.kind, AssetKind::Installer);
        assert_eq!(win.url, "https://updates.example/app/0.2.0/app-setup-0.2.0.exe");
    }

    /// 배포 앱이 실제로 비교하는 경로까지 돌려 본다.
    #[test]
    fn a_newer_manifest_is_offered_to_the_matching_target() {
        let target = nl_update::target_key();
        let entries = vec![(target.clone(), artifact("app.bin", "cc33", 9), AssetKind::Binary)];
        let manifest = build_manifest("9.9.9", "", &entries, "https://h/a").unwrap();

        let found = manifest.newer_for(&semver::Version::new(0, 1, 0), &target).unwrap();
        assert_eq!(found.version, semver::Version::new(9, 9, 9));
        assert_eq!(found.asset.url, "https://h/a/app.bin");
        // 다른 플랫폼에는 주지 않는다.
        assert!(manifest.newer_for(&semver::Version::new(0, 1, 0), "다른-플랫폼").is_none());
        // 같은 버전이면 새 것이 아니다.
        assert!(manifest.newer_for(&semver::Version::new(9, 9, 9), &target).is_none());
    }

    #[test]
    fn base_url_slashes_do_not_double_up() {
        assert_eq!(join_url("https://h/a/", "x.bin"), "https://h/a/x.bin");
        assert_eq!(join_url("https://h/a", "x.bin"), "https://h/a/x.bin");
        assert_eq!(join_url("https://h/a//", "/x.bin"), "https://h/a/x.bin");
    }

    #[test]
    fn bad_input_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_manifest("0.2.0", "", &[], "https://h", dir.path()).is_err(), "산출물이 없음");
        assert!(
            write_manifest("버전아님", "", &entries(), "https://h", dir.path()).is_err(),
            "semver 가 아닌 버전"
        );

        let dup = vec![
            ("linux-x86_64".to_string(), artifact("a.bin", "aa", 1), AssetKind::Binary),
            ("linux-x86_64".to_string(), artifact("b.bin", "bb", 2), AssetKind::Binary),
        ];
        assert!(write_manifest("0.2.0", "", &dup, "https://h", dir.path()).is_err(), "대상 키 중복");
    }

    #[test]
    fn manifest_file_ends_with_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_manifest("0.2.0", "", &entries(), "https://h", dir.path()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
    }
}
