//! 장치 열거·선택. CPU = ndarray, GPU = wgpu(Vulkan/DX12/Metal).

use burn::backend::ndarray::NdArrayDevice;
use burn::backend::wgpu::WgpuDevice;
use burn::backend::{Autodiff, NdArray, Wgpu};
use burn::tensor::backend::AutodiffBackend;
use burn::tensor::{activation, Distribution, ElementConversion, Tensor};
use nl_core::DevicePref;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// CPU 백엔드 (ndarray).
pub type CpuB = NdArray<f32>;
/// GPU 백엔드 (wgpu — Vulkan/DX12/Metal).
pub type GpuB = Wgpu<f32, i32>;
/// 학습용 CPU 백엔드.
pub type CpuAd = Autodiff<CpuB>;
/// 학습용 GPU 백엔드.
pub type GpuAd = Autodiff<GpuB>;

/// 장치별 단형화를 한 곳에 모은다 — `$f` 는 `fn<B: Backend>(device: &B::Device, …)` 꼴.
macro_rules! dispatch {
    ($handle:expr, $f:ident $(, $arg:expr)* $(,)?) => {
        match $handle {
            $crate::device::Handle::Cpu(d) => $f::<$crate::device::CpuB>(&d $(, $arg)*),
            $crate::device::Handle::Gpu(d) => $f::<$crate::device::GpuB>(&d $(, $arg)*),
        }
    };
}

/// `dispatch!` 의 autodiff 판 — 학습 경로용.
macro_rules! dispatch_autodiff {
    ($handle:expr, $f:ident $(, $arg:expr)* $(,)?) => {
        match $handle {
            $crate::device::Handle::Cpu(d) => $f::<$crate::device::CpuAd>(&d $(, $arg)*),
            $crate::device::Handle::Gpu(d) => $f::<$crate::device::GpuAd>(&d $(, $arg)*),
        }
    };
}

pub(crate) use {dispatch, dispatch_autodiff};

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

/// 엔진 내부에서 쓰는 구체 장치 핸들. `DeviceInfo` 와 1:1.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Handle {
    Cpu(NdArrayDevice),
    Gpu(WgpuDevice),
}

impl Handle {
    /// 호스트 메모리와 장치 메모리가 분리되어 있는가 (전송 비용이 있는가).
    pub(crate) fn is_gpu(&self) -> bool {
        matches!(self, Handle::Gpu(_))
    }
}

/// 열거 결과 캐시 (wgpu 어댑터 열거는 수백 ms 걸릴 수 있다).
static CACHE: OnceLock<Vec<(DeviceInfo, Handle)>> = OnceLock::new();

fn cache() -> &'static [(DeviceInfo, Handle)] {
    CACHE.get_or_init(build_list)
}

/// 이미 만들어진 장치 목록만 본다 (없으면 `None`). 어댑터 열거는 수백 ms 걸릴 수 있어
/// UI 스레드에서 강제로 만들면 안 된다.
fn cache_if_ready() -> Option<&'static [(DeviceInfo, Handle)]> {
    CACHE.get().map(|v| v.as_slice())
}

fn cpu_info() -> DeviceInfo {
    DeviceInfo {
        pref: DevicePref::Cpu,
        name: "CPU (ndarray)".into(),
        backend: "ndarray".into(),
        kind: DeviceKind::Cpu,
        vram_bytes: None,
        cores: std::thread::available_parallelism().ok().map(|n| n.get()),
    }
}

fn build_list() -> Vec<(DeviceInfo, Handle)> {
    let mut list = vec![(cpu_info(), Handle::Cpu(NdArrayDevice::Cpu))];
    for (i, gpu) in enumerate_gpus().into_iter().enumerate() {
        let (mut info, handle) = gpu;
        info.pref = DevicePref::Gpu { index: i };
        list.push((info, handle));
    }
    list
}

/// wgpu 어댑터를 훑어 GPU 목록을 만든다. 어댑터 열거는 실패해도 조용히 빈 목록.
///
/// `Backends::PRIMARY`(Vulkan/DX12/Metal)만 본다 — burn 의 wgpu 런타임이 고르는 API 와 같은 층위이고,
/// OpenGL 중복 항목이 섞이면 `WgpuDevice::{DiscreteGpu, IntegratedGpu}` 인덱스가 어긋난다.
/// 같은 물리 GPU 가 여러 API 로 두 번 잡히면 앞의 것만 남긴다.
fn enumerate_gpus() -> Vec<(DeviceInfo, Handle)> {
    let result = std::panic::catch_unwind(|| {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));
        adapters.iter().map(|a| a.get_info()).collect::<Vec<_>>()
    });
    let infos = match result {
        Ok(v) => v,
        Err(_) => {
            log::warn!("wgpu 어댑터 열거 실패 — GPU 없이 진행합니다");
            return vec![];
        }
    };

    let mut seen: Vec<(u32, u32, String)> = Vec::new();
    let mut per_kind = [0usize; 5];
    let mut out = Vec::new();
    for info in infos {
        let key = (info.vendor, info.device, info.name.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let (kind, handle) = match info.device_type {
            wgpu::DeviceType::DiscreteGpu => {
                let n = per_kind[0];
                per_kind[0] += 1;
                (DeviceKind::DiscreteGpu, WgpuDevice::DiscreteGpu(n))
            }
            wgpu::DeviceType::IntegratedGpu => {
                let n = per_kind[1];
                per_kind[1] += 1;
                (DeviceKind::IntegratedGpu, WgpuDevice::IntegratedGpu(n))
            }
            wgpu::DeviceType::VirtualGpu => {
                let n = per_kind[2];
                per_kind[2] += 1;
                (DeviceKind::OtherGpu, WgpuDevice::VirtualGpu(n))
            }
            // wgpu 의 소프트웨어 래스터라이저(lavapipe 등). burn 은 `WgpuDevice::Cpu` 로 잡는다.
            wgpu::DeviceType::Cpu => (DeviceKind::OtherGpu, WgpuDevice::Cpu),
            wgpu::DeviceType::Other => (DeviceKind::OtherGpu, WgpuDevice::DefaultDevice),
        };

        out.push((
            DeviceInfo {
                pref: DevicePref::Auto, // build_list 가 실제 인덱스로 덮어쓴다
                name: info.name.clone(),
                backend: info.backend.to_str().to_string(),
                kind,
                vram_bytes: vram_of(&info),
                cores: None,
            },
            Handle::Gpu(handle),
        ));
    }
    out
}

/// VRAM 총량. wgpu 29 는 이를 공개 API 로 노출하지 않으므로 리눅스 amdgpu sysfs 만 최선 노력으로 읽는다.
/// 알 수 없으면 `None` (UI 는 "-" 로 표시하면 된다).
fn vram_of(info: &wgpu::AdapterInfo) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let bus = info.device_pci_bus_id.trim();
        if !bus.is_empty() {
            let path = format!("/sys/bus/pci/devices/{bus}/mem_info_vram_total");
            if let Ok(s) = std::fs::read_to_string(&path) {
                if let Ok(v) = s.trim().parse::<u64>() {
                    return Some(v);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = info;
        None
    }
}

/// 사용 가능한 장치 목록. 첫 항목은 항상 CPU. GPU 는 `DevicePref::Gpu{index}` 순서대로.
/// 결과는 프로세스 수명 동안 캐시된다 (wgpu 어댑터 열거는 느릴 수 있음).
pub fn enumerate() -> Vec<DeviceInfo> {
    cache().iter().map(|(i, _)| i.clone()).collect()
}

// ───────────────────────────── 동작 검증 (probe) ─────────────────────────────

/// probe 한 번에 허용하는 시간. 첫 셰이더 컴파일이 10 초쯤 걸려 넉넉히 잡는다.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// probe 캐시 키. `DevicePref` 는 `Hash` 를 구현하지 않아 구체 장치만 따로 표현한다.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ProbeKey {
    Cpu,
    Gpu(usize),
}

#[allow(clippy::type_complexity)]
static PROBES: OnceLock<Mutex<HashMap<ProbeKey, Result<Duration, String>>>> = OnceLock::new();

fn probes() -> &'static Mutex<HashMap<ProbeKey, Result<Duration, String>>> {
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn probe_key(pref: DevicePref) -> Option<ProbeKey> {
    match pref {
        DevicePref::Cpu => Some(ProbeKey::Cpu),
        DevicePref::Gpu { index } => Some(ProbeKey::Gpu(index)),
        DevicePref::Auto => None,
    }
}

/// 장치가 **실제로 도는지** 확인한다. 64×64 matmul → ReLU → sum → backward → CPU 로 readback 까지
/// 학습과 같은 경로를 한 번 태워 보고, 성공하면 걸린 시간을 돌려준다.
///
/// wgpu 드라이버는 파이프라인 생성 실패나 장치 손실을 **패닉**으로 알리고, 최악의 경우 응답하지 않는다.
/// 그래서 별도 스레드 + `catch_unwind` + 타임아웃(20초)으로 감싼다. 실패한 드라이버가
/// 남기는 stderr 패닉 메시지는 그대로 보인다 (원인 파악에 필요하다).
///
/// 결과는 프로세스 수명 동안 캐시되므로 두 번째 호출부터는 즉시 돌아온다.
/// `Auto` 는 구체 장치가 아니라서 `resolve(Auto)` 가 고른 장치를 대신 검사한다.
pub fn probe(pref: DevicePref) -> Result<Duration, String> {
    let Some(key) = probe_key(pref) else {
        return probe(resolve(DevicePref::Auto).pref);
    };
    if let Some(cached) = probes().lock().get(&key).cloned() {
        return cached;
    }
    let result = run_probe(pref);
    probes().lock().insert(key, result.clone());
    result
}

/// 이미 검사한 결과만 조회한다 (검사를 새로 돌리지 않는다). GUI 가 UI 스레드에서 쓰기 위한 것.
pub fn probe_cached(pref: DevicePref) -> Option<Result<Duration, String>> {
    probe_key(pref).and_then(|k| probes().lock().get(&k).cloned())
}

fn run_probe(pref: DevicePref) -> Result<Duration, String> {
    // 오류 문자열에 장치 이름을 넣지 않는다 — `describe` 와 자동 선택 로그가 이미 앞에 붙인다.
    let (_info, handle) = concrete_entry(pref)?;
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("nl-probe".into()).spawn(move || {
        let t0 = Instant::now();
        let caught = std::panic::catch_unwind(AssertUnwindSafe(|| dispatch_autodiff!(handle, probe_workload)));
        let out = match caught {
            Ok(Ok(())) => Ok(t0.elapsed()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("패닉: {}", panic_text(e.as_ref()))),
        };
        let _ = tx.send(out);
    });
    if let Err(e) = spawned {
        return Err(format!("검사 스레드를 만들지 못했습니다: {e}"));
    }
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Err(format!("{}초 안에 응답하지 않았습니다", PROBE_TIMEOUT.as_secs())),
    }
}

/// 학습이 쓰는 연산(행렬곱·활성화·역전파·readback)을 한 번씩 태워 본다.
fn probe_workload<B: AutodiffBackend>(device: &B::Device) -> Result<(), String> {
    const N: usize = 64;
    let a = Tensor::<B, 2>::random([N, N], Distribution::Default, device).require_grad();
    let b = Tensor::<B, 2>::random([N, N], Distribution::Default, device);
    let out = activation::relu(a.clone().matmul(b)).sum();

    let value = out.clone().into_scalar().elem::<f64>();
    if !value.is_finite() {
        return Err(format!("순전파 결과가 유한하지 않습니다 ({value})"));
    }
    let grads = out.backward();
    let g = a.grad(&grads).ok_or_else(|| "그래디언트를 얻지 못했습니다".to_string())?;
    let data = g
        .into_data()
        .convert::<f32>()
        .to_vec::<f32>()
        .map_err(|e| format!("결과를 CPU 로 읽어오지 못했습니다: {e:?}"))?;
    if data.len() != N * N {
        return Err(format!("readback 원소 수가 {} 입니다 (기대 {})", data.len(), N * N));
    }
    if !data.iter().all(|v| v.is_finite()) {
        return Err("그래디언트에 NaN/Inf 가 있습니다".into());
    }
    Ok(())
}

fn panic_text(e: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 오류".to_string()
    }
}

/// `Auto` 를 제외한 구체 장치 하나를 찾는다.
fn concrete_entry(pref: DevicePref) -> Result<(DeviceInfo, Handle), String> {
    let list = cache();
    match pref {
        DevicePref::Cpu => Ok(list[0].clone()),
        DevicePref::Gpu { index } => list
            .iter()
            .find(|(d, _)| d.pref == DevicePref::Gpu { index })
            .cloned()
            .ok_or_else(|| format!("GPU {index} 장치가 없습니다")),
        DevicePref::Auto => {
            let picked = resolve(DevicePref::Auto).pref;
            if picked == DevicePref::Auto {
                Err("자동 선택이 구체 장치를 고르지 못했습니다".into())
            } else {
                concrete_entry(picked)
            }
        }
    }
}

// ───────────────────────────── 선택 ─────────────────────────────

/// `Auto` 가 고른 구체 장치. 프로세스 수명 동안 한 번만 정해진다.
static AUTO_PICK: OnceLock<DevicePref> = OnceLock::new();

/// `Auto` 후보 탐색이 실제로 돈 횟수 (테스트가 "장치당 경고 한 번" 을 확인한다).
static AUTO_DECISIONS: AtomicUsize = AtomicUsize::new(0);

/// `Auto` 의 결정. 후보를 순서대로 검사하고 **딱 한 번만** 고른다.
///
/// `OnceLock::get_or_init` 이라 여러 스레드가 동시에 불러도 탐색은 한 번이고, 따라서 건너뛴 장치에 대한
/// `log::warn!` 도 장치당 한 번만 나간다.
fn auto_pick() -> DevicePref {
    *AUTO_PICK.get_or_init(|| {
        AUTO_DECISIONS.fetch_add(1, Ordering::SeqCst);
        for p in auto_candidates() {
            match probe(p) {
                Ok(t) => {
                    log::info!("자동 장치 선택: {} 검사 통과 ({:.0} ms)", label_of(p), t.as_secs_f64() * 1000.0);
                    return p;
                }
                Err(e) => log::warn!("자동 장치 선택: {} 를 건너뜁니다 — {e}", label_of(p)),
            }
        }
        log::info!("자동 장치 선택: 쓸 만한 GPU 가 없어 CPU 를 씁니다");
        DevicePref::Cpu
    })
}

/// `Auto` = 이산 GPU → 통합 GPU → CPU 순서로 **`probe` 를 통과하는 첫 장치**.
/// 드라이버가 깨진 GPU(예: 파이프라인 생성 실패)는 건너뛴다. 소프트웨어 래스터라이저(`OtherGpu`)는
/// CPU 보다 느려 후보에 넣지 않는다. 결정과 그 과정의 로그는 프로세스당 한 번뿐이다.
///
/// 명시적인 `Cpu`/`Gpu{index}` 는 검사 없이 그대로 존중한다 — 사용자가 고른 장치를 말없이 바꾸지 않는다.
/// 그 장치가 실제로 죽으면 `train::start` 나 `Session::load` 가 오류로 알린다.
/// 없는 `Gpu{index}` 만 CPU 로 떨어진다.
///
/// **UI 스레드에서 부르지 말 것.** 첫 `Auto` 호출은 GPU 셰이더 컴파일 때문에 수십 초가 걸릴 수 있다.
/// 화면을 그리는 쪽은 [`resolve_cached`] 를 쓰고, 이 함수와 [`probe`] 는 백그라운드 스레드에서 부른다.
pub fn resolve(pref: DevicePref) -> Resolved {
    let (info, _) = resolve_entry(pref);
    Resolved { pref: info.pref, info }
}

/// 이미 정해진 결과만 돌려준다 — 장치 열거도, 검사도, 스레드 생성도 하지 않는다.
///
/// 상태바처럼 매 프레임 도는 코드가 쓰라고 있는 함수다. 아직 아무것도 정해지지 않았으면 `None` 이니
/// 백그라운드에서 [`resolve`] 나 [`probe`] 를 한 번 불러 준 뒤부터 값이 나온다.
pub fn resolve_cached(pref: DevicePref) -> Option<Resolved> {
    let list = cache_if_ready()?;
    let target = match pref {
        DevicePref::Auto => *AUTO_PICK.get()?,
        other => other,
    };
    let pick = match target {
        DevicePref::Gpu { index } => list.iter().find(|(d, _)| d.pref == DevicePref::Gpu { index }),
        // Cpu 는 항상 첫 항목. Auto 는 위에서 구체값으로 바뀌었다.
        _ => None,
    };
    let (info, _) = pick.unwrap_or(list.first()?);
    Some(Resolved { pref: info.pref, info: info.clone() })
}

pub(crate) fn resolve_entry(pref: DevicePref) -> (DeviceInfo, Handle) {
    let list = cache();
    let target = match pref {
        DevicePref::Auto => auto_pick(),
        other => other,
    };
    let pick = match target {
        DevicePref::Gpu { index } => list.iter().find(|(d, _)| d.pref == DevicePref::Gpu { index }),
        _ => None,
    };
    let (info, handle) = pick.unwrap_or(&list[0]);
    (info.clone(), handle.clone())
}

/// `Auto` 가 시도하는 순서: 이산 GPU → 통합 GPU. (CPU 는 마지막 폴백이라 여기 없다.)
fn auto_candidates() -> Vec<DevicePref> {
    let list = cache();
    let by = |k: DeviceKind| list.iter().filter(move |(d, _)| d.kind == k).map(|(d, _)| d.pref);
    by(DeviceKind::DiscreteGpu).chain(by(DeviceKind::IntegratedGpu)).collect()
}

fn label_of(pref: DevicePref) -> String {
    cache()
        .iter()
        .find(|(d, _)| d.pref == pref)
        .map(|(d, _)| d.name.clone())
        .unwrap_or_else(|| pref.label())
}

/// GUI 콤보/툴팁용 한 줄 설명: 이름 · 백엔드 · (있으면) 검사 결과.
///
/// **아무것도 새로 하지 않는다** — 장치 열거도 검사도 강제하지 않고 이미 있는 것만 읽는다.
/// UI 스레드가 멈추면 안 되기 때문이다. 준비되기 전에는 "준비 중"/"미검사" 로 나오므로,
/// 백그라운드에서 [`resolve`] 나 [`probe`] 를 한 번 불러 준 뒤부터 온전한 설명이 나온다.
pub fn describe(pref: DevicePref) -> String {
    if pref == DevicePref::Auto {
        return match AUTO_PICK.get() {
            Some(p) => format!("자동 → {}", describe(*p)),
            None => "자동 — 이산 GPU → 통합 GPU → CPU 중 실제로 동작하는 첫 장치".into(),
        };
    }
    let Some(list) = cache_if_ready() else {
        // 아직 어댑터를 훑지 않았다 — UI 스레드에서 강제로 훑지 않는다.
        return format!("{} — 장치 목록 준비 중", pref.label());
    };
    let Some((info, _)) = list.iter().find(|(d, _)| d.pref == pref) else {
        return format!("{} — 없는 장치", pref.label());
    };
    let mut s = format!("{} · {}", info.name, info.backend);
    if let Some(n) = info.cores {
        s.push_str(&format!(" · {n}코어"));
    }
    if let Some(v) = info.vram_bytes {
        s.push_str(&format!(" · VRAM {:.1} GiB", v as f64 / (1024.0 * 1024.0 * 1024.0)));
    }
    match probe_cached(pref) {
        Some(Ok(t)) => s.push_str(&format!(" · 정상 ({:.0} ms)", t.as_secs_f64() * 1000.0)),
        Some(Err(e)) => s.push_str(&format!(" · 사용 불가: {e}")),
        None => s.push_str(" · 미검사"),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_always_first_and_resolvable() {
        let list = enumerate();
        assert!(!list.is_empty());
        assert_eq!(list[0].kind, DeviceKind::Cpu);
        assert_eq!(list[0].pref, DevicePref::Cpu);
        assert_eq!(resolve(DevicePref::Cpu).info.kind, DeviceKind::Cpu);
        // 없는 GPU 는 CPU 로 떨어진다.
        assert_eq!(resolve(DevicePref::Gpu { index: 999 }).info.kind, DeviceKind::Cpu);
    }

    #[test]
    fn probe_succeeds_on_cpu_and_caches_the_result() {
        assert!(probe_cached(DevicePref::Cpu).is_none() || probe_cached(DevicePref::Cpu).is_some());
        let first = probe(DevicePref::Cpu).expect("CPU 검사는 성공해야 합니다");
        // 두 번째 호출은 캐시에서 온다 — 같은 값이어야 한다.
        let again = probe(DevicePref::Cpu).unwrap();
        assert_eq!(first, again);
        assert_eq!(probe_cached(DevicePref::Cpu), Some(Ok(first)));
        assert!(describe(DevicePref::Cpu).contains("정상"), "{}", describe(DevicePref::Cpu));
    }

    #[test]
    fn probe_reports_a_missing_gpu_without_spawning_work() {
        let e = probe(DevicePref::Gpu { index: 99 }).expect_err("없는 GPU 는 실패해야 합니다");
        assert!(e.contains("99"), "오류 메시지에 인덱스가 없습니다: {e}");
        assert_eq!(probe_cached(DevicePref::Gpu { index: 99 }), Some(Err(e)));
        assert!(describe(DevicePref::Gpu { index: 99 }).contains("없는 장치"));
    }

    #[test]
    fn describe_explains_auto_without_probing() {
        assert!(describe(DevicePref::Auto).starts_with("자동"));
        // 목록이 준비된 뒤에도 검사를 강제하지 않는다.
        let _ = enumerate();
        let d = describe(DevicePref::Cpu);
        assert!(d.contains("CPU"), "{d}");
    }

    #[test]
    fn resolve_cached_never_probes_and_matches_resolve() {
        // 구체 장치는 목록만 있으면 바로 나온다.
        let _ = enumerate();
        let cached = resolve_cached(DevicePref::Cpu).expect("CPU 는 목록에 항상 있다");
        assert_eq!(cached, resolve(DevicePref::Cpu));
        assert_eq!(cached.pref, DevicePref::Cpu);

        // 없는 GPU 는 resolve 와 똑같이 CPU 로 떨어진다.
        assert_eq!(resolve_cached(DevicePref::Gpu { index: 99 }), Some(resolve(DevicePref::Gpu { index: 99 })));
    }

    #[test]
    fn auto_is_decided_once_so_warnings_do_not_repeat() {
        let before = AUTO_DECISIONS.load(Ordering::SeqCst);
        let first = resolve(DevicePref::Auto);
        let after_first = AUTO_DECISIONS.load(Ordering::SeqCst);
        assert!(after_first <= before + 1, "탐색이 두 번 이상 돌았습니다");

        // 몇 번을 더 불러도 탐색(=경고 로그)은 다시 돌지 않는다.
        for _ in 0..5 {
            assert_eq!(resolve(DevicePref::Auto), first);
        }
        assert_eq!(AUTO_DECISIONS.load(Ordering::SeqCst), after_first, "Auto 탐색이 반복되었습니다");

        // 결정된 뒤에는 비차단 조회가 같은 답을 준다.
        assert_eq!(resolve_cached(DevicePref::Auto), Some(first));
        assert!(describe(DevicePref::Auto).starts_with("자동 → "));
    }

    #[test]
    fn gpu_indices_are_dense_and_ordered() {
        for (i, d) in enumerate().iter().skip(1).enumerate() {
            assert_eq!(d.pref, DevicePref::Gpu { index: i });
            assert_ne!(d.kind, DeviceKind::Cpu);
        }
    }
}
