//! `.nlproj` 파일 입출력과 최근 파일 목록.
//!
//! 모든 쓰기는 **원자적**이다: 같은 폴더에 임시 파일을 쓰고 `rename` 으로 갈아끼운다.
//! 쓰는 도중 죽어도 원본이 반쯤 잘린 채 남지 않는다 (trust-pms 계승).

use nl_core::model::PROJECT_EXT;
use nl_core::{Project, ProjectFile};
use std::path::{Path, PathBuf};

/// 최근 파일 목록에 남기는 개수.
pub const RECENT_LIMIT: usize = 5;

/// 열린 문서 하나. `newer` 는 앱보다 새 포맷 버전이라 모르는 필드를 버렸다는 뜻이다.
pub struct Loaded {
    pub project: Project,
    pub newer: bool,
}

/// 경로에 `.nlproj` 확장자를 붙인다 (저장 대화상자가 확장자를 빼고 돌려줄 때).
pub fn with_project_ext(path: PathBuf) -> PathBuf {
    match path.extension() {
        Some(e) if e.eq_ignore_ascii_case(PROJECT_EXT) => path,
        _ => path.with_extension(PROJECT_EXT),
    }
}

/// 프로젝트 파일이 있는 폴더. 데이터셋 상대 경로·실행 폴더의 기준이다.
pub fn base_dir(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 실행 기록 폴더: 설정에 적힌 곳, 없으면 `<프로젝트 파일 이름>.runs/`.
pub fn runs_dir(path: &Path, project: &Project) -> PathBuf {
    let base = base_dir(path);
    match &project.settings.runs_dir {
        Some(d) if !d.trim().is_empty() => {
            let p = PathBuf::from(d);
            if p.is_absolute() {
                p
            } else {
                base.join(p)
            }
        }
        _ => {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| project.name.clone());
            base.join(format!("{stem}.runs"))
        }
    }
}

pub fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file = ProjectFile::from_json(&text).map_err(|e| e.to_string())?;
    Ok(Loaded {
        newer: file.newer_than_app(),
        project: file.project,
    })
}

pub fn save(path: &Path, project: &Project) -> Result<(), String> {
    let json = ProjectFile::new(project.clone()).to_json();
    // 덮어쓰기 전에 지금 내용을 `.bak` 으로 한 벌 남긴다 (보안 리뷰 L2).
    // 한 벌뿐이라 무한히 쌓이지 않고, 방금 저장이 잘못됐을 때 되돌릴 자리는 생긴다.
    backup(path);
    write_atomic(path, json.as_bytes())
}

/// 저장 직전 백업. 실패해도 저장은 계속한다 — 백업이 없다고 저장을 막을 이유는 없다.
fn backup(path: &Path) {
    if !path.is_file() {
        return;
    }
    let mut bak = path.as_os_str().to_owned();
    bak.push(".bak");
    if let Err(e) = std::fs::copy(path, PathBuf::from(&bak)) {
        log::warn!("백업을 남기지 못했습니다: {} — {e}", path.display());
    }
}

/// 파일이 마지막으로 바뀐 시각. 외부에서 고쳤는지 보는 데 쓴다.
///
/// 못 읽으면 `None` 이고, 그때는 비교하지 않는다 — 시각을 모른다고 저장을 막으면 네트워크
/// 파일 시스템처럼 mtime 이 없는 곳에서 아무것도 저장할 수 없다.
pub fn modified_at(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// 우리가 마지막으로 본 뒤에 파일이 바뀌었는가.
pub fn changed_outside(path: &Path, seen: Option<std::time::SystemTime>) -> bool {
    match (modified_at(path), seen) {
        (Some(now), Some(seen)) => now != seen,
        // 우리가 만든 적 없는 파일이거나 시각을 모르면 비교할 것이 없다.
        _ => false,
    }
}

/// 임시 파일 + rename. 같은 폴더에 써야 rename 이 같은 파일 시스템 안에서 일어난다.
///
/// 임시 파일은 `tempfile` 이 만든다 (보안 리뷰 L1). 예전에는 이름이 `.<파일>.tmp<pid>` 로 예측
/// 가능했고 `O_EXCL` 없이 만들어, 남이 먼저 그 자리에 심볼릭 링크를 놓아 두면 엉뚱한 파일을 덮어썼다.
/// 실패하면 임시 파일이 자동으로 지워지고, rename 뒤에는 부모 폴더까지 fsync 해서 이름이 실제로
/// 디스크에 남게 한다 — 그러지 않으면 전원이 끊겼을 때 새 파일도 옛 파일도 없는 상태가 될 수 있다.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let mut tmp = tempfile::Builder::new()
        .prefix(".nl-save-")
        .suffix(".tmp")
        .tempfile_in(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    // 프로젝트 파일에는 HTTP 토큰 같은 것이 들어 있다. 같은 호스트의 다른 사용자가 읽지 못하게.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        let _ = tmp.as_file().set_permissions(std::fs::Permissions::from_mode(mode));
    }
    tmp.write_all(bytes).map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(path)
        .map_err(|e| format!("{}: {}", path.display(), e.error))?;

    // 이름 바꾸기 자체도 디스크에 내려야 한다. 실패해도 파일은 이미 자리에 있으니 오류로 보지 않는다.
    if let Ok(d) = std::fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// 최근 파일 목록. 저장은 eframe 스토리지에 한 줄 JSON 으로.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Recent(pub Vec<String>);

impl Recent {
    /// 맨 앞으로 올리고 중복을 지운다. 목록은 `RECENT_LIMIT` 개까지.
    pub fn push(&mut self, path: &Path) {
        let s = path.display().to_string();
        self.0.retain(|p| p != &s);
        self.0.insert(0, s);
        self.0.truncate(RECENT_LIMIT);
    }

    pub fn remove(&mut self, path: &str) {
        self.0.retain(|p| p != path);
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// CSV 의 첫 줄에서 열 이름을 읽는다. 헤더가 없으면 `열 0`, `열 1` … 로 번호를 만든다.
/// (데이터 뷰의 입력·타깃 열 체크박스가 쓴다 — 스캔은 엔진 몫이고 여기서는 이름만 본다)
pub fn csv_columns(path: &Path, header: bool) -> Result<Vec<String>, String> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut iter = rdr.records();
    let first = match iter.next() {
        Some(r) => r.map_err(|e| e.to_string())?,
        None => return Err("빈 파일".into()),
    };
    if header {
        Ok(first.iter().map(|s| s.trim().to_string()).collect())
    } else {
        Ok((0..first.len()).map(|i| i.to_string()).collect())
    }
}

#[cfg(test)]
mod tests {
    /// 저장은 임시 파일에 쓰고 이름을 바꾼다. 임시 파일 이름은 예측할 수 없어야 하고,
    /// 실패하든 성공하든 뒤에 남지 않아야 한다 (보안 리뷰 L1).
    #[test]
    fn atomic_writes_leave_no_temp_files_behind() {
        let dir = std::env::temp_dir().join(format!("nl-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("p.nlproj");

        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        // 폴더에 남은 것은 대상 파일 하나뿐이다.
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["p.nlproj".to_string()], "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 저장은 덮어쓰기 전에 `.bak` 한 벌을 남긴다 (보안 리뷰 L2).
    #[test]
    fn saving_keeps_one_backup() {
        let dir = std::env::temp_dir().join(format!("nl-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("p.nlproj");

        let first = nl_core::sample::xor_project();
        save(&target, &first).unwrap();
        // 첫 저장에는 덮어쓸 것이 없으니 백업도 없다.
        assert!(!dir.join("p.nlproj.bak").exists());

        let second = nl_core::sample::quadrants_cnn_project();
        save(&target, &second).unwrap();
        let bak = load(&dir.join("p.nlproj.bak")).unwrap();
        assert_eq!(bak.project, first, "백업에는 덮어쓰기 직전 내용이 있어야 한다");
        assert_eq!(load(&target).unwrap().project, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 바깥에서 파일이 바뀌었는지 본다. 시각을 모르면 비교하지 않는다 (보안 리뷰 L2).
    #[test]
    fn outside_changes_are_detected_only_when_we_have_a_timestamp() {
        let dir = std::env::temp_dir().join(format!("nl-mtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("p.nlproj");
        std::fs::write(&target, "a").unwrap();

        let seen = modified_at(&target);
        assert!(seen.is_some());
        assert!(!changed_outside(&target, seen), "아무도 건드리지 않았다");

        // 남이 고친 것처럼 다시 쓴다. mtime 해상도가 낮은 파일 시스템이면 시각이 같을 수 있어,
        // 그때는 이 확인을 건너뛴다 (판정 자체는 위아래 단언이 덮는다).
        std::fs::write(&target, "bbbbbbbb").unwrap();
        if modified_at(&target) != seen {
            assert!(changed_outside(&target, seen), "바뀐 것을 알아채야 한다");
        }

        // 시각을 모르면 비교하지 않는다 — 저장을 막아서는 안 된다.
        assert!(!changed_outside(&target, None));
        assert!(!changed_outside(&dir.join("없는파일"), seen));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nl-app-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("demo.nlproj");
        let project = crate::sample::xor_project();
        save(&path, &project).unwrap();
        let back = load(&path).unwrap();
        assert!(!back.newer);
        assert_eq!(back.project, project);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tmp_dir("atomic");
        let path = dir.join("a.nlproj");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        // 임시 파일이 남아 있으면 폴더에 항목이 둘이다.
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1, "임시 파일이 남았다");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_is_added_but_not_doubled() {
        assert_eq!(with_project_ext(PathBuf::from("/a/b")), PathBuf::from("/a/b.nlproj"));
        assert_eq!(
            with_project_ext(PathBuf::from("/a/b.nlproj")),
            PathBuf::from("/a/b.nlproj")
        );
        assert_eq!(
            with_project_ext(PathBuf::from("/a/b.NLPROJ")),
            PathBuf::from("/a/b.NLPROJ")
        );
        assert_eq!(
            with_project_ext(PathBuf::from("/a/b.json")),
            PathBuf::from("/a/b.nlproj")
        );
    }

    #[test]
    fn runs_dir_follows_the_file_name_then_the_setting() {
        let mut p = crate::sample::xor_project();
        let path = PathBuf::from("/tmp/proj/demo.nlproj");
        assert_eq!(runs_dir(&path, &p), PathBuf::from("/tmp/proj/demo.runs"));
        p.settings.runs_dir = Some("실행".into());
        assert_eq!(runs_dir(&path, &p), PathBuf::from("/tmp/proj/실행"));
        p.settings.runs_dir = Some("/var/runs".into());
        assert_eq!(runs_dir(&path, &p), PathBuf::from("/var/runs"));
    }

    #[test]
    fn recent_list_moves_to_front_and_caps() {
        let mut r = Recent::default();
        for i in 0..7 {
            r.push(Path::new(&format!("/p/{i}.nlproj")));
        }
        assert_eq!(r.0.len(), RECENT_LIMIT);
        assert_eq!(r.0[0], "/p/6.nlproj");
        r.push(Path::new("/p/3.nlproj"));
        assert_eq!(r.0[0], "/p/3.nlproj");
        assert_eq!(r.0.iter().filter(|p| p.as_str() == "/p/3.nlproj").count(), 1);
        r.remove("/p/3.nlproj");
        assert!(!r.0.contains(&"/p/3.nlproj".to_string()));
    }

    #[test]
    fn csv_columns_reads_header_or_numbers() {
        let dir = tmp_dir("csv");
        let path = dir.join("t.csv");
        std::fs::write(&path, "a,b,c\n1,2,3\n").unwrap();
        assert_eq!(csv_columns(&path, true).unwrap(), vec!["a", "b", "c"]);
        assert_eq!(csv_columns(&path, false).unwrap(), vec!["0", "1", "2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
