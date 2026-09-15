//! 프로젝트 폴더 바깥을 건드리지 않게 하는 경로 규칙.
//!
//! 파이프라인이 읽고 쓰는 파일, 모델 가중치, 가져온 ONNX — 경로가 문서에서 오는 것은 모두
//! 이 검사를 지난다. 신뢰할 수 없는 `.nlproj`/`.nlapp` 을 받아 열 수 있기 때문이다.
//!
//! **한 곳에만 둔다.** 러너와 빌드 양쪽에서 쓰는데 복제하면 한쪽만 고쳐질 수 있고, 이건
//! 한쪽만 고쳐지면 조용히 뚫리는 종류의 검사다.

use std::path::{Path, PathBuf};

/// 프로젝트·번들이 만지는 파일은 **모두 `base_dir` 안**이어야 한다.
///
/// 신뢰할 수 없는 `.nlapp` 이 `Sink::File { path: "~/.ssh/authorized_keys" }` 같은 것을 들고 올 수 있다.
/// 그래서 다음을 모두 거부한다.
///
/// - 절대 경로 (`/etc/passwd`, `C:\Windows\...`)
/// - `..` 로 올라가는 경로, 루트·드라이브 접두사
/// - 심볼릭 링크 (경로 중간이든 마지막이든) — 밖으로 빠져나가는 가장 흔한 길이다
///
/// 마지막 요소는 아직 없을 수 있으므로(쓰기 대상) **부모까지** 실제 경로로 풀어 확인하고,
/// 파일 이름만 그 위에 붙인다.
pub fn resolve_inside(base_dir: &Path, path: &str) -> Result<PathBuf, String> {
    use std::path::Component;
    let p = Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "절대 경로는 쓸 수 없다: {path} (프로젝트 폴더 기준 상대 경로만)"
        ));
    }
    let mut rel = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(seg) => rel.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "경로가 프로젝트 폴더 밖을 가리킨다: {path} ('..' 는 쓸 수 없다)"
                ))
            }
            Component::RootDir | Component::Prefix(_) => return Err(format!("절대 경로는 쓸 수 없다: {path}")),
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(format!("파일 이름이 비어 있다: {path:?}"));
    }
    let joined = base_dir.join(&rel);

    // 부모까지 실제 경로로 풀어 기준 폴더 안인지 본다. 아직 없는 폴더는 통과시킨다
    // (만들 때 그 위 단계가 검사를 이미 통과했다).
    let base_real = base_dir.canonicalize().unwrap_or_else(|_| base_dir.to_path_buf());
    if let Some(parent) = joined.parent() {
        if let Ok(real) = parent.canonicalize() {
            if !real.starts_with(&base_real) {
                return Err(format!(
                    "경로가 프로젝트 폴더 밖을 가리킨다: {path} (심볼릭 링크로 빠져나간다)"
                ));
            }
        }
    }
    // 마지막 요소가 이미 심볼릭 링크면 그 너머로 쓰게 된다.
    if let Ok(meta) = std::fs::symlink_metadata(&joined) {
        if meta.file_type().is_symlink() {
            return Err(format!("심볼릭 링크는 쓸 수 없다: {path}"));
        }
    }
    Ok(joined)
}
