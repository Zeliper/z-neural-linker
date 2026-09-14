//! 한글 폰트 폴백.
//!
//! 기본은 `nl_gui::font_definitions()` 다. 그 함수는 **고정된 경로 목록**을 위에서부터 훑는다.
//! 목록에 Fedora 경로가 들어간 뒤로 이 개발 기계에서는 여기 있는 폴백이 돌지 않는다.
//!
//! 그래도 남겨 두는 이유: 고정 목록은 사용자가 직접 설치한 폰트(`~/.local/share/fonts`)나 목록에
//! 없는 배포판 경로를 알지 못한다. 거기서는 한글이 전부 □ 로 나온다. 여기서는
//! **nl-gui 가 CJK 폰트를 못 찾았을 때만** 폰트 폴더를 실제로 훑어 하나를 얹는다.
//! nl-gui 가 찾았으면 아무 일도 하지 않으므로 비용은 0 이다.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// nl-gui 가 찾은 폰트를 넣는 키. 이 키가 있으면 폴백은 건너뛴다.
const NL_GUI_KEY: &str = "cjk";
/// 폴백으로 얹는 폰트 키.
const FALLBACK_KEY: &str = "cjk-fallback";

/// 폰트 폴더를 훑는 최대 깊이. 배포판들은 `<루트>/<패키지>/<파일>` 정도까지만 쓴다.
const MAX_DEPTH: usize = 3;

/// 앱이 쓰는 폰트 정의. nl-gui 결과에 한글이 없으면 시스템에서 찾아 채운다.
pub fn font_definitions() -> FontDefinitions {
    let mut defs = nl_gui::font_definitions();
    if defs.font_data.contains_key(NL_GUI_KEY) {
        return defs;
    }
    let Some(path) = find_cjk_font() else {
        log::warn!("한글 폰트를 찾지 못했습니다 — 한국어가 □ 로 보일 수 있습니다");
        return defs;
    };
    match std::fs::read(&path) {
        Ok(bytes) => {
            log::info!("CJK 폰트(폴백): {}", path.display());
            defs.font_data.insert(FALLBACK_KEY.into(), Arc::new(FontData::from_owned(bytes)));
            for fam in [FontFamily::Proportional, FontFamily::Monospace] {
                defs.families.entry(fam).or_default().push(FALLBACK_KEY.into());
            }
        }
        Err(e) => log::warn!("한글 폰트를 읽지 못했습니다: {} — {e}", path.display()),
    }
    defs
}

/// 폰트를 찾을 뿌리들. 없는 경로는 그냥 건너뛴다.
fn font_roots() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
        PathBuf::from("/Library/Fonts"),
        PathBuf::from("/System/Library/Fonts"),
        PathBuf::from("C:\\Windows\\Fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        v.push(home.join(".local/share/fonts"));
        v.push(home.join(".fonts"));
    }
    v
}

/// 파일 이름의 점수. 클수록 먼저 고른다. 0 이면 후보가 아니다.
///
/// 한국어가 목표이므로 한글 글리프를 가진 폰트를 우선한다: Noto Sans CJK(통합) → 나눔 → 맑은 고딕 →
/// Apple SD Gothic → Noto Sans KR. 굵기는 Regular 를 먼저, 가변 폰트(VF)는 뒤로 미룬다.
pub fn font_score(file_name: &str) -> u32 {
    let lower = file_name.to_ascii_lowercase();
    if !(lower.ends_with(".ttc") || lower.ends_with(".ttf") || lower.ends_with(".otf") || lower.ends_with(".otc")) {
        return 0;
    }
    let family: u32 = if lower.contains("notosanscjk") {
        100
    } else if lower.contains("nanumgothic") || lower.contains("nanumbarungothic") {
        90
    } else if lower.contains("malgun") {
        85
    } else if lower.contains("applesdgothic") {
        80
    } else if lower.contains("notosanskr") || lower.contains("noto_sans_kr") {
        70
    } else if lower.contains("notoserifcjk") {
        50
    } else {
        return 0;
    };
    // 모노 전용은 본문 폰트로 쓰기 나쁘고, 가변 폰트는 ttf-parser 지원이 갈린다.
    let penalty: u32 = u32::from(lower.contains("mono")) * 20 + u32::from(lower.contains("-vf") || lower.contains("_vf")) * 15;
    let weight: u32 = if lower.contains("regular") {
        9
    } else if lower.contains("medium") {
        6
    } else if lower.contains("bold") || lower.contains("black") || lower.contains("light") || lower.contains("thin") {
        1
    } else {
        7
    };
    (family + weight).saturating_sub(penalty)
}

/// 시스템에서 가장 점수가 높은 한글 폰트 파일.
fn find_cjk_font() -> Option<PathBuf> {
    let mut best: Option<(u32, PathBuf)> = None;
    for root in font_roots() {
        walk(&root, 0, &mut |path, name| {
            let score = font_score(name);
            if score > 0 && best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
                best = Some((score, path.to_path_buf()));
            }
        });
    }
    best.map(|(_, p)| p)
}

fn walk(dir: &Path, depth: usize, visit: &mut impl FnMut(&Path, &str)) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, depth + 1, visit),
            Ok(_) => {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    visit(&path, name);
                }
            }
            Err(_) => {}
        }
    }
}

/// UI 에 쓰는 아이콘 글리프 전부. 여기 없는 글자를 새로 쓰면 두부(□)로 보일 수 있으니
/// 반드시 이 목록에 넣고 `ui_icons_are_renderable` 로 확인한다.
pub const UI_ICONS: &[&str] = &[
    // 툴바
    "🗋", "📂", "💾", "⟲", "⟳", "🖳", "▶", "⏸", "⏹", "📋", "▤", "☰",
    // 아웃라인
    "📁", "⚛", "🗄", "🖼", "⏺", "✨", "🔌", "⇄", "＋",
    // 하단 도크
    "⚠", "🕘", "✖", "✔",
    // 캔버스 메뉴
    "⎘", "⊘", "🗑",
    // 데이터·학습 뷰
    "🔍", "👁", "▲", "▼", "●", "■", "⛶",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_korean_capable_families_score() {
        assert!(font_score("NotoSansCJK-Regular.ttc") > 0);
        assert!(font_score("NanumGothic.ttf") > 0);
        assert!(font_score("malgun.ttf") > 0);
        assert_eq!(font_score("DejaVuSans.ttf"), 0, "한글이 없는 폰트는 후보가 아니다");
        assert_eq!(font_score("NotoSansCJK-Regular.png"), 0, "폰트 파일이 아니다");
    }

    #[test]
    fn regular_beats_bold_and_variable_and_mono() {
        let regular = font_score("NotoSansCJK-Regular.ttc");
        assert!(regular > font_score("NotoSansCJK-Bold.ttc"));
        assert!(regular > font_score("NotoSansCJK-Medium.ttc"));
        assert!(regular > font_score("NotoSansMonoCJK-VF.ttc"));
        assert!(regular > font_score("NotoSansCJK-VF.ttc"));
    }

    #[test]
    fn the_unified_cjk_font_wins_over_the_korean_only_one() {
        assert!(font_score("NotoSansCJK-Regular.ttc") > font_score("NotoSansKR-Regular.otf"));
        assert!(font_score("NanumGothic.ttf") > font_score("NotoSerifCJK-Regular.ttc"));
    }

    /// UI 아이콘이 전부 그려지는가. 한글 폰트가 없는 환경에서는 건너뛴다
    /// (일부 기호는 CJK 폰트에서만 온다 — trust-pms 와 같은 판정 방식).
    #[test]
    fn ui_icons_are_renderable() {
        use eframe::egui::epaint::text::{Fonts, TextOptions};
        let defs = font_definitions();
        if !defs.font_data.contains_key(NL_GUI_KEY) && !defs.font_data.contains_key(FALLBACK_KEY) {
            eprintln!("한글 폰트 없음 — 아이콘 검사 건너뜀");
            return;
        }
        // `Font::has_glyph` 는 대체 문자(◻)를 가진 폰트의 글자를 전부 "없음" 으로 보고한다.
        // 커버리지 맵을 직접 본다.
        let mut fonts = Fonts::new(TextOptions::default(), defs);
        let chars = fonts.fonts.font(&FontFamily::Proportional).characters().clone();
        for icon in UI_ICONS {
            for c in icon.chars() {
                assert!(chars.contains_key(&c), "아이콘 {icon:?}(U+{:04X}) 글리프가 없어 두부로 보인다", c as u32);
            }
        }
    }

    /// nl-gui 가 찾았으면 폴백은 얹지 않는다 — 폰트가 두 번 실리면 글리프 선택이 흔들린다.
    #[test]
    fn the_fallback_stays_out_of_the_way_when_nl_gui_found_a_font() {
        let defs = font_definitions();
        if defs.font_data.contains_key(NL_GUI_KEY) {
            assert!(!defs.font_data.contains_key(FALLBACK_KEY), "nl-gui 가 찾았는데 폴백까지 실렸다");
        }
    }

    /// 이 기계에 한글 폰트가 있으면 폴백이든 nl-gui 든 반드시 하나는 실린다.
    /// (없는 환경에서는 조용히 통과 — CI 컨테이너에 폰트가 없을 수 있다)
    #[test]
    fn definitions_carry_a_korean_font_when_the_system_has_one() {
        let has_font = find_cjk_font().is_some();
        let defs = font_definitions();
        if has_font {
            let names: Vec<&String> = defs.font_data.keys().collect();
            assert!(
                names.iter().any(|n| n.as_str() == NL_GUI_KEY || n.as_str() == FALLBACK_KEY),
                "한글 폰트가 실리지 않았다: {names:?}"
            );
            let prop = defs.families.get(&FontFamily::Proportional).expect("비례 폰트 가족");
            assert!(prop.iter().any(|n| n == NL_GUI_KEY || n == FALLBACK_KEY));
        }
    }
}
