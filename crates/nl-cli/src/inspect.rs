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
    let mut rows = vec![vec![
        "모델".into(),
        "id".into(),
        "레이어".into(),
        "출력 형상".into(),
        "가중치".into(),
        "ONNX".into(),
    ]];
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
        // 내보내기 가능 여부. 가중치 파일을 읽지 않으므로 학습 전에도 답이 나온다.
        //
        // 막는 것이 둘이다. 형상 추론이 통과하지 못하면 그래프를 옮길 수 없고, 아직 매핑하지 못한
        // 레이어가 있어도 안 된다. `onnx::check` 는 뒤쪽만 보므로 앞쪽은 여기서 함께 본다 —
        // 그러지 않으면 형상이 깨진 모델에도 "가능" 이라고 적히고 내보낼 때 비로소 실패한다.
        let onnx = nl_engine::onnx::check(m);
        let onnx_cell = if !rep.is_ok() {
            yellow("형상 오류로 불가").to_string()
        } else if onnx.unsupported.is_empty() {
            green("가능").to_string()
        } else {
            // 어떤 레이어가 막는지까지 적는다 — "미지원" 만으로는 무엇을 바꿔야 할지 모른다.
            yellow(&format!("미지원: {}", onnx.unsupported.join(", ")))
        };
        rows.push(vec![
            m.name.clone(),
            m.id.short(),
            m.graph.nodes.len().to_string(),
            shape_cell,
            m.weights.clone().unwrap_or_else(|| dim("(없음)").to_string()),
            onnx_cell,
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

    // ── 다입출력 모델의 순서 계약 ──
    //
    // `Graph::input_nodes()`/`output_nodes()` 는 **노드 이름순**이고, 페이로드 필드는 그 순서와
    // 짝지어진다. 이름으로 맞추지 않으므로 순서가 곧 계약이다. 눈으로 확인할 수 있게 펼쳐 준다.
    for m in p.models.values() {
        let ins = m.graph.input_nodes();
        let outs = m.graph.output_nodes();
        if ins.len() < 2 && outs.len() < 2 {
            continue;
        }
        let spec = m.payload.and_then(|pid| p.payloads.get(&pid));
        println!("{} {}", bold("입출력 순서"), m.name);
        print_port_table(m, &ins, spec.map(|s| s.inputs.as_slice()), "입력");
        print_port_table(m, &outs, spec.map(|s| s.outputs.as_slice()), "출력");
        if outs.len() > 1 {
            let first = m
                .graph
                .nodes
                .get(&outs[0])
                .map(|n| n.display_name())
                .unwrap_or_default();
            println!(
                "  {}",
                dim(&format!(
                    "학습은 첫 출력 '{first}' 만 손실·지표에 씁니다. 나머지는 추론에서만 쓰입니다."
                ))
            );
        }
        println!();
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
    let mut rows = vec![vec![
        "파이프라인".into(),
        "id".into(),
        "노드".into(),
        "연결".into(),
        "tick_hz".into(),
    ]];
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

/// 필드 종류를 한 칸에 들어갈 짧은 말로. 길이·형상이 순서 확인에 제일 쓸모 있다.
fn field_kind_label(kind: &nl_core::payload::FieldKind) -> String {
    use nl_core::payload::FieldKind as K;
    match kind {
        K::Tensor { shape, .. } => format!("텐서 {shape:?}"),
        K::Image {
            width,
            height,
            channels,
        } => format!("이미지 {width}x{height}x{channels}"),
        K::Scalar => "스칼라".into(),
        K::Vector { len } => format!("벡터[{len}]"),
        K::ClassLabel { labels } => format!("클래스 {}종", labels.len()),
        K::Text => "텍스트".into(),
        K::Json => "JSON".into(),
    }
}

/// Input·Output 노드와 페이로드 필드를 순서대로 짝지어 보여 준다.
///
/// 개수가 어긋나면 그 줄을 붉게 표시한다 — 실행할 때 오류가 되는 것을 미리 보여 주는 것이다.
fn print_port_table(
    m: &nl_core::ModelDef,
    nodes: &[nl_core::ids::NodeId],
    fields: Option<&[nl_core::payload::Field]>,
    what: &str,
) {
    if nodes.is_empty() {
        return;
    }
    let mut rows = vec![vec![
        "#".into(),
        format!("{what} 노드"),
        "페이로드 필드".into(),
        "종류".into(),
    ]];
    let n = nodes.len().max(fields.map_or(0, <[_]>::len));
    for i in 0..n {
        let node = nodes
            .get(i)
            .and_then(|id| m.graph.nodes.get(id))
            .map(|x| {
                if x.name.is_empty() {
                    // 이름이 비면 정렬이 id 순으로 떨어져 순서가 뒤바뀔 수 있다.
                    yellow("(이름 없음)")
                } else {
                    x.name.clone()
                }
            })
            .unwrap_or_else(|| red("(없음)").to_string());
        let field = fields.and_then(|f| f.get(i));
        rows.push(vec![
            (i + 1).to_string(),
            node,
            field
                .map(|f| f.name.clone())
                .unwrap_or_else(|| red("(없음)").to_string()),
            field
                .map(|f| field_kind_label(&f.kind))
                .unwrap_or_else(|| dim("—").to_string()),
        ]);
    }
    table(&rows);
    if let Some(f) = fields {
        if f.len() != nodes.len() {
            println!(
                "  {}",
                red(&format!(
                    "{what} 노드는 {}개인데 페이로드 필드는 {}개입니다 — 실행하면 오류가 납니다",
                    nodes.len(),
                    f.len()
                ))
            );
        }
    } else {
        println!("  {}", yellow("페이로드가 없어 필드 이름을 확인할 수 없습니다"));
    }
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
            let nn = p
                .models
                .get(m)
                .and_then(|x| x.graph.nodes.get(n))
                .map(|x| x.display_name())
                .unwrap_or_default();
            format!("{mn}/{nn}")
        }
        Where::Pipeline(id) => p
            .pipelines
            .get(id)
            .map(|x| x.name.clone())
            .unwrap_or_else(|| id.short()),
        Where::PNode(pid, nid) => {
            let pn = p
                .pipelines
                .get(pid)
                .map(|x| x.name.clone())
                .unwrap_or_else(|| pid.short());
            let nn = p
                .pipelines
                .get(pid)
                .and_then(|x| x.nodes.get(nid))
                .map(|n| {
                    if n.name.is_empty() {
                        n.kind.label().to_string()
                    } else {
                        n.name.clone()
                    }
                })
                .unwrap_or_default();
            format!("{pn}/{nn}")
        }
    }
}
