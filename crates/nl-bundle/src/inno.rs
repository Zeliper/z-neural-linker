//! Windows 설치 프로그램(Inno Setup 6) 생성. `packaging/windows/neural-linker.iss` 를 본떠 앱별 `.iss` 를 만들고,
//! 컴파일러가 있으면 바로 `setup.exe` 까지 만든다. 없으면 `.iss` 와 준비된 파일만 남겨 사용자가 직접 컴파일할 수 있게 한다.

use crate::{sha256_hex, slugify, Artifact};
use anyhow::Context;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::process::Command;

const APP_ISS: &str = include_str!("../templates/app.iss");

/// Neural Linker 이름공간 UUID. 기존 `packaging/windows/neural-linker.iss` 의 AppId 를 그대로 쓴다.
/// 앱마다 다른 AppId 를 이 이름공간 아래에서 만들어 다른 제품과 충돌하지 않게 한다.
pub const NL_NAMESPACE: [u8; 16] = [
    0x7B, 0x1E, 0x2D, 0x44, 0x5A, 0x6C, 0x4F, 0x0B, 0x9E, 0x31, 0x2D, 0x8C, 0x0F, 0x5A, 0x7E, 0x19,
];

/// 찾아낸 Inno Setup 컴파일러.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnoSetup {
    /// `ISCC.exe`(또는 `iscc`) 경로.
    pub path: PathBuf,
    /// Linux·macOS 에서 wine 을 거쳐 실행해야 하는가.
    pub via_wine: bool,
}

impl InnoSetup {
    /// 사람이 읽는 한 줄 설명 (도구 상태 표용).
    pub fn describe(&self) -> String {
        if self.via_wine {
            format!("{} (wine 경유)", self.path.display())
        } else {
            self.path.display().to_string()
        }
    }
}

// ───────────────────────────── 컴파일러 탐색 ─────────────────────────────

/// 설치 프로그램 컴파일러를 찾는다. 순서는 `NL_ISCC` 환경 변수 → `PATH` → Windows 표준 경로 → wine 접두사.
pub fn find_inno_setup() -> Option<InnoSetup> {
    if let Some(raw) = std::env::var_os("NL_ISCC") {
        let path = PathBuf::from(raw);
        // 절대 경로만 받는다. 상대 경로는 그때그때의 현재 폴더에 딸려 가 무엇이 실행될지 알 수 없다.
        if !path.is_absolute() {
            log::warn!("NL_ISCC 는 절대 경로여야 합니다 (무시합니다): {}", path.display());
        } else if path.is_file() {
            let via_wine = !cfg!(windows) && is_exe_name(&path);
            return Some(InnoSetup { path, via_wine });
        } else {
            log::warn!("NL_ISCC 가 가리키는 파일이 없습니다: {}", path.display());
        }
    }
    first_existing(&iscc_candidates())
}

/// 후보 목록에서 실제로 있는 첫 항목. 탐색 순서를 테스트하기 위해 분리해 두었다.
fn first_existing(candidates: &[(PathBuf, bool)]) -> Option<InnoSetup> {
    candidates
        .iter()
        .find(|(path, _)| path.is_file())
        .map(|(path, via_wine)| InnoSetup {
            path: path.clone(),
            via_wine: *via_wine,
        })
}

/// `(경로, wine 경유 여부)` 후보를 우선순위 순으로.
fn iscc_candidates() -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = Vec::new();

    // ① PATH 에 있는 네이티브 컴파일러.
    for name in ["iscc", "ISCC.exe", "iscc.exe"] {
        if let Some(p) = which(name) {
            out.push((p, false));
        }
    }

    // ② Windows 표준 설치 경로.
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(dir) = std::env::var_os(var) {
            out.push((PathBuf::from(dir).join("Inno Setup 6").join("ISCC.exe"), false));
        }
    }

    // ③ Linux·macOS: wine 접두사 안의 설치본. wine 이 없으면 의미가 없다.
    if !cfg!(windows) && which("wine").is_some() {
        for prefix in wine_prefixes() {
            let drive_c = prefix.join("drive_c");
            out.push((
                drive_c
                    .join("Program Files (x86)")
                    .join("Inno Setup 6")
                    .join("ISCC.exe"),
                true,
            ));
            out.push((
                drive_c.join("Program Files").join("Inno Setup 6").join("ISCC.exe"),
                true,
            ));
        }
    }
    out
}

fn wine_prefixes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("WINEPREFIX") {
        out.push(PathBuf::from(p));
    }
    if let Some(home) = home_dir() {
        out.push(home.join(".wine"));
    }
    out
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `PATH` 안에서 실행 파일을 찾는다.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn is_exe_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

// ───────────────────────────── AppId ─────────────────────────────

/// 앱 이름 + 발행자로 결정적인 Inno Setup `AppId` (중괄호 없는 대문자 UUID).
///
/// 같은 앱이면 언제나 같은 값이라 새 버전이 이전 설치를 덮어쓴다. **발행자까지 넣는 이유는**
/// 이름만 베낀 악성 앱이 정품 설치를 "업그레이드"로 덮어쓰지 못하게 하기 위해서다.
pub fn app_id(app_name: &str, publisher: &str) -> String {
    format_uuid(&uuid_v5(
        &NL_NAMESPACE,
        &format!("{}\u{1f}{}", app_name.trim(), publisher.trim()),
    ))
}

/// RFC 4122 UUID v5 (SHA-1 기반, 이름 기반 결정적).
pub fn uuid_v5(namespace: &[u8; 16], name: &str) -> [u8; 16] {
    let mut h = Sha1::new();
    h.update(namespace);
    h.update(name.as_bytes());
    let digest = h.finalize();

    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out[6] = (out[6] & 0x0F) | 0x50; // 버전 5
    out[8] = (out[8] & 0x3F) | 0x80; // RFC 4122 변형
    out
}

/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX` (대문자). Inno Setup 이 쓰는 표기.
pub fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ───────────────────────────── .iss 생성 ─────────────────────────────

/// `.iss` 값의 공통 정리. Inno Setup 은 `{` 를 `{{` 로 적고, 줄바꿈은 지시문을 깨뜨린다.
///
/// 제어문자는 전부 공백으로 접는다 — 개행 하나로 새 지시문이나 새 섹션을 만들 수 있기 때문이다.
/// 입력단([`crate::check_app_name`])이 이미 개행을 거절하지만, 여기서 한 번 더 막는다.
fn iss_common(value: &str) -> String {
    let folded: String = value.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let escaped = folded.replace('{', "{{");
    escaped.trim().chars().take(crate::MAX_NAME_CHARS).collect()
}

/// 따옴표 **밖**에 놓이는 값(`AppName=`, `DefaultGroupName=`). 큰따옴표와 `#` 를 아예 지운다 —
/// 이 자리에서는 `""` 이중화가 통하지 않고, `#` 는 ISPP 전처리기 지시문의 시작 글자다.
pub fn iss_escape(value: &str) -> String {
    iss_common(value).replace(['"', '#'], "")
}

/// 큰따옴표로 감싸는 자리(`Source:`, `Filename:`, `Name:`)에 들어갈 값. Inno 는 `"` 를 `""` 로 적는다.
///
/// 이중화하지 않으면 `데모"; Parameters: "…` 같은 이름이 따옴표를 닫고 파라미터를 덧붙인다.
fn iss_quoted(value: &str) -> String {
    iss_common(value).replace('"', "\"\"")
}

/// `{localappdata}\Programs\<이름>` 에 쓸 폴더 이름. Windows 경로에 못 쓰는 문자를 걷어낸다.
pub fn windows_dir_name(app_name: &str) -> String {
    let cleaned: String = app_name
        .chars()
        .filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim().to_string();
    if cleaned.is_empty() {
        slugify(app_name)
    } else {
        cleaned
    }
}

/// 앱별 `.iss` 본문을 만든다. `with_icon` 이면 `<slug>.ico` 가 스크립트 옆에 있다고 가정한다.
///
/// 템플릿 자리표시자는 `@이름@` 꼴이다. Inno Setup 이 `{` 를 `{{` 로 이스케이프하므로 `{{…}}` 는 쓸 수 없다.
pub fn render_iss(app_name: &str, version: &str, publisher: &str, slug: &str, with_icon: bool) -> String {
    let (setup_icon, icon_file, icon_ref) = if with_icon {
        (
            format!("SetupIconFile={slug}.ico"),
            format!("Source: \"{slug}.ico\"; DestDir: \"{{app}}\"; Flags: ignoreversion"),
            format!("; IconFilename: \"{{app}}\\{slug}.ico\""),
        )
    } else {
        (String::new(), String::new(), String::new())
    };

    // 한 번만 훑어 치환한다. 앱 이름에 `@APP_SLUG@` 같은 글자가 들어 있어도 다시 치환되지 않는다.
    crate::fill_tokens(
        APP_ISS,
        "@",
        &[
            ("@APP_ID@", &app_id(app_name, publisher)),
            ("@APP_NAME@", &iss_escape(app_name)),
            ("@APP_NAME_Q@", &iss_quoted(app_name)),
            ("@APP_DIR@", &iss_escape(&windows_dir_name(app_name))),
            ("@APP_VERSION@", &iss_escape(version)),
            ("@PUBLISHER@", &iss_escape(publisher)),
            ("@APP_SLUG@", &iss_quoted(slug)),
            ("@SETUP_ICON@", &setup_icon),
            ("@ICON_FILE@", &icon_file),
            ("@ICON_REF@", &icon_ref),
        ],
    )
}

// ───────────────────────────── 설치 프로그램 빌드 ─────────────────────────────

/// Windows 설치 프로그램을 만든다.
///
/// `out_dir/<slug>-installer/` 에 실행 파일·아이콘·`.iss` 를 모아 두고 컴파일러를 부른다.
/// 컴파일러가 없으면 `Ok(None)` — 준비된 파일은 그대로 남으니 사용자가 Windows 에서 `iscc <slug>.iss` 만 돌리면 된다.
/// 호출자는 `None` 을 받으면 `archive(Target::WindowsX64, …)` 의 zip 으로 대체하면 된다.
pub fn windows_installer(
    app_exe: &Path,
    app_name: &str,
    version: &str,
    publisher: &str,
    out_dir: &Path,
    icon: Option<&Path>,
) -> anyhow::Result<Option<Artifact>> {
    let slug = slugify(app_name);
    let stage = out_dir.join(format!("{slug}-installer"));
    std::fs::create_dir_all(&stage)?;

    std::fs::copy(app_exe, stage.join(format!("{slug}.exe")))
        .with_context(|| format!("앱 실행 파일을 복사하지 못했습니다: {}", app_exe.display()))?;

    let with_icon = match icon {
        Some(png) => {
            crate::icon::png_to_ico(png, &stage.join(format!("{slug}.ico")))?;
            true
        }
        None => false,
    };

    let script = stage.join(format!("{slug}.iss"));
    std::fs::write(&script, render_iss(app_name, version, publisher, &slug, with_icon))?;

    let Some(compiler) = find_inno_setup() else {
        log::info!(
            "Inno Setup 컴파일러가 없어 스크립트만 만들었습니다: {}",
            script.display()
        );
        return Ok(None);
    };

    compile(&compiler, &stage, &format!("{slug}.iss"))?;

    let produced = stage.join("Output").join(format!("{slug}-setup-{version}.exe"));
    if !produced.is_file() {
        anyhow::bail!("설치 프로그램이 만들어지지 않았습니다: {}", produced.display());
    }
    let final_path = out_dir.join(format!("{slug}-setup-{version}.exe"));
    move_file(&produced, &final_path)?;

    let bytes = std::fs::read(&final_path)?;
    Ok(Some(Artifact {
        sha256: sha256_hex(&bytes),
        size: bytes.len() as u64,
        path: final_path,
    }))
}

/// 스크립트가 있는 폴더를 작업 디렉터리로 삼아 컴파일러를 부른다.
/// 상대 경로로 넘겨야 wine 경로 변환을 신경 쓰지 않아도 된다.
fn compile(compiler: &InnoSetup, work_dir: &Path, script_name: &str) -> anyhow::Result<()> {
    let mut cmd = if compiler.via_wine {
        let mut c = Command::new("wine");
        c.arg(&compiler.path).arg(script_name);
        c
    } else {
        let mut c = Command::new(&compiler.path);
        c.arg(script_name);
        c
    };
    cmd.current_dir(work_dir);

    let out = cmd
        .output()
        .with_context(|| format!("컴파일러를 실행하지 못했습니다: {}", compiler.describe()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::bail!(
            "Inno Setup 컴파일 실패 (코드 {:?})\n{stdout}\n{stderr}",
            out.status.code()
        );
    }
    Ok(())
}

/// 같은 파일 시스템이면 rename, 아니면 복사 후 원본 삭제.
fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4122 부록의 표준 시험값 — DNS 이름공간 + "python.org".
    const DNS_NS: [u8; 16] = [
        0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
    ];

    #[test]
    fn uuid_v5_matches_reference_vector() {
        let id = format_uuid(&uuid_v5(&DNS_NS, "python.org")).to_lowercase();
        assert_eq!(id, "886313e1-3b8a-5372-9b90-0c9aee199e5d");
    }

    #[test]
    fn app_id_is_deterministic_and_specific_to_name_and_publisher() {
        assert_eq!(app_id("내 앱", "나"), app_id("내 앱", "나"));
        assert_eq!(
            app_id("내 앱", "나"),
            app_id("  내 앱  ", " 나 "),
            "앞뒤 공백은 무시한다"
        );
        assert_ne!(app_id("내 앱", "나"), app_id("다른 앱", "나"));
        // L18: 이름만 베껴도 발행자가 다르면 다른 설치로 취급된다.
        assert_ne!(app_id("내 앱", "나"), app_id("내 앱", "사칭"));
        // 구분자가 없으면 ("ab","c") 와 ("a","bc") 가 같아진다.
        assert_ne!(app_id("ab", "c"), app_id("a", "bc"));

        let id = app_id("내 앱", "나");
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "5", "UUID 버전은 5 여야 합니다: {id}");
        assert!(
            matches!(&id[19..20], "8" | "9" | "A" | "B"),
            "RFC 4122 변형이 아닙니다: {id}"
        );
        assert!(
            id.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-'),
            "{id}"
        );
    }

    /// H8-c: 따옴표를 닫고 파라미터를 덧붙이는 이름이 스크립트를 바꾸지 못한다.
    #[test]
    fn a_quote_injecting_name_cannot_add_iss_parameters() {
        let evil = r#"데모"; Parameters: "/x"#;
        let iss = render_iss(evil, "0.1.0", "나", "demo", false);

        // 인용 자리에서는 따옴표가 이중화돼 문자열 안에 갇힌다.
        assert!(iss.contains(r#"Name: "{group}\데모""; Parameters: ""/x""#), "{iss}");
        // 새 파라미터가 생기지 않는다 — `; Parameters:` 로 시작하는 조각이 없다.
        for line in iss
            .lines()
            .filter(|l| l.starts_with("Name:") || l.starts_with("Filename:"))
        {
            let outside: String = line
                .split('"')
                .step_by(2) // 짝수 조각 = 따옴표 바깥
                .collect();
            assert!(!outside.contains("Parameters:"), "따옴표 밖으로 샜습니다: {line}");
        }
        // 비인용 자리에서는 따옴표를 아예 지운다.
        let app_name_line = iss.lines().find(|l| l.starts_with("AppName=")).unwrap();
        assert!(!app_name_line.contains('"'), "{app_name_line}");
    }

    #[test]
    fn newlines_and_control_characters_cannot_add_iss_directives() {
        let evil = "데모\n[Run]\nFilename: \"cmd.exe\"; Parameters: \"/c calc\"";
        let iss = render_iss(evil, "0.1.0", "나", "demo", false);
        // 주입한 글자는 값 안에 남지만 **줄의 시작**이 되지 못한다 — 지시문도 섹션도 늘지 않는다.
        let clean = render_iss("데모", "0.1.0", "나", "demo", false);
        let sections = |t: &str| t.lines().filter(|l| l.trim_start().starts_with('[')).count();
        assert_eq!(sections(&iss), sections(&clean), "섹션이 늘었습니다: {iss}");
        assert_eq!(iss.lines().count(), clean.lines().count(), "줄이 늘었습니다: {iss}");
        assert_eq!(iss.lines().filter(|l| l.trim() == "[Run]").count(), 1, "{iss}");
        let runs: Vec<&str> = iss.lines().filter(|l| l.starts_with("Filename:")).collect();
        assert_eq!(runs.len(), 1, "{runs:?}");
        // 주입한 글자는 따옴표가 이중화돼 문자열 **안**에 갇힌다.
        let outside: String = runs[0].split('"').step_by(2).collect();
        assert!(!outside.contains("cmd.exe"), "따옴표 밖으로 샜습니다: {}", runs[0]);
        // 개행이 접혔으므로 AppName 은 한 줄이고 그 줄에만 흔적이 남는다.
        let names: Vec<&str> = iss.lines().filter(|l| l.starts_with("AppName=")).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].contains("cmd.exe"), "{}", names[0]);
    }

    #[test]
    fn ispp_directive_characters_are_stripped_outside_quotes() {
        let iss = render_iss("데모 #define X 1", "0.1.0", "나", "demo", false);
        let line = iss.lines().find(|l| l.starts_with("AppName=")).unwrap();
        assert!(!line.contains('#'), "{line}");
    }

    #[test]
    fn iss_escapes_braces_quotes_and_newlines() {
        assert_eq!(iss_escape("{app}"), "{{app}");
        assert_eq!(iss_escape("줄\n바꿈"), "줄 바꿈");
        assert_eq!(iss_quoted("따\"옴표"), "따\"\"옴표");
        assert_eq!(iss_escape("  공백  "), "공백");
        // 비인용 자리에서는 따옴표와 ISPP 지시문 글자를 지운다.
        assert_eq!(iss_escape("따\"옴표"), "따옴표");
        assert_eq!(iss_escape("가#나"), "가나");
        // 인용 자리에서는 `#` 이 그대로다 — 문자열 안이라 지시문이 되지 않는다.
        assert_eq!(iss_quoted("가#나"), "가#나");
    }

    #[test]
    fn windows_dir_name_strips_invalid_characters() {
        assert_eq!(windows_dir_name("내 앱"), "내 앱");
        assert_eq!(windows_dir_name("a/b:c*d?"), "abcd");
        assert_eq!(windows_dir_name("트레일링."), "트레일링");
        // 전부 걸러지면 슬러그로 떨어진다.
        assert_eq!(windows_dir_name("///"), "app");
    }

    #[test]
    fn rendered_iss_has_no_placeholders_and_quotes_paths() {
        let iss = render_iss("내 앱", "1.2.3", "Trust A&C", "app", true);
        for token in [
            "@APP_ID@",
            "@APP_NAME@",
            "@APP_NAME_Q@",
            "@APP_DIR@",
            "@APP_VERSION@",
            "@PUBLISHER@",
            "@APP_SLUG@",
            "@SETUP_ICON@",
            "@ICON_FILE@",
            "@ICON_REF@",
        ] {
            assert!(!iss.contains(token), "치환되지 않은 자리표시자 {token}: {iss}");
        }
        assert!(
            iss.contains(&format!("AppId={{{{{}}}", app_id("내 앱", "Trust A&C"))),
            "{iss}"
        );
        assert!(iss.contains("AppName=내 앱"));
        assert!(iss.contains("AppVersion=1.2.3"));
        assert!(iss.contains("AppPublisher=Trust A&C"));
        assert!(iss.contains("PrivilegesRequired=lowest"));
        assert!(iss.contains(r"DefaultDirName={localappdata}\Programs\내 앱"));
        assert!(iss.contains("OutputBaseFilename=app-setup-1.2.3"));
        assert!(iss.contains("SetupIconFile=app.ico"));
        assert!(iss.contains(r#"Source: "app.ico"; DestDir: "{app}""#));
        assert!(iss.contains(r#"IconFilename: "{app}\app.ico""#));
        assert!(iss.contains("Tasks: desktopicon"));
        assert!(iss.contains("postinstall"));
    }

    #[test]
    fn rendered_iss_without_icon_omits_icon_lines() {
        let iss = render_iss("데모", "0.1.0", "누군가", "demo", false);
        assert!(!iss.contains("SetupIconFile"));
        assert!(!iss.contains(".ico"));
        assert!(iss.contains(r#"Source: "demo.exe""#));
    }

    /// 앱 이름에 중괄호가 들어가도 Inno 지시문이 깨지지 않는다.
    #[test]
    fn braces_in_app_name_are_escaped() {
        let iss = render_iss("{위험} 앱", "1.0", "publisher", "danger", false);
        assert!(iss.contains("AppName={{위험} 앱"), "{iss}");
        // 템플릿이 쓰는 Inno 상수는 그대로 남아야 한다.
        assert!(iss.contains("{localappdata}"));
        assert!(iss.contains("{app}"));
    }

    #[test]
    fn no_compiler_means_none() {
        assert_eq!(first_existing(&[]), None);
        assert_eq!(first_existing(&[(PathBuf::from("/없는/경로/ISCC.exe"), true)]), None);
    }

    #[test]
    fn first_existing_picks_the_first_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("ISCC.exe");
        std::fs::write(&real, b"x").unwrap();
        let found = first_existing(&[(dir.path().join("nope"), false), (real.clone(), true)]).unwrap();
        assert_eq!(found.path, real);
        assert!(found.via_wine);
        assert!(found.describe().contains("wine"));
    }

    #[test]
    fn installer_leaves_script_when_compiler_is_missing() {
        // 이 머신에는 Inno Setup 이 없다. 있더라도 NL_ISCC 로 없는 경로를 가리켜 탐색을 끊는다.
        if find_inno_setup().is_some() {
            eprintln!("Inno Setup 이 설치돼 있어 이 테스트를 건너뜁니다");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built.exe");
        std::fs::write(&exe, b"MZ fake").unwrap();
        let icon = dir.path().join("icon.png");
        std::fs::write(&icon, crate::icon::sample_png(64, 64)).unwrap();
        let out = dir.path().join("out");

        let result = windows_installer(&exe, "내 앱", "2.0.0", "Trust A&C", &out, Some(&icon)).unwrap();
        assert!(result.is_none(), "컴파일러가 없으면 None 이어야 합니다");

        let stage = out.join("app-installer");
        assert!(stage.join("app.exe").is_file(), "실행 파일이 준비돼 있어야 합니다");
        assert!(stage.join("app.ico").is_file(), "아이콘이 ico 로 변환돼 있어야 합니다");
        let script = std::fs::read_to_string(stage.join("app.iss")).unwrap();
        assert!(script.contains("AppName=내 앱"));
        assert!(script.contains("SetupIconFile=app.ico"));
    }

    #[test]
    fn installer_without_icon_skips_ico() {
        if find_inno_setup().is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("built.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let out = dir.path().join("out");
        assert!(windows_installer(&exe, "Demo App", "0.1.0", "p", &out, None)
            .unwrap()
            .is_none());
        let stage = out.join("demo-app-installer");
        assert!(stage.join("demo-app.iss").is_file());
        assert!(!stage.join("demo-app.ico").exists());
    }
}
