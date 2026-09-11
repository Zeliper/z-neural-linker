//! `nl record` — 화면을 찍어 학습용 폴더를 만든다. 라벨은 표준 입력 한 줄(숫자)로 바꾼다.

use crate::common::*;
use anyhow::{Context, Result};
use nl_core::pipeline::Region;
use nl_io::record::Recorder;
use std::io::BufRead;
use std::path::Path;
use std::time::{Duration, Instant};

pub struct Args<'a> {
    pub out: &'a Path,
    pub monitor: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub seconds: Option<f64>,
    /// 받아들일 라벨. `None` 이면 아무 정수나.
    pub allowed: Option<Vec<i64>>,
}

/// `"0,1,2"` → `[0, 1, 2]`.
pub fn parse_label_keys(list: &str) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    for part in list.split(',') {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        out.push(t.parse::<i64>().map_err(|_| anyhow::anyhow!("--label-keys 의 '{t}' 은 정수가 아니다"))?);
    }
    if out.is_empty() {
        anyhow::bail!("--label-keys 가 비어 있다");
    }
    Ok(out)
}

pub fn run(args: Args<'_>) -> Result<i32> {
    install_signal_handler();
    let region = Region {
        monitor: args.monitor,
        x: args.x,
        y: args.y,
        width: args.width,
        height: args.height,
    };

    let handle = Recorder::start(args.out, region, args.fps).context("녹화를 시작하지 못했다")?;
    println!("{} {}", bold("녹화"), args.out.display());
    println!(
        "  {} 모니터 {} · {} · {:.1} fps",
        dim("영역"),
        args.monitor,
        if args.width == 0 { "전체".to_string() } else { format!("{}x{} @ ({}, {})", args.width, args.height, args.x, args.y) },
        args.fps
    );
    println!("  {}", dim("라벨을 바꾸려면 숫자를 입력하고 엔터. 끝내려면 빈 줄이나 Ctrl+C."));
    if let Some(keys) = &args.allowed {
        println!("  {} {}", dim("받는 라벨"), keys.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", "));
    }

    // 표준 입력은 블로킹이라 별도 스레드에서 읽는다.
    let (tx, rx) = crossbeam_channel::unbounded::<String>();
    std::thread::Builder::new()
        .name("nl-record-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        })
        .context("표준 입력 스레드를 만들지 못했다")?;

    let start = Instant::now();
    let deadline = args.seconds.map(|s| start + Duration::from_secs_f64(s));
    let mut last_report = 0usize;

    loop {
        if interrupted() {
            println!("{}", dim("  중단 요청"));
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            println!("{}", dim("  시간이 다 됐다"));
            break;
        }
        if handle.is_done() {
            break;
        }
        match rx.try_recv() {
            Ok(line) => {
                let t = line.trim();
                if t.is_empty() {
                    println!("{}", dim("  빈 줄 — 녹화를 끝낸다"));
                    break;
                }
                match t.parse::<i64>() {
                    Ok(v) if args.allowed.as_ref().is_none_or(|a| a.contains(&v)) => {
                        handle.set_label(v);
                        println!("  {} {v}", dim("라벨"));
                    }
                    Ok(v) => println!("  {} {v} 는 --label-keys 목록에 없다", yellow("무시")),
                    Err(_) => println!("  {} '{t}' 은 숫자가 아니다", yellow("무시")),
                }
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            // 표준 입력이 닫혔다(파이프). 시간이나 Ctrl+C 로만 끝난다.
            Err(crossbeam_channel::TryRecvError::Disconnected) => {}
        }
        let n = handle.frames_written();
        if n != last_report {
            last_report = n;
            print_progress(n, handle.frames_dropped(), handle.label(), start);
        }
        std::thread::sleep(Duration::from_millis(30));
    }

    let dropped = handle.frames_dropped();
    let last_error = handle.error();
    let total = handle.finish();
    println!();
    println!("{} {total}장", bold("저장"));
    if dropped > 0 {
        println!("  {} {dropped}장 (저장이 캡처를 못 따라갔다)", yellow("버림"));
    }
    if let Some(e) = last_error {
        println!("  {} {e}", yellow("마지막 오류"));
    }
    println!("  {} {}", dim("폴더"), args.out.display());
    if total == 0 {
        return Ok(1);
    }
    Ok(0)
}

fn print_progress(written: usize, dropped: usize, label: i64, start: Instant) {
    if !color_enabled() {
        return;
    }
    let fps = written as f64 / start.elapsed().as_secs_f64().max(0.001);
    print!("\r\x1b[2K  {written}장 · 버림 {dropped} · 라벨 {label} · {fps:.1} fps");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_key_lists_parse() {
        assert_eq!(parse_label_keys("0,1,2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_label_keys(" 3 , 7 ").unwrap(), vec![3, 7]);
        assert!(parse_label_keys("").is_err());
        assert!(parse_label_keys("a,1").is_err());
    }
}
