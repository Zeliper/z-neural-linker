//! 자동 저장과 비정상 종료 복구 (trust-pms `pms-app/src/recovery.rs` 계승).
//!
//! 두 층으로 나뉜다.
//! - **복구 스냅샷**: 저장하지 않은 변경이 있으면 편집이 잠잠해진 뒤([`QUIET_SECS`]) 또는 늦어도
//!   [`MAX_INTERVAL_SECS`] 마다 문서 전체를 앱 데이터 폴더의 `recovery/` 에 적는다. 사용자의 원본
//!   파일은 건드리지 않는다. 명시적 저장·정상 종료에서 지워지므로, 다음 시작 때 남아 있으면
//!   비정상 종료였다는 뜻이고 앱이 복구를 제안한다.
//! - **원본 파일 자동 저장**(선택): 켜 두면 파일로 연 문서를 [`FILE_INTERVAL_SECS`] 마다 원본에 저장한다.
//!
//! 쓰기는 전부 "임시 파일에 쓰고 rename" 이라 쓰는 도중 죽어도 이전 파일이 남는다. 직렬화는 UI
//! 스레드에서 하고(문서를 빌린 채) IO 만 스레드로 넘기므로, 학습이나 파이프라인이 도는 중에도
//! 화면이 멎지 않는다.

use chrono::{DateTime, Local, Utc};
use nl_core::{Project, ProjectFile};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// 마지막 편집 뒤 이만큼 조용하면 스냅샷을 적는다.
pub const QUIET_SECS: f64 = 3.0;
/// 계속 편집 중이어도 이 간격마다는 적는다.
pub const MAX_INTERVAL_SECS: f64 = 60.0;
/// 원본 파일 자동 저장 간격.
pub const FILE_INTERVAL_SECS: f64 = 120.0;
/// 이보다 오래된 복구 파일은 목록을 만들 때 지운다 (고아 스냅샷이 무한히 쌓이지 않게).
pub const PRUNE_DAYS: i64 = 30;
/// 쓰기 실패 뒤 다시 시도할 때까지.
pub const RETRY_SECS: f64 = 10.0;

const SNAPSHOT_EXT: &str = "recovery.json";
const TMP_MARK: &str = ".tmp-";

/// 복구 파일 내용. 문서 자체(`ProjectFile`, 저장 포맷과 같다)에 출처를 덧붙인다.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryFile {
    /// 원본 파일 경로. 저장한 적 없는 새 문서면 `None`.
    pub original_path: Option<String>,
    pub project_name: String,
    pub saved_at: DateTime<Utc>,
    pub file: ProjectFile,
}

/// 시작 때 복구를 제안하며 보여 주는 요약.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub project_name: String,
    pub original_path: Option<String>,
    pub saved_at: DateTime<Utc>,
}

impl Entry {
    /// 모달에 보여 줄 시각 (현지 시간).
    pub fn when(&self) -> String {
        self.saved_at
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }
}

/// 앱 데이터 폴더 아래 `recovery/`. eframe 저장 위치를 못 얻으면 임시 폴더를 쓰되 사용자 이름을 섞어
/// 같은 호스트의 다른 사용자와 겹치지 않게 한다.
pub fn default_dir() -> PathBuf {
    eframe::storage_dir("neural-linker")
        .map(|d| d.join("recovery"))
        .unwrap_or_else(|| {
            let who = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("neural-linker-recovery-{who}"))
        })
}

/// 문서 하나의 복구 키. 파일로 연 문서는 경로로, 새 문서는 프로젝트 id 로 — 같은 파일을 다시 열면
/// 같은 스냅샷 자리를 쓰고 다른 문서끼리는 겹치지 않는다.
///
/// 경로 해시는 FNV-1a 다. 표준 `DefaultHasher` 는 릴리스 간 값이 달라질 수 있어 쓰지 않는다.
pub fn key_for(file_path: Option<&Path>, project: &Project) -> String {
    match file_path {
        Some(p) => format!("file-{:016x}", fnv1a(canonical_or_best(p).to_string_lossy().as_bytes())),
        None => format!("new-{}", project.id.0.simple()),
    }
}

/// 파일이 아직 없어도(첫 저장 전) 부모 폴더까지는 정규화해, 파일이 생긴 뒤와 같은 키가 나오게 한다.
fn canonical_or_best(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => std::fs::canonicalize(parent)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn snapshot_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.{SNAPSHOT_EXT}"))
}

/// 임시 파일 이름의 일련번호. 같은 프로세스의 두 쓰기가 같은 임시 파일을 나눠 쓰지 않게.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_path(target: &Path) -> PathBuf {
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(format!(
        "{TMP_MARK}{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(tmp)
}

/// 스냅샷 내용을 만든다. 직렬화는 호출 스레드에서 하고 쓰기만 넘긴다.
pub fn encode(project: &Project, original_path: Option<&Path>) -> Result<Vec<u8>, String> {
    let rf = RecoveryFile {
        original_path: original_path.map(|p| p.display().to_string()),
        project_name: project.name.clone(),
        saved_at: Utc::now(),
        file: ProjectFile::new(project.clone()),
    };
    serde_json::to_vec(&rf).map_err(|e| e.to_string())
}

pub fn load(path: &Path) -> Result<RecoveryFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("형식 오류: {e}"))?;
    // 문서 본문은 저장 포맷과 같으므로 같은 경로로 읽는다(옛 버전 이행 규칙을 그대로 탄다).
    let Some(file_value) = v.get("file") else {
        return Err("문서가 없습니다".to_owned());
    };
    let file = ProjectFile::from_json(&file_value.to_string()).map_err(|e| format!("형식 오류: {e}"))?;
    Ok(RecoveryFile {
        original_path: v.get("original_path").and_then(|x| x.as_str()).map(str::to_owned),
        project_name: v
            .get("project_name")
            .and_then(|x| x.as_str())
            .unwrap_or("(이름 없음)")
            .to_owned(),
        saved_at: v
            .get("saved_at")
            .and_then(|x| serde_json::from_value(x.clone()).ok())
            .unwrap_or_else(Utc::now),
        file,
    })
}

/// 폴더의 복구 파일 목록 (최근 것 먼저).
///
/// 읽을 수 없는 파일은 건너뛰고, [`PRUNE_DAYS`] 보다 오래된 것과 죽은 프로세스가 남긴 임시
/// 파일(`*.tmp-*`)은 지운다.
pub fn list(dir: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let cutoff = Utc::now() - chrono::Duration::days(PRUNE_DAYS);
    let mut out: Vec<Entry> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.contains(TMP_MARK) && !name.contains(&format!("{TMP_MARK}{}-", std::process::id())) {
                // 지금 이 프로세스가 쓰는 중인 임시 파일이 아니면 죽은 쓰기의 잔재다.
                let _ = std::fs::remove_file(p);
                return false;
            }
            name.ends_with(&format!(".{SNAPSHOT_EXT}"))
        })
        .filter_map(|p| {
            let rf = load(&p).ok()?;
            if rf.saved_at < cutoff {
                let _ = std::fs::remove_file(&p);
                return None;
            }
            Some(Entry {
                path: p,
                project_name: rf.project_name,
                original_path: rf.original_path,
                saved_at: rf.saved_at,
            })
        })
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.saved_at));
    out
}

pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// 백그라운드 쓰기의 결과 슬롯. UI 스레드가 프레임마다 비워 토스트로 알린다.
type ErrorSlot = Arc<Mutex<Option<String>>>;

/// 쓰기 스레드끼리의 순서 보장. 늦게 끝난 옛 스냅샷이 새것을 덮거나, 지운 스냅샷을 되살리지 않게 한다.
#[derive(Debug, Default)]
struct WriteGate {
    /// 실제로 rename 까지 끝난 마지막 편집 번호.
    last_written: u64,
    /// 이 번호 이하의 쓰기는 버린다 (스냅샷을 지운 시점의 편집 번호).
    cancelled_upto: u64,
}

/// "언제 스냅샷을 적을까" 를 정하는 상태 기계. 파일 IO 를 갖지 않아 테스트가 쉽다.
#[derive(Debug)]
pub struct AutoSaver {
    /// 마지막으로 스냅샷에 담긴 편집 번호.
    last_seq: u64,
    /// 마지막으로 **본** 편집 번호와 그때 시각. 디바운스는 이 시각 기준이라 타이핑뿐 아니라
    /// 노드 추가·되돌리기도 3초 잠잠할 때까지 기다린다 (편집 하나마다 직렬화하지 않게).
    seen_seq: u64,
    last_change: f64,
    /// 마지막 스냅샷 뒤 첫 변경을 본 시각. 60초 상한은 여기서부터 잰다.
    first_pending: f64,
    /// 마지막 스냅샷 시각(앱 시계).
    last_snapshot: f64,
    /// 마지막 원본 파일 자동 저장 시각.
    last_file_save: f64,
    key: Option<String>,
    error: ErrorSlot,
    gate: Arc<Mutex<WriteGate>>,
}

impl Default for AutoSaver {
    fn default() -> Self {
        Self {
            last_seq: 0,
            seen_seq: 0,
            last_change: f64::NEG_INFINITY,
            first_pending: f64::INFINITY,
            last_snapshot: f64::NEG_INFINITY,
            last_file_save: f64::NEG_INFINITY,
            key: None,
            error: ErrorSlot::default(),
            gate: Arc::default(),
        }
    }
}

impl AutoSaver {
    /// 이 문서(`key`)에 스냅샷을 적을 때가 됐는가. 문서가 바뀌면(`key` 변경) 상태를 새로 시작한다.
    pub fn snapshot_due(&mut self, key: &str, modified: bool, seq: u64, now: f64) -> bool {
        if self.key.as_deref() != Some(key) {
            self.key = Some(key.to_owned());
            self.last_seq = seq;
            self.seen_seq = seq;
            self.last_snapshot = now;
            self.last_file_save = now;
            return false;
        }
        if seq != self.seen_seq {
            if self.seen_seq == self.last_seq {
                self.first_pending = now;
            }
            self.seen_seq = seq;
            self.last_change = now;
        }
        if !modified || seq == self.last_seq {
            return false;
        }
        now - self.last_change >= QUIET_SECS || now - self.first_pending >= MAX_INTERVAL_SECS
    }

    /// 다음 판정까지 남은 시간(초). 잠잠해질 때 깨어나려고 repaint 를 예약하는 데 쓴다.
    pub fn wake_in(&self, modified: bool, seq: u64, now: f64) -> Option<f64> {
        if !modified || seq == self.last_seq {
            return None;
        }
        let quiet = (self.last_change + QUIET_SECS - now).max(0.0);
        let cap = (self.first_pending + MAX_INTERVAL_SECS - now).max(0.0);
        Some(quiet.min(cap) + 0.05)
    }

    /// 스냅샷을 적었다고 기록한다.
    pub fn mark_snapshot(&mut self, seq: u64, now: f64) {
        self.last_seq = seq;
        self.seen_seq = seq;
        self.last_snapshot = now;
        self.first_pending = f64::INFINITY;
    }

    /// 스냅샷을 지웠다. 아직 끝나지 않은 쓰기가 지운 파일을 되살리지 않게 이 번호까지 취소한다.
    pub fn mark_discarded(&mut self, seq: u64, now: f64) {
        self.mark_snapshot(seq, now);
        self.gate.lock().unwrap_or_else(|p| p.into_inner()).cancelled_upto = seq;
    }

    /// 원본 파일 자동 저장 때가 됐는가.
    pub fn file_save_due(&mut self, enabled: bool, has_file: bool, modified: bool, now: f64) -> bool {
        if !enabled || !has_file || !modified {
            return false;
        }
        now - self.last_file_save >= FILE_INTERVAL_SECS
    }

    pub fn mark_file_save(&mut self, now: f64) {
        self.last_file_save = now;
    }

    /// 스냅샷을 백그라운드 스레드로 쓴다. 직렬화는 여기서(문서를 빌린 채) 하고 IO 만 넘긴다.
    pub fn write_snapshot(
        &mut self,
        dir: &Path,
        key: &str,
        project: &Project,
        original: Option<&Path>,
        seq: u64,
        now: f64,
    ) {
        let path = snapshot_path(dir, key);
        match encode(project, original) {
            Ok(bytes) => {
                let slot = self.error.clone();
                let gate = self.gate.clone();
                let spawned = std::thread::Builder::new().name("nl-recovery".into()).spawn(move || {
                    if let Err(e) = write_gated(&gate, seq, &path, &bytes) {
                        log::warn!("복구 스냅샷 쓰기 실패: {e}");
                        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                    }
                });
                if let Err(e) = spawned {
                    *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                }
                self.mark_snapshot(seq, now);
            }
            Err(e) => {
                *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                // 같은 오류를 프레임마다 되풀이하지 않게 이번 편집은 적은 것으로 친다.
                self.mark_snapshot(seq, now);
            }
        }
    }

    /// 백그라운드 쓰기 오류를 한 번 꺼낸다.
    ///
    /// 꺼내면 같은 편집을 [`RETRY_SECS`] 뒤에 다시 적도록 예약한다 — 일시적 실패(디스크가 찼다든지)
    /// 뒤에 다음 편집이 있을 때까지 낡은 스냅샷만 남지 않게.
    pub fn take_error(&mut self, now: f64) -> Option<String> {
        let e = self.error.lock().unwrap_or_else(|p| p.into_inner()).take();
        if e.is_some() {
            self.last_seq = self.last_seq.wrapping_sub(1);
            self.first_pending = now;
            self.last_change = now + RETRY_SECS - QUIET_SECS;
        }
        e
    }
}

/// 임시 파일은 각자 쓰고, rename 은 게이트 아래에서 편집 번호 순서로만 한다.
fn write_gated(gate: &Mutex<WriteGate>, seq: u64, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    let written = std::fs::write(&tmp, bytes).and_then(|()| {
        // 스냅샷은 문서 전체다 — 같은 호스트의 다른 사용자가 읽지 못하게.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    });
    let result = written.and_then(|()| {
        let mut g = gate.lock().unwrap_or_else(|p| p.into_inner());
        if seq <= g.last_written || seq <= g.cancelled_upto {
            // 더 새로운 스냅샷이 이미 있거나 그사이 지워졌다 — 이 쓰기는 버린다.
            return Ok(());
        }
        std::fs::rename(&tmp, path)?;
        g.last_written = seq;
        Ok(())
    });
    let _ = std::fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nl-recovery-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn snapshot_round_trips_and_lists_newest_first() {
        let dir = tmp_dir("roundtrip");
        let p = nl_core::sample::xor_project();
        let key = key_for(None, &p);
        assert!(key.starts_with("new-"), "저장한 적 없는 문서는 프로젝트 id 로 구분한다");
        crate::project::write_atomic(&snapshot_path(&dir, &key), &encode(&p, None).unwrap()).unwrap();

        let q = nl_core::sample::quadrants_cnn_project();
        let file = dir.join("orig.nlproj");
        std::fs::write(&file, "x").unwrap();
        let key_q = key_for(Some(&file), &q);
        assert!(key_q.starts_with("file-"), "파일로 연 문서는 경로로 구분한다");
        // 두 번째가 더 나중에 저장된 것으로 바꿔 둔다.
        let mut v: serde_json::Value = serde_json::from_slice(&encode(&q, Some(&file)).unwrap()).unwrap();
        v["saved_at"] = serde_json::json!("2030-01-01T00:00:00Z");
        crate::project::write_atomic(&snapshot_path(&dir, &key_q), &serde_json::to_vec(&v).unwrap()).unwrap();

        let entries = list(&dir);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].original_path.as_deref(),
            Some(file.display().to_string()).as_deref()
        );
        assert_eq!(entries[1].project_name, p.name);

        // 문서가 그대로 돌아온다.
        let back = load(&snapshot_path(&dir, &key)).unwrap();
        assert_eq!(back.file.project, p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_snapshots_and_dead_temp_files_are_pruned() {
        let dir = tmp_dir("prune");
        let p = nl_core::sample::xor_project();
        let mut v: serde_json::Value = serde_json::from_slice(&encode(&p, None).unwrap()).unwrap();
        v["saved_at"] = serde_json::json!("2000-01-01T00:00:00Z");
        let old = snapshot_path(&dir, "file-0000000000000001");
        std::fs::write(&old, serde_json::to_vec(&v).unwrap()).unwrap();
        // 다른 프로세스가 남긴 임시 파일.
        let dead = dir.join(format!("x.{SNAPSHOT_EXT}{TMP_MARK}999999-0"));
        std::fs::write(&dead, "쓰다 만 것").unwrap();

        assert!(list(&dir).is_empty(), "오래된 스냅샷은 목록에 없다");
        assert!(!old.exists(), "오래된 스냅샷은 지워진다");
        assert!(!dead.exists(), "죽은 임시 파일도 지워진다");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 새 문서로 시작하면 첫 프레임에 바로 적지 않는다 — 키를 등록만 한다.
    #[test]
    fn a_new_document_registers_without_writing() {
        let mut a = AutoSaver::default();
        assert!(!a.snapshot_due("k", true, 5, 100.0));
        assert_eq!(a.wake_in(true, 5, 100.0), None, "아직 새 편집이 없다");
    }

    #[test]
    fn a_snapshot_waits_for_quiet_then_fires_once() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);

        // 편집 직후에는 적지 않는다.
        assert!(!a.snapshot_due("k", true, 1, 1.0));
        assert!(!a.snapshot_due("k", true, 1, 1.0 + QUIET_SECS - 0.1));
        // 잠잠해지면 적는다.
        assert!(a.snapshot_due("k", true, 1, 1.0 + QUIET_SECS));
        a.mark_snapshot(1, 1.0 + QUIET_SECS);
        // 같은 편집으로 두 번 적지 않는다.
        assert!(!a.snapshot_due("k", true, 1, 100.0));
        assert_eq!(a.wake_in(true, 1, 100.0), None);
    }

    #[test]
    fn continuous_editing_still_gets_a_snapshot_at_the_cap() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);
        let mut seq = 0;
        let mut t = 0.0;
        // 1초마다 계속 고친다 — 잠잠해지는 순간이 없다.
        let mut fired = None;
        for _ in 0..120 {
            t += 1.0;
            seq += 1;
            if a.snapshot_due("k", true, seq, t) {
                fired = Some(t);
                break;
            }
        }
        let t = fired.expect("상한에서는 반드시 적는다");
        assert!(t <= MAX_INTERVAL_SECS + 1.0, "{t}");
    }

    #[test]
    fn saving_clears_the_pending_state_and_switching_documents_resets() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);
        // 편집을 본 프레임에는 적지 않고, 잠잠해진 뒤에 적는다.
        assert!(!a.snapshot_due("k", true, 1, 10.0));
        assert!(a.snapshot_due("k", true, 1, 10.0 + QUIET_SECS));
        a.mark_snapshot(1, 10.0 + QUIET_SECS);
        // 저장하면 modified 가 꺼지고 더 적을 것이 없다.
        assert!(!a.snapshot_due("k", false, 1, 20.0));

        // 다른 문서로 바꾸면 그 문서 기준으로 다시 센다.
        assert!(!a.snapshot_due("다른", true, 99, 21.0), "키가 바뀐 첫 프레임은 등록만");
        assert!(!a.snapshot_due("다른", true, 99, 40.0), "같은 편집 번호면 적지 않는다");
        assert!(!a.snapshot_due("다른", true, 100, 41.0), "새 편집을 본 프레임에는 아직");
        assert!(a.snapshot_due("다른", true, 100, 41.0 + QUIET_SECS));
    }

    #[test]
    fn wake_in_shrinks_as_quiet_time_passes() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);
        a.snapshot_due("k", true, 1, 1.0);
        let early = a.wake_in(true, 1, 1.0).unwrap();
        let later = a.wake_in(true, 1, 2.0).unwrap();
        assert!(later < early, "{later} < {early}");
        assert!(early <= QUIET_SECS + 0.1);
    }

    #[test]
    fn file_autosave_needs_a_file_and_unsaved_changes() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);
        assert!(!a.file_save_due(false, true, true, 1000.0), "꺼져 있으면 안 한다");
        assert!(!a.file_save_due(true, false, true, 1000.0), "파일이 없으면 안 한다");
        assert!(!a.file_save_due(true, true, false, 1000.0), "고친 것이 없으면 안 한다");
        assert!(
            !a.file_save_due(true, true, true, FILE_INTERVAL_SECS - 1.0),
            "아직 이르다"
        );
        assert!(a.file_save_due(true, true, true, FILE_INTERVAL_SECS + 1.0));
        a.mark_file_save(FILE_INTERVAL_SECS + 1.0);
        assert!(!a.file_save_due(true, true, true, FILE_INTERVAL_SECS + 2.0));
    }

    /// 쓰기에 실패하면 같은 편집을 다시 시도한다 — 낡은 스냅샷만 남으면 안 된다.
    #[test]
    fn a_write_error_schedules_a_retry() {
        let mut a = AutoSaver::default();
        a.snapshot_due("k", false, 0, 0.0);
        a.mark_snapshot(7, 10.0);
        *a.error.lock().unwrap() = Some("디스크가 가득 찼습니다".into());

        let e = a.take_error(10.0).expect("오류를 꺼낸다");
        assert!(e.contains("디스크"));
        assert_eq!(a.take_error(10.0), None, "오류는 한 번만 나온다");
        // 같은 편집 번호로 다시 적을 때가 온다.
        assert!(!a.snapshot_due("k", true, 7, 10.0 + RETRY_SECS - 1.0));
        assert!(a.snapshot_due("k", true, 7, 10.0 + RETRY_SECS));
    }

    /// 스냅샷을 지운 뒤 뒤늦게 끝난 쓰기가 파일을 되살리면 안 된다.
    #[test]
    fn a_late_write_does_not_resurrect_a_discarded_snapshot() {
        let dir = tmp_dir("gate");
        let path = snapshot_path(&dir, "file-0000000000000002");
        let gate = Mutex::new(WriteGate {
            last_written: 0,
            cancelled_upto: 5,
        });
        write_gated(&gate, 5, &path, b"old").unwrap();
        assert!(!path.exists(), "취소된 번호는 쓰지 않는다");
        write_gated(&gate, 6, &path, b"new").unwrap();
        assert!(path.exists(), "그 뒤 편집은 정상적으로 쓴다");
        // 순서가 뒤집힌 옛 쓰기는 새것을 덮지 않는다.
        write_gated(&gate, 6, &path, b"older").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
