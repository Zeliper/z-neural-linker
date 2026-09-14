//! 빌더(`nl-app`) 자체 업데이트의 매니페스트 서명 공개키.
//!
//! `packaging/make-manifest.sh` 가 `MINISIGN_KEY` 로 만든 `latest.json.minisig` 를 검증할 때 쓴다.
//! `minisign.pub` 의 **둘째 줄**(`RWQ…` 로 시작하는 base64 한 줄)을 그대로 넣으면 된다.
//!
//! ```ignore
//! let updater = nl_update::Updater::new(UPDATE_URL, nl_update::current_version!())
//!     .with_public_key(update_key::PUBLIC_KEY);
//! ```
//!
//! `None` 이면 `nl-update` 가 서명 검증을 건너뛰고 경고 로그만 남긴다. 배포 서버가 뚫렸을 때
//! 바꿔치기된 매니페스트를 막으려면 키를 채워야 한다.
//!
//! 배포 앱(`nl-runtime`)은 이 상수가 아니라 번들 매니페스트의 `update_public_key` 를 쓴다 —
//! 앱마다 배포 주체가 다를 수 있기 때문이다.

/// 빌더 업데이트 매니페스트를 검증할 minisign 공개키.
pub const PUBLIC_KEY: Option<&str> = None;

/// 빌더 업데이트 매니페스트 주소. `NL_UPDATE_URL` 환경 변수가 우선한다.
pub const UPDATE_URL: &str = "https://updates.trustanc.dev/neural-linker/latest.json";

/// 자동 업데이트를 켤 수 있는가. 공개키가 없으면 **확인 자체를 하지 않는다**.
///
/// 키가 없는 채로 확인만 하면 서버가 뚫렸을 때 바꿔치기된 매니페스트를 그대로 믿게 된다.
/// 검증할 수 없으면 아예 묻지 않는 쪽이 안전하다.
pub const ENABLED: bool = PUBLIC_KEY.is_some();
