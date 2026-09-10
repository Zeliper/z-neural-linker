#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    env_logger::init();
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("nl-runtime {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    eprintln!("nl-runtime: M0 구현 중");
}
