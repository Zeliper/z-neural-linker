//! 내려받은 자산 적용. 성공하면 호출자가 앱을 끝내야 한다.

use crate::AssetKind;
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Seek;
use std::path::{Path, PathBuf};

/// 시스템 영역 접두사. 여기 설치돼 있으면 실행 파일을 바꿔치울 수 없으니 미리 알려 준다.
#[cfg(unix)]
const SYSTEM_PREFIXES: &[&str] = &["/usr/", "/opt/", "/bin/", "/sbin/", "/nix/store/", "/snap/"];
#[cfg(not(unix))]
const SYSTEM_PREFIXES: &[&str] = &[r"C:\Program Files", r"C:\Program Files (x86)", r"C:\Windows"];

/// 적용 결과. 어느 쪽이든 호출자는 앱을 끝내야 한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// 실행 파일을 새 것으로 바꿨다.
    Replaced {
        exe: PathBuf,
        /// 새 프로세스를 예약했는가. `false` 면 사용자가 직접 다시 실행해야 한다.
        relaunched: bool,
    },
    /// 설치 프로그램을 조용히 띄웠다. 설치가 끝나면 설치 프로그램이 앱을 다시 띄운다.
    InstallerLaunched { installer: PathBuf },
}

impl Applied {
    /// 사용자에게 보여 줄 한 줄 안내.
    pub fn message(&self) -> String {
        match self {
            Applied::Replaced { relaunched: true, .. } => "업데이트를 적용했습니다. 앱이 곧 다시 시작됩니다.".into(),
            Applied::Replaced { exe, relaunched: false } => {
                format!("업데이트를 적용했습니다. {} 를 다시 실행하세요.", exe.display())
            }
            Applied::InstallerLaunched { .. } => "설치 프로그램이 실행됐습니다. 앱을 종료하세요.".into(),
        }
    }
}

/// 현재 실행 파일 경로. 리눅스에서 실행 중에 파일이 지워지면 `/proc` 이 `" (deleted)"` 를 덧붙여
/// 돌려주므로 그 꼬리를 떼어 낸다 — 한 세션에서 두 번 적용할 때 엉뚱한 이름의 파일이 생기는 것을 막는다.
pub fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("현재 실행 파일 경로를 알 수 없습니다")?;
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else {
        return Ok(exe);
    };
    match name.strip_suffix(" (deleted)") {
        Some(real) if !real.is_empty() => Ok(exe.with_file_name(real)),
        _ => Ok(exe),
    }
}

/// 내려받은 자산을 현재 실행 파일에 적용한다.
///
/// `expected_sha256` 은 **서명된 매니페스트가 말한 해시**다. 적용 직전에 다시 계산해 맞춰 보므로
/// 내려받은 뒤 적용 사이(사용자가 버튼을 누를 때까지 몇 시간일 수 있다)에 파일이 바뀌면 거부한다.
pub fn apply(downloaded: &Path, kind: AssetKind, expected_sha256: &str) -> anyhow::Result<Applied> {
    let exe = current_exe()?;
    apply_to(downloaded, kind, &exe, true, expected_sha256)
}

/// 심볼릭 링크를 따라가지 않고 연다. 공격자가 경로를 링크로 바꿔치워도 엉뚱한 파일을 읽지 않는다.
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// 열린 핸들의 내용을 해싱해 기대값과 맞춘다. 맞으면 핸들을 처음으로 되감아 돌려준다.
///
/// **경로가 아니라 핸들을 검사하는 것이 핵심이다.** 검사한 뒤 경로를 다시 열면 그 사이에
/// 바꿔치울 수 있는 틈(TOCTOU)이 되살아난다.
fn verify_open(path: &Path, expected_sha256: &str) -> anyhow::Result<File> {
    let expected = expected_sha256.trim().to_ascii_lowercase();
    anyhow::ensure!(
        !expected.is_empty(),
        "적용할 파일의 기대 sha256 이 없습니다: {}",
        path.display()
    );

    let mut f = open_no_follow(path).with_context(|| format!("내려받은 파일을 열지 못했습니다: {}", path.display()))?;
    anyhow::ensure!(
        f.metadata().map(|m| m.is_file()).unwrap_or(false),
        "내려받은 것이 일반 파일이 아닙니다: {}",
        path.display()
    );

    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher).with_context(|| format!("파일을 읽지 못했습니다: {}", path.display()))?;
    let got = format!("{:x}", hasher.finalize());
    anyhow::ensure!(
        got == expected,
        "적용 직전 체크섬이 달라졌습니다 (기대 {expected}, 지금 {got}) — 내려받은 뒤 파일이 바뀌었습니다"
    );
    f.rewind().context("파일을 되감지 못했습니다")?;
    Ok(f)
}

/// [`apply`] 와 같되 바꿔칠 실행 파일을 직접 받는다. 테스트와 특수 배포 구조를 위해 열어 둔다.
/// `relaunch` 가 `false` 면 새 프로세스를 띄우지 않고 경로만 돌려준다.
pub fn apply_to(
    downloaded: &Path,
    kind: AssetKind,
    exe: &Path,
    relaunch: bool,
    expected_sha256: &str,
) -> anyhow::Result<Applied> {
    let mut verified = verify_open(downloaded, expected_sha256)?;
    match kind {
        AssetKind::Installer => {
            // 설치 프로그램은 파일 경로로만 실행할 수 있어 검증과 실행 사이에 아주 짧은 틈이 남는다.
            // 핸들을 열어 둔 채 바로 띄워 그 틈을 최소로 줄인다. Authenticode 검증은 아직 없다 —
            // 신뢰의 뿌리는 서명된 매니페스트의 sha256 이다(docs/ARCHITECTURE.md 참고).
            let child = std::process::Command::new(downloaded)
                .args(["/SILENT", "/NORESTART", "/CLOSEAPPLICATIONS"])
                .spawn()
                .with_context(|| format!("설치 프로그램을 실행하지 못했습니다: {}", downloaded.display()));
            drop(verified);
            child?;
            Ok(Applied::InstallerLaunched {
                installer: downloaded.to_path_buf(),
            })
        }
        AssetKind::Binary => {
            replace_binary_from(&mut verified, exe)?;
            let relaunched = if relaunch {
                match relaunch_after_exit(exe) {
                    Ok(()) => true,
                    Err(e) => {
                        // 교체는 이미 끝났다. 다시 띄우기만 실패한 것이니 사용자에게 알리고 성공으로 본다.
                        log::warn!("새 버전을 띄우지 못했습니다: {e:#}");
                        false
                    }
                }
            } else {
                false
            };
            Ok(Applied::Replaced {
                exe: exe.to_path_buf(),
                relaunched,
            })
        }
    }
}

/// `new` 를 `exe` 자리에 원자적으로 놓는다.
///
/// 검증된 핸들이 이미 있으면 [`apply_to`] 가 쓰는 내부 경로를 타는 편이 낫다 — 이쪽은 경로를 다시 연다.
pub fn replace_binary(new: &Path, exe: &Path) -> anyhow::Result<()> {
    let mut src = open_no_follow(new).with_context(|| format!("파일을 열지 못했습니다: {}", new.display()))?;
    replace_binary_from(&mut src, exe)
}

/// 열린 핸들의 내용을 `exe` 자리에 원자적으로 놓는다.
///
/// 같은 폴더에 `O_EXCL`·0600 임시 파일을 만들어 붓고 rename 한다. `tempfile` 이 이름을 무작위로 짓고
/// 실패 시 지워 주므로, 예측 가능한 `app.new` 를 공격자가 심볼릭 링크로 선점하던 길이 막힌다.
/// 같은 파일 시스템이라 rename 이 원자적이고, Linux 는 실행 중인 파일도 이렇게 바꿀 수 있다.
fn replace_binary_from(src: &mut File, exe: &Path) -> anyhow::Result<()> {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(".nl-update-")
        .tempfile_in(dir)
        .map_err(|e| system_path_error(exe, dir, e))?;

    std::io::copy(src, tmp.as_file_mut()).map_err(|e| system_path_error(exe, tmp.path(), e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| system_path_error(exe, tmp.path(), e))?;
    set_executable(tmp.path())?;

    // persist 는 rename 이다 — 실패하면 tempfile 이 임시 파일을 도로 지운다.
    tmp.persist(exe).map_err(|e| system_path_error(exe, exe, e.error))?;
    Ok(())
}

/// 쓰기 실패를 무엇을 해야 하는지 아는 오류로 바꾼다.
fn system_path_error(exe: &Path, target: &Path, e: std::io::Error) -> anyhow::Error {
    let in_system_path = SYSTEM_PREFIXES.iter().any(|p| exe.to_string_lossy().starts_with(p));
    if in_system_path || e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow::anyhow!(
            "{} 에 쓸 수 없습니다 ({e}). 설치 경로가 시스템 영역이라 앱이 스스로 갱신할 수 없습니다 — \
             패키지 관리자나 install.sh 로 갱신하세요.",
            target.display()
        )
    } else {
        anyhow::anyhow!("{} 에 쓰지 못했습니다: {e}", target.display())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // 0700 — 아직 제자리에 놓이기 전인 새 바이너리를 같은 호스트의 다른 사용자가 실행하지 못하게 한다.
    // rename 뒤에도 이 권한이 유지되므로 배포 실행 파일은 소유자 전용이 된다.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("실행 권한을 주지 못했습니다: {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 옛 프로세스가 설정을 다 쓰고 끝난 뒤에 새 프로세스가 뜨도록 1초 늦춰 띄운다 —
/// 두 인스턴스가 동시에 설정 저장소를 쓰면 업데이트 직전 설정이 사라진다 (trust-pms 교훈).
fn relaunch_after_exit(exe: &Path) -> anyhow::Result<()> {
    let mut cmd = if cfg!(unix) {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg("sleep 1; exec \"$0\"").arg(exe);
        c
    } else {
        std::process::Command::new(exe)
    };
    cmd.spawn()
        .with_context(|| format!("새 버전을 실행하지 못했습니다: {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256_hex;

    #[test]
    fn replace_binary_swaps_the_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        let new = dir.path().join("app-0.2.0");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();

        replace_binary(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!exe.with_extension("new").exists(), "임시 파일이 남았습니다");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // 소유자만 읽고 쓰고 실행한다 — 아직 자리에 놓이기 전인 바이너리를 남이 실행하지 못하게.
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    /// 가짜 실행 파일에 대해 Binary 적용 전체 경로를 돈다. 새 프로세스는 띄우지 않는다.
    #[test]
    fn apply_binary_to_a_fake_exe() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("가짜앱");
        let downloaded = dir.path().join("가짜앱-0.2.0");
        std::fs::write(&exe, b"v1").unwrap();
        std::fs::write(&downloaded, b"v2").unwrap();

        let applied = apply_to(&downloaded, AssetKind::Binary, &exe, false, &sha256_hex(b"v2")).unwrap();
        assert_eq!(
            applied,
            Applied::Replaced {
                exe: exe.clone(),
                relaunched: false
            }
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"v2");
        assert!(applied.message().contains("다시 실행"));
        // 내려받은 파일은 그대로 남는다 — 정리는 호출자 몫.
        assert!(downloaded.is_file());
    }

    #[test]
    fn apply_rejects_a_missing_download() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        std::fs::write(&exe, b"v1").unwrap();
        let err = apply_to(
            &dir.path().join("없음"),
            AssetKind::Binary,
            &exe,
            false,
            &sha256_hex(b"x"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("열지 못했습니다"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"v1",
            "실패했으면 원본이 그대로여야 합니다"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_only_directory_gives_an_actionable_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let exe = locked.join("app");
        std::fs::write(&exe, b"old").unwrap();
        let new = dir.path().join("new");
        std::fs::write(&new, b"new").unwrap();

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = replace_binary(&new, &exe).unwrap_err().to_string();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(err.contains("install.sh") || err.contains("쓰지 못했습니다"), "{err}");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"old",
            "실패했으면 원본이 그대로여야 합니다"
        );
    }

    #[test]
    fn system_path_error_explains_what_to_do() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg =
            system_path_error(Path::new("/usr/local/bin/app"), Path::new("/usr/local/bin/app.new"), e).to_string();
        assert!(msg.contains("시스템 영역"), "{msg}");
        assert!(msg.contains("install.sh"), "{msg}");
    }

    /// H11: 내려받은 뒤 적용 사이에 파일이 바뀌면 거부해야 한다.
    #[test]
    fn a_swapped_download_is_refused_at_apply_time() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        let downloaded = dir.path().join("app-0.2.0");
        std::fs::write(&exe, b"v1").unwrap();
        std::fs::write(&downloaded, b"v2").unwrap();
        let signed = sha256_hex(b"v2");

        // 공격자가 확인 창이 떠 있는 동안 파일을 바꿔치운다.
        std::fs::write(&downloaded, "악성 바이너리".as_bytes()).unwrap();

        let err = apply_to(&downloaded, AssetKind::Binary, &exe, false, &signed)
            .unwrap_err()
            .to_string();
        assert!(err.contains("체크섬이 달라졌습니다"), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"v1", "실행 파일은 그대로여야 합니다");
    }

    #[test]
    fn an_empty_expected_hash_is_refused_rather_than_skipping_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        let downloaded = dir.path().join("new");
        std::fs::write(&exe, b"v1").unwrap();
        std::fs::write(&downloaded, b"v2").unwrap();
        let err = apply_to(&downloaded, AssetKind::Binary, &exe, false, "  ")
            .unwrap_err()
            .to_string();
        assert!(err.contains("기대 sha256"), "{err}");
    }

    /// M9: 임시 파일 이름이 예측 가능하면 공격자가 링크로 선점한다. 이제 무작위 이름이라 선점할 대상이 없다.
    #[cfg(unix)]
    #[test]
    fn a_preplaced_dot_new_symlink_no_longer_diverts_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app-0.1.0");
        let victim = dir.path().join("소중한파일");
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&victim, "건드리지 마시오".as_bytes()).unwrap();
        // 예전 구현이 쓰던 이름을 미리 링크로 잡아 둔다.
        std::os::unix::fs::symlink(&victim, dir.path().join("app-0.1.new")).unwrap();

        let new = dir.path().join("내려받음");
        std::fs::write(&new, b"new").unwrap();
        replace_binary(&new, &exe).unwrap();

        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(
            std::fs::read(&victim).unwrap(),
            "건드리지 마시오".as_bytes(),
            "링크 타깃이 덮어써졌습니다"
        );
    }

    /// M9 부수 버그: `with_extension` 이 `app-0.1.0` 을 `app-0.1.new` 로 잘라 먹던 것.
    #[test]
    fn a_dotted_exe_name_survives_the_swap() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app-0.1.0");
        std::fs::write(&exe, b"old").unwrap();
        let new = dir.path().join("new");
        std::fs::write(&new, b"new").unwrap();

        replace_binary(&new, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert!(!left.iter().any(|n| n.ends_with(".new")), "{left:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_download_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        std::fs::write(&exe, b"v1").unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, b"v2").unwrap();
        let link = dir.path().join("내려받음");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        // 내용의 해시는 맞지만 링크라서 열리지 않는다.
        let err = apply_to(&link, AssetKind::Binary, &exe, false, &sha256_hex(b"v2"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("열지 못했습니다"), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"v1");
    }

    #[test]
    fn a_deleted_suffix_is_stripped_from_the_exe_path() {
        // current_exe 자체는 시험 프로세스의 경로라 바꿀 수 없으니 꼬리 제거 규칙만 확인한다.
        let p = PathBuf::from("/tmp/app (deleted)");
        let name = p.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.strip_suffix(" (deleted)"), Some("app"));
        assert_eq!(
            current_exe()
                .unwrap()
                .file_name()
                .map(|n| n.to_string_lossy().contains("(deleted)")),
            Some(false)
        );
    }

    #[test]
    fn applied_messages_are_distinct() {
        let replaced = Applied::Replaced {
            exe: PathBuf::from("/a/b"),
            relaunched: true,
        };
        let installer = Applied::InstallerLaunched {
            installer: PathBuf::from("/a/s.exe"),
        };
        assert!(replaced.message().contains("다시 시작"));
        assert!(installer.message().contains("종료"));
        assert_ne!(replaced.message(), installer.message());
    }
}
