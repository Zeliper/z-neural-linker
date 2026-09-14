//! 모든 명령이 함께 쓰는 것: 프로젝트 읽기·원자적 쓰기, 이름/id 로 찾기, 장치 해석, 터미널 색.

use anyhow::{anyhow, bail, Context, Result};
use nl_core::{DatasetSpec, DevicePref, ModelDef, Pipeline, Project, ProjectFile};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

// ───────────────────────────── 색 ─────────────────────────────

/// 터미널이 아니거나 `NO_COLOR` 가 있으면 색을 끈다 (파이프로 넘길 때 제어문자가 섞이지 않게).
pub fn color_enabled() -> bool {
    static ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ONCE.get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_owned()
    }
}

pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}

// ───────────────────────────── 프로젝트 ─────────────────────────────

/// 읽어 들인 프로젝트와 그 파일 위치.
pub struct Loaded {
    pub project: Project,
    pub path: PathBuf,
    /// 상대 경로(데이터셋·가중치)의 기준 = 프로젝트 파일이 있는 폴더.
    pub base_dir: PathBuf,
}

pub fn load_project(path: &Path) -> Result<Loaded> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("프로젝트 파일을 읽지 못했다: {}", path.display()))?;
    let file = ProjectFile::from_json(&text)
        .with_context(|| format!("프로젝트 파일을 해석하지 못했다: {}", path.display()))?;
    if file.newer_than_app() {
        eprintln!(
            "{}",
            yellow(&format!(
                "경고: 이 파일은 이 도구보다 새 형식이다 (format_version {}). 모르는 필드는 저장할 때 사라진다.",
                file.format_version
            ))
        );
    }
    let base_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(Loaded {
        project: file.project,
        path: path.to_path_buf(),
        base_dir,
    })
}

/// **원자적 저장**: 같은 폴더의 임시 파일에 다 쓰고 `fsync` 한 뒤 이름을 바꾼다.
/// 도중에 죽어도 원본이 반쯤 덮인 채 남지 않는다.
pub fn save_project_atomic(path: &Path, project: &Project) -> Result<()> {
    let json = ProjectFile::new(project.clone()).to_json();
    write_atomic(path, json.as_bytes())
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("{} 폴더를 만들지 못했다", dir.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("파일 이름이 없는 경로다: {}", path.display()))?;
    let tmp = dir.join(format!(".{}.tmp-{}", name.to_string_lossy(), std::process::id()));

    let mut f = std::fs::File::create(&tmp).with_context(|| format!("{} 를 만들지 못했다", tmp.display()))?;
    let write = f.write_all(bytes).and_then(|()| f.sync_all());
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("{} 에 쓰지 못했다", tmp.display()));
    }
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("{} 로 이름을 바꾸지 못했다", path.display()));
    }
    Ok(())
}

// ───────────────────────────── 이름/id 로 찾기 ─────────────────────────────

/// 이름이 정확히 같거나, uuid 전체 또는 앞부분이 일치하는 항목을 고른다.
/// 여러 개가 걸리면 어느 것들인지 알려 주고 실패한다.
fn pick<'a, T, I>(items: I, what: &str, key: &str) -> Result<&'a T>
where
    I: IntoIterator<Item = (&'a T, String, String)>,
{
    let key_l = key.trim().to_lowercase();
    let all: Vec<(&T, String, String)> = items.into_iter().collect();
    let mut exact: Vec<&(&T, String, String)> = all.iter().filter(|(_, name, _)| name == key).collect();
    if exact.is_empty() {
        exact = all.iter().filter(|(_, _, id)| *id == key_l).collect();
    }
    if exact.is_empty() {
        exact = all
            .iter()
            .filter(|(_, _, id)| id.starts_with(&key_l) && !key_l.is_empty())
            .collect();
    }
    match exact.len() {
        1 => Ok(exact[0].0),
        0 => {
            let names: Vec<String> = all
                .iter()
                .map(|(_, n, i)| format!("{n} ({})", &i[..8.min(i.len())]))
                .collect();
            bail!(
                "{what} '{key}' 을 찾을 수 없다. 있는 것: {}",
                if names.is_empty() {
                    "(없음)".into()
                } else {
                    names.join(", ")
                }
            )
        }
        _ => {
            let names: Vec<String> = exact
                .iter()
                .map(|(_, n, i)| format!("{n} ({})", &i[..8.min(i.len())]))
                .collect();
            bail!("{what} '{key}' 이 여럿에 걸린다: {}", names.join(", "))
        }
    }
}

pub fn find_model<'a>(p: &'a Project, key: &str) -> Result<&'a ModelDef> {
    pick(
        p.models
            .values()
            .map(|m| (m, m.name.clone(), m.id.0.simple().to_string())),
        "모델",
        key,
    )
}

pub fn find_dataset<'a>(p: &'a Project, key: &str) -> Result<&'a DatasetSpec> {
    pick(
        p.datasets
            .values()
            .map(|d| (d, d.name.clone(), d.id.0.simple().to_string())),
        "데이터셋",
        key,
    )
}

pub fn find_pipeline<'a>(p: &'a Project, key: &str) -> Result<&'a Pipeline> {
    pick(
        p.pipelines
            .values()
            .map(|x| (x, x.name.clone(), x.id.0.simple().to_string())),
        "파이프라인",
        key,
    )
}

// ───────────────────────────── 장치 ─────────────────────────────

/// `cpu` | `gpu:N` | `auto`.
pub fn parse_device(s: &str) -> Result<DevicePref> {
    let t = s.trim().to_lowercase();
    match t.as_str() {
        "cpu" => Ok(DevicePref::Cpu),
        "auto" => Ok(DevicePref::Auto),
        other => match other.strip_prefix("gpu:").or_else(|| other.strip_prefix("gpu")) {
            Some(n) if !n.is_empty() => n
                .parse::<usize>()
                .map(|index| DevicePref::Gpu { index })
                .map_err(|_| anyhow!("gpu 번호를 읽을 수 없다: {s}")),
            _ => bail!("모르는 장치: {s} (cpu | gpu:0 | auto)"),
        },
    }
}

// ───────────────────────────── 표 ─────────────────────────────

/// 열 너비를 맞춰 표를 찍는다. 첫 줄은 머리글.
pub fn table(rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut width = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            width[i] = width[i].max(display_width(c));
        }
    }
    for (ri, r) in rows.iter().enumerate() {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            let pad = width[i].saturating_sub(display_width(c));
            line.push_str(c);
            if i + 1 < r.len() {
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        if ri == 0 {
            println!("{}", bold(&line));
        } else {
            println!("{line}");
        }
    }
}

/// 색 제어문자를 빼고, 한글·한자는 두 칸으로 센다.
fn display_width(s: &str) -> usize {
    let mut w = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        w += if is_wide(c) { 2 } else { 1 };
    }
    w
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F9FF)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

// ───────────────────────────── Ctrl+C ─────────────────────────────

static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn interrupted() -> bool {
    INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Ctrl+C 를 플래그로 받는다. 명령이 스스로 마무리(학습 결과 저장 등)할 수 있게 하려는 것이다.
#[cfg(unix)]
pub fn install_signal_handler() {
    extern "C" fn on_signal(_sig: libc::c_int) {
        // 신호 처리기 안에서는 원자적 저장만 한다 (비동기 신호 안전).
        INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    // SAFETY: 처리기가 하는 일이 원자적 저장뿐이다.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Windows 에서는 기본 동작(즉시 종료)을 그대로 둔다.
#[cfg(not(unix))]
pub fn install_signal_handler() {}

// ───────────────────────────── TLS 주입 ─────────────────────────────

/// `--tls-cert` / `--tls-key` / `--http-node` 를 한 덩어리로.
pub struct TlsInject<'a> {
    pub cert: Option<&'a Path>,
    pub key: Option<&'a Path>,
    /// 이 이름의 `HttpServer` 노드에만 붙인다. 없으면 전부.
    pub node: Option<&'a str>,
}

/// 프로젝트의 `HttpServer` 노드에 `tls` 를 끼워 넣는다. 붙인 노드 수를 돌려준다.
///
/// **프로젝트 파일은 건드리지 않는다.** 메모리 위의 사본만 바뀌므로, 인증서 경로가 실수로
/// 저장소에 커밋되는 일이 없다. 배포판에 남기고 싶으면 `nl build` 에 같은 옵션을 준다.
pub fn apply_tls(project: &mut Project, inject: &TlsInject<'_>, base_dir: &Path) -> Result<usize> {
    use nl_core::{PNodeKind, Source, TlsConfig};

    let (cert, key) = match (inject.cert, inject.key) {
        (Some(c), Some(k)) => (c, k),
        (None, None) => {
            // 노드만 지정하고 인증서를 안 준 경우를 조용히 넘기지 않는다.
            if inject.node.is_some() {
                bail!("--http-node 는 --tls-cert/--tls-key 와 함께 써야 한다");
            }
            return Ok(0);
        }
        _ => bail!("--tls-cert 와 --tls-key 는 둘 다 주어야 한다"),
    };

    let cert_rel = relative_inside(base_dir, cert, "--tls-cert")?;
    let key_rel = relative_inside(base_dir, key, "--tls-key")?;
    // 없는 파일을 가리킨 채 배포하면 배포판이 켜질 때 비로소 터진다. 여기서 먼저 잡는다.
    for (opt, rel) in [("--tls-cert", &cert_rel), ("--tls-key", &key_rel)] {
        let full = base_dir.join(rel);
        if !full.exists() {
            bail!(
                "{opt} 가 가리키는 파일이 없다: {} ('nl tls-cert' 로 만들 수 있다)",
                full.display()
            );
        }
    }

    let cfg = TlsConfig {
        cert_pem: cert_rel,
        key_pem: key_rel,
    };

    let mut hit = 0usize;
    let mut seen_names: Vec<String> = Vec::new();
    for pl in project.pipelines.values_mut() {
        for n in pl.nodes.values_mut() {
            let PNodeKind::Source {
                source: Source::HttpServer { tls, .. },
            } = &mut n.kind
            else {
                continue;
            };
            seen_names.push(n.name.clone());
            if let Some(want) = inject.node {
                if n.name != want {
                    continue;
                }
            }
            *tls = Some(cfg.clone());
            hit += 1;
        }
    }

    if hit == 0 {
        if let Some(want) = inject.node {
            bail!(
                "--http-node {want} 에 맞는 HTTP 서버 노드가 없다 (있는 것: {})",
                if seen_names.is_empty() {
                    "없음".to_string()
                } else {
                    seen_names.join(", ")
                }
            );
        }
        bail!("이 프로젝트에 HTTP 서버 노드가 없어 TLS 를 붙일 곳이 없다");
    }
    Ok(hit)
}

/// 경로를 프로젝트 폴더 기준 상대 경로로 바꾼다. 밖을 가리키면 오류.
///
/// 실행기가 어차피 담장 검사를 하지만, 빌드할 때 걸러 주는 편이 낫다 —
/// 배포판을 만들고 나서 켤 때 알게 되면 늦다.
fn relative_inside(base_dir: &Path, path: &Path, opt: &str) -> Result<String> {
    let rel = if path.is_absolute() {
        let base_real = base_dir.canonicalize().unwrap_or_else(|_| base_dir.to_path_buf());
        let p_real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        p_real.strip_prefix(&base_real).map(Path::to_path_buf).map_err(|_| {
            anyhow!(
                "{opt} 는 프로젝트 폴더 안이어야 한다: {} (기준 {})",
                path.display(),
                base_dir.display()
            )
        })?
    } else {
        path.to_path_buf()
    };

    let mut out = String::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(seg) => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&seg.to_string_lossy());
            }
            std::path::Component::CurDir => {}
            _ => bail!("{opt} 가 프로젝트 폴더 밖을 가리킨다: {}", path.display()),
        }
    }
    if out.is_empty() {
        bail!("{opt} 가 비어 있다");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_strings_parse() {
        assert_eq!(parse_device("cpu").unwrap(), DevicePref::Cpu);
        assert_eq!(parse_device(" AUTO ").unwrap(), DevicePref::Auto);
        assert_eq!(parse_device("gpu:2").unwrap(), DevicePref::Gpu { index: 2 });
        assert_eq!(parse_device("gpu0").unwrap(), DevicePref::Gpu { index: 0 });
        assert!(parse_device("tpu").is_err());
        assert!(parse_device("gpu:x").is_err());
    }

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("nl-cli-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.json");
        write_atomic(&f, b"first").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"first");
        write_atomic(&f, b"second").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "임시 파일이 남았다");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_by_name_id_and_prefix() {
        let p = nl_core::sample::xor_project();
        let m = p.models.values().next().unwrap();
        assert_eq!(find_model(&p, &m.name).unwrap().id, m.id);
        let full = m.id.0.simple().to_string();
        assert_eq!(find_model(&p, &full).unwrap().id, m.id);
        assert_eq!(find_model(&p, &full[..8]).unwrap().id, m.id);
        let err = find_model(&p, "없는모델").unwrap_err().to_string();
        assert!(err.contains("찾을 수 없다"), "{err}");
    }

    /// TLS 주입은 프로젝트 파일이 아니라 메모리 위 사본만 바꾼다.
    #[test]
    fn tls_injection_touches_every_http_server_node() {
        let dir = std::env::temp_dir().join(format!("nl-cli-inject-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join("certs/server.crt"), "x").unwrap();
        std::fs::write(dir.join("certs/server.key"), "x").unwrap();

        let mut p = nl_core::sample::xor_project();
        let inject = TlsInject {
            cert: Some(Path::new("certs/server.crt")),
            key: Some(Path::new("certs/server.key")),
            node: None,
        };
        let n = apply_tls(&mut p, &inject, &dir).expect("주입");
        assert!(n >= 1, "붙은 노드가 없다");
        let tls_count = p
            .pipelines
            .values()
            .flat_map(|pl| pl.nodes.values())
            .filter(|node| {
                matches!(
                    &node.kind,
                    nl_core::PNodeKind::Source {
                        source: nl_core::Source::HttpServer { tls: Some(_), .. }
                    }
                )
            })
            .count();
        assert_eq!(tls_count, n, "센 것과 붙은 것이 다르다");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tls_injection_rejects_half_a_pair_and_bad_paths() {
        let dir = std::env::temp_dir().join(format!("nl-cli-inject-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = nl_core::sample::xor_project();

        // 한쪽만 준 경우.
        let only_cert = TlsInject {
            cert: Some(Path::new("a.crt")),
            key: None,
            node: None,
        };
        let err = apply_tls(&mut p, &only_cert, &dir).unwrap_err().to_string();
        assert!(err.contains("둘 다"), "{err}");

        // 프로젝트 폴더 밖.
        let outside = TlsInject {
            cert: Some(Path::new("../secret.crt")),
            key: Some(Path::new("../secret.key")),
            node: None,
        };
        let err = apply_tls(&mut p, &outside, &dir).unwrap_err().to_string();
        assert!(err.contains("밖을 가리킨다"), "{err}");

        // 없는 파일.
        let missing = TlsInject {
            cert: Some(Path::new("certs/none.crt")),
            key: Some(Path::new("certs/none.key")),
            node: None,
        };
        let err = apply_tls(&mut p, &missing, &dir).unwrap_err().to_string();
        assert!(err.contains("파일이 없다"), "{err}");

        // 없는 노드 이름.
        std::fs::create_dir_all(dir.join("certs")).unwrap();
        std::fs::write(dir.join("certs/server.crt"), "x").unwrap();
        std::fs::write(dir.join("certs/server.key"), "x").unwrap();
        let wrong_node = TlsInject {
            cert: Some(Path::new("certs/server.crt")),
            key: Some(Path::new("certs/server.key")),
            node: Some("없는노드"),
        };
        let err = apply_tls(&mut p, &wrong_node, &dir).unwrap_err().to_string();
        assert!(err.contains("없는노드"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 아무 옵션도 없으면 아무 일도 일어나지 않는다.
    #[test]
    fn no_tls_options_means_no_change() {
        let mut p = nl_core::sample::xor_project();
        let before = serde_json::to_string(&p).unwrap();
        let n = apply_tls(
            &mut p,
            &TlsInject {
                cert: None,
                key: None,
                node: None,
            },
            Path::new("."),
        )
        .expect("빈 주입");
        assert_eq!(n, 0);
        assert_eq!(serde_json::to_string(&p).unwrap(), before, "프로젝트가 바뀌었다");
    }

    #[test]
    fn human_size_rounds_up_the_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn display_width_ignores_color_and_counts_hangul_as_two() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("모델"), 4);
        assert_eq!(display_width("\x1b[31m오류\x1b[0m"), 4);
    }
}
