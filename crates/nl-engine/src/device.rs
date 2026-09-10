//! 장치 열거·선택. CPU = ndarray, GPU = wgpu(Vulkan/DX12/Metal).

use nl_core::DevicePref;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    OtherGpu,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// 이 장치를 고르는 선호값 (`Cpu` 또는 `Gpu{index}`).
    pub pref: DevicePref,
    pub name: String,
    /// "ndarray", "vulkan", "dx12", "metal" …
    pub backend: String,
    pub kind: DeviceKind,
    pub vram_bytes: Option<u64>,
    /// CPU 면 논리 코어 수.
    pub cores: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// 실제로 고른 장치 (Auto → 구체값).
    pub pref: DevicePref,
    pub info: DeviceInfo,
}

/// 사용 가능한 장치 목록. 첫 항목은 항상 CPU. GPU 는 `DevicePref::Gpu{index}` 순서대로.
/// 결과는 프로세스 수명 동안 캐시된다 (wgpu 어댑터 열거는 느릴 수 있음).
pub fn enumerate() -> Vec<DeviceInfo> {
    vec![DeviceInfo {
        pref: DevicePref::Cpu,
        name: "CPU (ndarray)".into(),
        backend: "ndarray".into(),
        kind: DeviceKind::Cpu,
        vram_bytes: None,
        cores: std::thread::available_parallelism().ok().map(|n| n.get()),
    }]
}

/// `Auto` = 첫 이산 GPU → 통합 GPU → CPU. 없는 `Gpu{index}` 는 CPU 로 떨어진다.
pub fn resolve(pref: DevicePref) -> Resolved {
    let list = enumerate();
    let pick = match pref {
        DevicePref::Cpu => None,
        DevicePref::Gpu { index } => list.iter().find(|d| d.pref == DevicePref::Gpu { index }),
        DevicePref::Auto => list
            .iter()
            .find(|d| d.kind == DeviceKind::DiscreteGpu)
            .or_else(|| list.iter().find(|d| d.kind == DeviceKind::IntegratedGpu)),
    };
    let info = pick.cloned().unwrap_or_else(|| list[0].clone());
    Resolved { pref: info.pref, info }
}
