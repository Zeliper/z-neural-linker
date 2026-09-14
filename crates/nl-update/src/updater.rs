//! 상태 기계. 네트워크 작업은 전부 별도 스레드에서 돌고 결과는 채널로 온다.
//! `Idle → Checking → Available → Downloading → Downloaded → Applying → Applied` 순으로 흐르고,
//! 어느 단계에서든 실패하면 `Failed` 로 간다.
//!
//! UI 스레드는 프레임마다 [`Updater::poll`] 만 부르면 된다.

use crate::{apply_to, check_signed, current_exe, download, require_https, Applied, AssetKind, Available, Progress};
use crossbeam_channel::{Receiver, Sender};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 백그라운드 작업이 보내는 이벤트.
#[derive(Clone, Debug)]
pub enum Event {
    Checking,
    /// 서명을 검증할 수 없어 업데이트를 끈다.
    Disabled(String),
    /// 확인 끝: 새 버전 없음.
    UpToDate,
    Available(Available),
    Progress(Progress),
    Downloaded(PathBuf),
    Applying,
    Applied(Applied),
    Failed(String),
}

/// 지금 어디까지 왔는지.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Idle,
    /// 공개키가 없어 업데이트를 쓸 수 없다. 서명을 검증할 수 없으면 확인 자체를 하지 않는다.
    /// 호출자는 이 상태에서 업데이트 UI 를 아예 감춰야 한다.
    Disabled(String),
    Checking,
    /// 확인했고 최신이다.
    UpToDate,
    Available(Available),
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Downloaded {
        path: PathBuf,
        kind: AssetKind,
        /// 서명된 매니페스트가 말한 해시. 적용 직전에 다시 맞춰 본다 —
        /// 내려받은 뒤 적용을 누르기까지 몇 시간이 지날 수 있고 그 사이에 파일이 바뀔 수 있다.
        sha256: String,
    },
    Applying,
    Applied(Applied),
    Failed(String),
}

impl State {
    /// 백그라운드 작업이 도는 중인가.
    pub fn is_busy(&self) -> bool {
        matches!(self, State::Checking | State::Downloading { .. } | State::Applying)
    }

    /// 업데이트를 쓸 수 없는 상태인가. UI 를 감출지 정하는 데 쓴다.
    pub fn is_disabled(&self) -> bool {
        matches!(self, State::Disabled(_))
    }

    /// 사용자에게 보여 줄 한 줄.
    pub fn message(&self) -> String {
        match self {
            State::Idle => "확인한 적 없음".into(),
            State::Disabled(why) => format!("업데이트 사용 불가: {why}"),
            State::Checking => "확인 중…".into(),
            State::UpToDate => "최신입니다".into(),
            State::Available(a) => format!("새 버전 v{} 사용 가능", a.version),
            State::Downloading { received, total } => {
                let progress = Progress {
                    received: *received,
                    total: *total,
                };
                match progress.fraction() {
                    Some(f) => format!("내려받는 중… {:.0}%", f * 100.0),
                    None => format!("내려받는 중… {received} 바이트"),
                }
            }
            State::Downloaded { .. } => "내려받았습니다 — 적용하면 다시 시작됩니다".into(),
            State::Applying => "적용 중…".into(),
            State::Applied(a) => a.message(),
            State::Failed(e) => format!("실패: {e}"),
        }
    }
}

/// 업데이트 상태 기계. 앱과 런타임이 같은 것을 쓴다.
pub struct Updater {
    manifest_url: String,
    current: semver::Version,
    public_key: Option<String>,
    timeout: Duration,
    state: State,
    rx: Option<Receiver<Event>>,
    /// 확인 단계에서 알아낸 자산. 다운로드가 끝난 뒤 종류를 알아내는 데 쓴다.
    last_available: Option<Available>,
}

impl Updater {
    /// 매니페스트 주소는 호출자가 정한다 — 빌더와 배포 앱의 배포 채널이 다르다.
    ///
    /// 주소가 https 가 아니거나([`require_https`]) 공개키가 없으면 [`State::Disabled`] 로 시작하고
    /// [`Updater::check`] 는 아무 일도 하지 않는다. 서명을 검증할 수 없는 채로 실행 파일을 바꿔치우는
    /// 경로를 열어 두지 않기 위해서다.
    pub fn new(manifest_url: impl Into<String>, current: semver::Version) -> Self {
        let manifest_url = manifest_url.into();
        let state = match require_https(&manifest_url) {
            Ok(()) => State::Disabled("서명 공개키가 없습니다".into()),
            Err(e) => State::Disabled(format!("{e:#}")),
        };
        Self {
            manifest_url,
            current,
            public_key: None,
            timeout: crate::DEFAULT_CHECK_TIMEOUT,
            state,
            rx: None,
            last_available: None,
        }
    }

    /// minisign 공개키. **이것이 있어야 업데이트가 켜진다.** 없으면 [`State::Disabled`] 로 남는다.
    /// 빈 문자열은 없는 것으로 본다.
    pub fn with_public_key(mut self, key: Option<impl Into<String>>) -> Self {
        self.public_key = key.map(Into::into).filter(|k: &String| !k.trim().is_empty());
        self.state = match self.blocked_reason() {
            Some(why) => State::Disabled(why),
            None => State::Idle,
        };
        self
    }

    /// 업데이트를 막는 이유. 없으면 `None`.
    fn blocked_reason(&self) -> Option<String> {
        if let Err(e) = require_https(&self.manifest_url) {
            return Some(format!("{e:#}"));
        }
        self.public_key.is_none().then(|| "서명 공개키가 없습니다".to_string())
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_busy(&self) -> bool {
        self.state.is_busy()
    }

    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    /// 주소를 바꾼다. https 가 아니거나 공개키가 없으면 곧바로 [`State::Disabled`] 가 된다.
    pub fn set_manifest_url(&mut self, url: impl Into<String>) {
        self.manifest_url = url.into();
        self.state = match self.blocked_reason() {
            Some(why) => State::Disabled(why),
            None => State::Idle,
        };
    }

    /// 찾아낸 새 버전 (있으면).
    pub fn available(&self) -> Option<&Available> {
        match &self.state {
            State::Available(a) => Some(a),
            _ => None,
        }
    }

    /// 내려받아 둔 파일 (있으면).
    pub fn downloaded(&self) -> Option<&Path> {
        match &self.state {
            State::Downloaded { path, .. } => Some(path),
            _ => None,
        }
    }

    /// 매니페스트를 받아 비교한다 (백그라운드). 이전에 찾았거나 내려받은 것은 버린다 —
    /// 그 사이 더 새 버전이 올라왔으면 옛 자산을 "새 버전"이라며 설치하면 안 된다.
    pub fn check(&mut self) {
        if self.is_busy() {
            return;
        }
        // 검증할 수 없으면 확인조차 하지 않는다. 여기서 막지 않으면 서명 없는 매니페스트를 믿게 된다.
        if let Some(why) = self.blocked_reason() {
            log::warn!("업데이트 확인을 건너뜁니다: {why}");
            self.state = State::Disabled(why);
            return;
        }
        let Some(key) = self.public_key.clone() else { return };
        let tx = self.arm(State::Checking);
        let (url, current, timeout) = (self.manifest_url.clone(), self.current.clone(), self.timeout);
        spawn("nl-update-check", move || {
            let _ = tx.send(Event::Checking);
            let ev = match check_signed(&url, &current, timeout, &key) {
                Ok(Some(available)) => Event::Available(available),
                Ok(None) => Event::UpToDate,
                Err(e) => Event::Failed(format!("{e:#}")),
            };
            let _ = tx.send(ev);
        });
    }

    /// 찾아낸 자산을 `dir` 에 내려받는다 (백그라운드). `Available` 상태가 아니면 아무 일도 하지 않는다.
    pub fn download(&mut self, dir: PathBuf) {
        let Some(available) = self.available().cloned() else {
            return;
        };
        let tx = self.arm(State::Downloading {
            received: 0,
            total: None,
        });
        spawn("nl-update-download", move || {
            let (ptx, prx) = crossbeam_channel::unbounded::<Progress>();
            let forward = tx.clone();
            let pump = std::thread::spawn(move || {
                while let Ok(p) = prx.recv() {
                    let _ = forward.send(Event::Progress(p));
                }
            });
            let ev = match download(&available.asset, &dir, &ptx) {
                Ok(path) => Event::Downloaded(path),
                Err(e) => Event::Failed(format!("{e:#}")),
            };
            drop(ptx);
            let _ = pump.join();
            let _ = tx.send(ev);
        });
    }

    /// 내려받은 자산을 적용한다 (백그라운드). `Downloaded` 상태가 아니면 아무 일도 하지 않는다.
    /// 성공하면 호출자가 앱을 끝내야 한다.
    pub fn apply(&mut self) {
        let State::Downloaded { path, kind, sha256 } = self.state.clone() else {
            return;
        };
        let tx = self.arm(State::Applying);
        spawn("nl-update-apply", move || {
            let _ = tx.send(Event::Applying);
            let ev = match current_exe() {
                Ok(exe) => match apply_to(&path, kind, &exe, true, &sha256) {
                    Ok(applied) => Event::Applied(applied),
                    Err(e) => Event::Failed(format!("{e:#}")),
                },
                Err(e) => Event::Failed(format!("{e:#}")),
            };
            let _ = tx.send(ev);
        });
    }

    /// 프레임마다 부른다. 도착한 이벤트를 전부 소비해 상태를 갱신하고 그 이벤트들을 돌려준다.
    pub fn poll(&mut self) -> Vec<Event> {
        let Some(rx) = &self.rx else { return Vec::new() };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        for ev in &out {
            self.on_event(ev.clone());
        }
        if !self.is_busy() {
            self.rx = None;
        }
        out
    }

    /// 이벤트 하나로 상태를 옮긴다. 다른 이벤트 원천을 붙이거나 테스트할 때 직접 부를 수 있다.
    pub fn on_event(&mut self, ev: Event) {
        // 자산 종류는 확인 단계에서만 알 수 있다. 여기서 붙잡아 두었다가 다운로드가 끝나면 쓴다.
        match &ev {
            Event::Available(a) => self.last_available = Some(a.clone()),
            Event::Checking => self.last_available = None,
            _ => {}
        }
        self.state = match ev {
            Event::Checking => State::Checking,
            Event::Disabled(why) => State::Disabled(why),
            Event::UpToDate => State::UpToDate,
            Event::Available(a) => State::Available(a),
            Event::Progress(p) => State::Downloading {
                received: p.received,
                total: p.total,
            },
            Event::Downloaded(path) => match &self.last_available {
                // 종류와 해시는 **서명된 매니페스트**에서만 온다. 파일 이름은 공격자가 정하는 URL 에서
                // 오므로 확장자로 짐작하면 임의의 `.exe` 를 설치 프로그램으로 실행하게 된다.
                Some(a) => State::Downloaded {
                    path,
                    kind: a.asset.kind,
                    sha256: a.asset.sha256.clone(),
                },
                None => {
                    State::Failed("어떤 자산을 받았는지 알 수 없습니다 — 확인 결과가 없으면 적용하지 않습니다".into())
                }
            },
            Event::Applying => State::Applying,
            Event::Applied(a) => State::Applied(a),
            Event::Failed(e) => State::Failed(e),
        };
    }

    /// 새 작업을 시작하며 채널을 건다.
    fn arm(&mut self, state: State) -> Sender<Event> {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.rx = Some(rx);
        self.state = state;
        tx
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.to_string()).spawn(f) {
        log::error!("{name} 스레드를 만들지 못했습니다: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Asset;

    fn available(kind: AssetKind) -> Available {
        Available {
            version: semver::Version::new(0, 2, 0),
            notes: "고침".into(),
            asset: Asset {
                url: "https://h/app".into(),
                sha256: "ab".into(),
                kind,
                size: 10,
            },
            target: "linux-x86_64".into(),
        }
    }

    /// 제대로 설정된 업데이터 — https 주소와 공개키가 다 있다.
    fn updater() -> Updater {
        Updater::new("https://h/latest.json", semver::Version::new(0, 1, 0)).with_public_key(Some("RWQ…"))
    }

    #[test]
    fn starts_idle_and_not_busy() {
        let u = updater();
        assert_eq!(*u.state(), State::Idle);
        assert!(!u.is_busy());
        assert!(u.available().is_none());
        assert!(u.downloaded().is_none());
        assert_eq!(u.manifest_url(), "https://h/latest.json");
    }

    #[test]
    fn happy_path_walks_through_every_state() {
        let mut u = updater();

        u.on_event(Event::Checking);
        assert_eq!(*u.state(), State::Checking);
        assert!(u.is_busy());

        u.on_event(Event::Available(available(AssetKind::Binary)));
        assert_eq!(u.available().unwrap().version, semver::Version::new(0, 2, 0));
        assert!(!u.is_busy());

        u.on_event(Event::Progress(Progress {
            received: 5,
            total: Some(10),
        }));
        assert_eq!(
            *u.state(),
            State::Downloading {
                received: 5,
                total: Some(10)
            }
        );
        assert!(u.state().message().contains("50%"));

        u.on_event(Event::Downloaded(PathBuf::from("/tmp/app")));
        assert_eq!(u.downloaded(), Some(Path::new("/tmp/app")));

        u.on_event(Event::Applying);
        assert!(u.is_busy());

        u.on_event(Event::Applied(Applied::Replaced {
            exe: PathBuf::from("/tmp/app"),
            relaunched: true,
        }));
        assert!(u.state().message().contains("다시 시작"));
        assert!(!u.is_busy());
    }

    #[test]
    fn up_to_date_and_failure_end_the_busy_state() {
        let mut u = updater();
        u.on_event(Event::Checking);
        u.on_event(Event::UpToDate);
        assert_eq!(*u.state(), State::UpToDate);
        assert!(!u.is_busy());

        u.on_event(Event::Checking);
        u.on_event(Event::Failed("연결 실패".into()));
        assert!(matches!(u.state(), State::Failed(m) if m == "연결 실패"));
        assert!(!u.is_busy());
        assert!(u.state().message().contains("연결 실패"));
    }

    /// 자산 종류는 확인 단계에서 알아낸 것을 다운로드 완료 상태까지 들고 간다.
    #[test]
    fn download_keeps_the_asset_kind_from_the_check() {
        let mut u = updater();
        u.on_event(Event::Available(available(AssetKind::Installer)));
        u.on_event(Event::Progress(Progress {
            received: 1,
            total: None,
        }));
        u.on_event(Event::Downloaded(PathBuf::from("/tmp/setup.bin")));
        // 확장자는 exe 가 아니지만 매니페스트가 installer 라고 했으므로 installer 다.
        assert_eq!(
            *u.state(),
            State::Downloaded {
                path: "/tmp/setup.bin".into(),
                kind: AssetKind::Installer,
                sha256: "ab".into()
            }
        );
    }

    /// L11: 확인 결과가 없으면 확장자로 짐작하지 않고 실패한다 — 파일 이름은 공격자가 정하는 URL 에서 온다.
    #[test]
    fn without_a_check_result_a_download_is_not_applied() {
        let mut u = updater();
        u.on_event(Event::Downloaded(PathBuf::from("/tmp/setup.exe")));
        assert!(
            matches!(u.state(), State::Failed(m) if m.contains("알 수 없습니다")),
            "{:?}",
            u.state()
        );

        // 확인 결과가 있으면 그 종류를 그대로 쓴다.
        let mut u = updater();
        u.on_event(Event::Available(available(AssetKind::Binary)));
        u.on_event(Event::Downloaded(PathBuf::from("/tmp/app")));
        assert!(matches!(
            u.state(),
            State::Downloaded {
                kind: AssetKind::Binary,
                ..
            }
        ));
    }

    #[test]
    fn download_and_apply_do_nothing_in_the_wrong_state() {
        let mut u = updater();
        u.download(std::env::temp_dir());
        assert_eq!(*u.state(), State::Idle, "Available 이 아니면 다운로드하지 않는다");
        u.apply();
        assert_eq!(*u.state(), State::Idle, "Downloaded 가 아니면 적용하지 않는다");
    }

    #[test]
    fn without_a_key_it_starts_disabled_and_check_does_nothing() {
        let mut u = Updater::new("https://h/latest.json", semver::Version::new(0, 1, 0));
        assert!(u.state().is_disabled(), "{:?}", u.state());
        u.check();
        assert!(u.state().is_disabled(), "확인을 시작하면 안 됩니다: {:?}", u.state());
        assert!(!u.is_busy());
    }

    #[test]
    fn a_non_https_url_is_disabled_too() {
        let u = Updater::new("http://h/latest.json", semver::Version::new(0, 1, 0)).with_public_key(Some("RWQ…"));
        assert!(u.state().is_disabled(), "{:?}", u.state());
    }

    #[test]
    fn an_empty_key_counts_as_no_key() {
        let u = updater().with_public_key(Some("   "));
        assert!(u.public_key.is_none());
        assert!(u.state().is_disabled(), "{:?}", u.state());
    }

    #[test]
    fn poll_without_a_running_task_is_empty() {
        let mut u = updater();
        assert!(u.poll().is_empty());
    }

    #[test]
    fn public_key_and_timeout_are_configurable() {
        let u = updater()
            .with_public_key(Some("RWQ…"))
            .with_timeout(Duration::from_secs(3));
        assert_eq!(u.public_key.as_deref(), Some("RWQ…"));
        assert_eq!(u.timeout, Duration::from_secs(3));
        let u = updater().with_public_key(None::<String>);
        assert!(u.public_key.is_none());
        assert!(u.state().is_disabled(), "키를 지우면 업데이트가 꺼진다");
    }
}
