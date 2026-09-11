//! `nl devices` — 쓸 수 있는 장치 목록과, 원하면 실제 동작 확인.

use crate::common::*;
use anyhow::Result;

pub fn run(probe: bool) -> Result<i32> {
    let list = nl_engine::enumerate();
    let mut rows = vec![vec![
        "선택값".into(),
        "이름".into(),
        "백엔드".into(),
        "종류".into(),
        "VRAM/코어".into(),
    ]];
    if probe {
        rows[0].push("확인".into());
    }
    for d in &list {
        let amount = match (d.vram_bytes, d.cores) {
            (Some(v), _) => human_size(v),
            (None, Some(c)) => format!("{c} 코어"),
            _ => "-".into(),
        };
        let mut row = vec![
            device_key(d.pref),
            d.name.clone(),
            d.backend.clone(),
            format!("{:?}", d.kind),
            amount,
        ];
        if probe {
            row.push(match nl_engine::probe(d.pref) {
                Ok(t) => green(&format!("정상 {:.0}ms", t.as_secs_f64() * 1000.0)),
                Err(e) => red(&format!("실패: {}", first_line(&e))),
            });
        }
        rows.push(row);
    }
    table(&rows);

    let resolved = nl_engine::resolve(nl_core::DevicePref::Auto);
    println!();
    println!("{} {} → {}", dim("auto"), dim("는"), resolved.info.name);
    Ok(0)
}

fn device_key(p: nl_core::DevicePref) -> String {
    match p {
        nl_core::DevicePref::Cpu => "cpu".into(),
        nl_core::DevicePref::Auto => "auto".into(),
        nl_core::DevicePref::Gpu { index } => format!("gpu:{index}"),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}
