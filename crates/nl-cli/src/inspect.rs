//! `nl inspect` — 프로젝트 안에 무엇이 있고 어디가 문제인지 한눈에.

use crate::common::*;
use anyhow::Result;
use nl_core::validate::Where;
use nl_core::{shape, validate, Severity};
use std::path::Path;

pub fn run(path: &Path) -> Result<i32> {
    let l = load_project(path)?;
    let p = &l.project;

    println!("{} {}", bold("프로젝트"), p.name);
    if !p.description.is_empty() {
        println!("  {}", dim(&p.description));
    }
    println!("  {} {}", dim("파일"), l.path.display());
    println!("  {} {}", dim("기본 장치"), p.settings.default_device.label());
    println!();

    // ── 모델 ──
    let mut rows = vec![vec!["모델".into(), "id".into(), "레이어".into(), "출력 형상".into(), "가중치".into()]];
    for m in p.models.values() {
        let rep = shape::infer(&m.graph);
        let out_shape = m
            .graph
            .output_nodes()
            .first()
            .and_then(|n| rep.shape(*n))
            .map(|s| format!("{:?}", s.sample()))
            .unwrap_or_else(|| red("?").to_string());
        let shape_cell = if rep.is_ok() {
            out_shape
        } else {
            red(&format!("{} (형상 오류 {}건)", out_shape, rep.errors.len()))
        };
        rows.push(vec![
            m.name.clone(),
            m.id.short(),
            m.graph.nodes.len().to_string(),
            shape_cell,
            m.weights.clone().unwrap_or_else(|| dim("(없음)").to_string()),
        ]);
    }
    if rows.len() == 1 {
        println!("{}", dim("모델 없음"));
    } else {
        table(&rows);
    }
    println!();

    // 형상 오류는 모델별로 자세히.
    for m in p.models.values() {
        let rep = shape::infer(&m.graph);
        for (nid, e) in &rep.errors {
            let name = m.graph.nodes.get(nid).map(|n| n.display_name()).unwrap_or_default();
            println!("{} {} / {}: {e}", red("형상 오류"), m.name, name);
        }
    }

    // ── 데이터셋 ──
    let mut rows = vec![vec!["데이터셋".into(), "id".into(), "소스".into()]];
    for d in p.datasets.values() {
        rows.push(vec![d.name.clone(), d.id.short(), describe_source(&d.source)]);
    }
    if rows.len() > 1 {
        table(&rows);
        println!();
    }

    // ── 페이로드 ──
    let mut rows = vec![vec!["페이로드".into(), "id".into(), "입력".into(), "출력".into()]];
    for s in p.payloads.values() {
        rows.push(vec![
            s.name.clone(),
            s.id.short(),
            s.inputs.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", "),
            s.outputs.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", "),
        ]);
    }
    if rows.len() > 1 {
        table(&rows);
        println!();
    }

    // ── 파이프라인 ──
    let mut rows = vec![vec!["파이프라인".into(), "id".into(), "노드".into(), "연결".into(), "tick_hz".into()]];
    for pl in p.pipelines.values() {
        rows.push(vec![
            pl.name.clone(),
            pl.id.short(),
            pl.nodes.len().to_string(),
            pl.links.len().to_string(),
            format!("{:.0}", pl.tick_hz),
        ]);
    }
    if rows.len() > 1 {
        table(&rows);
        println!();
    }

    // ── 검증 ──
    let issues = validate(p);
    let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
    let warns = issues.len() - errors;
    if issues.is_empty() {
        println!("{} 문제 없음", green("검증"));
        return Ok(0);
    }
    println!("{} 오류 {errors}건, 경고 {warns}건", bold("검증"));
    for i in &issues {
        let tag = match i.severity {
            Severity::Error => red("오류"),
            Severity::Warning => yellow("경고"),
        };
        println!("  {tag} {} — {}", where_label(p, &i.at), i.message);
    }
    // 오류가 있으면 스크립트가 알아챌 수 있게 종료 코드 1.
    Ok(if errors > 0 { 1 } else { 0 })
}

fn describe_source(s: &nl_core::DataSource) -> String {
    match s {
        nl_core::DataSource::Csv { path, .. } => format!("CSV {path}"),
        nl_core::DataSource::ImageFolder { path } => format!("이미지 폴더 {path}"),
        nl_core::DataSource::Recorded { path } => format!("녹화 {path}"),
        nl_core::DataSource::Synthetic { kind, samples } => format!("합성 {} × {samples}", kind.label()),
    }
}

fn where_label(p: &nl_core::Project, w: &Where) -> String {
    match w {
        Where::Project => "프로젝트".into(),
        Where::Model(m) => p.models.get(m).map(|x| x.name.clone()).unwrap_or_else(|| m.short()),
        Where::Node(m, n) => {
            let mn = p.models.get(m).map(|x| x.name.clone()).unwrap_or_else(|| m.short());
            let nn = p.models.get(m).and_then(|x| x.graph.nodes.get(n)).map(|x| x.display_name()).unwrap_or_default();
            format!("{mn}/{nn}")
        }
        Where::Pipeline(id) => p.pipelines.get(id).map(|x| x.name.clone()).unwrap_or_else(|| id.short()),
        Where::PNode(pid, nid) => {
            let pn = p.pipelines.get(pid).map(|x| x.name.clone()).unwrap_or_else(|| pid.short());
            let nn = p
                .pipelines
                .get(pid)
                .and_then(|x| x.nodes.get(nid))
                .map(|n| if n.name.is_empty() { n.kind.label().to_string() } else { n.name.clone() })
                .unwrap_or_default();
            format!("{pn}/{nn}")
        }
    }
}
