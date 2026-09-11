//! Neural Linker IO 어댑터 + 파이프라인 실행기. `docs/ARCHITECTURE.md` "nl-io" 절 참조.
//! 공개 API 는 이 파일의 re-export 가 전부다.

pub mod http;
pub mod input;
pub mod record;
pub mod resources;
pub mod runner;
pub mod screen;

pub use http::{call, HttpResponse};
pub use input::{perform, InputSim};
pub use resources::{snapshot, ResourceSnapshot};
pub use runner::{Runner, RunnerEvent, RunnerHandle};
pub use screen::{capture, monitors, Frame, MonitorInfo};

// 아래는 구현하면서 추가한 것. 기존 re-export 는 그대로 둔다.
/// 키 이름 문자열 → `enigo::Key`. 파이프라인 편집기의 키 입력 검증에 쓴다.
pub use input::parse_key;
/// GUI 위젯 이벤트·수동 입력을 실행 중인 파이프라인에 넣는 통로.
pub use runner::RunnerInput;
/// 연결을 유지하는 화면 캡처기(반복 캡처용)와 실제로 쓰인 백엔드 이름.
pub use screen::{Backend, Capturer};
/// 화면 녹화 → `DataSource::Recorded` 폴더. `FrameSource` 로 다른 프레임 공급원을 끼울 수 있다.
pub use record::{FrameSource, Recorder, RecorderHandle};
/// `RunnerEvent::ValuePreview` 썸네일의 최장변 상한. 미리보기 위젯 크기를 잡을 때 쓴다.
pub use runner::PREVIEW_MAX_SIDE;
