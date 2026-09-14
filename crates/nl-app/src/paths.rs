//! 프로젝트가 가리키는 파일이 프로젝트 폴더 안에 있는지 본다 (보안 리뷰 H9).
//!
//! `.nlproj` 는 그냥 JSON 이라 남에게 받아 열 수 있다. 그 안의 경로가 `/etc/shadow` 나
//! `../../.ssh/id_rsa` 를 가리키면, 스캔 한 번이나 빌드 한 번으로 그 파일이 읽히거나
//! 배포 번들에 실려 나간다. 여기서는 그런 경로를 **찾아서 알리기만** 한다 —
//! 막는 일은 부르는 쪽이 한다(문제 탭은 경고, 빌드는 오류).
//!
//! 판정은 문자열로만 한다. `canonicalize` 는 파일이 있어야 하고 심볼릭 링크를 따라가
//! "아직 없는 산출물 폴더" 같은 흔한 경우를 다루지 못한다.

use nl_core::dataset::DataSource;
use nl_core::{Project, Severity};
use std::path::{Component, Path};

/// 프로젝트 폴더 밖을 가리키는 항목 하나.
#[derive(Clone, Debug, PartialEq)]
pub struct Outside {
    /// 사람이 읽는 대상 이름 ("데이터셋 'XOR 표'").
    pub what: String,
    /// 파일에 적힌 그대로의 경로.
    pub path: String,
    /// 왜 밖인가.
    pub why: Reason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// 절대 경로 (`/etc/...`, `C:\...`).
    Absolute,
    /// `..` 로 프로젝트 폴더를 벗어난다.
    Escapes,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Absolute => "절대 경로",
            Reason::Escapes => "상위 폴더로 벗어남",
        }
    }
}

impl Outside {
    /// 문제 탭 한 줄.
    pub fn message(&self) -> String {
        format!(
            "{}: 프로젝트 폴더 밖을 가리킵니다 ({}) — {}",
            self.what,
            self.why.label(),
            self.path
        )
    }
}

/// 프로젝트 폴더 밖을 가리키는 상대 경로인가. 프로젝트 폴더 기준으로만 판단한다.
pub fn outside_project(rel: &str) -> Option<Reason> {
    let p = Path::new(rel.trim());
    if p.as_os_str().is_empty() {
        return None;
    }
    // 절대 경로와 윈도우 드라이브·UNC 접두사.
    if p.is_absolute()
        || p.components()
            .next()
            .is_some_and(|c| matches!(c, Component::Prefix(_) | Component::RootDir))
    {
        return Some(Reason::Absolute);
    }
    // `a/../b` 는 안에 머물지만 `../b` 는 벗어난다. 깊이를 세어 본다.
    let mut depth: i32 = 0;
    for c in p.components() {
        match c {
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Some(Reason::Escapes);
                }
            }
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return Some(Reason::Absolute),
        }
    }
    None
}

/// 프로젝트가 가리키는 파일 중 폴더 밖에 있는 것 전부.
pub fn scan(project: &Project) -> Vec<Outside> {
    let mut out = Vec::new();
    let mut push = |what: String, path: &str| {
        if let Some(why) = outside_project(path) {
            out.push(Outside {
                what,
                path: path.to_string(),
                why,
            });
        }
    };

    for d in project.datasets.values() {
        let path = match &d.source {
            DataSource::Csv { path, .. } => Some(path),
            DataSource::ImageFolder { path } => Some(path),
            DataSource::Recorded { path } => Some(path),
            DataSource::Synthetic { .. } => None,
        };
        if let Some(p) = path {
            push(format!("데이터셋 '{}'", d.name), p);
        }
    }
    for m in project.models.values() {
        if let Some(w) = &m.weights {
            push(format!("모델 '{}' 의 가중치", m.name), w);
        }
    }
    if let Some(b) = &project.settings.build {
        if let Some(icon) = &b.icon {
            push("빌드 아이콘".to_string(), icon);
        }
    }
    out
}

/// 문제 탭에 넣을 경고들. 막지는 않는다 — 직접 만든 프로젝트라면 바깥 경로가 정상일 수 있다.
pub fn issues(project: &Project) -> Vec<nl_core::Issue> {
    scan(project)
        .into_iter()
        .map(|o| nl_core::Issue {
            severity: Severity::Warning,
            at: nl_core::validate::Where::Project,
            message: o.message(),
        })
        .collect()
}

/// 번들에 실을 수 없는 경로가 있으면 왜 안 되는지. 빌드는 이것을 오류로 다룬다.
pub fn build_blockers(project: &Project) -> Vec<String> {
    scan(project)
        .into_iter()
        .map(|o| {
            format!(
                "{} 가 프로젝트 폴더 밖입니다 ({}) — 번들에 넣지 않습니다: {}",
                o.what,
                o.why.label(),
                o.path
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_and_escaping_paths_are_outside() {
        assert_eq!(outside_project("data/x.csv"), None);
        assert_eq!(outside_project("./a/b"), None);
        // 들어갔다 나오면 제자리다.
        assert_eq!(outside_project("a/../b.csv"), None);
        assert_eq!(outside_project(""), None, "빈 경로는 볼 것이 없다");

        assert_eq!(outside_project("/etc/shadow"), Some(Reason::Absolute));
        assert_eq!(outside_project("../secrets.csv"), Some(Reason::Escapes));
        assert_eq!(outside_project("a/../../etc/passwd"), Some(Reason::Escapes));
        // 앞뒤 공백으로 판정을 피할 수 없다.
        assert_eq!(outside_project("  /etc/passwd "), Some(Reason::Absolute));
    }

    #[test]
    fn scan_finds_dataset_and_weight_paths() {
        let mut p = nl_core::sample::xor_project();
        let did = *p.datasets.keys().next().unwrap();
        p.datasets.get_mut(&did).unwrap().source = DataSource::Csv {
            path: "../../etc/passwd".into(),
            input_cols: vec![],
            target_cols: vec![],
            header: true,
        };
        let mid = *p.models.keys().next().unwrap();
        p.models.get_mut(&mid).unwrap().weights = Some("/tmp/evil.safetensors".into());

        let found = scan(&p);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|o| o.why == Reason::Escapes));
        assert!(found.iter().any(|o| o.why == Reason::Absolute));
        // 빌드는 막고, 문제 탭은 경고로만 알린다.
        assert_eq!(build_blockers(&p).len(), 2);
        assert!(issues(&p).iter().all(|i| i.severity == Severity::Warning));
    }

    /// 샘플은 깨끗해야 한다 — 기본 프로젝트가 경고를 달고 열리면 경고가 무뎌진다.
    #[test]
    fn samples_have_no_outside_paths() {
        for (name, make) in nl_core::sample::SAMPLES {
            assert!(scan(&make()).is_empty(), "{name}");
        }
    }
}
