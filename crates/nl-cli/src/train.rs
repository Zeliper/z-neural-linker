//! `nl train` — 헤드리스 학습. 끝나면 실행 기록과 가중치 경로를 프로젝트에 적고 원자적으로 저장한다.

use crate::common::*;
use anyhow::{bail, Context, Result};
use nl_core::{ops::Op, RunId, RunStatus};
use nl_engine::{TrainEvent, TrainRequest};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Args<'a> {
    pub project: &'a Path,
    pub model: &'a str,
    pub dataset: Option<&'a str>,
    pub device: Option<&'a str>,
    pub epochs: Option<usize>,
    pub run_dir: Option<&'a Path>,
    pub resume: bool,
}

pub fn run(args: Args<'_>) -> Result<i32> {
    install_signal_handler();
    let mut l = load_project(args.project)?;

    let mut model = find_model(&l.project, args.model)?.clone();
    let dataset = match args.dataset {
        Some(key) => find_dataset(&l.project, key)?.clone(),
        None => {
            let id = model
                .train
                .dataset
                .with_context(|| format!("모델 '{}' 에 학습 데이터셋이 없다. --dataset 으로 지정하라", model.name))?;
            l.project
                .datasets
                .get(&id)
                .with_context(|| format!("모델 '{}' 이 가리키는 데이터셋이 프로젝트에 없다", model.name))?
                .clone()
        }
    };
    model.train.dataset = Some(dataset.id);
    if let Some(e) = args.epochs {
        if e == 0 {
            bail!("--epochs 는 1 이상이어야 한다");
        }
        model.train.epochs = e;
    }
    if let Some(d) = args.device {
        model.train.device = parse_device(d)?;
    }

    let run_id = RunId::new();
    let run_dir = match args.run_dir {
        Some(p) => p.to_path_buf(),
        None => default_run_dir(&l.base_dir, run_id),
    };
    let resume_from = if args.resume {
        model
            .weights
            .as_ref()
            .map(|w| l.base_dir.join(w))
            .filter(|p| p.is_file())
    } else {
        None
    };

    println!(
        "{} {} · 데이터셋 {} · {} 에포크 · 장치 {}",
        bold("학습"),
        model.name,
        dataset.name,
        model.train.epochs,
        model.train.device.label()
    );
    if let Some(r) = &resume_from {
        println!("  {} {}", dim("이어서"), r.display());
    }
    println!("  {} {}", dim("실행 폴더"), run_dir.display());

    let handle = nl_engine::start(TrainRequest {
        run_id,
        model: model.clone(),
        dataset,
        base_dir: l.base_dir.clone(),
        run_dir: run_dir.clone(),
        resume_from,
    })
    .context("학습을 시작하지 못했다")?;

    let mut final_run = None;
    let mut failure: Option<String> = None;
    let mut asked_stop = false;
    loop {
        if interrupted() && !asked_stop {
            asked_stop = true;
            clear_line();
            println!("{}", yellow("중단 요청 — 지금까지의 결과를 저장하고 끝낸다"));
            handle.stop();
        }
        match handle.events.recv_timeout(Duration::from_millis(100)) {
            Ok(TrainEvent::Started {
                device,
                batches_per_epoch,
                params,
            }) => {
                println!(
                    "  {} {device} · 에포크당 {batches_per_epoch} 배치 · 파라미터 {params}개",
                    dim("장치")
                );
            }
            Ok(TrainEvent::Step { epoch, step, loss }) => {
                progress(&format!("에포크 {epoch} · 스텝 {step} · loss {loss:.4}"));
            }
            Ok(TrainEvent::Epoch(m)) => {
                clear_line();
                println!("{}", epoch_line(&m, &model.train.metric));
            }
            Ok(TrainEvent::Checkpoint { path }) => {
                progress(&format!("체크포인트 {}", path.display()));
            }
            Ok(TrainEvent::Log(msg)) => {
                clear_line();
                println!("  {}", dim(&msg));
            }
            Ok(TrainEvent::Finished { run }) => {
                final_run = Some(run);
                break;
            }
            Ok(TrainEvent::Failed { run, error }) => {
                final_run = Some(run);
                failure = Some(error);
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    clear_line();

    let Some(run) = final_run else {
        bail!("학습 스레드가 결과를 남기지 않고 끝났다");
    };

    // ── 프로젝트에 기록: 실행 기록 + 가중치 경로 ──
    let weights_rel = run.checkpoint.clone();
    nl_core::ops::apply_op(&mut l.project, &Op::UpsertRun { run: run.clone() });
    if let (Some(rel), Some(m)) = (weights_rel.as_ref(), l.project.models.get_mut(&model.id)) {
        m.weights = Some(rel.clone());
        m.train = model.train.clone();
    }
    save_project_atomic(&l.path, &l.project).with_context(|| format!("{} 저장 실패", l.path.display()))?;

    // ── 요약 ──
    println!();
    let last = run.epochs.last();
    println!("{} {:?}", bold("상태"), run.status);
    println!("  {} {}", dim("장치"), run.device_name);
    println!("  {} {}", dim("에포크"), run.epochs.len());
    if let Some(m) = last {
        println!("  {} {:.4}", dim("마지막 train loss"), m.train_loss);
        if let Some(v) = m.val_loss {
            println!("  {} {v:.4}", dim("val loss"));
        }
        if let Some(v) = m.val_metric {
            println!(
                "  {} {}",
                dim(&format!("{} (val)", model.train.metric.label())),
                metric_text(v, &model.train.metric)
            );
        }
    }
    match &weights_rel {
        Some(w) => println!("  {} {}", dim("가중치"), w),
        None => println!("  {}", yellow("가중치가 저장되지 않았다")),
    }
    println!("  {} {}", dim("프로젝트 갱신"), l.path.display());

    if let Some(e) = failure {
        eprintln!("{} {e}", red("학습 실패"));
        return Ok(1);
    }
    Ok(if run.status == RunStatus::Failed { 1 } else { 0 })
}

fn epoch_line(m: &nl_core::EpochMetrics, metric: &nl_core::Metric) -> String {
    let mut s = format!("  에포크 {:>3} · loss {:.4}", m.epoch, m.train_loss);
    if let Some(v) = m.val_loss {
        s.push_str(&format!(" · val {v:.4}"));
    }
    if let Some(v) = m.val_metric {
        s.push_str(&format!(" · {} {}", metric.label(), metric_text(v, metric)));
    }
    s.push_str(&format!(" · {:.1}s", m.seconds));
    s
}

fn metric_text(v: f64, metric: &nl_core::Metric) -> String {
    match metric {
        nl_core::Metric::Accuracy => format!("{:.1}%", v * 100.0),
        _ => format!("{v:.4}"),
    }
}

/// 한 줄을 덮어쓰는 진행 표시. 터미널이 아니면 아무것도 하지 않는다(로그가 지저분해지지 않게).
fn progress(text: &str) {
    if !color_enabled() {
        return;
    }
    print!("\r\x1b[2K  {text}");
    let _ = std::io::stdout().flush();
}

fn clear_line() {
    if color_enabled() {
        print!("\r\x1b[2K");
        let _ = std::io::stdout().flush();
    }
}

/// `--run-dir` 기본값 계산을 테스트에서도 쓰기 위해 분리.
pub fn default_run_dir(base: &Path, run: RunId) -> PathBuf {
    base.join("runs").join(run.short())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_run_dir_is_under_the_project_folder() {
        let id = RunId::from_u128(0xabc);
        let p = default_run_dir(Path::new("/tmp/proj"), id);
        assert!(p.starts_with("/tmp/proj/runs"), "{}", p.display());
        assert!(p.to_string_lossy().ends_with(&id.short()));
    }

    #[test]
    fn accuracy_is_shown_as_a_percentage() {
        assert_eq!(metric_text(0.935, &nl_core::Metric::Accuracy), "93.5%");
        assert_eq!(metric_text(0.25, &nl_core::Metric::Mae), "0.2500");
    }
}
