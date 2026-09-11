//! 배포 앱의 자동 업데이트. 확인·다운로드는 `nl-update` 가 하고, 여기서는 언제 시작할지와
//! 무엇을 보여 줄지를 정한다.
//!
//! 적용은 **언제나 사용자 확인을 거친다.** `auto_update` 가 켜져 있어도 자동으로 하는 일은
//! "파이프라인이 멈춰 있을 때 미리 내려받기"까지다.

use nl_core::BundleManifest;
use nl_update::{Event, State, Updater};
use std::path::PathBuf;

/// 번들 매니페스트의 주소를 덮어쓰는 환경 변수.
pub const URL_ENV: &str = "NL_UPDATE_URL";

/// 업데이트 상태와 창 표시 여부.
pub struct UpdateUi {
    updater: Updater,
    /// 매니페스트의 `auto_update`.
    auto: bool,
    /// 자동 다운로드를 이미 걸었는가 (한 번만).
    auto_started: bool,
    /// 자동 다운로드가 끝나 확인 창을 띄웠는가 (한 번만).
    prompted: bool,
    /// 업데이트 창을 띄울지.
    pub show: bool,
    /// 내려받은 자산을 둘 폴더.
    dir: PathBuf,
    /// 바깥에서 밀어 넣은 이벤트. 다음 `poll` 이 백그라운드 이벤트와 같은 자격으로 돌려준다.
    injected: Vec<Event>,
}

impl UpdateUi {
    /// 번들 매니페스트에서 만든다. 주소가 없거나 앱 버전이 semver 가 아니면 `None` — 업데이트를 끈다.
    /// 네트워크를 건드리지 않는다. 확인은 [`UpdateUi::start_check`] 가 시작한다.
    pub fn new(manifest: &BundleManifest) -> Option<Self> {
        let url = manifest_url(manifest)?;
        let current = match semver::Version::parse(manifest.app_version.trim()) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "앱 버전이 semver 가 아니라 업데이트를 끕니다 ({}): {e}",
                    manifest.app_version
                );
                return None;
            }
        };
        let updater =
            Updater::new(url, current).with_public_key(manifest.update_public_key.clone());
        Some(Self {
            updater,
            auto: manifest.auto_update,
            auto_started: false,
            prompted: false,
            show: false,
            dir: download_dir(&manifest.app_name),
            injected: Vec::new(),
        })
    }

    /// 매니페스트 확인을 백그라운드에서 시작한다.
    pub fn start_check(&mut self) {
        self.updater.check();
    }

    pub fn state(&self) -> &State {
        self.updater.state()
    }

    pub fn is_busy(&self) -> bool {
        self.updater.is_busy()
    }

    pub fn manifest_url(&self) -> &str {
        self.updater.manifest_url()
    }

    /// 상단 바에 띄울 배지 문구. 알릴 것이 없으면 `None`.
    pub fn badge(&self) -> Option<String> {
        match self.updater.state() {
            State::Available(a) => Some(format!("⬆ 새 버전 {}", a.version)),
            State::Downloading { .. } => Some("⬆ 내려받는 중".into()),
            State::Downloaded { .. } => Some("⬆ 적용 준비됨".into()),
            State::Applying => Some("⬆ 적용 중".into()),
            State::Applied(_) => Some("⬆ 적용됨".into()),
            State::Idle | State::Checking | State::UpToDate | State::Failed(_) => None,
        }
    }

    /// 도착한 이벤트를 소비해 상태를 갱신한다. 자동 다운로드가 끝나면 확인 창을 한 번 띄운다.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = self.updater.poll();
        // 밀어 넣은 이벤트는 상태에 이미 반영돼 있고, 여기서 호출자에게도 알린다.
        events.append(&mut self.injected);
        if self.auto && !self.prompted && matches!(self.updater.state(), State::Downloaded { .. }) {
            self.prompted = true;
            self.show = true;
        }
        events
    }

    /// 조건이 맞으면 자동 다운로드를 건다: `auto_update` 가 켜져 있고, 새 버전이 있고,
    /// 파이프라인이 멈춰 있을 때. 실행 중에 내려받으면 추론이 느려진다.
    pub fn maybe_auto_download(&mut self, pipeline_running: bool) {
        if !self.auto || self.auto_started || pipeline_running {
            return;
        }
        if matches!(self.updater.state(), State::Available(_)) {
            self.auto_started = true;
            self.start_download();
        }
    }

    pub fn start_download(&mut self) {
        self.updater.download(self.dir.clone());
    }

    /// 내려받은 자산을 적용한다. 성공하면 앱을 끝내야 한다.
    pub fn apply(&mut self) {
        self.updater.apply();
    }

    /// 다시 확인한다 (실패 뒤 재시도).
    pub fn recheck(&mut self) {
        self.updater.check();
    }

    /// 테스트가 백그라운드 작업 없이 상태를 옮길 때 쓴다.
    /// 상태는 바로 바뀌고, 다음 [`UpdateUi::poll`] 이 같은 이벤트를 호출자에게 돌려준다 —
    /// 로그·창 갱신이 실제 이벤트와 똑같이 흐른다.
    #[cfg(test)]
    pub fn inject(&mut self, ev: Event) {
        self.updater.on_event(ev.clone());
        self.injected.push(ev);
    }
}

/// 환경 변수가 번들 매니페스트보다 우선한다. 둘 다 없으면 업데이트를 끈다.
pub fn manifest_url(manifest: &BundleManifest) -> Option<String> {
    if let Ok(url) = std::env::var(URL_ENV) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            return Some(url);
        }
    }
    manifest.update_url.as_ref().map(|u| u.trim().to_string()).filter(|u| !u.is_empty())
}

/// 내려받은 자산을 두는 폴더. 앱마다 나눠 두 앱이 서로의 파일을 지우지 않게 한다.
fn download_dir(app_name: &str) -> PathBuf {
    std::env::temp_dir().join("nl-runtime-updates").join(nl_bundle::slugify(app_name))
}

/// 헤드리스 로그와 GUI 로그가 같은 문장을 쓰도록 이벤트를 한 줄로 만든다. 알릴 것이 없으면 `None`.
pub fn describe(ev: &Event) -> Option<String> {
    match ev {
        Event::Checking => Some("업데이트를 확인합니다".into()),
        Event::UpToDate => Some("최신 버전입니다".into()),
        Event::Available(a) => Some(format!("새 버전 {} 이(가) 있습니다", a.version)),
        Event::Downloaded(path) => Some(format!("업데이트를 내려받았습니다: {}", path.display())),
        Event::Applying => Some("업데이트를 적용합니다".into()),
        Event::Applied(a) => Some(a.message()),
        Event::Failed(e) => Some(format!("업데이트 확인 실패: {e}")),
        // 진행률은 초당 수십 번 와서 로그를 채운다.
        Event::Progress(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nl_update::{Asset, AssetKind, Available, Progress};

    fn manifest(url: Option<&str>, auto: bool) -> BundleManifest {
        let mut m = BundleManifest::new("데모 앱", "0.1.0");
        m.update_url = url.map(str::to_string);
        m.auto_update = auto;
        m
    }

    fn available(version: &str) -> Available {
        Available {
            version: semver::Version::parse(version).unwrap(),
            notes: "고친 것".into(),
            asset: Asset {
                url: "https://h/app".into(),
                sha256: "ab".into(),
                kind: AssetKind::Binary,
                size: 10,
            },
            target: nl_update::target_key(),
        }
    }

    #[test]
    fn no_url_means_no_updates() {
        assert!(UpdateUi::new(&manifest(None, false)).is_none());
        assert!(UpdateUi::new(&manifest(Some("  "), false)).is_none(), "빈 주소도 끈 것으로 본다");
    }

    #[test]
    fn a_non_semver_app_version_disables_updates() {
        let mut m = manifest(Some("https://h/latest.json"), false);
        m.app_version = "버전아님".into();
        assert!(UpdateUi::new(&m).is_none(), "비교할 수 없으면 무엇이든 새 버전으로 보인다");
    }

    #[test]
    fn url_comes_from_the_manifest() {
        let ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), false)).unwrap();
        assert_eq!(ui.manifest_url(), "https://h/latest.json");
        assert_eq!(*ui.state(), State::Idle, "만들기만 해서는 네트워크를 건드리지 않는다");
        assert!(ui.badge().is_none());
        assert!(!ui.show);
    }

    #[test]
    fn badge_follows_the_state() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), false)).unwrap();
        assert!(ui.badge().is_none(), "Idle 에는 배지가 없다");

        ui.inject(Event::Checking);
        assert!(ui.badge().is_none(), "확인 중에는 조용히 있는다");

        ui.inject(Event::UpToDate);
        assert!(ui.badge().is_none(), "최신이면 알리지 않는다");

        ui.inject(Event::Available(available("0.2.0")));
        assert_eq!(ui.badge().as_deref(), Some("⬆ 새 버전 0.2.0"));

        ui.inject(Event::Progress(Progress { received: 1, total: Some(2) }));
        assert_eq!(ui.badge().as_deref(), Some("⬆ 내려받는 중"));

        ui.inject(Event::Downloaded("/tmp/app".into()));
        assert_eq!(ui.badge().as_deref(), Some("⬆ 적용 준비됨"));

        ui.inject(Event::Failed("연결 실패".into()));
        assert!(ui.badge().is_none(), "실패는 배지가 아니라 로그로 알린다");
    }

    #[test]
    fn auto_download_waits_for_the_pipeline_to_stop() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), true)).unwrap();
        ui.inject(Event::Available(available("0.2.0")));

        ui.maybe_auto_download(true);
        assert!(!ui.auto_started, "파이프라인이 도는 동안에는 내려받지 않는다");

        ui.maybe_auto_download(false);
        assert!(ui.auto_started, "멈춰 있으면 내려받는다");
    }

    #[test]
    fn auto_download_is_off_by_default() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), false)).unwrap();
        ui.inject(Event::Available(available("0.2.0")));
        ui.maybe_auto_download(false);
        assert!(!ui.auto_started, "auto_update 가 꺼져 있으면 사용자가 눌러야 한다");
    }

    #[test]
    fn auto_download_prompts_once_when_it_finishes() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), true)).unwrap();
        ui.inject(Event::Available(available("0.2.0")));
        ui.inject(Event::Downloaded("/tmp/app".into()));

        ui.poll();
        assert!(ui.show, "확인 창을 한 번 띄운다");

        ui.show = false;
        ui.poll();
        assert!(!ui.show, "닫은 창을 다시 띄우지 않는다");
    }

    #[test]
    fn manual_mode_does_not_pop_a_window() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), false)).unwrap();
        ui.inject(Event::Downloaded("/tmp/app".into()));
        ui.poll();
        assert!(!ui.show, "직접 누른 다운로드는 창을 가로채지 않는다");
    }

    #[test]
    fn env_var_overrides_the_bundle_url() {
        // 이 테스트만 환경 변수를 쓴다.
        let m = manifest(Some("https://bundle/latest.json"), false);
        assert_eq!(manifest_url(&m).as_deref(), Some("https://bundle/latest.json"));

        std::env::set_var(URL_ENV, " https://env/latest.json ");
        let from_env = manifest_url(&m);
        std::env::remove_var(URL_ENV);
        assert_eq!(from_env.as_deref(), Some("https://env/latest.json"), "환경 변수가 이긴다");
    }

    #[test]
    fn injected_events_come_back_from_poll() {
        let mut ui = UpdateUi::new(&manifest(Some("https://h/latest.json"), false)).unwrap();
        ui.inject(Event::Failed("연결 실패".into()));
        // 상태는 바로 바뀐다.
        assert!(matches!(ui.state(), State::Failed(m) if m == "연결 실패"));
        // 그리고 poll 이 한 번 돌려준다.
        let events = ui.poll();
        assert_eq!(events.len(), 1);
        assert!(describe(&events[0]).unwrap().contains("연결 실패"));
        assert!(ui.poll().is_empty(), "두 번 돌려주지 않는다");
    }

    #[test]
    fn download_dir_is_per_app() {
        assert_ne!(download_dir("앱 하나"), download_dir("Another App"));
        assert!(download_dir("Demo App").ends_with("demo-app"));
    }

    #[test]
    fn progress_events_are_not_logged() {
        assert!(describe(&Event::Progress(Progress { received: 1, total: None })).is_none());
        assert!(describe(&Event::UpToDate).unwrap().contains("최신"));
        assert!(describe(&Event::Available(available("0.3.0"))).unwrap().contains("0.3.0"));
        assert!(describe(&Event::Failed("x".into())).unwrap().contains("실패"));
    }
}
