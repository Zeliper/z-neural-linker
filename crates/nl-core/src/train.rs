//! 학습 설정과 실행 기록. 순수 데이터 — 실제 학습은 `nl-engine::train`.

use crate::ids::{DatasetId, ModelId, RunId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 장치 선호. 실제 장치 해석은 `nl-engine::device`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type")]
pub enum DevicePref {
    /// 첫 이산 GPU → 통합 GPU → CPU.
    #[default]
    Auto,
    Cpu,
    /// wgpu 어댑터 번호.
    Gpu {
        index: usize,
    },
}

impl DevicePref {
    pub fn label(&self) -> String {
        match self {
            DevicePref::Auto => "자동".into(),
            DevicePref::Cpu => "CPU".into(),
            DevicePref::Gpu { index } => format!("GPU {index}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Optimizer {
    Sgd {
        lr: f64,
        momentum: f64,
    },
    Adam {
        lr: f64,
        beta1: f64,
        beta2: f64,
        eps: f64,
    },
    AdamW {
        lr: f64,
        beta1: f64,
        beta2: f64,
        eps: f64,
        weight_decay: f64,
    },
}

impl Optimizer {
    pub fn label(&self) -> &'static str {
        match self {
            Optimizer::Sgd { .. } => "SGD",
            Optimizer::Adam { .. } => "Adam",
            Optimizer::AdamW { .. } => "AdamW",
        }
    }
    pub fn lr(&self) -> f64 {
        match self {
            Optimizer::Sgd { lr, .. } | Optimizer::Adam { lr, .. } | Optimizer::AdamW { lr, .. } => *lr,
        }
    }
    pub fn set_lr(&mut self, v: f64) {
        match self {
            Optimizer::Sgd { lr, .. } | Optimizer::Adam { lr, .. } | Optimizer::AdamW { lr, .. } => *lr = v,
        }
    }
    pub fn default_sgd() -> Self {
        Optimizer::Sgd {
            lr: 1e-2,
            momentum: 0.9,
        }
    }
    pub fn default_adam() -> Self {
        Optimizer::Adam {
            lr: 1e-3,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
        }
    }
    pub fn default_adamw() -> Self {
        Optimizer::AdamW {
            lr: 1e-3,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
            weight_decay: 1e-2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Loss {
    /// 회귀. 출력·타깃 같은 형상.
    Mse,
    /// 다중 분류. 출력 `[C]` 로짓, 타깃 클래스 인덱스(정수) 또는 one-hot.
    #[default]
    CrossEntropy,
    /// 이진/다중 라벨. 출력 로짓, 타깃 0/1.
    BceWithLogits,
    /// 회귀 (L1).
    Mae,
}

impl Loss {
    pub const ALL: [Loss; 4] = [Loss::CrossEntropy, Loss::Mse, Loss::BceWithLogits, Loss::Mae];
    pub fn label(&self) -> &'static str {
        match self {
            Loss::Mse => "MSE",
            Loss::CrossEntropy => "CrossEntropy",
            Loss::BceWithLogits => "BCE (logits)",
            Loss::Mae => "MAE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Metric {
    #[default]
    Accuracy,
    Mae,
    None,
}

impl Metric {
    pub const ALL: [Metric; 3] = [Metric::Accuracy, Metric::Mae, Metric::None];
    pub fn label(&self) -> &'static str {
        match self {
            Metric::Accuracy => "정확도",
            Metric::Mae => "MAE",
            Metric::None => "없음",
        }
    }
}

/// 학습률 스케줄. 적용은 `nl-engine::train`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "type")]
pub enum LrSchedule {
    /// 고정 학습률.
    #[default]
    None,
    /// `every` 에포크마다 `gamma` 를 곱한다.
    Step { every: usize, gamma: f64 },
    /// 전체 에포크에 걸쳐 기본 학습률 → `min_lr` 로 코사인 감쇠.
    Cosine { min_lr: f64 },
    /// 검증 손실이 `patience` 에포크 동안 나아지지 않으면 `factor` 를 곱한다.
    Plateau { patience: usize, factor: f64 },
}

impl LrSchedule {
    /// 팔레트에 보이는 기본 인스턴스 (종류당 하나).
    pub const ALL: [LrSchedule; 4] = [
        LrSchedule::None,
        LrSchedule::Step { every: 10, gamma: 0.5 },
        LrSchedule::Cosine { min_lr: 1e-5 },
        LrSchedule::Plateau {
            patience: 5,
            factor: 0.5,
        },
    ];

    pub fn label(&self) -> &'static str {
        match self {
            LrSchedule::None => "없음",
            LrSchedule::Step { .. } => "단계 감쇠",
            LrSchedule::Cosine { .. } => "코사인",
            LrSchedule::Plateau { .. } => "정체 시 감쇠",
        }
    }

    /// 인스펙터용 짧은 요약.
    pub fn summary(&self) -> String {
        match self {
            LrSchedule::None => String::new(),
            LrSchedule::Step { every, gamma } => format!("{every} 에포크마다 ×{gamma}"),
            LrSchedule::Cosine { min_lr } => format!("→ {min_lr}"),
            LrSchedule::Plateau { patience, factor } => format!("{patience} 에포크 정체 시 ×{factor}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrainConfig {
    #[serde(default)]
    pub dataset: Option<DatasetId>,
    #[serde(default = "Optimizer::default_adam")]
    pub optimizer: Optimizer,
    #[serde(default)]
    pub loss: Loss,
    #[serde(default)]
    pub metric: Metric,
    #[serde(default = "d_epochs")]
    pub epochs: usize,
    #[serde(default = "d_batch")]
    pub batch_size: usize,
    #[serde(default)]
    pub device: DevicePref,
    #[serde(default = "d_seed")]
    pub seed: u64,
    /// 검증 분할 비율 (데이터셋에 명시 분할이 없을 때).
    #[serde(default = "d_val")]
    pub val_split: f64,
    /// N 에포크마다 체크포인트 (0 = 마지막만).
    #[serde(default)]
    pub checkpoint_every: usize,
    /// 그래디언트 클리핑 노름 (0 = 없음).
    #[serde(default)]
    pub grad_clip: f64,
    /// 학습률 스케줄.
    #[serde(default)]
    pub schedule: LrSchedule,
    /// 검증 손실이 이 에포크 수 동안 나아지지 않으면 학습을 멈춘다 (0 = 끄기).
    /// 검증 집합이 없으면 무시된다.
    #[serde(default)]
    pub early_stop_patience: usize,
    /// 처음 N 스텝 동안 학습률을 0 → 기본값으로 선형 증가 (0 = 끄기).
    #[serde(default)]
    pub warmup_steps: usize,
}

fn d_epochs() -> usize {
    10
}
fn d_batch() -> usize {
    32
}
fn d_seed() -> u64 {
    42
}
fn d_val() -> f64 {
    0.2
}

impl Default for TrainConfig {
    fn default() -> Self {
        Self {
            dataset: None,
            optimizer: Optimizer::default_adam(),
            loss: Loss::default(),
            metric: Metric::default(),
            epochs: d_epochs(),
            batch_size: d_batch(),
            device: DevicePref::Auto,
            seed: d_seed(),
            val_split: d_val(),
            checkpoint_every: 0,
            grad_clip: 0.0,
            schedule: LrSchedule::None,
            early_stop_patience: 0,
            warmup_steps: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Running,
    Finished,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct EpochMetrics {
    pub epoch: usize,
    pub train_loss: f64,
    #[serde(default)]
    pub val_loss: Option<f64>,
    #[serde(default)]
    pub val_metric: Option<f64>,
    /// 에포크 소요 초.
    #[serde(default)]
    pub seconds: f64,
    /// 이 에포크의 마지막 스텝에 쓴 학습률 (스케줄·워밍업 반영).
    #[serde(default)]
    pub lr: Option<f64>,
}

/// 학습 실행 한 번의 기록. 프로젝트에 남아 모델 관리(비교·되돌리기)의 단위가 된다.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub model: ModelId,
    #[serde(default)]
    pub dataset: Option<DatasetId>,
    pub config: TrainConfig,
    pub started: DateTime<Utc>,
    #[serde(default)]
    pub finished: Option<DateTime<Utc>>,
    pub status: RunStatus,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub epochs: Vec<EpochMetrics>,
    /// 학습 결과로 쓸 체크포인트 (프로젝트 폴더 기준 상대 경로).
    /// 조기 종료를 켰고 검증 손실이 갱신된 적이 있으면 `best_checkpoint` 와 같다.
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// 검증 손실이 가장 낮았던 에포크의 가중치 (있을 때만).
    #[serde(default)]
    pub best_checkpoint: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// 사용자 메모/태그.
    #[serde(default)]
    pub note: String,
}

impl RunRecord {
    pub fn best_val_loss(&self) -> Option<f64> {
        self.epochs
            .iter()
            .filter_map(|e| e.val_loss)
            .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.min(v))))
    }
    pub fn last(&self) -> Option<&EpochMetrics> {
        self.epochs.last()
    }
}
