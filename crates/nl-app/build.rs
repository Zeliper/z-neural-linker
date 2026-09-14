//! Windows 실행 파일에 아이콘과 버전 정보를 박는다.
//!
//! Windows 대상일 때만 동작한다 — 다른 대상에서는 아무 일도 하지 않으므로 Linux·macOS 빌드에 끼어들지 않는다.
//! 아이콘 원본은 `packaging/linux/neural-linker.svg` 이고, 저장소에 들어 있는 `.ico` 를 그대로 쓴다
//! (빌드 때 SVG 래스터라이저를 요구하지 않기 위해서다).

fn main() {
    // 아이콘 파일이 바뀌면 다시 돌린다.
    println!("cargo:rerun-if-changed=../../packaging/windows/neural-linker.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::path::Path::new("../../packaging/windows/neural-linker.ico");
    if !ico.is_file() {
        // 아이콘이 없다고 빌드를 막지는 않는다 — 기본 아이콘으로 나갈 뿐이다.
        println!("cargo:warning=아이콘을 찾지 못했습니다: {}", ico.display());
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico.to_string_lossy());
    if let Err(e) = res.compile() {
        println!("cargo:warning=아이콘을 넣지 못했습니다: {e}");
    }
}
