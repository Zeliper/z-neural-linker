//! `nl` — Neural Linker 명령줄 도구.
//!
//! GUI 없이 프로젝트를 들여다보고, 학습하고, 추론하고, 파이프라인을 돌리고, 화면을 녹화하고, 배포판을 만든다.
//! 스크립트와 CI 에서 쓰라고 만든 것이라 **출력은 사람이 읽기 좋게, 종료 코드는 기계가 읽기 좋게** 둔다.
//!
//! 종료 코드: `0` 성공, `1` 명령이 문제를 찾음(검증 오류·학습 실패·실행 중 오류), `2` 인자나 환경 문제.

mod build;
mod common;
mod devices;
mod infer;
mod inspect;
mod record;
mod run;
mod train;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "nl",
    version,
    about = "Neural Linker 명령줄 도구",
    long_about = "GUI 없이 Neural Linker 프로젝트를 다룬다. 스크립트·CI 용.\n\
                  종료 코드: 0 성공, 1 문제를 찾음, 2 인자/환경 문제."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 프로젝트 내용과 검증 결과를 본다.
    Inspect {
        /// `.nlproj` 파일.
        project: PathBuf,
    },
    /// 쓸 수 있는 장치 목록.
    Devices {
        /// 각 장치에서 실제로 작은 연산을 돌려 본다 (느릴 수 있다).
        #[arg(long)]
        probe: bool,
    },
    /// 모델을 학습하고 결과를 프로젝트에 적는다.
    Train {
        project: PathBuf,
        /// 모델 이름 또는 id(앞부분도 된다).
        #[arg(long)]
        model: String,
        /// 데이터셋 이름 또는 id. 없으면 모델의 학습 설정을 쓴다.
        #[arg(long)]
        dataset: Option<String>,
        /// cpu | gpu:0 | auto.
        #[arg(long)]
        device: Option<String>,
        /// 에포크 수를 덮어쓴다.
        #[arg(long)]
        epochs: Option<usize>,
        /// 체크포인트를 쓸 폴더. 기본은 `<프로젝트>/runs/<run-id>`.
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// 모델의 기존 가중치에서 이어서 학습한다.
        #[arg(long)]
        resume: bool,
    },
    /// 학습된 모델로 추론한다.
    Infer {
        project: PathBuf,
        #[arg(long)]
        model: String,
        /// JSON 값. 예: `[0,1]`
        #[arg(long, conflicts_with_all = ["image", "csv"])]
        input: Option<String>,
        /// 이미지 파일 하나.
        #[arg(long, conflicts_with = "csv")]
        image: Option<PathBuf>,
        /// 쉼표로 나뉜 수. 행마다 한 번씩 추론한다.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// 페이로드 이름 또는 id. 없으면 모델에 붙은 것.
        #[arg(long)]
        payload: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
    /// 파이프라인을 헤드리스로 돌린다.
    Run {
        project: PathBuf,
        /// 파이프라인 이름 또는 id. 하나뿐이면 생략해도 된다.
        #[arg(long)]
        pipeline: Option<String>,
        /// 이 시간이 지나면 정지한다 (초).
        #[arg(long = "for", value_name = "초")]
        seconds: Option<f64>,
        /// 마우스·키보드 싱크를 무장한다. 실제 입력이 나간다.
        #[arg(long)]
        arm_input: bool,
        #[arg(long)]
        device: Option<String>,
    },
    /// 화면을 찍어 학습용 폴더를 만든다.
    Record {
        /// 만들 폴더.
        out: PathBuf,
        #[arg(long, default_value_t = 0)]
        monitor: usize,
        /// 모니터 기준 가로 위치.
        #[arg(long, default_value_t = 0)]
        x: i32,
        /// 모니터 기준 세로 위치.
        #[arg(long, default_value_t = 0)]
        y: i32,
        /// 0 이면 모니터 전체.
        #[arg(long, default_value_t = 0)]
        width: u32,
        #[arg(long, default_value_t = 0)]
        height: u32,
        #[arg(long, default_value_t = 4.0)]
        fps: f32,
        /// 이 시간이 지나면 멈춘다 (초).
        #[arg(long = "for", value_name = "초")]
        seconds: Option<f64>,
        /// 받아들일 라벨 목록 (예: "0,1,2"). 비우면 아무 정수나 받는다.
        #[arg(long, value_name = "목록")]
        label_keys: Option<String>,
    },
    /// 배포판을 만든다.
    Build {
        project: PathBuf,
        /// linux | windows | all | host.
        #[arg(long, default_value = "host")]
        target: String,
        /// 산출물을 놓을 폴더.
        #[arg(long, default_value = "dist")]
        out: PathBuf,
        /// 앱 이름. 기본은 프로젝트 이름.
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "0.1.0")]
        version: String,
        /// 시작할 파이프라인. 하나뿐이면 생략해도 된다.
        #[arg(long)]
        pipeline: Option<String>,
        /// 런타임 실행 파일. 없으면 표준 위치에서 찾는다.
        #[arg(long)]
        runtime: Option<PathBuf>,
        /// 앱 아이콘 PNG. 기본은 프로젝트 폴더의 icon.png.
        #[arg(long)]
        icon: Option<PathBuf>,
        /// Windows 설치 프로그램에 적을 배포자.
        #[arg(long, default_value = "Neural Linker")]
        publisher: String,
        /// 배포 앱이 마우스·키보드를 실제로 조작하도록 허용한다. 기본은 금지.
        #[arg(long)]
        arm_input: bool,
    },
    /// XOR 샘플 프로젝트를 만든다.
    Sample {
        /// 만들 `.nlproj` 파일. `--list` 와 함께면 생략한다.
        #[arg(required_unless_present = "list")]
        out: Option<PathBuf>,
        /// 어떤 샘플을 만들지. `nl sample --list` 로 목록을 본다.
        #[arg(long, default_value = "xor")]
        kind: String,
        /// 만들 수 있는 샘플 목록만 보여 준다.
        #[arg(long, conflicts_with = "kind")]
        list: bool,
    },
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let code = match dispatch() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} {e:#}", common::red("오류"));
            2
        }
    };
    std::process::exit(code);
}

fn dispatch() -> Result<i32> {
    match Cli::parse().command {
        Command::Inspect { project } => inspect::run(&project),
        Command::Devices { probe } => devices::run(probe),
        Command::Train {
            project,
            model,
            dataset,
            device,
            epochs,
            run_dir,
            resume,
        } => train::run(train::Args {
            project: &project,
            model: &model,
            dataset: dataset.as_deref(),
            device: device.as_deref(),
            epochs,
            run_dir: run_dir.as_deref(),
            resume,
        }),
        Command::Infer {
            project,
            model,
            input,
            image,
            csv,
            payload,
            device,
        } => infer::run(infer::Args {
            project: &project,
            model: &model,
            input: input.as_deref(),
            image: image.as_deref(),
            csv: csv.as_deref(),
            payload: payload.as_deref(),
            device: device.as_deref(),
        }),
        Command::Run {
            project,
            pipeline,
            seconds,
            arm_input,
            device,
        } => run::run(run::Args {
            project: &project,
            pipeline: pipeline.as_deref(),
            seconds,
            arm_input,
            device: device.as_deref(),
        }),
        Command::Record {
            out,
            monitor,
            x,
            y,
            width,
            height,
            fps,
            seconds,
            label_keys,
        } => {
            let allowed = match label_keys.as_deref() {
                Some(list) => Some(record::parse_label_keys(list)?),
                None => None,
            };
            record::run(record::Args {
                out: &out,
                monitor,
                x,
                y,
                width,
                height,
                fps,
                seconds,
                allowed,
            })
        }
        Command::Build {
            project,
            target,
            out,
            name,
            version,
            pipeline,
            runtime,
            icon,
            publisher,
            arm_input,
        } => build::run(build::Args {
            project: &project,
            target: &target,
            out: &out,
            name: name.as_deref(),
            version: Some(&version),
            pipeline: pipeline.as_deref(),
            runtime: runtime.as_deref(),
            icon: icon.as_deref(),
            publisher: &publisher,
            arm_input,
        }),
        Command::Sample { out, kind, list } => {
            if list {
                list_samples()
            } else {
                make_sample(
                    out.as_deref().expect("clap 이 required_unless_present 로 보장한다"),
                    &kind,
                )
            }
        }
    }
}

/// `--kind` 값 → nl-core 의 샘플 함수.
///
/// 샘플 자체는 `nl-core` 한 곳에 있다 — 빌더의 "샘플 열기" 와 정확히 같은 프로젝트가 나온다.
fn sample_factory(kind: &str) -> Result<(&'static str, nl_core::sample::SampleFactory)> {
    match kind.trim().to_lowercase().as_str() {
        "xor" => Ok(("xor", nl_core::sample::xor_project as nl_core::sample::SampleFactory)),
        "cnn" | "quadrants" => Ok(("cnn", nl_core::sample::quadrants_cnn_project as _)),
        other => anyhow::bail!("모르는 샘플: {other} (xor | cnn). `nl sample --list` 로 목록을 본다"),
    }
}

fn make_sample(out: &std::path::Path, kind: &str) -> Result<i32> {
    let (key, make) = sample_factory(kind)?;
    let project = make();
    common::save_project_atomic(out, &project)?;

    let model = project.models.values().next();
    println!("{} {} ({key})", common::bold("샘플"), out.display());
    println!(
        "  {} {}",
        common::dim("모델"),
        model.map(|m| m.name.as_str()).unwrap_or("")
    );
    println!(
        "  {} {}",
        common::dim("데이터셋"),
        project.datasets.values().next().map(|d| d.name.as_str()).unwrap_or("")
    );
    if !project.pipelines.is_empty() {
        let names: Vec<&str> = project.pipelines.values().map(|p| p.name.as_str()).collect();
        println!("  {} {}", common::dim("파이프라인"), names.join(", "));
    }

    if let Some(m) = model {
        println!();
        println!("{}", common::dim("다음:"));
        println!(
            "  nl train {} --model '{}' --device cpu --epochs 30",
            out.display(),
            m.name
        );
        // 입력 형태가 달라 예시도 갈린다.
        if key == "cnn" {
            println!("  nl infer {} --model '{}' --image 그림.png", out.display(), m.name);
        } else {
            println!("  nl infer {} --model '{}' --input '[1,0]'", out.display(), m.name);
        }
    }
    Ok(0)
}

/// 만들 수 있는 샘플 목록.
fn list_samples() -> Result<i32> {
    let mut rows = vec![vec!["--kind".into(), "이름".into(), "모델".into(), "설명".into()]];
    for (key, make) in [
        ("xor", nl_core::sample::xor_project as nl_core::sample::SampleFactory),
        ("cnn", nl_core::sample::quadrants_cnn_project as _),
    ] {
        let p = make();
        rows.push(vec![
            key.to_string(),
            p.name.clone(),
            p.models.values().next().map(|m| m.name.clone()).unwrap_or_default(),
            p.description.clone(),
        ]);
    }
    common::table(&rows);
    println!();
    println!("{}", common::dim("예: nl sample /tmp/x.nlproj --kind cnn"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommands_parse_their_arguments() {
        let c = Cli::parse_from(["nl", "inspect", "a.nlproj"]);
        assert!(matches!(c.command, Command::Inspect { .. }));

        let c = Cli::parse_from([
            "nl", "train", "a.nlproj", "--model", "M", "--epochs", "7", "--device", "cpu",
        ]);
        match c.command {
            Command::Train {
                model,
                epochs,
                device,
                resume,
                ..
            } => {
                assert_eq!(model, "M");
                assert_eq!(epochs, Some(7));
                assert_eq!(device.as_deref(), Some("cpu"));
                assert!(!resume);
            }
            _ => panic!("train 이 아니다"),
        }

        let c = Cli::parse_from(["nl", "run", "a.nlproj", "--for", "2.5", "--arm-input"]);
        match c.command {
            Command::Run { seconds, arm_input, .. } => {
                assert_eq!(seconds, Some(2.5));
                assert!(arm_input);
            }
            _ => panic!("run 이 아니다"),
        }

        let c = Cli::parse_from([
            "nl",
            "record",
            "/tmp/out",
            "--fps",
            "10",
            "--for",
            "3",
            "--label-keys",
            "0,1",
        ]);
        match c.command {
            Command::Record {
                fps,
                seconds,
                monitor,
                width,
                label_keys,
                ..
            } => {
                assert_eq!(fps, 10.0);
                assert_eq!(seconds, Some(3.0));
                assert_eq!(monitor, 0, "기본 모니터는 0");
                assert_eq!(width, 0, "기본은 모니터 전체");
                assert_eq!(label_keys.as_deref(), Some("0,1"));
            }
            _ => panic!("record 가 아니다"),
        }

        let c = Cli::parse_from(["nl", "build", "a.nlproj"]);
        match c.command {
            Command::Build {
                target,
                out,
                version,
                publisher,
                ..
            } => {
                assert_eq!(target, "host");
                assert_eq!(out, PathBuf::from("dist"));
                assert_eq!(version, "0.1.0");
                assert_eq!(publisher, "Neural Linker");
            }
            _ => panic!("build 가 아니다"),
        }
    }

    #[test]
    fn infer_input_sources_are_mutually_exclusive() {
        assert!(
            Cli::try_parse_from(["nl", "infer", "a.nlproj", "--model", "M", "--input", "[1]", "--image", "a.png"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["nl", "infer", "a.nlproj", "--model", "M", "--image", "a.png", "--csv", "a.csv"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["nl", "infer", "a.nlproj", "--model", "M", "--input", "[1]"]).is_ok());
    }

    #[test]
    fn missing_required_arguments_fail() {
        // 모델 없이 학습할 수 없다.
        assert!(Cli::try_parse_from(["nl", "train", "a.nlproj"]).is_err());
        // 프로젝트 경로가 없으면 안 된다.
        assert!(Cli::try_parse_from(["nl", "inspect"]).is_err());
    }

    #[test]
    fn sample_kinds_map_to_the_core_factories() {
        assert_eq!(sample_factory("xor").unwrap().0, "xor");
        assert_eq!(sample_factory(" XOR ").unwrap().0, "xor");
        assert_eq!(sample_factory("cnn").unwrap().0, "cnn");
        assert_eq!(sample_factory("quadrants").unwrap().0, "cnn");
        let err = sample_factory("mlp").unwrap_err().to_string();
        assert!(err.contains("--list"), "{err}");
    }

    #[test]
    fn the_cnn_kind_writes_the_cnn_project() {
        let dir = std::env::temp_dir().join(format!("nl-cli-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cnn.nlproj");
        assert_eq!(make_sample(&path, "cnn").unwrap(), 0);

        let p = common::load_project(&path).unwrap().project;
        assert_eq!(p.id, nl_core::sample::quadrants_cnn_project().id);
        assert!(p.models.values().any(|m| m.name == "사분면 CNN"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn listing_samples_needs_no_output_path() {
        // `--list` 면 경로가 없어도 된다.
        assert!(Cli::try_parse_from(["nl", "sample", "--list"]).is_ok());
        // 경로도 `--kind` 도 없으면 오류.
        assert!(Cli::try_parse_from(["nl", "sample"]).is_err());
        // `--list` 와 `--kind` 는 함께 쓸 수 없다.
        assert!(Cli::try_parse_from(["nl", "sample", "--list", "--kind", "cnn"]).is_err());

        let c = Cli::parse_from(["nl", "sample", "/tmp/x.nlproj", "--kind", "cnn"]);
        match c.command {
            Command::Sample { out, kind, list } => {
                assert_eq!(out.as_deref(), Some(std::path::Path::new("/tmp/x.nlproj")));
                assert_eq!(kind, "cnn");
                assert!(!list);
            }
            _ => panic!("sample 이 아니다"),
        }
        // 기본은 xor.
        match Cli::parse_from(["nl", "sample", "/tmp/x.nlproj"]).command {
            Command::Sample { kind, .. } => assert_eq!(kind, "xor"),
            _ => panic!("sample 이 아니다"),
        }
        assert_eq!(list_samples().unwrap(), 0);
    }

    /// nl-core 샘플이 파일을 거쳐도 온전한지: 검증 문제 없음 + 파이프라인 2개 + GUI 4위젯 + 빌드 설정.
    /// 빌더와 명령줄이 같은 것을 쓰는지 지키는 자리다 (nl-core 는 nl-app 을 볼 수 없어 여기서 확인한다).
    #[test]
    fn the_core_sample_survives_a_file_round_trip() {
        use nl_core::Severity;
        let dir = std::env::temp_dir().join(format!("nl-cli-core-sample-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("xor.nlproj");
        assert_eq!(make_sample(&path, "xor").unwrap(), 0);

        let p = common::load_project(&path).unwrap().project;
        let errors: Vec<_> = nl_core::validate(&p)
            .into_iter()
            .filter(|i| i.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "검증 오류: {errors:?}");

        assert_eq!(p.pipelines.len(), 2, "시험 + 추론 API");
        assert!(p.pipelines.values().any(|x| x.name == "XOR 시험"));
        assert!(p.pipelines.values().any(|x| x.name == "추론 API"));
        assert_eq!(p.gui.widgets.len(), 4, "GUI 위젯 4개");
        let build = p.settings.build.as_ref().expect("빌드 설정이 파일을 거치며 사라졌다");
        assert_eq!(build.app_name, "XOR 분류기");
        assert!(!build.arm_input);

        // 모델·데이터셋·페이로드도 그대로.
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.datasets.len(), 1);
        assert_eq!(p.payloads.len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// CNN 샘플도 같은 왕복을 견딘다.
    #[test]
    fn the_cnn_sample_survives_a_file_round_trip() {
        use nl_core::Severity;
        let dir = std::env::temp_dir().join(format!("nl-cli-cnn-sample-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cnn.nlproj");
        common::save_project_atomic(&path, &nl_core::sample::quadrants_cnn_project()).unwrap();

        let p = common::load_project(&path).unwrap().project;
        let errors: Vec<_> = nl_core::validate(&p)
            .into_iter()
            .filter(|i| i.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "검증 오류: {errors:?}");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.pipelines.len(), 1, "CNN 은 추론 API 하나만");
        assert!(p.pipelines.values().all(|x| x.name == "추론 API"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `nl sample` → 파일 → `inspect` 가 읽을 수 있는지 왕복으로 확인한다.
    #[test]
    fn sample_round_trips_through_the_project_file() {
        let dir = std::env::temp_dir().join(format!("nl-cli-sample-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("xor.nlproj");

        assert_eq!(make_sample(&path, "xor").unwrap(), 0);
        assert!(path.is_file());

        let mut loaded = common::load_project(&path).unwrap().project;
        let mut expected = nl_core::sample::xor_project();
        // `created` 는 만든 시각이라 비교에서 뺀다 (나머지는 전부 같아야 한다).
        expected.created = loaded.created;
        assert_eq!(loaded, expected, "저장/읽기로 프로젝트가 달라졌다");
        loaded.created = expected.created;

        // 검증 오류가 없어야 inspect 가 0 을 돌려준다.
        assert_eq!(inspect::run(&path).unwrap(), 0, "샘플에 검증 오류가 있다");

        std::fs::remove_dir_all(&dir).ok();
    }
}
