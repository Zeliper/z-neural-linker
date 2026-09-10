//! Neural Linker IO 어댑터 + 파이프라인 실행기. `docs/ARCHITECTURE.md` "nl-io" 절 참조.
//! 공개 API 는 이 파일의 re-export 가 전부다.

pub mod http;
pub mod input;
pub mod resources;
pub mod runner;
pub mod screen;

pub use http::{call, HttpResponse};
pub use input::{perform, InputSim};
pub use resources::{snapshot, ResourceSnapshot};
pub use runner::{Runner, RunnerEvent, RunnerHandle};
pub use screen::{capture, monitors, Frame, MonitorInfo};
