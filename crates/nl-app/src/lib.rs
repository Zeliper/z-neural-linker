//! Neural Linker 빌더 GUI 라이브러리 (bin 에서 사용 + 헤드리스 렌더 테스트에서 재사용).

pub mod app;
pub mod canvas;
pub mod fonts;
pub mod inspector;
pub mod outline;
pub mod paths;
pub mod pcanvas;
pub mod project;
pub mod record;
pub mod sample;
pub mod session;
pub mod tools;
pub mod update_key;
pub mod views;

pub use app::{DocState, NlApp, View};
pub use canvas::{CanvasAction, CanvasState, Selection, SelectionState};
