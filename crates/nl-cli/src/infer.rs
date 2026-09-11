//! `nl infer` — 모델 하나에 값을 넣어 결과를 JSON 으로 찍는다.

use crate::common::*;
use anyhow::{bail, Context, Result};
use nl_core::payload::PayloadSpec;
use nl_engine::{HostTensor, Session, Value};
use std::path::Path;

pub struct Args<'a> {
    pub project: &'a Path,
    pub model: &'a str,
    pub input: Option<&'a str>,
    pub image: Option<&'a Path>,
    pub csv: Option<&'a Path>,
    pub payload: Option<&'a str>,
    pub device: Option<&'a str>,
}

pub fn run(args: Args<'_>) -> Result<i32> {
    let l = load_project(args.project)?;
    let model = find_model(&l.project, args.model)?.clone();

    let payload: Option<PayloadSpec> = match args.payload {
        Some(key) => Some(
            l.project
                .payloads
                .values()
                .find(|s| s.name == key || s.id.0.simple().to_string().starts_with(&key.to_lowercase()))
                .with_context(|| format!("페이로드 '{key}' 을 찾을 수 없다"))?
                .clone(),
        ),
        None => model.payload.and_then(|id| l.project.payloads.get(&id).cloned()),
    };

    let device = match args.device {
        Some(d) => parse_device(d)?,
        None => model.train.device,
    };
    let weights = model.weights.as_ref().map(|w| l.base_dir.join(w));
    if let Some(w) = &weights {
        if !w.is_file() {
            bail!("가중치 파일이 없다: {} (먼저 nl train 을 돌려라)", w.display());
        }
    } else {
        eprintln!("{}", yellow("가중치가 없어 무작위 초기화로 추론한다 (구조 확인용)"));
    }

    let mut session = Session::load(&model, weights.as_deref(), device).context("모델을 올리지 못했다")?;
    eprintln!("{} {} · 장치 {}", dim("모델"), model.name, session.device_name());

    let inputs = collect_inputs(&args)?;
    let mut results = Vec::with_capacity(inputs.len());
    for v in &inputs {
        results.push(infer_one(&mut session, payload.as_ref(), v)?);
    }

    let out = if results.len() == 1 { results.remove(0) } else { serde_json::Value::Array(results) };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(0)
}

/// 입력 소스는 셋 중 하나. CSV 는 행마다 하나씩이라 결과도 배열이 된다.
fn collect_inputs(args: &Args<'_>) -> Result<Vec<Value>> {
    match (args.input, args.image, args.csv) {
        (Some(text), None, None) => Ok(vec![parse_input_text(text)?]),
        (None, Some(path), None) => {
            let img = image::open(path).with_context(|| format!("이미지를 열지 못했다: {}", path.display()))?;
            let rgba = img.to_rgba8();
            Ok(vec![Value::Image { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }])
        }
        (None, None, Some(path)) => {
            let text =
                std::fs::read_to_string(path).with_context(|| format!("CSV 를 읽지 못했다: {}", path.display()))?;
            let mut out = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let row: Result<Vec<f32>> = line
                    .split(',')
                    .map(|c| {
                        c.trim().parse::<f32>().map_err(|_| {
                            anyhow::anyhow!("{} {}행: '{}' 을 수로 읽을 수 없다", path.display(), i + 1, c.trim())
                        })
                    })
                    .collect();
                out.push(Value::Numbers(row?));
            }
            if out.is_empty() {
                bail!("CSV 에 읽을 행이 없다: {}", path.display());
            }
            Ok(out)
        }
        (None, None, None) => bail!("입력이 없다. --input, --image, --csv 중 하나를 주어라"),
        _ => bail!("--input, --image, --csv 는 하나만 쓸 수 있다"),
    }
}

/// `'[0,1]'` 이나 `'{"x":1}'` 이나 그냥 `'3'`. JSON 으로 읽히면 그 구조를 살린다.
fn parse_input_text(text: &str) -> Result<Value> {
    let json: serde_json::Value = match serde_json::from_str(text.trim()) {
        Ok(j) => j,
        Err(_) => return Ok(Value::Text(text.to_owned())),
    };
    Ok(match &json {
        serde_json::Value::Array(a) if a.iter().all(|v| v.is_number()) => {
            Value::Numbers(a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect())
        }
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        _ => Value::Json(json),
    })
}

fn infer_one(session: &mut Session, payload: Option<&PayloadSpec>, v: &Value) -> Result<serde_json::Value> {
    let tensor = match payload.and_then(|p| p.inputs.first()) {
        Some(field) => nl_engine::encode(field, v).context("입력 인코딩 실패")?,
        None => match v {
            Value::Tensor(t) => t.clone(),
            Value::Numbers(n) => HostTensor::new(vec![1, n.len()], n.clone()),
            Value::Number(x) => HostTensor::new(vec![1, 1], vec![*x as f32]),
            other => bail!(
                "페이로드가 없으면 숫자·벡터·텐서만 넣을 수 있다 (받은 값: {})",
                kind_name(other)
            ),
        },
    };
    let outs = session.run(&[tensor]).context("추론 실패")?;
    let first = outs.into_iter().next().context("모델이 출력을 내지 않았다")?;
    match payload.and_then(|p| p.outputs.first()) {
        Some(field) => {
            let decoded = nl_engine::decode(field, &first).context("출력 디코딩 실패")?;
            Ok(value_to_json(&decoded))
        }
        None => Ok(value_to_json(&Value::Tensor(first))),
    }
}

fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Number(_) => "숫자",
        Value::Numbers(_) => "숫자 벡터",
        Value::Text(_) => "텍스트",
        Value::Json(_) => "JSON",
        Value::Image { .. } => "이미지",
        Value::Tensor(_) => "텐서",
    }
}

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Number(n) => json!(n),
        Value::Numbers(n) => json!(n),
        Value::Text(s) => json!(s),
        Value::Json(j) => j.clone(),
        Value::Image { width, height, rgba } => json!({"image": {"width": width, "height": height, "bytes": rgba.len()}}),
        Value::Tensor(t) => json!({"shape": t.shape, "data": t.data}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_text_becomes_the_right_value() {
        assert_eq!(parse_input_text("[0,1]").unwrap(), Value::Numbers(vec![0.0, 1.0]));
        assert_eq!(parse_input_text(" 3.5 ").unwrap(), Value::Number(3.5));
        assert_eq!(parse_input_text("\"hi\"").unwrap(), Value::Text("hi".into()));
        assert_eq!(parse_input_text("그냥 글").unwrap(), Value::Text("그냥 글".into()));
        assert!(matches!(parse_input_text("{\"a\":1}").unwrap(), Value::Json(_)));
    }

    #[test]
    fn only_one_input_source_is_allowed() {
        let args = Args {
            project: Path::new("x"),
            model: "m",
            input: Some("[1]"),
            image: Some(Path::new("a.png")),
            csv: None,
            payload: None,
            device: None,
        };
        assert!(collect_inputs(&args).is_err());

        let none = Args { input: None, image: None, csv: None, ..args };
        assert!(collect_inputs(&none).is_err());
    }

    #[test]
    fn csv_rows_become_number_vectors() {
        let dir = std::env::temp_dir().join(format!("nl-cli-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.csv");
        std::fs::write(&p, "0,1\n1, 0\n\n").unwrap();
        let args = Args {
            project: Path::new("x"),
            model: "m",
            input: None,
            image: None,
            csv: Some(&p),
            payload: None,
            device: None,
        };
        let v = collect_inputs(&args).unwrap();
        assert_eq!(v, vec![Value::Numbers(vec![0.0, 1.0]), Value::Numbers(vec![1.0, 0.0])]);

        std::fs::write(&p, "0,x\n").unwrap();
        assert!(collect_inputs(&args).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
