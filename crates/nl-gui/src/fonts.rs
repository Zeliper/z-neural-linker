//! 한글 폰트 폴백 (trust-pms `font_definitions` 계승). egui 기본 폰트에 CJK 글리프가 없어 시스템 폰트를 얹는다.

use egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const CANDIDATES: &[&str] = &[
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
    "/usr/share/fonts/nanum/NanumGothic.ttf",
    "C:\\Windows\\Fonts\\malgun.ttf",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
];

pub fn font_definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    for path in CANDIDATES {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data.insert("cjk".into(), Arc::new(FontData::from_owned(bytes)));
            for fam in [FontFamily::Proportional, FontFamily::Monospace] {
                defs.families.entry(fam).or_default().push("cjk".into());
            }
            log::info!("CJK 폰트: {path}");
            break;
        }
    }
    defs
}
