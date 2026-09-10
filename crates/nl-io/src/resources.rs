//! 시스템 자원 스냅샷 (상태바·자원 패널). GPU 목록은 `nl_engine::enumerate()` 가 진실이고 여기서는 합쳐 보여 준다.

use nl_engine::DeviceInfo;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ResourceSnapshot {
    pub cpu_usage_percent: f32,
    pub cpu_cores: usize,
    pub cpu_name: String,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub gpus: Vec<DeviceInfo>,
}

/// 호출 간 CPU 사용률을 재려면 내부 `System` 을 유지해야 하므로 전역 캐시를 쓴다. 200ms 이내 재호출은 캐시 반환.
pub fn snapshot() -> ResourceSnapshot {
    ResourceSnapshot {
        cpu_cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        gpus: nl_engine::enumerate().into_iter().filter(|d| d.kind != nl_engine::DeviceKind::Cpu).collect(),
        ..Default::default()
    }
}
