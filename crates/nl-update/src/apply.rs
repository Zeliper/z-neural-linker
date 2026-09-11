//! 내려받은 자산 적용. 성공하면 호출자가 앱을 끝내야 한다.

use crate::AssetKind;
use anyhow::Context;
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

/// 내려받은 자산을 현재 실행 파일에 적용한다.
pub fn apply(downloaded: &Path, kind: AssetKind) -> anyhow::Result<Applied> {
    let exe = std::env::current_exe().context("현재 실행 파일 경로를 알 수 없습니다")?;
    apply_to(downloaded, kind, &exe, true)
}

/// [`apply`] 와 같되 바꿔칠 실행 파일을 직접 받는다. 테스트와 특수 배포 구조를 위해 열어 둔다.
/// `relaunch` 가 `false` 면 새 프로세스를 띄우지 않고 경로만 돌려준다.
pub fn apply_to(downloaded: &Path, kind: AssetKind, exe: &Path, relaunch: bool) -> anyhow::Result<Applied> {
    if !downloaded.is_file() {
        anyhow::bail!("내려받은 파일이 없습니다: {}", downloaded.display());
    }
    match kind {
        AssetKind::Installer => {
            // Inno Setup 의 조용한 업데이트 인자. 설치가 끝나면 설치 프로그램이 앱을 다시 띄운다.
            std::process::Command::new(downloaded)
                .args(["/SILENT", "/NORESTART", "/CLOSEAPPLICATIONS"])
                .spawn()
                .with_context(|| format!("설치 프로그램을 실행하지 못했습니다: {}", downloaded.display()))?;
            Ok(Applied::InstallerLaunched { installer: downloaded.to_path_buf() })
        }
        AssetKind::Binary => {
            replace_binary(downloaded, exe)?;
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
            Ok(Applied::Replaced { exe: exe.to_path_buf(), relaunched })
        }
    }
}

/// `new` 를 `exe` 자리에 원자적으로 놓는다. 같은 파일 시스템의 임시 이름으로 복사한 뒤 rename —
/// Linux 는 실행 중인 파일도 이렇게 바꿀 수 있다.
pub fn replace_binary(new: &Path, exe: &Path) -> anyhow::Result<()> {
    let tmp = exe.with_extension("new");
    std::fs::copy(new, &tmp).map_err(|e| system_path_error(exe, &tmp, e))?;
    set_executable(&tmp)?;
    std::fs::rename(&tmp, exe).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        system_path_error(exe, exe, e)
    })?;
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
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
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
    cmd.spawn().with_context(|| format!("새 버전을 실행하지 못했습니다: {}", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111, 0o111);
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

        let applied = apply_to(&downloaded, AssetKind::Binary, &exe, false).unwrap();
        assert_eq!(applied, Applied::Replaced { exe: exe.clone(), relaunched: false });
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
        let err = apply_to(&dir.path().join("없음"), AssetKind::Binary, &exe, false).unwrap_err().to_string();
        assert!(err.contains("없습니다"), "{err}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"v1", "실패했으면 원본이 그대로여야 합니다");
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
        assert_eq!(std::fs::read(&exe).unwrap(), b"old", "실패했으면 원본이 그대로여야 합니다");
    }

    #[test]
    fn system_path_error_explains_what_to_do() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let msg = system_path_error(Path::new("/usr/local/bin/app"), Path::new("/usr/local/bin/app.new"), e)
            .to_string();
        assert!(msg.contains("시스템 영역"), "{msg}");
        assert!(msg.contains("install.sh"), "{msg}");
    }

    #[test]
    fn applied_messages_are_distinct() {
        let replaced = Applied::Replaced { exe: PathBuf::from("/a/b"), relaunched: true };
        let installer = Applied::InstallerLaunched { installer: PathBuf::from("/a/s.exe") };
        assert!(replaced.message().contains("다시 시작"));
        assert!(installer.message().contains("종료"));
        assert_ne!(replaced.message(), installer.message());
    }
}
