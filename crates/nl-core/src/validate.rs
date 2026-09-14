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
    Issue {
        severity: Severity::Error,
        at,
        message: m.into(),
    }
}
fn warn(at: Where, m: impl Into<String>) -> Issue {
    Issue {
        severity: Severity::Warning,
        at,
        message: m.into(),
    }
}

/// `from` 에서 링크를 따라 닿을 수 있는 모든 노드 (자기 자신 제외).
fn reachable_from(pl: &crate::pipeline::Pipeline, from: PNodeId) -> std::collections::BTreeSet<PNodeId> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![from];
    while let Some(id) = stack.pop() {
        for next in pl.downstream(id) {
            if seen.insert(next) {
                stack.push(next);
            }
        }
    }
    seen
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
                v.push(warn(
                    Where::Node(*mid, n.id),
                    format!("{}: 출력에 연결되지 않음", n.display_name()),
                ));
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
        // ── HTTP 서버가 마우스·키보드를 구동할 수 있는가 ──
        // 원격 요청 하나로 남의 컴퓨터를 조작하게 되는 조합이라, 토큰이 없으면 오류로 막는다.
        for (server_id, token) in pl.nodes.values().filter_map(|n| match &n.kind {
            PNodeKind::Source {
                source: Source::HttpServer { token, .. },
            } => Some((n.id, token.clone())),
            _ => None,
        }) {
            let reached = reachable_from(pl, server_id);
            let driven: Vec<PNodeId> = reached
                .iter()
                .copied()
                .filter(|id| {
                    matches!(
                        pl.nodes.get(id).map(|x| &x.kind),
                        Some(PNodeKind::Sink {
                            sink: Sink::MouseKeyboard { .. }
                        })
                    )
                })
                .collect();
            if driven.is_empty() {
                continue;
            }
            let has_token = token.is_some_and(|t| !t.trim().is_empty());
            let what = "HTTP 요청이 마우스·키보드를 움직인다";
            for sink in driven {
                if has_token {
                    v.push(warn(
                        Where::PNode(*pid, sink),
                        format!("{what} (토큰이 있어 아무나 부르지는 못한다)"),
                    ));
                } else {
                    v.push(err(
                        Where::PNode(*pid, sink),
                        format!("{what} — HTTP 서버에 토큰이 없어 이 주소에 닿는 누구나 조작할 수 있다"),
                    ));
                }
            }
        }

        for n in pl.nodes.values() {
            if let PNodeKind::Model { model, payload } = &n.kind {
                if !p.models.contains_key(model) {
                    v.push(err(Where::PNode(*pid, n.id), "참조하는 모델이 없음"));
                } else if p.models[model].weights.is_none() {
                    v.push(warn(
                        Where::PNode(*pid, n.id),
                        format!("모델 '{}' 에 학습된 가중치가 없음", p.models[model].name),
                    ));
                }
                if let Some(pay) = payload {
                    if !p.payloads.contains_key(pay) {
                        v.push(err(Where::PNode(*pid, n.id), "참조하는 페이로드가 없음"));
                    }
                }
            }
            // HTTP 응답 싱크는 같은 파이프라인의 HTTP 서버 소스를 가리켜야 한다.
            if let PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            } = &n.kind
            {
                match pl.nodes.get(server) {
                    None => v.push(err(
                        Where::PNode(*pid, n.id),
                        "HTTP 응답: 가리키는 서버 노드가 이 파이프라인에 없음",
                    )),
                    Some(target) => {
                        if !matches!(
                            target.kind,
                            PNodeKind::Source {
                                source: Source::HttpServer { .. }
                            }
                        ) {
                            v.push(err(
                                Where::PNode(*pid, n.id),
                                format!("HTTP 응답: 가리키는 노드가 HTTP 서버가 아님 ({})", target.kind.label()),
                            ));
                        }
                    }
                }
            }
            // 인증 없는 HTTP 서버는 루프백에서만 열 수 있다. 그 밖의 주소는 실행기가 거부한다.
            if let PNodeKind::Source {
                source: Source::HttpServer { bind, token, .. },
            } = &n.kind
            {
                if token.as_ref().is_none_or(|t| t.trim().is_empty()) && !crate::pipeline::is_loopback_bind(bind) {
                    v.push(err(
                        Where::PNode(*pid, n.id),
                        format!(
                            "HTTP 서버: {bind} 는 바깥에서 닿는 주소인데 토큰이 없다                              (누구나 이 파이프라인을 구동할 수 있다 — 토큰을 넣거나 127.0.0.1 에 묶어라)"
                        ),
                    ));
                }
            }

            // 바깥 주소에 평문으로 여는 것은 막지는 않되 짚어 준다. 토큰이 있어도 평문이면
            // 그 토큰과 요청 본문이 전선 위에 그대로 흐른다.
            if let PNodeKind::Source {
                source: Source::HttpServer { bind, tls, .. },
            } = &n.kind
            {
                if tls.is_none() && !crate::pipeline::is_loopback_bind(bind) {
                    v.push(warn(
                        Where::PNode(*pid, n.id),
                        format!(
                            "HTTP 서버: {bind} 는 바깥에서 닿는 주소인데 TLS 가 없다 \
                             (토큰과 본문이 평문으로 오간다 — 인증서를 넣거나 앞에 리버스 프록시를 두어라)"
                        ),
                    ));
                }
            }

            // 응답할 싱크가 없는 HTTP 서버는 모든 요청이 시간 초과로 끝난다.
            if let PNodeKind::Source {
                source: Source::HttpServer { .. },
            } = &n.kind
            {
                let replied = pl.nodes.values().any(
                    |o| matches!(&o.kind, PNodeKind::Sink { sink: Sink::HttpReply { server } } if *server == n.id),
                );
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
                v.push(warn(
                    Where::PNode(*pid, n.id),
                    format!("{}: 입력이 연결되지 않음", n.kind.label()),
                ));
            }
            if !n.kind.is_sink() && downs == 0 {
                v.push(warn(
                    Where::PNode(*pid, n.id),
                    format!("{}: 출력이 연결되지 않음", n.kind.label()),
                ));
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
            PNodeKind::Source {
                source: Source::HttpServer {
                    bind: "127.0.0.1:0".into(),
                    path: "/x".into(),
                    token: None,
                    tls: None,
                },
            },
            [0.0, 0.0],
        ));
        let log = pl.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        let reply = pl.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [2.0, 0.0],
        ));
        pl.add_link(server, reply).unwrap();
        let pid = pl.id;
        p.pipelines.insert(pid, pl);

        // 올바른 짝이면 HTTP 관련 오류가 없다.
        let issues = validate(&p);
        assert!(
            !issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("HTTP 응답")),
            "{issues:?}"
        );

        // 서버가 아닌 노드를 가리키면 오류.
        p.pipelines.get_mut(&pid).unwrap().nodes.get_mut(&reply).unwrap().kind = PNodeKind::Sink {
            sink: Sink::HttpReply { server: log },
        };
        let issues = validate(&p);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("HTTP 서버가 아님")),
            "{issues:?}"
        );

        // 없는 노드를 가리키면 오류.
        p.pipelines.get_mut(&pid).unwrap().nodes.get_mut(&reply).unwrap().kind = PNodeKind::Sink {
            sink: Sink::HttpReply {
                server: PNodeId::from_u128(999),
            },
        };
        let issues = validate(&p);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("이 파이프라인에 없음")),
            "{issues:?}"
        );
    }

    /// HTTP 서버 → (로직) → 마우스·키보드 는 원격 조작이 된다.
    /// 토큰이 있으면 경고, 없으면 오류다.
    #[test]
    fn an_http_server_that_can_drive_input_needs_a_token() {
        use crate::pipeline::{InputAction, Logic, MouseButton, PNode, PNodeKind, Pipeline, Sink, Source};

        let build = |token: Option<&str>| {
            let mut p = Project::new("p");
            let mut pl = Pipeline::new("원격");
            let server = pl.add_node(PNode::new(
                PNodeKind::Source {
                    source: Source::HttpServer {
                        bind: "127.0.0.1:0".into(),
                        path: "/x".into(),
                        token: token.map(str::to_string),
                        tls: None,
                    },
                },
                [0.0, 0.0],
            ));
            // 중간에 로직을 하나 끼워 "직접이 아니라 거쳐서도" 잡히는지 본다.
            let logic = pl.add_node(PNode::new(
                PNodeKind::Logic {
                    logic: Logic::Threshold { value: 0.5 },
                },
                [1.0, 0.0],
            ));
            let click = pl.add_node(PNode::new(
                PNodeKind::Sink {
                    sink: Sink::MouseKeyboard {
                        actions: vec![InputAction::Click {
                            button: MouseButton::Left,
                        }],
                        cooldown_ms: 0,
                    },
                },
                [2.0, 0.0],
            ));
            pl.add_link(server, logic).unwrap();
            pl.add_link(logic, click).unwrap();
            p.pipelines.insert(pl.id, pl);
            p
        };

        // 토큰 없음 → 오류.
        let issues = validate(&build(None));
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("마우스·키보드를 움직인다")),
            "{issues:?}"
        );

        // 토큰 있음 → 경고로 낮아진다.
        let issues = validate(&build(Some("s3cret")));
        assert!(
            !issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("마우스·키보드를 움직인다")),
            "토큰이 있는데 오류가 남았다: {issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Warning && i.message.contains("마우스·키보드를 움직인다")),
            "경고가 없다: {issues:?}"
        );

        // 빈 토큰은 없는 것과 같다.
        let issues = validate(&build(Some("   ")));
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("마우스·키보드를 움직인다")),
            "빈 토큰이 통과했다: {issues:?}"
        );
    }

    /// 마우스·키보드가 없으면 이 검사가 끼어들지 않는다.
    #[test]
    fn an_http_server_without_input_sinks_is_not_flagged() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
        let mut p = Project::new("p");
        let mut pl = Pipeline::new("api");
        let server = pl.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::HttpServer {
                    bind: "127.0.0.1:0".into(),
                    path: "/x".into(),
                    token: None,
                    tls: None,
                },
            },
            [0.0, 0.0],
        ));
        let reply = pl.add_node(PNode::new(
            PNodeKind::Sink {
                sink: Sink::HttpReply { server },
            },
            [1.0, 0.0],
        ));
        pl.add_link(server, reply).unwrap();
        p.pipelines.insert(pl.id, pl);
        assert!(
            !validate(&p)
                .iter()
                .any(|i| i.message.contains("마우스·키보드를 움직인다")),
            "{:?}",
            validate(&p)
        );
    }

    /// 바깥 주소를 평문으로 여는 것은 경고다 — 토큰이 있어도 전선 위에서는 보인다.
    #[test]
    fn a_non_loopback_bind_without_tls_is_a_warning() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source, TlsConfig};
        let build = |bind: &str, tls: Option<TlsConfig>| {
            let mut p = Project::new("p");
            let mut pl = Pipeline::new("api");
            let server = pl.add_node(PNode::new(
                PNodeKind::Source {
                    source: Source::HttpServer {
                        // 토큰은 넣어 둔다 — TLS 경고가 토큰 오류에 묻히지 않게.
                        bind: bind.into(),
                        path: "/x".into(),
                        token: Some("t".repeat(32)),
                        tls,
                    },
                },
                [0.0, 0.0],
            ));
            let reply = pl.add_node(PNode::new(
                PNodeKind::Sink {
                    sink: Sink::HttpReply { server },
                },
                [1.0, 0.0],
            ));
            pl.add_link(server, reply).unwrap();
            p.pipelines.insert(pl.id, pl);
            validate(&p)
        };
        let has_tls_warning = |issues: &[Issue]| {
            issues
                .iter()
                .any(|i| i.severity == Severity::Warning && i.message.contains("TLS 가 없다"))
        };

        let pem = |name: &str| TlsConfig {
            cert_pem: format!("{name}.crt"),
            key_pem: format!("{name}.key"),
        };
        assert!(has_tls_warning(&build("0.0.0.0:8799", None)), "평문 노출을 안 짚었다");
        assert!(
            !has_tls_warning(&build("0.0.0.0:8799", Some(pem("a")))),
            "TLS 가 있는데 경고했다"
        );
        // 루프백은 전선을 타지 않으므로 평문이어도 짚지 않는다.
        assert!(!has_tls_warning(&build("127.0.0.1:8799", None)), "루프백을 짚었다");
        assert!(!has_tls_warning(&build("localhost:8799", None)), "localhost 를 짚었다");
    }

    /// 바깥에서 닿는 주소에 토큰 없이 여는 것은 오류다.
    #[test]
    fn a_non_loopback_bind_without_a_token_is_an_error() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
        let build = |bind: &str, token: Option<&str>| {
            let mut p = Project::new("p");
            let mut pl = Pipeline::new("api");
            let server = pl.add_node(PNode::new(
                PNodeKind::Source {
                    source: Source::HttpServer {
                        bind: bind.into(),
                        path: "/x".into(),
                        token: token.map(str::to_string),
                        tls: None,
                    },
                },
                [0.0, 0.0],
            ));
            let reply = pl.add_node(PNode::new(
                PNodeKind::Sink {
                    sink: Sink::HttpReply { server },
                },
                [1.0, 0.0],
            ));
            pl.add_link(server, reply).unwrap();
            p.pipelines.insert(pl.id, pl);
            p
        };
        let flagged = |p: &Project| {
            validate(p)
                .iter()
                .any(|i| i.severity == Severity::Error && i.message.contains("바깥에서 닿는 주소"))
        };

        assert!(
            flagged(&build("0.0.0.0:8799", None)),
            "0.0.0.0 에 토큰 없이 여는데 통과했다"
        );
        assert!(flagged(&build("192.168.0.5:8799", None)));
        assert!(
            !flagged(&build("0.0.0.0:8799", Some("t"))),
            "토큰이 있으면 열 수 있어야 한다"
        );
        assert!(!flagged(&build("127.0.0.1:8799", None)), "루프백은 토큰 없이도 된다");
        assert!(!flagged(&build("localhost:8799", None)));
        assert!(!flagged(&build("[::1]:8799", None)));
    }

    #[test]
    fn http_server_without_a_reply_sink_is_a_warning() {
        use crate::pipeline::{PNode, PNodeKind, Pipeline, Sink, Source};
        let mut p = Project::new("p");
        let mut pl = Pipeline::new("api");
        let server = pl.add_node(PNode::new(
            PNodeKind::Source {
                source: Source::HttpServer {
                    bind: "127.0.0.1:0".into(),
                    path: "/x".into(),
                    token: None,
                    tls: None,
                },
            },
            [0.0, 0.0],
        ));
        let log = pl.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [1.0, 0.0]));
        pl.add_link(server, log).unwrap();
        p.pipelines.insert(pl.id, pl);
        let issues = validate(&p);
        assert!(
            issues
                .iter()
                .any(|i| i.severity == Severity::Warning && i.message.contains("시간 초과")),
            "{issues:?}"
        );
    }

    #[test]
    fn reports_missing_io_and_dangling() {
        let mut p = Project::new("p");
        let m = p.add_model("m");
        let g = &mut p.models.get_mut(&m).unwrap().graph;
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = g.add_node(Node::new(
            LayerKind::Linear {
                out_features: 2,
                bias: true,
            },
            [0.0, 0.0],
        ));
        g.add_edge(a, Port::new(b, 0)).unwrap();
        let issues = validate(&p);
        assert!(issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.message.contains("Output")));
        assert!(issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.message.contains("연결되지 않음")));
    }
}
