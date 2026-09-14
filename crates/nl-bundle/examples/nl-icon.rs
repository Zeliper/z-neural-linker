//! PNG 를 Windows `.ico` 로 바꾼다. `nl_bundle::png_to_ico` 를 셸에서 부를 수 있게 하는 얇은 껍데기다.
//!
//! ```text
//! cargo run -p nl-bundle --example nl-icon -- packaging/linux/neural-linker-256.png packaging/windows/neural-linker.ico
//! ```
//!
//! 아이콘 원본(`packaging/linux/neural-linker.svg`)을 고치면 `packaging/make-icon.py` 로 PNG 를 다시 만들고
//! 이 명령으로 `.ico` 를 다시 만든다. 두 산출물은 저장소에 함께 둔다 — 빌드 때 래스터라이저를 요구하지 않기 위해서다.

use std::path::Path;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("사용법: nl-icon <입력.png> <출력.ico>");
        return std::process::ExitCode::from(2);
    }
    match nl_bundle::png_to_ico(Path::new(&args[0]), Path::new(&args[1])) {
        Ok(()) => {
            let size = std::fs::metadata(&args[1]).map(|m| m.len()).unwrap_or(0);
            println!("{} ({size} 바이트, 프레임 {:?})", args[1], nl_bundle::icon::ICO_SIZES);
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("오류: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
