//! 시스템 자원 스냅샷 (상태바·자원 패널). GPU 목록은 `nl_engine::enumerate()` 가 진실이고 여기서는 합쳐 보여 준다.
//!
//! CPU 사용률은 **두 번의 갱신 사이 차이**로 계산되므로 `System` 을 프로세스 전역에 하나 유지한다.
//! sysinfo 가 권장하는 최소 갱신 간격(200ms)보다 자주 부르면 마지막 결과를 그대로 돌려준다.

use nl_engine::{DeviceInfo, DeviceKind};
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// sysinfo 가 요구하는 최소 CPU 갱신 간격. 이보다 자주 물으면 캐시를 돌려준다.
pub const CACHE_TTL: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ResourceSnapshot {
    pub cpu_usage_percent: f32,
    pub cpu_cores: usize,
    pub cpu_name: String,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub gpus: Vec<DeviceInfo>,
}

impl ResourceSnapshot {
    /// 0.0 ~ 1.0. 총 메모리를 모르면 0.
    pub fn mem_ratio(&self) -> f32 {
        if self.mem_total_bytes == 0 {
            0.0
        } else {
            (self.mem_used_bytes as f64 / self.mem_total_bytes as f64) as f32
        }
    }
}

struct Cache {
    sys: System,
    last: Option<(ResourceSnapshot, Instant)>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let kinds = RefreshKind::nothing()
            .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(MemoryRefreshKind::nothing().with_ram());
        Mutex::new(Cache { sys: System::new_with_specifics(kinds), last: None })
    })
}

/// 호출 간 CPU 사용률을 재려면 내부 `System` 을 유지해야 하므로 전역 캐시를 쓴다. 200ms 이내 재호출은 캐시 반환.
pub fn snapshot() -> ResourceSnapshot {
    let mut c = cache().lock();
    if let Some((snap, at)) = &c.last {
        if at.elapsed() < CACHE_TTL {
            return snap.clone();
        }
    }

    c.sys.refresh_cpu_specifics(CpuRefreshKind::nothing().with_cpu_usage());
    c.sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());

    let cpus = c.sys.cpus();
    let cpu_cores = if cpus.is_empty() {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    } else {
        cpus.len()
    };
    // 브랜드 문자열이 비는 플랫폼이 있어 이름 → 고정 문구 순으로 물러난다.
    let cpu_name = cpus
        .first()
        .map(|c| {
            let b = c.brand().trim();
            if b.is_empty() {
                c.name().trim().to_owned()
            } else {
                b.to_owned()
            }
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "알 수 없는 CPU".to_string());

    let snap = ResourceSnapshot {
        // 첫 호출은 기준점이 없어 0 이 나온다. 두 번째 호출부터 의미 있는 값이 된다.
        cpu_usage_percent: c.sys.global_cpu_usage().clamp(0.0, 100.0),
        cpu_cores,
        cpu_name,
        mem_used_bytes: c.sys.used_memory(),
        mem_total_bytes: c.sys.total_memory(),
        gpus: nl_engine::enumerate().into_iter().filter(|d| d.kind != DeviceKind::Cpu).collect(),
    };
    c.last = Some((snap.clone(), Instant::now()));
    snap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_values_are_in_range() {
        let s = snapshot();
        assert!(s.cpu_cores >= 1, "코어 수가 0 이다");
        assert!(!s.cpu_name.is_empty(), "CPU 이름이 비었다");
        assert!((0.0..=100.0).contains(&s.cpu_usage_percent), "CPU 사용률 범위 밖: {}", s.cpu_usage_percent);
        assert!(s.mem_total_bytes > 0, "총 메모리가 0 이다");
        assert!(s.mem_used_bytes <= s.mem_total_bytes, "사용 메모리가 총 메모리보다 크다");
        assert!((0.0..=1.0).contains(&s.mem_ratio()));
        // GPU 목록에는 CPU 가 섞이지 않는다.
        assert!(s.gpus.iter().all(|d| d.kind != DeviceKind::Cpu));
    }

    #[test]
    fn repeated_calls_hit_the_cache() {
        let a = snapshot();
        let b = snapshot();
        // 200ms 안이면 같은 값이 그대로 나온다.
        assert_eq!(a, b);
    }

    #[test]
    fn cpu_usage_becomes_measurable_after_the_interval() {
        let _ = snapshot();
        std::thread::sleep(CACHE_TTL + Duration::from_millis(60));
        let s = snapshot();
        assert!((0.0..=100.0).contains(&s.cpu_usage_percent));
        assert!(s.mem_total_bytes > 0);
    }
}
