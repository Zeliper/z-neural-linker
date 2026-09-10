//! 문서 안의 모든 객체는 uuid 기반 newtype id 로 가리킨다. `BTreeMap` 키로 쓰여 순회가 결정적이다.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! define_id {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {$(
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self { Self(Uuid::new_v4()) }
            /// 테스트·샘플용 결정적 id.
            pub fn from_u128(v: u128) -> Self { Self(Uuid::from_u128(v)) }
            pub fn short(&self) -> String { self.0.simple().to_string()[..8].to_string() }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}({})", stringify!($name), self.short()) }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "{}", self.0) }
        }
    )*};
}

define_id! {
    /// 프로젝트 문서.
    ProjectId,
    /// 모델 정의(레이어 그래프 + 학습 설정).
    ModelId,
    /// 레이어 그래프의 노드.
    NodeId,
    /// 레이어 그래프의 엣지.
    EdgeId,
    /// 데이터셋 스펙.
    DatasetId,
    /// 페이로드 스펙.
    PayloadId,
    /// 파이프라인.
    PipelineId,
    /// 파이프라인 노드.
    PNodeId,
    /// 파이프라인 링크.
    LinkId,
    /// GUI 위젯.
    WidgetId,
    /// 학습 실행 기록.
    RunId,
}
