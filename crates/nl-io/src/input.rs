//! 마우스·키보드 시뮬레이션 (enigo). **안전장치**: `InputSim` 은 `armed` 가 켜진 동안만 실제 입력을 보낸다.

use nl_core::pipeline::InputAction;

pub struct InputSim {
    _private: (),
}

impl InputSim {
    pub fn new() -> anyhow::Result<Self> {
        anyhow::bail!("입력 시뮬레이션 미구현")
    }
    pub fn perform(&mut self, _action: &InputAction) -> anyhow::Result<()> {
        anyhow::bail!("입력 시뮬레이션 미구현")
    }
}

/// 일회성 실행 (시험 버튼).
pub fn perform(action: &InputAction) -> anyhow::Result<()> {
    InputSim::new()?.perform(action)
}
