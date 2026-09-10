//! Neural Linker 실행 엔진 (burn 0.21). `docs/ARCHITECTURE.md` "nl-engine" 절 참조.
//!
//! 공개 API 는 이 파일의 re-export 가 전부다. GUI(`nl-app`)와 런타임(`nl-runtime`)은 여기 있는 것만 쓴다.

pub mod codec;
pub mod data;
pub mod device;
pub mod infer;
pub mod tensor;
pub mod train;

pub use codec::{decode, encode, Value};
pub use data::{preview, scan, Sample};
pub use device::{enumerate, resolve, DeviceInfo, DeviceKind, Resolved};
pub use infer::Session;
pub use tensor::HostTensor;
pub use train::{start, TrainEvent, TrainHandle, TrainRequest};
