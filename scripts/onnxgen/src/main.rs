// onnx.proto3 → Rust (prost). protoc 바이너리 없이 순수 Rust 로 돈다.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args().nth(1).expect("사용법: onnxgen <출력 폴더>");
    let fds = protox::compile(["onnx.proto3"], ["proto"])?;
    let mut cfg = prost_build::Config::new();
    cfg.out_dir(&out);
    cfg.compile_fds(fds)?;
    println!("생성 완료 → {out}");
    Ok(())
}
