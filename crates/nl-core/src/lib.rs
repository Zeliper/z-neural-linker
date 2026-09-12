//! Neural Linker 의 순수 로직 크레이트. GUI·네트워크·ML 프레임워크 의존성이 없다.
//! 빌더(`nl-app`)와 배포 런타임(`nl-runtime`)이 같은 모델·같은 op 적용 코드를 공유한다.

pub mod bundle;
pub mod dataset;
pub mod gui;
pub mod ids;
pub mod model;
pub mod ops;
pub mod payload;
pub mod sample;
pub mod pipeline;
pub mod shape;
pub mod train;
pub mod validate;

pub use bundle::{BuildSpec, BuildTarget, BundleManifest, BUNDLE_TRAILER_MAGIC};
pub use dataset::{DataSource, DatasetSpec, Split};
pub use gui::{Binding, GuiLayout, Widget, WidgetKind, WindowSpec};
pub use ids::*;
pub use model::{
    Act, Edge, Graph, LayerCategory, LayerKind, LayerSpec, ModelDef, Node, Port, Project, ProjectFile, ProjectSettings,
    FORMAT_VERSION,
};
pub use ops::{apply_op, apply_ops, diff_ops, inverse_ops, Op};
pub use payload::{Dtype, Field, FieldKind, PayloadSpec, Transform};
pub use sample::{api_pipeline, new_project, quadrants_cnn_project, xor_project, SampleFactory, API_BIND, API_PATH, SAMPLES};
pub use pipeline::{Link, Logic, PNode, PNodeKind, Pipeline, Sink, Source, InputAction, Region};
pub use shape::{infer, Dim, GraphError, Shape, ShapeReport};
pub use train::{DevicePref, EpochMetrics, Loss, LrSchedule, Metric, Optimizer, RunRecord, RunStatus, TrainConfig};
pub use validate::{validate, Issue, Severity};
