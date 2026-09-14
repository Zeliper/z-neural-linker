//! 매니페스트 서명 키를 만들고 매니페스트에 서명하는 도구.
//!
//! `minisign` CLI 를 깔지 않아도 되도록 같은 형식을 Rust 로 다룬다. CI 러너에도 따로 설치할 것이 없다.
//!
//! ```text
//! cargo run -p nl-update --example nl-keygen -- keygen [--out packaging/keys] [--name neural-linker]
//! cargo run -p nl-update --example nl-keygen -- sign <파일> [--key <비밀키>] [--out <서명파일>]
//! cargo run -p nl-update --example nl-keygen -- verify <파일> --pubkey <base64 또는 .pub 경로>
//! ```
//!
//! `sign` 은 비밀키를 `--key` 경로에서 읽고, 없으면 `MINISIGN_KEY` 환경 변수의 **키 파일 내용**을 쓴다.
//! 릴리스 워크플로가 시크릿을 그대로 넘길 수 있게 한 것이다.
//!
//! **비밀번호 없는 키만 만든다**(`minisign -G -W` 와 같다). CI 는 대화형 입력을 받을 수 없고,
//! 비밀번호가 걸린 키는 서명할 때 반드시 물어보기 때문이다. 그러므로 비밀키 파일 자체가 곧 비밀이다 —
//! 저장소에 넣지 말고 CI 시크릿으로만 다룬다.

use std::path::{Path, PathBuf};

fn main() -> std::process::ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("오류: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

fn run(args: Vec<String>) -> anyhow::Result<()> {
    let Some(cmd) = args.first().map(String::as_str) else {
        usage();
        anyhow::bail!("하위 명령이 필요합니다");
    };
    match cmd {
        "keygen" => keygen(&Args::parse(&args[1..])),
        "sign" => sign_cmd(&Args::parse(&args[1..])),
        "verify" => verify_cmd(&Args::parse(&args[1..])),
        "-h" | "--help" | "help" => {
            usage();
            Ok(())
        }
        other => {
            usage();
            anyhow::bail!("모르는 하위 명령입니다: {other}")
        }
    }
}

fn usage() {
    eprintln!(
        "\
매니페스트 서명 키 도구 (minisign 형식)

  keygen [--out <폴더>] [--name <이름>] [--force]
      비밀번호 없는 키 쌍을 만들어 <폴더>/<이름>.key 와 <이름>.pub 에 쓴다.
      기본 폴더는 packaging/keys, 기본 이름은 neural-linker.

  sign <파일> [--key <비밀키>] [--out <서명파일>]
      <파일>.minisig 를 만든다. --key 가 없으면 MINISIGN_KEY 환경 변수의 키 내용을 쓴다.

  verify <파일> --pubkey <base64 또는 .pub 경로> [--sig <서명파일>]
      서명을 검증한다. 배포 앱이 쓰는 nl_update::verify_manifest 를 그대로 부른다."
    );
}

// ───────────────────────────── 인자 ─────────────────────────────

/// `--이름 값` 과 위치 인자만 받는 최소 파서. 도구 하나 때문에 clap 을 들이지 않는다.
struct Args {
    positional: Vec<String>,
    flags: std::collections::BTreeMap<String, String>,
    switches: std::collections::BTreeSet<String>,
}

impl Args {
    fn parse(args: &[String]) -> Self {
        let mut positional = Vec::new();
        let mut flags = std::collections::BTreeMap::new();
        let mut switches = std::collections::BTreeSet::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                match args.get(i + 1) {
                    // 다음 것이 값처럼 보이면 값 있는 옵션.
                    Some(v) if !v.starts_with("--") => {
                        flags.insert(name.to_string(), v.clone());
                        i += 2;
                    }
                    _ => {
                        switches.insert(name.to_string());
                        i += 1;
                    }
                }
            } else {
                positional.push(a.clone());
                i += 1;
            }
        }
        Self {
            positional,
            flags,
            switches,
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    fn has(&self, name: &str) -> bool {
        self.switches.contains(name)
    }

    fn positional(&self, i: usize) -> Option<&str> {
        self.positional.get(i).map(String::as_str)
    }
}

// ───────────────────────────── keygen ─────────────────────────────

/// 비밀키를 두는 기본 폴더. `.gitignore` 가 `*.key` 를 막는다.
const DEFAULT_KEY_DIR: &str = "packaging/keys";
const DEFAULT_KEY_NAME: &str = "neural-linker";

fn keygen(args: &Args) -> anyhow::Result<()> {
    let dir = PathBuf::from(args.get("out").unwrap_or(DEFAULT_KEY_DIR));
    let name = args.get("name").unwrap_or(DEFAULT_KEY_NAME);
    let sk_path = dir.join(format!("{name}.key"));
    let pk_path = dir.join(format!("{name}.pub"));

    if !args.has("force") {
        for p in [&sk_path, &pk_path] {
            anyhow::ensure!(!p.exists(), "이미 있습니다: {} (덮어쓰려면 --force)", p.display());
        }
    }

    let pair = minisign::KeyPair::generate_unencrypted_keypair()
        .map_err(|e| anyhow::anyhow!("키를 만들지 못했습니다: {e}"))?;
    let pubkey = pair.pk.to_base64();

    let sk_box = pair
        .sk
        .to_box(Some(&format!("{name} 매니페스트 서명 비밀키 (비밀번호 없음)")))
        .map_err(|e| anyhow::anyhow!("비밀키를 직렬화하지 못했습니다: {e}"))?;
    let pk_box = pair
        .pk
        .to_box()
        .map_err(|e| anyhow::anyhow!("공개키를 직렬화하지 못했습니다: {e}"))?;

    create_private_dir(&dir)?;
    write_private(&sk_path, sk_box.into_string().as_bytes())?;
    std::fs::write(&pk_path, pk_box.into_string())
        .map_err(|e| anyhow::anyhow!("{} 를 쓰지 못했습니다: {e}", pk_path.display()))?;

    println!("키 쌍을 만들었습니다.\n");
    println!(
        "  비밀키  {}   ← 저장소에 넣지 마세요 (.gitignore 가 *.key 를 막습니다)",
        sk_path.display()
    );
    println!("  공개키  {}\n", pk_path.display());
    println!("공개키 (이 한 줄을 아래 두 곳에 넣습니다):\n");
    println!("  {pubkey}\n");
    println!("① 빌더 자체 업데이트 — crates/nl-app/src/update_key.rs");
    println!("     pub const PUBLIC_KEY: Option<&str> = Some(\"{pubkey}\");\n");
    println!("② 배포 앱 — 빌더의 빌드 설정에서 '배포 앱 자동 업데이트' 의 공개키 칸에 붙여 넣습니다.");
    println!("     번들 매니페스트의 update_public_key 로 들어갑니다.\n");
    println!("③ CI 시크릿 — 비밀키 **파일 내용 전체**를 MINISIGN_KEY 로 등록합니다.");
    println!("     gh secret set MINISIGN_KEY < {}\n", sk_path.display());
    println!("릴리스 워크플로는 minisign CLI 없이 이 도구로 서명합니다:");
    println!("     cargo run -p nl-update --example nl-keygen -- sign dist/latest.json");
    Ok(())
}

// ───────────────────────────── sign ─────────────────────────────

fn sign_cmd(args: &Args) -> anyhow::Result<()> {
    let target = args
        .positional(0)
        .ok_or_else(|| anyhow::anyhow!("서명할 파일이 필요합니다"))?;
    let target = Path::new(target);
    let sig_path = match args.get("out") {
        Some(p) => PathBuf::from(p),
        None => default_sig_path(target),
    };

    let sk_text = read_secret_key_text(args.get("key"))?;
    let sig = sign_bytes(
        &sk_text,
        &std::fs::read(target).map_err(|e| anyhow::anyhow!("{} 를 읽지 못했습니다: {e}", target.display()))?,
    )?;

    std::fs::write(&sig_path, &sig).map_err(|e| anyhow::anyhow!("{} 를 쓰지 못했습니다: {e}", sig_path.display()))?;
    println!("서명: {}", sig_path.display());
    Ok(())
}

/// `latest.json` → `latest.json.minisig`. 배포 앱이 [`nl_update::signature_url`] 로 찾는 이름과 같다.
fn default_sig_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".minisig");
    target.with_file_name(name)
}

/// `--key` 경로가 있으면 그 파일을, 없으면 `MINISIGN_KEY` 환경 변수의 내용을 쓴다.
fn read_secret_key_text(path: Option<&str>) -> anyhow::Result<String> {
    if let Some(p) = path {
        return std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{p} 를 읽지 못했습니다: {e}"));
    }
    match std::env::var("MINISIGN_KEY") {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => anyhow::bail!("--key 도 MINISIGN_KEY 도 없습니다"),
    }
}

/// 비밀키 파일 내용으로 `body` 에 서명해 `.minisig` 본문을 돌려준다.
///
/// 비밀번호가 걸린 키는 거절한다 — CI 에서 대화형 입력을 받을 수 없다.
fn sign_bytes(secret_key_text: &str, body: &[u8]) -> anyhow::Result<String> {
    let sk_box = minisign::SecretKeyBox::from_string(secret_key_text)
        .map_err(|e| anyhow::anyhow!("비밀키 형식이 아닙니다: {e}"))?;
    let sk = sk_box
        .into_unencrypted_secret_key()
        .map_err(|e| anyhow::anyhow!("비밀번호 없는 키여야 합니다 (minisign -G -W 와 같은 키): {e}"))?;
    let sig = minisign::sign(None, &sk, body, None, None).map_err(|e| anyhow::anyhow!("서명하지 못했습니다: {e}"))?;
    Ok(sig.into_string())
}

// ───────────────────────────── verify ─────────────────────────────

fn verify_cmd(args: &Args) -> anyhow::Result<()> {
    let target = args
        .positional(0)
        .ok_or_else(|| anyhow::anyhow!("검증할 파일이 필요합니다"))?;
    let target = Path::new(target);
    let sig_path = match args.get("sig") {
        Some(p) => PathBuf::from(p),
        None => default_sig_path(target),
    };
    let pubkey = read_public_key(
        args.get("pubkey")
            .ok_or_else(|| anyhow::anyhow!("--pubkey 가 필요합니다"))?,
    )?;

    let body = std::fs::read(target).map_err(|e| anyhow::anyhow!("{} 를 읽지 못했습니다: {e}", target.display()))?;
    let sig = std::fs::read_to_string(&sig_path)
        .map_err(|e| anyhow::anyhow!("{} 를 읽지 못했습니다: {e}", sig_path.display()))?;

    // 배포 앱이 쓰는 바로 그 함수다 — 여기서 통과하면 앱에서도 통과한다.
    nl_update::verify_manifest(&body, &sig, &pubkey)?;
    println!("서명이 맞습니다: {}", target.display());
    Ok(())
}

/// base64 한 줄이거나 `.pub` 파일 경로.
fn read_public_key(arg: &str) -> anyhow::Result<String> {
    let p = Path::new(arg);
    if p.is_file() {
        return std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{arg} 를 읽지 못했습니다: {e}"));
    }
    Ok(arg.to_string())
}

// ───────────────────────────── 파일 권한 ─────────────────────────────

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
        .map_err(|e| anyhow::anyhow!("{} 를 만들지 못했습니다: {e}", dir.display()))
}

/// 비밀키는 소유자만 읽을 수 있게 쓴다.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| anyhow::anyhow!("{} 를 쓰지 못했습니다: {e}", path.display()))?;
    std::io::Write::write_all(&mut f, bytes)
        .map_err(|e| anyhow::anyhow!("{} 를 쓰지 못했습니다: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 이 도구가 만든 키로 서명하면 **배포 앱이 그대로 검증한다.** 형식이 어긋나면 여기서 잡힌다.
    #[test]
    fn a_generated_key_signs_a_manifest_that_the_app_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args::parse(&[
            "--out".into(),
            dir.path().display().to_string(),
            "--name".into(),
            "시험키".into(),
        ]);
        keygen(&args).unwrap();

        let sk_path = dir.path().join("시험키.key");
        let pk_path = dir.path().join("시험키.pub");
        assert!(sk_path.is_file() && pk_path.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sk_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "비밀키가 남에게 읽힙니다: {mode:o}");
        }

        // 진짜 매니페스트 모양으로 서명한다.
        let manifest = br#"{"version":"0.2.0","notes":"","published_at":"2026-09-14T00:00:00Z","assets":{}}"#;
        let sk_text = std::fs::read_to_string(&sk_path).unwrap();
        let sig = sign_bytes(&sk_text, manifest).unwrap();

        let pubkey = read_public_key(pk_path.to_str().unwrap()).unwrap();
        nl_update::verify_manifest(manifest, &sig, &pubkey).expect("배포 앱이 받아들여야 합니다");

        // 내용이 한 글자만 달라도 거절한다.
        let tampered = br#"{"version":"9.9.9","notes":"","published_at":"2026-09-14T00:00:00Z","assets":{}}"#;
        assert!(nl_update::verify_manifest(tampered, &sig, &pubkey).is_err());

        // 다른 키로도 거절한다.
        let other = minisign::KeyPair::generate_unencrypted_keypair()
            .unwrap()
            .pk
            .to_base64();
        assert!(nl_update::verify_manifest(manifest, &sig, &other).is_err());
    }

    #[test]
    fn the_public_key_file_and_the_base64_line_mean_the_same_thing() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args::parse(&["--out".into(), dir.path().display().to_string()]);
        keygen(&args).unwrap();

        let pk_path = dir.path().join("neural-linker.pub");
        let from_file = read_public_key(pk_path.to_str().unwrap()).unwrap();
        // .pub 은 주석 + base64 두 줄이다. 둘째 줄만 떼어도 같은 키다.
        let line = from_file.lines().last().unwrap().trim().to_string();
        let manifest = b"{}";
        let sk_text = std::fs::read_to_string(dir.path().join("neural-linker.key")).unwrap();
        let sig = sign_bytes(&sk_text, manifest).unwrap();

        nl_update::verify_manifest(manifest, &sig, &from_file).unwrap();
        nl_update::verify_manifest(manifest, &sig, &line).unwrap();
    }

    #[test]
    fn keygen_refuses_to_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let args = Args::parse(&["--out".into(), dir.path().display().to_string()]);
        keygen(&args).unwrap();
        let err = keygen(&args).unwrap_err().to_string();
        assert!(err.contains("이미 있습니다"), "{err}");

        let forced = Args::parse(&["--out".into(), dir.path().display().to_string(), "--force".into()]);
        keygen(&forced).expect("--force 면 덮어쓴다");
    }

    #[test]
    fn the_signature_file_name_is_what_the_app_looks_for() {
        assert_eq!(
            default_sig_path(Path::new("dist/latest.json")),
            PathBuf::from("dist/latest.json.minisig")
        );
        assert_eq!(
            default_sig_path(Path::new("latest.json")),
            PathBuf::from("latest.json.minisig")
        );
        // 앱이 찾는 이름과 같아야 한다.
        assert!(nl_update::signature_url("https://h/latest.json").ends_with("latest.json.minisig"));
    }

    #[test]
    fn a_password_protected_key_is_refused_with_a_clear_message() {
        let pair = minisign::KeyPair::generate_encrypted_keypair(Some("비밀번호".into())).unwrap();
        let sk_text = pair.sk.to_box(None).unwrap().into_string();
        let err = sign_bytes(&sk_text, b"{}").unwrap_err().to_string();
        assert!(err.contains("비밀번호 없는 키"), "{err}");
    }

    #[test]
    fn junk_arguments_are_errors_not_panics() {
        assert!(run(vec![]).is_err());
        assert!(run(vec!["없는명령".into()]).is_err());
        assert!(run(vec!["sign".into()]).is_err(), "파일 없이 sign");
        assert!(run(vec!["verify".into(), "없는파일".into()]).is_err());
        run(vec!["--help".into()]).unwrap();
    }
}
