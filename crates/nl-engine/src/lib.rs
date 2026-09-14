//! Neural Linker 실행 엔진 (burn 0.21). `docs/ARCHITECTURE.md` "nl-engine" 절 참조.
//!
//! 공개 API 는 이 파일의 re-export 가 전부다. GUI(`nl-app`)와 런타임(`nl-runtime`)은 여기 있는 것만 쓴다.

pub mod codec;
pub mod data;
pub mod device;
pub mod exec;
pub mod infer;
pub mod limits;
pub mod onnx;
/// ONNX 가져오기 (추론 전용). `--features onnx-import` 로만 들어온다.
#[cfg(feature = "onnx-import")]
pub mod onnx_import;
pub mod paths;
pub mod tensor;
pub mod train;
pub mod weights;

pub use codec::{decode, decode_outputs, encode, encode_inputs, Value};
pub use data::{preview, scan, Sample};
pub use device::{enumerate, resolve, DeviceInfo, DeviceKind, Resolved};
pub use infer::Session;
pub use onnx::{export as export_onnx, Batch, ExportOptions, ExportReport};
pub use tensor::HostTensor;
pub use train::{active_count, start, TrainEvent, TrainHandle, TrainRequest};

// ── 아래는 스텁 계약에 더해진 항목 ──
pub use data::{count_csv_rows, load_all, load_source};
pub use device::{describe, probe, probe_cached, resolve_cached, CpuB, GpuB};
pub use exec::{param_name, DynTensor, Model};
pub use limits::{MAX_IMAGE_DIM, MAX_IMAGE_PIXELS, MAX_TENSOR_ELEMS};
pub use train::checkpoint_summary;
pub use weights::WEIGHTS_FORMAT;
