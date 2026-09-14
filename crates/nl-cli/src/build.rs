//! `nl build` — 프로젝트 → `.nlapp` 번들 → 런타임 첨부 → 배포 아카이브.
//!
//! ## 매니페스트 규약
//! `nl-runtime` 의 `prepare_workspace` 가 기대하는 모양에 정확히 맞춘다.
//!
//! - 번들 zip 안의 가중치 항목은 `weights/<model_id>.safetensors`
//! - `manifest.models[].weights_file` 은 **`weights/` 접두사 없이** `<model_id>.safetensors`
//!   (런타임이 `format!("weights/{}", weights_file)` 으로 다시 붙인다 — 여기서 붙이면 두 번 붙는다)
//! - `project.json` 의 `ModelDef.weights` 는 `weights/<model_id>.safetensors`
//!
//! 이렇게 두면 런타임이 가중치를 한 번만 풀고, `Runner` 가 프로젝트 상대 경로로 찾아도 같은 파일에 닿는다.

use crate::common::*;
use anyhow::{bail, Context, Result};
use nl_bundle::{archive_with, attach, find_runtime, ArchiveOptions, Artifact, Bundle, Target};
use nl_core::bundle::BundledModel;
use nl_core::{BundleManifest, Project};
use std::path::{Path, PathBuf};

pub struct Args<'a> {
    pub project: &'a Path,
    pub target: &'a str,
    pub out: &'a Path,
    pub name: Option<&'a str>,
    pub version: Option<&'a str>,
    pub pipeline: Option<&'a str>,
    pub runtime: Option<&'a Path>,
    pub icon: Option<&'a Path>,
    pub publisher: &'a str,
    /// 배포 앱이 실제 마우스·키보드 입력을 보내도 되는지. 기본 금지.
    pub arm_input: bool,
    /// `--tls-cert/--tls-key/--http-node`. **경로만** 번들에 들어간다 — 인증서 파일은 담지 않는다.
    pub tls: TlsInject<'a>,
}

pub fn run(args: Args<'_>) -> Result<i32> {
    let mut l = load_project(args.project)?;
    let targets = parse_targets(args.target)?;

    // 번들에 담기는 것은 `tls` 의 **경로 문자열뿐**이다. 인증서 파일은 넣지 않는다 —
    // 개인키가 든 `.nlapp` 은 그 자체가 유출이기 때문이다. 배포 앱은 실행 파일 옆에서 찾는다.
    let tls_nodes = apply_tls(&mut l.project, &args.tls, &l.base_dir)?;

    let app_name = args.name.unwrap_or(&l.project.name).to_string();
    let version = args.version.unwrap_or("0.1.0").to_string();

    let entry = match args.pipeline {
        Some(key) => Some(find_pipeline(&l.project, key)?.id),
        None => match l.project.pipelines.len() {
            1 => Some(l.project.pipelines.values().next().expect("하나 있다").id),
            0 => None,
            _ => {
                eprintln!(
                    "{}",
                    yellow("파이프라인이 여럿이라 진입 파이프라인을 비워 둔다 (--pipeline 으로 지정)")
                );
                None
            }
        },
    };
    if entry.is_none() && !l.project.pipelines.is_empty() {
        eprintln!(
            "{}",
            yellow("진입 파이프라인이 없으면 배포 앱이 아무것도 실행하지 않는다")
        );
    }

    // 아이콘: 지정이 없으면 프로젝트 폴더의 icon.png.
    let default_icon = l.base_dir.join("icon.png");
    let icon: Option<PathBuf> = match args.icon {
        Some(p) => {
            if !p.is_file() {
                bail!("아이콘 파일이 없다: {}", p.display());
            }
            Some(p.to_path_buf())
        }
        None => default_icon.is_file().then_some(default_icon),
    };

    let (bundle, missing) = make_bundle(&l.project, &l.base_dir, &app_name, &version, entry, args.arm_input)?;
    println!("{} {} {}", bold("번들"), app_name, version);
    println!("  {} {}개", dim("모델"), bundle.manifest.models.len());
    for m in &missing {
        println!(
            "  {} 모델 '{m}' 의 가중치가 없어 빼놓았다 (먼저 nl train)",
            yellow("경고")
        );
    }
    match entry {
        Some(id) => println!(
            "  {} {}",
            dim("진입 파이프라인"),
            l.project
                .pipelines
                .get(&id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.short())
        ),
        None => println!("  {}", dim("진입 파이프라인 없음")),
    }
    if let Some(i) = &icon {
        println!("  {} {}", dim("아이콘"), i.display());
    }
    if args.arm_input {
        println!("  {}", yellow("입력 무장 — 이 앱은 마우스·키보드를 실제로 조작한다"));
    }
    if tls_nodes > 0 {
        let cert = args.tls.cert.map(Path::display);
        println!("  {} HTTP 서버 노드 {tls_nodes}개가 https 로 열린다", dim("TLS"));
        if let Some(c) = cert {
            println!(
                "  {}",
                yellow(&format!(
                    "인증서 파일은 번들에 담지 않는다 — 설치한 기계의 실행 파일 옆에 {c} 를 그대로 두어라"
                ))
            );
        }
    }

    let zip = bundle.to_zip().context("번들 zip 을 만들지 못했다")?;
    println!("  {} {}", dim("크기"), human_size(zip.len() as u64));

    std::fs::create_dir_all(args.out).with_context(|| format!("{} 폴더를 만들지 못했다", args.out.display()))?;
    // 번들 자체도 남겨 둔다 — `nl-runtime <파일.nlapp>` 으로 바로 돌려 볼 수 있다.
    let slug = nl_bundle::slugify(&app_name);
    let nlapp = args.out.join(format!("{slug}-{version}.nlapp"));
    write_atomic(&nlapp, &zip)?;

    let mut artifacts: Vec<(String, Artifact)> = vec![(
        "번들".into(),
        Artifact {
            sha256: sha256_hex(&zip),
            size: zip.len() as u64,
            path: nlapp.clone(),
        },
    )];

    for target in &targets {
        println!();
        println!("{} {}", bold("대상"), target.label());
        let runtime = match args.runtime {
            Some(p) => {
                if !p.is_file() {
                    bail!("런타임 실행 파일이 없다: {}", p.display());
                }
                p.to_path_buf()
            }
            None => find_runtime(*target).with_context(|| {
                format!(
                    "{} 용 런타임({})을 찾지 못했다. --runtime 으로 경로를 주거나 \
                     `cargo build --release -p nl-runtime` 뒤 NL_RUNTIMES_DIR 를 설정하라",
                    target.label(),
                    target.runtime_file_name()
                )
            })?,
        };
        println!("  {} {}", dim("런타임"), runtime.display());

        let exe_name = match target {
            Target::WindowsX64 => format!("{slug}.exe"),
            Target::LinuxX64 => slug.clone(),
        };
        let staged = args.out.join(format!("stage-{}", target.triple())).join(&exe_name);
        attach(&runtime, &zip, &staged).context("런타임에 번들을 붙이지 못했다")?;
        println!("  {} {}", dim("첨부"), staged.display());

        let art =
            archive_with(ArchiveOptions::new(*target, &staged, &app_name, &version, args.out).icon(icon.as_deref()))
                .context("배포 아카이브를 만들지 못했다")?;
        artifacts.push((format!("{} 아카이브", target.label()), art));

        if *target == Target::WindowsX64 {
            match nl_bundle::windows_installer(&staged, &app_name, &version, args.publisher, args.out, icon.as_deref())
            {
                Ok(Some(a)) => artifacts.push(("Windows 설치 프로그램".into(), a)),
                Ok(None) => println!(
                    "  {}",
                    dim("Inno Setup 이 없어 설치 프로그램은 건너뛰었다 (zip 은 만들어졌다)")
                ),
                Err(e) => println!("  {} 설치 프로그램 생성 실패: {e:#}", yellow("경고")),
            }
        }
    }

    // ── 결과 표 + latest.json ──
    println!();
    let mut rows = vec![vec!["산출물".into(), "파일".into(), "크기".into(), "sha256".into()]];
    for (kind, a) in &artifacts {
        rows.push(vec![
            kind.clone(),
            a.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            human_size(a.size),
            a.sha256[..16].to_string(),
        ]);
    }
    table(&rows);

    let latest = serde_json::json!({
        "app_name": app_name,
        "version": version,
        "built_at": chrono::Utc::now().to_rfc3339(),
        "artifacts": artifacts.iter().map(|(kind, a)| serde_json::json!({
            "kind": kind,
            "file": a.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            "path": a.path.to_string_lossy(),
            "size": a.size,
            "sha256": a.sha256,
        })).collect::<Vec<_>>(),
    });
    let latest_path = args.out.join("latest.json");
    write_atomic(&latest_path, serde_json::to_string_pretty(&latest)?.as_bytes())?;
    println!();
    println!("  {} {}", dim("목록"), latest_path.display());
    Ok(0)
}

/// 프로젝트를 번들로. 가중치가 있는 모델만 싣고, 없는 모델 이름을 함께 돌려준다.
fn make_bundle(
    project: &Project,
    base_dir: &Path,
    app_name: &str,
    version: &str,
    entry: Option<nl_core::PipelineId>,
    arm_input: bool,
) -> Result<(Bundle, Vec<String>)> {
    let mut manifest = BundleManifest::new(app_name, version);
    manifest.built_with = format!("nl-cli {}", env!("CARGO_PKG_VERSION"));
    manifest.entry_pipeline = entry;
    manifest.default_device = project.settings.default_device;
    manifest.arm_input = arm_input;

    // 프로젝트 사본의 가중치 경로를 번들 규약에 맞춰 다시 쓴다.
    let mut packed = project.clone();
    let mut missing = Vec::new();
    let mut weights = std::collections::BTreeMap::new();

    for (id, model) in &project.models {
        let Some(rel) = model.weights.as_deref() else {
            missing.push(model.name.clone());
            continue;
        };
        let src = base_dir.join(rel);
        let bytes = match std::fs::read(&src) {
            Ok(b) => b,
            Err(e) => {
                missing.push(format!("{} ({e})", model.name));
                continue;
            }
        };
        // zip 안에서는 `weights/<model_id>.safetensors`, 매니페스트에는 접두사 없이.
        let file = format!("{}.safetensors", id.0.simple());
        weights.insert(file.clone(), bytes);
        manifest.models.push(BundledModel {
            model: *id,
            weights_file: file.clone(),
        });
        if let Some(m) = packed.models.get_mut(id) {
            m.weights = Some(format!("weights/{file}"));
        }
    }
    // 가중치를 못 실은 모델은 배포판에서 경로를 비운다 (없는 파일을 가리키면 런타임이 헤맨다).
    for name in &missing {
        if let Some(m) = packed.models.values_mut().find(|m| name.starts_with(&m.name)) {
            m.weights = None;
        }
    }

    let mut bundle = Bundle::new(manifest, packed);
    bundle.weights = weights;
    Ok((bundle, missing))
}

fn parse_targets(s: &str) -> Result<Vec<Target>> {
    match s.trim().to_lowercase().as_str() {
        "linux" => Ok(vec![Target::LinuxX64]),
        "windows" => Ok(vec![Target::WindowsX64]),
        "all" => Ok(vec![Target::LinuxX64, Target::WindowsX64]),
        "host" => Target::host().map(|t| vec![t]).context("이 플랫폼용 대상이 없다"),
        other => bail!("모르는 대상: {other} (linux | windows | all | host)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_words_parse() {
        assert_eq!(parse_targets("linux").unwrap(), vec![Target::LinuxX64]);
        assert_eq!(parse_targets(" WINDOWS ").unwrap(), vec![Target::WindowsX64]);
        assert_eq!(parse_targets("all").unwrap().len(), 2);
        assert!(parse_targets("bsd").is_err());
    }

    /// 매니페스트가 런타임의 기대와 어긋나면 배포판이 가중치를 못 찾는다.
    #[test]
    fn manifest_paths_match_what_the_runtime_expects() {
        let dir = std::env::temp_dir().join(format!("nl-cli-bundle-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("runs")).unwrap();
        let mut project = nl_core::sample::xor_project();
        let model_id = *project.models.keys().next().unwrap();
        std::fs::write(dir.join("runs/w.safetensors"), b"fake-weights").unwrap();
        project.models.get_mut(&model_id).unwrap().weights = Some("runs/w.safetensors".into());

        let (bundle, missing) = make_bundle(&project, &dir, "앱", "1.0.0", None, false).unwrap();
        assert!(missing.is_empty(), "{missing:?}");
        assert_eq!(bundle.manifest.models.len(), 1);

        let bm = &bundle.manifest.models[0];
        let expected = format!("{}.safetensors", model_id.0.simple());
        // 매니페스트에는 접두사가 없어야 한다 — 런타임이 "weights/" 를 다시 붙인다.
        assert_eq!(bm.weights_file, expected);
        assert!(!bm.weights_file.starts_with("weights/"));
        // 가중치 맵의 키도 접두사 없이.
        assert!(bundle.weights.contains_key(&expected), "{:?}", bundle.weights.keys());
        // 프로젝트 쪽 경로에는 접두사가 붙는다.
        assert_eq!(
            bundle.project.models[&model_id].weights.as_deref(),
            Some(format!("weights/{expected}").as_str())
        );
        // 이 둘이 맞아떨어져야 런타임이 파일을 두 번 쓰지 않는다.
        assert_eq!(
            bundle.project.models[&model_id].weights.as_deref(),
            Some(format!("weights/{}", bm.weights_file).as_str())
        );

        // zip 왕복 뒤에도 키가 유지된다.
        let zip = bundle.to_zip().unwrap();
        let back = Bundle::from_zip(&zip).unwrap();
        assert!(back.weights.contains_key(&expected), "{:?}", back.weights.keys());
        assert_eq!(back.manifest.models[0].weights_file, expected);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn models_without_weights_are_reported_and_left_out() {
        let dir = std::env::temp_dir().join(format!("nl-cli-bundle2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let project = nl_core::sample::xor_project();
        let (bundle, missing) = make_bundle(&project, &dir, "앱", "1.0.0", None, false).unwrap();
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(bundle.manifest.models.is_empty());
        assert!(bundle.weights.is_empty());
        assert!(bundle.project.models.values().all(|m| m.weights.is_none()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
