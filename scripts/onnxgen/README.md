# ONNX protobuf 코드 생성기

`crates/nl-engine/src/onnx/pb.rs` 를 만드는 도구다. **평소에는 돌릴 필요가 없다** — 생성 결과를
저장소에 커밋해 두므로 사용자도 CI 도 `protoc` 없이 빌드된다 (tract-onnx 가 쓰는 것과 같은 방식).

`onnx.proto3` 를 갈아 끼울 때만 돌린다.

## 왜 `prost-build` 만으로는 안 되는가

`prost-build` 는 0.11(2022-07)부터 `protoc` 바이너리를 요구한다. 그래서 앞단에
[`protox`](https://crates.io/crates/protox) 를 둔다 — 순수 Rust protobuf 컴파일러다.
`protox::compile()` 이 `FileDescriptorSet` 을 만들고 `prost_build::Config::compile_fds()` 가
그걸 Rust 코드로 바꾼다. 시스템 의존성이 없다.

## 돌리는 법

```sh
cd scripts/onnxgen
mkdir -p out && cargo run --release -- out
# out/onnx.rs 가 나온다. crates/nl-engine/src/onnx/pb.rs 의 머리말(출처·라이선스·재생성 안내)은
# 남기고 그 아래 본문만 갈아 끼운다.
```

이 폴더는 워크스페이스 멤버가 아니다(독립 크레이트). 워크스페이스 빌드에 끼어들지 않는다.

## 스키마 출처

- ONNX v1.13.1 의 `onnx/onnx.proto3` — IR_VERSION 8, opset 18 세대
- https://github.com/onnx/onnx/blob/v1.13.1/onnx/onnx.proto3
- sha256 `ac4c84fdc9dbbc26626f9e079368ec8f3af594fa5324b47d7b670db2e257f133`
- 라이선스 Apache-2.0 (ONNX 프로젝트)

우리 내보내기는 **opset 17** 을 겨냥한다. 스키마를 18 세대로 둔 것은 `TensorProto` 데이터 타입
같은 주변 정의를 넉넉히 받기 위한 것이고, 실제로 내보내는 `opset_import` 값은 별개다.
