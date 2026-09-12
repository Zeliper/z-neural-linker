//! `nl run` — 파이프라인을 헤드리스로 돌리고 이벤트를 찍는다.

use crate::common::*;
use anyhow::{Context, Result};
use nl_io::runner::{Runner, RunnerEvent};
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Args<'a> {
    pub project: &'a Path,
    pub pipeline: Option<&'a str>,
    pub seconds: Option<f64>,
    pub arm_input: bool,
    pub device: Option<&'a str>,
}

pub fn run(args: Args<'_>) -> Result<i32> {
    install_signal_handler();
    let l = load_project(args.project)?;

    let pipeline = match args.pipeline {
        Some(key) => find_pipeline(&l.project, key)?.clone(),
        None => match l.project.pipelines.len() {
            1 => l.project.pipelines.values().next().expect("하나 있다").clone(),
            0 => anyhow::bail!("프로젝트에 파이프라인이 없다"),
            n => anyhow::bail!("파이프라인이 {n}개다. --pipeline 으로 하나를 지정하라"),
        },
    };
    let device = match args.device {
        Some(d) => parse_device(d)?,
        None => l.project.settings.default_device,
    };

    println!(
        "{} {} · 노드 {} · {:.0}Hz · 장치 {}",
        bold("실행"),
        pipeline.name,
        pipeline.nodes.len(),
        pipeline.tick_hz,
        device.label()
    );
    if args.arm_input {
        println!("{}", yellow("  마우스·키보드 싱크가 무장됐다 — 실제 입력이 나간다"));
    }
    match args.seconds {
        Some(s) => println!("  {} {s}초 뒤 자동 정지", dim("기간")),
        None => println!("  {}", dim("Ctrl+C 로 정지")),
    }

    let mut runner = Runner::new(l.project.clone(), pipeline, l.base_dir.clone(), device);
    runner.arm_input = args.arm_input;
    let handle = runner.start().context("파이프라인을 시작하지 못했다")?;

    let start = Instant::now();
    let deadline = args.seconds.map(|s| start + Duration::from_secs_f64(s));
    let mut errors = 0usize;
    let mut asked_stop = false;

    loop {
        let over = deadline.is_some_and(|d| Instant::now() >= d);
        if (interrupted() || over) && !asked_stop {
            asked_stop = true;
            println!("{}", dim(if over { "  시간이 다 됐다 — 정지" } else { "  중단 요청 — 정지" }));
            handle.stop();
        }
        match handle.events.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => {
                if matches!(ev, RunnerEvent::Error { .. }) {
                    errors += 1;
                }
                if let Some(line) = format_event(&l.project, &ev, start) {
                    println!("{line}");
                }
                if matches!(ev, RunnerEvent::Stopped) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    println!();
    println!("{} {:.1}초 · 오류 {errors}건", bold("종료"), start.elapsed().as_secs_f64());
    Ok(if errors > 0 { 1 } else { 0 })
}

fn format_event(project: &nl_core::Project, ev: &RunnerEvent, start: Instant) -> Option<String> {
    let t = format!("{:>7.2}s", start.elapsed().as_secs_f64());
    Some(match ev {
        RunnerEvent::Started => format!("{} {}", dim(&t), green("시작")),
        RunnerEvent::Stopped => format!("{} {}", dim(&t), green("정지")),
        RunnerEvent::Log(m) => format!("{} {m}", dim(&t)),
        RunnerEvent::Error { node, message } => {
            format!("{} {} {}{}", dim(&t), red("오류"), node_label(project, *node), message)
        }
        RunnerEvent::Widget { widget, value } => {
            format!("{} {} {} = {}", dim(&t), dim("위젯"), widget.short(), brief(value))
        }
        // 값 이벤트는 초당 수십 개라 기본으로는 접는다. `RUST_LOG=debug` 면 로그로 나간다.
        RunnerEvent::Value { node, value } => {
            log::debug!("값 {} = {}", node.short(), brief(value));
            return None;
        }
        // 미리보기 축소판은 터미널에서 쓸 데가 없다.
        RunnerEvent::ValuePreview { node, width, height, .. } => {
            log::debug!("미리보기 {} {width}x{height}", node.short());
            return None;
        }
        RunnerEvent::Stats { tick, tick_ms, hz } => {
            log::debug!("틱 {tick} · {hz:.1}Hz · 틱당 {tick_ms:.1}ms");
            return None;
        }
    })
}

fn node_label(project: &nl_core::Project, node: Option<nl_core::PNodeId>) -> String {
    let Some(id) = node else { return String::new() };
    for p in project.pipelines.values() {
        if let Some(n) = p.nodes.get(&id) {
            let name = if n.name.is_empty() { n.kind.label().to_string() } else { n.name.clone() };
            return format!("[{name}] ");
        }
    }
    format!("[{}] ", id.short())
}

fn brief(v: &nl_engine::Value) -> String {
    match v {
        nl_engine::Value::Image { width, height, .. } => format!("이미지 {width}x{height}"),
        nl_engine::Value::Tensor(t) => format!("텐서 {:?}", t.shape),
        nl_engine::Value::Text(s) => format!("{:?}", s.lines().next().unwrap_or_default()),
        nl_engine::Value::Number(n) => format!("{n}"),
        nl_engine::Value::Numbers(n) => format!("{n:?}"),
        nl_engine::Value::Json(j) => j.to_string(),
    }
}
