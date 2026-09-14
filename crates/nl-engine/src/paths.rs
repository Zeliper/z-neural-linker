//! 오류·로그에 넣을 짧은 경로.
//!
//! 사용자 홈 전체 경로가 그대로 찍히면 로그를 공유하거나 화면을 캡처할 때 **계정 이름과 폴더
//! 구조가 함께 나간다**. 엔진이 내는 메시지는 전부 여기를 거쳐 줄인다.
//!
//! 줄이는 규칙은 세 단계다.
//!
//! 1. `base` 가 있고 그 아래면 **상대 경로** (`runs/3f2a/final.safetensors`)
//! 2. 아니면 홈 아래는 **`~/…`** (`~/프로젝트/model.onnx`)
//! 3. 그것도 아니면 **파일 이름만** (`model.onnx`)
//!
//! 1 번이 가장 쓸모 있다 — 사용자가 프로젝트 폴더를 기준으로 생각하기 때문이다. 그래서 `base` 를
//! 아는 자리(데이터 로더·학습)는 넘기고, 모르는 자리(가중치 로더)는 2·3 번으로 떨어진다.

use std::path::{Path, PathBuf};

/// 오류·로그에 넣을 짧은 경로. `base` 를 알면 그 아래 상대 경로가 된다.
pub fn show(path: &Path, base: Option<&Path>) -> String {
    if let Some(b) = base {
        if let Ok(rel) = path.strip_prefix(b) {
            let s = rel.to_string_lossy();
            if !s.is_empty() {
                return s.into_owned();
            }
        }
    }
    if let Some(home) = home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            let s = rel.to_string_lossy();
            // 홈 자체를 가리키면 `~/` 가 아니라 `~` 다.
            return if s.is_empty() {
                "~".to_string()
            } else {
                format!("~/{s}")
            };
        }
    }
    file_name(path)
}

/// `base` 를 모르는 자리에서 쓰는 [`show`].
pub fn short(path: &Path) -> String {
    show(path, None)
}

/// 파일 이름만. 경로를 절대 흘리면 안 되는 자리에서 쓴다.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "(이름 없는 경로)".to_string(), |n| n.to_string_lossy().into_owned())
}

/// 홈 폴더. Windows 는 `HOME` 이 보통 없어 `USERPROFILE` 도 본다.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_relative_wins_over_the_home_form() {
        let base = Path::new("/home/사람/프로젝트");
        let p = base.join("runs/3f2a/final.safetensors");
        assert_eq!(show(&p, Some(base)), "runs/3f2a/final.safetensors");
    }

    #[test]
    fn outside_the_base_falls_back_to_the_home_form() {
        // `base` 를 줘도 그 아래가 아니면 다음 단계로 내려간다.
        let base = Path::new("/srv/프로젝트");
        let p = Path::new("/tmp/어딘가/model.onnx");
        // HOME 밖이기도 하므로 파일 이름만 남는다.
        let got = show(p, Some(base));
        assert_eq!(got, "model.onnx");
    }

    #[test]
    fn the_base_itself_is_not_an_empty_string() {
        // `path == base` 면 상대 경로가 빈 문자열이라 쓸모없다 — 다음 단계로 내려가야 한다.
        let base = Path::new("/srv/프로젝트");
        let got = show(base, Some(base));
        assert!(!got.is_empty(), "빈 문자열이 나왔다");
        assert_eq!(got, "프로젝트");
    }

    #[test]
    fn a_file_name_is_the_last_resort() {
        assert_eq!(file_name(Path::new("/a/b/c.txt")), "c.txt");
        assert_eq!(file_name(Path::new("/")), "(이름 없는 경로)");
    }

    #[test]
    fn nothing_leaks_the_parent_directories() {
        // 어떤 규칙으로 줄어들든 중간 폴더 이름이 그대로 남으면 안 된다.
        let p = Path::new("/home/비밀계정/깊은/폴더/구조/w.safetensors");
        for base in [None, Some(Path::new("/전혀/다른/곳"))] {
            let got = show(p, base);
            assert!(!got.contains("비밀계정"), "계정 이름이 샜다: {got}");
            assert!(!got.contains("깊은"), "폴더 구조가 샜다: {got}");
        }
    }

    /// `HOME` 이 있을 때만 의미 있는 검사. 없는 환경에서는 건너뛴다.
    #[test]
    fn home_becomes_a_tilde() {
        let Some(home) = home_dir() else {
            return;
        };
        let p = home.join("프로젝트/model.onnx");
        assert_eq!(show(&p, None), "~/프로젝트/model.onnx");
        assert_eq!(show(&home, None), "~", "홈 자체는 `~` 다");
    }
}
