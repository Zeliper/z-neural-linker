//! 빌드 스크립트: Linux 에서 `libxkbcommon` 링크를 개발 패키지 없이 성사시킨다.
//!
//! ## 왜 필요한가
//! enigo 의 `x11rb`/`wayland` feature 는 `xkbcommon` 크레이트를 끌어오고, 그 크레이트는
//! `cargo:rustc-link-lib=dylib=xkbcommon` 만 내보낸다. 링커는 그 이름을 보고 `libxkbcommon.so` 를 찾는데,
//! 이 파일은 **개발 패키지**(`libxkbcommon-devel`, `libxkbcommon-dev`)에만 들어 있다.
//! 일반 데스크톱에는 실행용 `libxkbcommon.so.0` 만 깔려 있어 링크가 `unable to find library -lxkbcommon` 으로 죽는다.
//!
//! ## 무엇을 하는가
//! 표준 라이브러리 경로에 `libxkbcommon.so` 가 없고 `libxkbcommon.so.<N>` 만 있으면,
//! `OUT_DIR/xkb-link/libxkbcommon.so` 심볼릭 링크를 만들고 그 폴더를 링크 검색 경로로 추가한다.
//! 링크된 바이너리의 `DT_NEEDED` 에는 실제 파일의 SONAME(`libxkbcommon.so.0`)이 박히므로
//! **실행 시에는 `libxkbcommon.so.0` 만 있으면 되고, 배포 대상에 개발 패키지를 요구하지 않는다.**
//!
//! 개발 패키지가 이미 깔린 환경에서는 아무것도 하지 않는다.

use std::path::{Path, PathBuf};

const LIB_STEM: &str = "libxkbcommon";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    if let Err(msg) = ensure_link_target() {
        // 링크가 진짜로 실패할지는 링커가 정한다. 여기서는 원인을 알려 주기만 한다.
        println!("cargo:warning={msg}");
    }
}

fn ensure_link_target() -> Result<(), String> {
    let dirs = search_dirs();

    // 1. 개발용 심볼릭 링크가 이미 있으면 손댈 것이 없다.
    if dirs.iter().any(|d| d.join(format!("{LIB_STEM}.so")).exists()) {
        return Ok(());
    }

    // 2. 실행용 `libxkbcommon.so.<N>` 을 찾는다. 버전이 여럿이면 가장 높은 것.
    let Some(real) = newest_versioned(&dirs) else {
        return Err(format!(
            "{LIB_STEM}.so 도 {LIB_STEM}.so.<버전> 도 찾지 못했다. \
             xkbcommon 런타임 라이브러리를 설치해야 한다 (Fedora: libxkbcommon, Debian/Ubuntu: libxkbcommon0). \
             찾아본 경로: {}",
            dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")
        ));
    };

    // 3. OUT_DIR 안에 개발용 이름으로 심볼릭 링크를 만든다.
    let out = PathBuf::from(std::env::var("OUT_DIR").map_err(|_| "OUT_DIR 이 없다".to_string())?);
    let dir = out.join("xkb-link");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} 폴더를 만들지 못했다: {e}", dir.display()))?;
    let link = dir.join(format!("{LIB_STEM}.so"));

    // 이미 같은 곳을 가리키면 그대로 둔다 (증분 빌드에서 매번 지웠다 만들지 않게).
    let already = std::fs::read_link(&link).map(|t| t == real).unwrap_or(false);
    if !already {
        if link.exists() || std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link)
                .map_err(|e| format!("낡은 링크 {} 를 지우지 못했다: {e}", link.display()))?;
        }
        std::os::unix::fs::symlink(&real, &link).map_err(|e| {
            format!("{} → {} 심볼릭 링크를 만들지 못했다: {e}", link.display(), real.display())
        })?;
    }

    println!("cargo:rustc-link-search=native={}", dir.display());
    Ok(())
}

/// 표준 라이브러리 경로 + 타깃 멀티아치 경로 + `LD_LIBRARY_PATH`.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Ok(paths) = std::env::var("LD_LIBRARY_PATH") {
        dirs.extend(std::env::split_paths(&paths));
    }

    // Debian/Ubuntu 계열 멀티아치 경로 (예: /usr/lib/x86_64-linux-gnu).
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if !arch.is_empty() {
        let abi = if env == "musl" { "linux-musl" } else { "linux-gnu" };
        let triple = format!("{arch}-{abi}");
        dirs.push(PathBuf::from(format!("/usr/lib/{triple}")));
        dirs.push(PathBuf::from(format!("/lib/{triple}")));
    }

    for p in ["/usr/lib64", "/usr/lib", "/lib64", "/lib", "/usr/local/lib64", "/usr/local/lib"] {
        dirs.push(PathBuf::from(p));
    }

    dirs.retain(|d| d.is_dir());
    dirs.dedup();
    dirs
}

/// `libxkbcommon.so.<N>[.<M>...]` 중 버전이 가장 높은 실제 경로.
fn newest_versioned(dirs: &[PathBuf]) -> Option<PathBuf> {
    let prefix = format!("{LIB_STEM}.so.");
    let mut best: Option<(Vec<u64>, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            // `libxkbcommon-x11.so.0` 같은 이웃 라이브러리는 걸러진다 (접두사가 정확히 다르다).
            let Some(rest) = name.strip_prefix(&prefix) else { continue };
            let ver: Vec<u64> = rest.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect();
            if ver.is_empty() {
                continue;
            }
            let path = e.path();
            if !is_readable_file(&path) {
                continue;
            }
            if best.as_ref().is_none_or(|(b, _)| ver > *b) {
                best = Some((ver, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// 심볼릭 링크를 따라가 실제 파일인지 본다.
fn is_readable_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}
