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
    path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
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
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| project.name.clone());
            base.join(format!("{stem}.runs"))
        }
    }
}

pub fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file = ProjectFile::from_json(&text).map_err(|e| e.to_string())?;
    Ok(Loaded { newer: file.newer_than_app(), project: file.project })
}

pub fn save(path: &Path, project: &Project) -> Result<(), String> {
    let json = ProjectFile::new(project.clone()).to_json();
    write_atomic(path, json.as_bytes())
}

/// 임시 파일 + rename. 같은 폴더에 써야 rename 이 같은 파일 시스템 안에서 일어난다.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "project".into());
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("{}: {e}", path.display()))
        }
    }
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
        assert_eq!(with_project_ext(PathBuf::from("/a/b.nlproj")), PathBuf::from("/a/b.nlproj"));
        assert_eq!(with_project_ext(PathBuf::from("/a/b.NLPROJ")), PathBuf::from("/a/b.NLPROJ"));
        assert_eq!(with_project_ext(PathBuf::from("/a/b.json")), PathBuf::from("/a/b.nlproj"));
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
