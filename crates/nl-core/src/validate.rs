//! 프로젝트 수준 검증. 형상 오류 + 참조 무결성 + 학습 가능 조건을 한 목록으로 (하단 도크 "문제" 탭, 빌드 전 점검).

use crate::ids::{ModelId, NodeId, PNodeId, PipelineId};
use crate::model::{LayerKind, Project};
use crate::pipeline::{PNodeKind, Sink, Source};
use crate::shape::infer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Where {
    Project,
    Model(ModelId),
    Node(ModelId, NodeId),
    Pipeline(PipelineId),
    PNode(PipelineId, PNodeId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    pub at: Where,
    pub message: String,
}

fn err(at: Where, m: impl Into<String>) -> Issue {
    Issue { severity: Severity::Error, at, message: m.into() }
}
fn warn(at: Where, m: impl Into<String>) -> Issue {
    Issue { severity: Severity::Warning, at, message: m.into() }
}

pub fn validate(p: &Project) -> Vec<Issue> {
    let mut v = Vec::new();
    for (mid, m) in &p.models {
        let at = Where::Model(*mid);
        if m.graph.nodes.is_empty() {
            v.push(warn(at.clone(), format!("모델 '{}' 이 비어 있음", m.name)));
            continue;
        }
        if m.graph.input_nodes().is_empty() {
            v.push(err(at.clone(), format!("모델 '{}' 에 Input 노드가 없음", m.name)));
        }
        if m.graph.output_nodes().is_empty() {
            v.push(err(at.clone(), format!("모델 '{}' 에 Output 노드가 없음", m.name)));
        }
        let rep = infer(&m.graph);
        for (nid, e) in &rep.errors {
            let name = m.graph.nodes.get(nid).map(|n| n.display_name()).unwrap_or_default();
            v.push(err(Where::Node(*mid, *nid), format!("{name}: {e}")));
        }
        // 출력에 닿지 않는 노드 (경고)
        let outs = m.graph.output_nodes();
        let mut reach = std::collections::BTreeSet::new();
        let mut stack = outs.clone();
        while let Some(n) = stack.pop() {
            if !reach.insert(n) {
                continue;
            }
            for (_, from) in m.graph.inputs_of(n) {
                stack.push(from);
            }
        }
        for n in m.graph.nodes.values() {
            if !reach.contains(&n.id) && !matches!(n.kind, LayerKind::Output) && !rep.errors.contains_key(&n.id) {
                v.push(warn(Where::Node(*mid, n.id), format!("{}: 출력에 연결되지 않음", n.display_name())));
            }
        }
        if let Some(d) = m.train.dataset {
            if !p.datasets.contains_key(&d) {
                v.push(err(at.clone(), format!("모델 '{}' 의 학습 데이터셋이 없음", m.name)));
            }
        }
        if let Some(pl) = m.payload {
            if !p.payloads.contains_key(&pl) {
                v.push(err(at.clone(), format!("모델 '{}' 의 페이로드가 없음", m.name)));
            }
        }
    }
    for (pid, pl) in &p.pipelines {
        for n in pl.nodes.values() {
            if let PNodeKind::Model { model, payload } = &n.kind {
                if !p.models.contains_key(model) {
                    v.push(err(Where::PNode(*pid, n.id), "참조하는 모델이 없음"));
                } else if p.models[model].weights.is_none() {
                    v.push(warn(Where::PNode(*pid, n.id), format!("모델 '{}' 에 학습된 가중치가 없음", p.models[model].name)));
                }
                if let Some(pay) = payload {
                    if !p.payloads.contains_key(pay) {
                        v.push(err(Where::PNode(*pid, n.id), "참조하는 페이로드가 없음"));
                    }
                }
            }
            // HTTP 응답 싱크는 같은 파이프라인의 HTTP 서버 소스를 가리켜야 한다.
            if let PNodeKind::Sink { sink: Sink::HttpReply { server } } = &n.kind {
                match pl.nodes.get(server) {
                    None => v.push(err(Where::PNode(*pid, n.id), "HTTP 응답: 가리키는 서버 노드가 이 파이프라인에 없음")),
                    Some(target) => {
                        if !matches!(target.kind, PNodeKind::Source { source: Source::HttpServer { .. } }) {
                            v.push(err(
                                Where::PNode(*pid, n.id),
                                format!("HTTP 응답: 가리키는 노드가 HTTP 서버가 아님 ({})", target.kind.label()),
                            ));
                        }
                    }
                }
            }
            // 응답할 싱크가 없는 HTTP 서버는 모든 요청이 시간 초과로 끝난다.
            if let PNodeKind::Source { source: Source::HttpServer { .. } } = &n.kind {
                let replied = pl.nodes.values().any(|o| {
                    matches!(&o.kind, PNodeKind::Sink { sink: Sink::HttpReply { server } } if *server == n.id)
                });
                if !replied {
                    v.push(warn(
                        Where::PNode(*pid, n.id),
                        "HTTP 서버: 짝이 되는 HTTP 응답 싱크가 없어 모든 요청이 시간 초과로 끝남",
                    ));
                }
            }

            let ups = pl.upstream(n.id).len();
            let downs = pl.downstream(n.id).len();
            if !n.kind.is_source() && ups == 0 {
                v.push(warn(Where::PNode(*pid, n.id), format!("{}: 입력이 연결되지 않음", n.kind.label())));
            }
            if !n.kind.is_sink() && downs == 0 {
                v.push(warn(Where::PNode(*pid, n.id), format!("{}: 출력이 연결되지 않음", n.kind.label())));
            }
        }
    }
    // 심각한 것부터. `sort_by_key` 는 안정 정렬이라 같은 심각도 안의 순서는 유지된다.
    v.sort_by_key(|a| std::cmp::Reverse(a.severity));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Port};

    #[test]
    fn http_reply_must_point_at_an_http_server_in_the_same_pipeline() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
        let mut p = Project::new("p");
        let mut pl = Pipeline::new("api");
        let server = pl.add_node(PNode::new(
            PNodeKind::Source { source: Source::HttpServer { bind: "127.0.0.1:0".into(), path: "/x".into() } },
            [0.0, 0.0],
        ));
        let log = pl.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        let reply = pl.add_node(PNode::new(PNodeKind::Sink { sink: Sink::HttpReply { server } }, [2.0, 0.0]));
        pl.add_link(server, reply).unwrap();
        let pid = pl.id;
        p.pipelines.insert(pid, pl);

        // 올바른 짝이면 HTTP 관련 오류가 없다.
        let issues = validate(&p);
        assert!(
            !issues.iter().any(|i| i.severity == Severity::Error && i.message.contains("HTTP 응답")),
            "{issues:?}"
        );

        // 서버가 아닌 노드를 가리키면 오류.
        p.pipelines.get_mut(&pid).unwrap().nodes.get_mut(&reply).unwrap().kind =
            PNodeKind::Sink { sink: Sink::HttpReply { server: log } };
        let issues = validate(&p);
        assert!(
            issues.iter().any(|i| i.severity == Severity::Error && i.message.contains("HTTP 서버가 아님")),
            "{issues:?}"
        );

        // 없는 노드를 가리키면 오류.
        p.pipelines.get_mut(&pid).unwrap().nodes.get_mut(&reply).unwrap().kind =
            PNodeKind::Sink { sink: Sink::HttpReply { server: PNodeId::from_u128(999) } };
        let issues = validate(&p);
        assert!(
            issues.iter().any(|i| i.severity == Severity::Error && i.message.contains("이 파이프라인에 없음")),
            "{issues:?}"
        );
    }

    #[test]
    fn http_server_without_a_reply_sink_is_a_warning() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
        let mut p = Project::new("p");
        let mut pl = Pipeline::new("api");
        let server = pl.add_node(PNode::new(
            PNodeKind::Source { source: Source::HttpServer { bind: "127.0.0.1:0".into(), path: "/x".into() } },
            [0.0, 0.0],
        ));
        let log = pl.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        pl.add_link(server, log).unwrap();
        p.pipelines.insert(pl.id, pl);
        let issues = validate(&p);
        assert!(
            issues.iter().any(|i| i.severity == Severity::Warning && i.message.contains("시간 초과")),
            "{issues:?}"
        );
    }

    #[test]
    fn reports_missing_io_and_dangling() {
        let mut p = Project::new("p");
        let m = p.add_model("m");
        let g = &mut p.models.get_mut(&m).unwrap().graph;
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = g.add_node(Node::new(LayerKind::Linear { out_features: 2, bias: true }, [0.0, 0.0]));
        g.add_edge(a, Port::new(b, 0)).unwrap();
        let issues = validate(&p);
        assert!(issues.iter().any(|i| i.severity == Severity::Error && i.message.contains("Output")));
        assert!(issues.iter().any(|i| i.severity == Severity::Warning && i.message.contains("연결되지 않음")));
    }
}
