//! `KEY=VALUE` 줄을 읽어 환경 변수로 넣는다.
//!
//! 토큰 같은 비밀을 **명령줄에 적지 않으려고** 있다. 명령줄은 같은 기계의 누구나
//! `ps` 로 볼 수 있고, Windows 작업 스케줄러의 작업 목록에도 그대로 남는다.
//!
//! systemd 의 `EnvironmentFile=` 과 같은 자리를 채운다. 리눅스에서는 둘 중 무엇을 써도 되고,
//! 작업 스케줄러처럼 환경 변수를 넣어 줄 방법이 없는 곳에서는 이쪽이 유일한 길이다.
//!
//! 형식은 일부러 좁게 잡았다. 셸이 아니므로 따옴표 해석도, 변수 치환도, 이어 쓰기도 없다.
//! 값은 `=` 뒤부터 줄 끝까지 그대로 들어간다 — 따옴표도 벗기지 않는다.
//!
//! 다만 **줄 양끝 공백은 없앤다.** Windows 에서 만든 파일은 줄 끝에 `\r` 이 붙는데,
//! 그것이 토큰 값에 들어가면 인증이 조용히 어긋난다. 값 끝에 공백을 넣어야 한다면 이 형식으로는 안 된다.

use std::path::Path;

/// 파일을 읽어 환경 변수로 넣는다. **이미 있는 변수는 덮어쓰지 않는다.**
///
/// 덮어쓰지 않는 이유는 실행하는 쪽이 언제나 이길 수 있어야 해서다 —
/// `NL_HTTP_TOKEN=... ./앱 --env-file ...` 로 한 번만 다른 값을 주는 일이 가능해야 한다.
///
/// 돌려주는 값은 실제로 넣은 변수 이름들이다(값은 비밀이라 돌려주지 않는다).
pub fn load(path: &Path) -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("환경 파일을 읽지 못했습니다 ({}): {e}", path.display()))?;
    warn_if_readable_by_others(path);

    let mut applied = Vec::new();
    for (name, value) in parse(&text).map_err(|e| anyhow::anyhow!("{} {e}", path.display()))? {
        if std::env::var_os(&name).is_some() {
            log::debug!("{name} 은 이미 설정돼 있어 환경 파일 값을 쓰지 않습니다");
            continue;
        }
        std::env::set_var(&name, &value);
        applied.push(name);
    }
    Ok(applied)
}

/// `KEY=VALUE` 줄을 뽑는다. 빈 줄과 `#` 주석은 건너뛴다.
///
/// 키는 영문자·숫자·밑줄만 받는다. 셸이 읽을 수 없는 이름을 넣어 두면
/// 같은 파일을 `EnvironmentFile=` 로 쓸 때 조용히 어긋난다.
fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export KEY=VALUE` 도 받아 준다 — 사람이 셸 파일에서 복사해 오는 일이 흔하다.
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{}행: `=` 가 없습니다: {raw}", i + 1));
        };
        let key = key.trim();
        // 값의 끝 공백은 줄을 trim 할 때 이미 사라졌다 (CRLF 대응).
        if key.is_empty() {
            return Err(format!("{}행: 이름이 비어 있습니다", i + 1));
        }
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{}행: 이름에 쓸 수 없는 글자가 있습니다: {key}", i + 1));
        }
        // 값은 그대로다. 따옴표를 벗기지 않는다 — 벗기면 따옴표가 든 토큰을 넣을 수 없다.
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

/// 소유자 말고도 읽을 수 있으면 알린다. 막지는 않는다 — 파일은 사용자 것이다.
#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        log::warn!(
            "환경 파일이 다른 사용자에게도 열립니다 ({mode:o}): {} — `chmod 600` 을 권합니다",
            path.display()
        );
    }
}

/// Windows 에는 유닉스 권한 비트가 없다. ACL 을 읽어 판단하는 대신, 파일을 사용자 폴더에
/// 두었다는 전제에 기댄다 — 이 차이는 `docs/GUIDE.md` 에 적어 두었다.
#[cfg(not(unix))]
fn warn_if_readable_by_others(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines_become_pairs() {
        let got = parse("A=1\nB=hello world\n").unwrap();
        assert_eq!(got, vec![("A".into(), "1".into()), ("B".into(), "hello world".into())]);
    }

    #[test]
    fn blanks_and_comments_are_skipped() {
        let got = parse("\n# 주석\n  \nA=1\n\t# 들여쓴 주석\nB=2\n").unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn values_are_taken_verbatim() {
        // 따옴표를 벗기지 않는다 — 벗기면 따옴표가 든 토큰을 넣을 수 없다.
        let got = parse("A=\"따옴표째\"\nB=a=b=c\nC=\nD=  앞뒤 공백  \n").unwrap();
        assert_eq!(got[0].1, "\"따옴표째\"");
        assert_eq!(got[1].1, "a=b=c", "첫 `=` 만 가른다");
        assert_eq!(got[2].1, "", "빈 값도 받는다");
        assert_eq!(
            got[3].1, "  앞뒤 공백",
            "앞 공백은 남고 끝 공백은 줄 trim 으로 사라진다"
        );
    }

    /// Windows 에서 만든 파일은 줄 끝에 `\r` 이 붙는다. 토큰에 섞이면 인증이 조용히 어긋난다.
    #[test]
    fn crlf_files_do_not_leak_a_carriage_return_into_values() {
        let got = parse("NL_HTTP_TOKEN=비밀\r\nOTHER=2\r\n").unwrap();
        assert_eq!(got[0].1, "비밀", "\\r 이 값에 섞였습니다: {:?}", got[0].1);
        assert_eq!(got[1].1, "2");
    }

    #[test]
    fn export_prefix_is_tolerated() {
        let got = parse("export NL_HTTP_TOKEN=abc\n").unwrap();
        assert_eq!(got, vec![("NL_HTTP_TOKEN".into(), "abc".into())]);
    }

    #[test]
    fn bad_lines_are_errors_with_a_line_number() {
        let err = parse("A=1\n그냥글자\n").unwrap_err();
        assert!(err.contains("2행"), "{err}");
        assert!(err.contains("`=` 가 없습니다"), "{err}");

        let err = parse("=값\n").unwrap_err();
        assert!(err.contains("이름이 비어 있습니다"), "{err}");

        // 셸이 읽을 수 없는 이름은 막는다 — 같은 파일을 EnvironmentFile= 로도 쓰기 때문이다.
        let err = parse("가나=1\n").unwrap_err();
        assert!(err.contains("쓸 수 없는 글자"), "{err}");
        assert!(parse("A-B=1\n").is_err());
    }

    /// 환경 변수는 프로세스 전역이라 한 시험으로 묶는다.
    #[test]
    fn loading_sets_new_vars_but_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("app.env");
        std::fs::write(&f, "NL_TEST_ENVFILE_NEW=넣음\nNL_TEST_ENVFILE_KEPT=파일값\n").unwrap();

        std::env::remove_var("NL_TEST_ENVFILE_NEW");
        std::env::set_var("NL_TEST_ENVFILE_KEPT", "원래값");

        let applied = load(&f).unwrap();
        assert_eq!(
            applied,
            vec!["NL_TEST_ENVFILE_NEW".to_string()],
            "이미 있는 것은 건드리지 않는다"
        );
        assert_eq!(std::env::var("NL_TEST_ENVFILE_NEW").unwrap(), "넣음");
        assert_eq!(std::env::var("NL_TEST_ENVFILE_KEPT").unwrap(), "원래값", "덮어썼습니다");

        std::env::remove_var("NL_TEST_ENVFILE_NEW");
        std::env::remove_var("NL_TEST_ENVFILE_KEPT");
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let err = load(Path::new("/없는/경로/app.env")).unwrap_err().to_string();
        assert!(err.contains("읽지 못했습니다"), "{err}");
    }
}
