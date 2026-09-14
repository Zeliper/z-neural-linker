//! Windows 설치 프로그램 하나를 만든다. `nl_bundle::windows_installer` 를 셸에서 부를 수 있게 하는 얇은 껍데기다.
//!
//! ```text
//! cargo run -p nl-bundle --example nl-installer -- <app.exe> <앱 이름> <버전> <배포자> <출력 폴더> [아이콘.png]
//! ```
//!
//! Inno Setup 이 없으면 `.iss` 와 스테이징 폴더만 남기고 성공으로 끝난다 — 호출자가 zip 으로 대체하면 된다.
//! `nl-cli build` 는 `.nlproj` 로 **배포 앱**을 만들 때 같은 함수를 부른다. 이쪽은 빌더 자신을 포장할 때 쓴다.

use std::path::{Path, PathBuf};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        eprintln!("사용법: nl-installer <app.exe> <앱 이름> <버전> <배포자> <출력 폴더> [아이콘.png]");
        return std::process::ExitCode::from(2);
    }
    let (exe, name, version, publisher, out) = (Path::new(&args[0]), &args[1], &args[2], &args[3], Path::new(&args[4]));
    let icon = args.get(5).map(PathBuf::from);

    match nl_bundle::windows_installer(exe, name, version, publisher, out, icon.as_deref()) {
        Ok(Some(a)) => {
            println!(
                "설치 프로그램: {} ({} 바이트, sha256 {})",
                a.path.display(),
                a.size,
                a.sha256
            );
            std::process::ExitCode::SUCCESS
        }
        Ok(None) => {
            // 컴파일러가 없는 것은 실패가 아니다 — 준비된 파일은 남아 있다.
            println!("Inno Setup 이 없어 .iss 만 남겼습니다 ({})", out.display());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("오류: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
