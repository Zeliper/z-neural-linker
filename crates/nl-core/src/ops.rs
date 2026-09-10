//! 편집 op. trust-pms 와 같은 원칙: `apply_op` 은 항상 성공(멱등·수렴 지향), `inverse_ops` 가 역연산을 만들고,
//! `diff_ops` 가 두 스냅샷의 차이를 op 목록으로 돌려준다(burst 편집·undo 에 사용).

use crate::dataset::DatasetSpec;
use crate::gui::Widget;
use crate::ids::*;
use crate::model::{Edge, ModelDef, Node, Project, ProjectSettings};
use crate::payload::PayloadSpec;
use crate::pipeline::{Link, PNode, Pipeline};
use crate::train::{RunRecord, TrainConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    // 프로젝트
    SetProjectMeta { name: String, description: String },
    SetSettings { settings: ProjectSettings },
    // 모델 (그래프는 통째로 갈지 않고 노드/엣지 단위)
    UpsertModelMeta { id: ModelId, name: String, description: String, payload: Option<PayloadId>, weights: Option<String> },
    DeleteModel { id: ModelId },
    SetTrainConfig { model: ModelId, config: TrainConfig },
    UpsertNode { model: ModelId, node: Node },
    DeleteNode { model: ModelId, id: NodeId },
    UpsertEdge { model: ModelId, edge: Edge },
    DeleteEdge { model: ModelId, id: EdgeId },
    // 데이터·페이로드
    UpsertDataset { dataset: DatasetSpec },
    DeleteDataset { id: DatasetId },
    UpsertPayload { payload: PayloadSpec },
    DeletePayload { id: PayloadId },
    // 파이프라인
    UpsertPipelineMeta { id: PipelineId, name: String, tick_hz: f32 },
    DeletePipeline { id: PipelineId },
    UpsertPNode { pipeline: PipelineId, node: PNode },
    DeletePNode { pipeline: PipelineId, id: PNodeId },
    UpsertLink { pipeline: PipelineId, link: Link },
    DeleteLink { pipeline: PipelineId, id: LinkId },
    // GUI
    SetGuiWindow { window: crate::gui::WindowSpec },
    UpsertWidget { widget: Widget },
    DeleteWidget { id: WidgetId },
    // 실행 기록
    UpsertRun { run: RunRecord },
    DeleteRun { id: RunId },
}

/// 항상 성공. 없는 대상 삭제는 no-op, 없는 모델/파이프라인에 대한 노드 upsert 는 빈 컨테이너를 만들어 넣는다
/// (순서가 뒤바뀐 op 도 결국 같은 상태로 수렴).
pub fn apply_op(p: &mut Project, op: &Op) {
    match op {
        Op::SetProjectMeta { name, description } => {
            p.name = name.clone();
            p.description = description.clone();
        }
        Op::SetSettings { settings } => p.settings = settings.clone(),
        Op::UpsertModelMeta { id, name, description, payload, weights } => {
            let m = p.models.entry(*id).or_insert_with(|| ModelDef { id: *id, ..ModelDef::new("") });
            m.name = name.clone();
            m.description = description.clone();
            m.payload = *payload;
            m.weights = weights.clone();
        }
        Op::DeleteModel { id } => {
            p.models.remove(id);
        }
        Op::SetTrainConfig { model, config } => {
            let m = p.models.entry(*model).or_insert_with(|| ModelDef { id: *model, ..ModelDef::new("") });
            m.train = config.clone();
        }
        Op::UpsertNode { model, node } => {
            let m = p.models.entry(*model).or_insert_with(|| ModelDef { id: *model, ..ModelDef::new("") });
            m.graph.nodes.insert(node.id, node.clone());
        }
        Op::DeleteNode { model, id } => {
            if let Some(m) = p.models.get_mut(model) {
                m.graph.remove_node(*id);
            }
        }
        Op::UpsertEdge { model, edge } => {
            let m = p.models.entry(*model).or_insert_with(|| ModelDef { id: *model, ..ModelDef::new("") });
            m.graph.edges.insert(edge.id, edge.clone());
        }
        Op::DeleteEdge { model, id } => {
            if let Some(m) = p.models.get_mut(model) {
                m.graph.edges.remove(id);
            }
        }
        Op::UpsertDataset { dataset } => {
            p.datasets.insert(dataset.id, dataset.clone());
        }
        Op::DeleteDataset { id } => {
            p.datasets.remove(id);
        }
        Op::UpsertPayload { payload } => {
            p.payloads.insert(payload.id, payload.clone());
        }
        Op::DeletePayload { id } => {
            p.payloads.remove(id);
        }
        Op::UpsertPipelineMeta { id, name, tick_hz } => {
            let pl = p.pipelines.entry(*id).or_insert_with(|| Pipeline { id: *id, ..Pipeline::new("") });
            pl.name = name.clone();
            pl.tick_hz = *tick_hz;
        }
        Op::DeletePipeline { id } => {
            p.pipelines.remove(id);
        }
        Op::UpsertPNode { pipeline, node } => {
            let pl = p.pipelines.entry(*pipeline).or_insert_with(|| Pipeline { id: *pipeline, ..Pipeline::new("") });
            pl.nodes.insert(node.id, node.clone());
        }
        Op::DeletePNode { pipeline, id } => {
            if let Some(pl) = p.pipelines.get_mut(pipeline) {
                pl.remove_node(*id);
            }
        }
        Op::UpsertLink { pipeline, link } => {
            let pl = p.pipelines.entry(*pipeline).or_insert_with(|| Pipeline { id: *pipeline, ..Pipeline::new("") });
            pl.links.insert(link.id, link.clone());
        }
        Op::DeleteLink { pipeline, id } => {
            if let Some(pl) = p.pipelines.get_mut(pipeline) {
                pl.links.remove(id);
            }
        }
        Op::SetGuiWindow { window } => p.gui.window = window.clone(),
        Op::UpsertWidget { widget } => {
            p.gui.widgets.insert(widget.id, widget.clone());
        }
        Op::DeleteWidget { id } => {
            p.gui.widgets.remove(id);
        }
        Op::UpsertRun { run } => {
            p.runs.insert(run.id, run.clone());
        }
        Op::DeleteRun { id } => {
            p.runs.remove(id);
        }
    }
}

pub fn apply_ops(p: &mut Project, ops: &[Op]) {
    for op in ops {
        apply_op(p, op);
    }
}

/// `ops` 를 `p` 에 적용하기 **전** 상태에서 역연산을 만든다. 돌려주는 순서는 이미 뒤집혀 있어 그대로 적용하면 된다.
pub fn inverse_ops(p: &Project, ops: &[Op]) -> Vec<Op> {
    let mut cur = p.clone();
    let mut inv: Vec<Vec<Op>> = Vec::with_capacity(ops.len());
    for op in ops {
        inv.push(inverse_one(&cur, op));
        apply_op(&mut cur, op);
    }
    // 역연산은 뒤에서부터. 여러 op 를 낳는 경우(노드 삭제 → 엣지 복원)는 Vec 으로 묶여 있으니 평탄화.
    inv.into_iter().rev().flatten().collect()
}

fn inverse_one(p: &Project, op: &Op) -> Vec<Op> {
    match op {
        Op::SetProjectMeta { .. } => vec![Op::SetProjectMeta { name: p.name.clone(), description: p.description.clone() }],
        Op::SetSettings { .. } => vec![Op::SetSettings { settings: p.settings.clone() }],
        Op::UpsertModelMeta { id, .. } => match p.models.get(id) {
            Some(m) => vec![Op::UpsertModelMeta {
                id: *id,
                name: m.name.clone(),
                description: m.description.clone(),
                payload: m.payload,
                weights: m.weights.clone(),
            }],
            None => vec![Op::DeleteModel { id: *id }],
        },
        Op::DeleteModel { id } => match p.models.get(id) {
            Some(m) => restore_model(m),
            None => vec![],
        },
        Op::SetTrainConfig { model, .. } => match p.models.get(model) {
            Some(m) => vec![Op::SetTrainConfig { model: *model, config: m.train.clone() }],
            None => vec![Op::DeleteModel { id: *model }],
        },
        Op::UpsertNode { model, node } => match p.models.get(model).and_then(|m| m.graph.nodes.get(&node.id)) {
            Some(old) => vec![Op::UpsertNode { model: *model, node: old.clone() }],
            None => vec![Op::DeleteNode { model: *model, id: node.id }],
        },
        Op::DeleteNode { model, id } => match p.models.get(model) {
            Some(m) => match m.graph.nodes.get(id) {
                Some(n) => {
                    let mut v = vec![Op::UpsertNode { model: *model, node: n.clone() }];
                    for e in m.graph.edges.values().filter(|e| e.from == *id || e.to.node == *id) {
                        v.push(Op::UpsertEdge { model: *model, edge: e.clone() });
                    }
                    v
                }
                None => vec![],
            },
            None => vec![],
        },
        Op::UpsertEdge { model, edge } => match p.models.get(model).and_then(|m| m.graph.edges.get(&edge.id)) {
            Some(old) => vec![Op::UpsertEdge { model: *model, edge: old.clone() }],
            None => vec![Op::DeleteEdge { model: *model, id: edge.id }],
        },
        Op::DeleteEdge { model, id } => match p.models.get(model).and_then(|m| m.graph.edges.get(id)) {
            Some(e) => vec![Op::UpsertEdge { model: *model, edge: e.clone() }],
            None => vec![],
        },
        Op::UpsertDataset { dataset } => match p.datasets.get(&dataset.id) {
            Some(old) => vec![Op::UpsertDataset { dataset: old.clone() }],
            None => vec![Op::DeleteDataset { id: dataset.id }],
        },
        Op::DeleteDataset { id } => p.datasets.get(id).map(|d| vec![Op::UpsertDataset { dataset: d.clone() }]).unwrap_or_default(),
        Op::UpsertPayload { payload } => match p.payloads.get(&payload.id) {
            Some(old) => vec![Op::UpsertPayload { payload: old.clone() }],
            None => vec![Op::DeletePayload { id: payload.id }],
        },
        Op::DeletePayload { id } => p.payloads.get(id).map(|d| vec![Op::UpsertPayload { payload: d.clone() }]).unwrap_or_default(),
        Op::UpsertPipelineMeta { id, .. } => match p.pipelines.get(id) {
            Some(pl) => vec![Op::UpsertPipelineMeta { id: *id, name: pl.name.clone(), tick_hz: pl.tick_hz }],
            None => vec![Op::DeletePipeline { id: *id }],
        },
        Op::DeletePipeline { id } => p.pipelines.get(id).map(restore_pipeline).unwrap_or_default(),
        Op::UpsertPNode { pipeline, node } => match p.pipelines.get(pipeline).and_then(|pl| pl.nodes.get(&node.id)) {
            Some(old) => vec![Op::UpsertPNode { pipeline: *pipeline, node: old.clone() }],
            None => vec![Op::DeletePNode { pipeline: *pipeline, id: node.id }],
        },
        Op::DeletePNode { pipeline, id } => match p.pipelines.get(pipeline) {
            Some(pl) => match pl.nodes.get(id) {
                Some(n) => {
                    let mut v = vec![Op::UpsertPNode { pipeline: *pipeline, node: n.clone() }];
                    for l in pl.links.values().filter(|l| l.from == *id || l.to == *id) {
                        v.push(Op::UpsertLink { pipeline: *pipeline, link: l.clone() });
                    }
                    v
                }
                None => vec![],
            },
            None => vec![],
        },
        Op::UpsertLink { pipeline, link } => match p.pipelines.get(pipeline).and_then(|pl| pl.links.get(&link.id)) {
            Some(old) => vec![Op::UpsertLink { pipeline: *pipeline, link: old.clone() }],
            None => vec![Op::DeleteLink { pipeline: *pipeline, id: link.id }],
        },
        Op::DeleteLink { pipeline, id } => match p.pipelines.get(pipeline).and_then(|pl| pl.links.get(id)) {
            Some(l) => vec![Op::UpsertLink { pipeline: *pipeline, link: l.clone() }],
            None => vec![],
        },
        Op::SetGuiWindow { .. } => vec![Op::SetGuiWindow { window: p.gui.window.clone() }],
        Op::UpsertWidget { widget } => match p.gui.widgets.get(&widget.id) {
            Some(old) => vec![Op::UpsertWidget { widget: old.clone() }],
            None => vec![Op::DeleteWidget { id: widget.id }],
        },
        Op::DeleteWidget { id } => p.gui.widgets.get(id).map(|w| vec![Op::UpsertWidget { widget: w.clone() }]).unwrap_or_default(),
        Op::UpsertRun { run } => match p.runs.get(&run.id) {
            Some(old) => vec![Op::UpsertRun { run: old.clone() }],
            None => vec![Op::DeleteRun { id: run.id }],
        },
        Op::DeleteRun { id } => p.runs.get(id).map(|r| vec![Op::UpsertRun { run: r.clone() }]).unwrap_or_default(),
    }
}

/// 모델 하나를 통째로 복원하는 op 묶음.
pub fn restore_model(m: &ModelDef) -> Vec<Op> {
    let mut v = vec![
        Op::UpsertModelMeta {
            id: m.id,
            name: m.name.clone(),
            description: m.description.clone(),
            payload: m.payload,
            weights: m.weights.clone(),
        },
        Op::SetTrainConfig { model: m.id, config: m.train.clone() },
    ];
    v.extend(m.graph.nodes.values().map(|n| Op::UpsertNode { model: m.id, node: n.clone() }));
    v.extend(m.graph.edges.values().map(|e| Op::UpsertEdge { model: m.id, edge: e.clone() }));
    v
}

pub fn restore_pipeline(pl: &Pipeline) -> Vec<Op> {
    let mut v = vec![Op::UpsertPipelineMeta { id: pl.id, name: pl.name.clone(), tick_hz: pl.tick_hz }];
    v.extend(pl.nodes.values().map(|n| Op::UpsertPNode { pipeline: pl.id, node: n.clone() }));
    v.extend(pl.links.values().map(|l| Op::UpsertLink { pipeline: pl.id, link: l.clone() }));
    v
}

/// `from` 을 `to` 로 바꾸는 op 목록. 같으면 빈 목록. (burst 편집의 forward 추출, 스냅샷 비교)
pub fn diff_ops(from: &Project, to: &Project) -> Vec<Op> {
    let mut ops = Vec::new();
    if from.name != to.name || from.description != to.description {
        ops.push(Op::SetProjectMeta { name: to.name.clone(), description: to.description.clone() });
    }
    if from.settings != to.settings {
        ops.push(Op::SetSettings { settings: to.settings.clone() });
    }
    // 모델
    for (id, m) in &to.models {
        match from.models.get(id) {
            None => ops.extend(restore_model(m)),
            Some(old) => {
                if old.name != m.name || old.description != m.description || old.payload != m.payload || old.weights != m.weights {
                    ops.push(Op::UpsertModelMeta {
                        id: *id,
                        name: m.name.clone(),
                        description: m.description.clone(),
                        payload: m.payload,
                        weights: m.weights.clone(),
                    });
                }
                if old.train != m.train {
                    ops.push(Op::SetTrainConfig { model: *id, config: m.train.clone() });
                }
                for (nid, n) in &m.graph.nodes {
                    if old.graph.nodes.get(nid) != Some(n) {
                        ops.push(Op::UpsertNode { model: *id, node: n.clone() });
                    }
                }
                for nid in old.graph.nodes.keys() {
                    if !m.graph.nodes.contains_key(nid) {
                        ops.push(Op::DeleteNode { model: *id, id: *nid });
                    }
                }
                for (eid, e) in &m.graph.edges {
                    if old.graph.edges.get(eid) != Some(e) {
                        ops.push(Op::UpsertEdge { model: *id, edge: e.clone() });
                    }
                }
                for eid in old.graph.edges.keys() {
                    if !m.graph.edges.contains_key(eid) {
                        ops.push(Op::DeleteEdge { model: *id, id: *eid });
                    }
                }
            }
        }
    }
    for id in from.models.keys() {
        if !to.models.contains_key(id) {
            ops.push(Op::DeleteModel { id: *id });
        }
    }
    // 데이터·페이로드
    for (id, d) in &to.datasets {
        if from.datasets.get(id) != Some(d) {
            ops.push(Op::UpsertDataset { dataset: d.clone() });
        }
    }
    for id in from.datasets.keys() {
        if !to.datasets.contains_key(id) {
            ops.push(Op::DeleteDataset { id: *id });
        }
    }
    for (id, d) in &to.payloads {
        if from.payloads.get(id) != Some(d) {
            ops.push(Op::UpsertPayload { payload: d.clone() });
        }
    }
    for id in from.payloads.keys() {
        if !to.payloads.contains_key(id) {
            ops.push(Op::DeletePayload { id: *id });
        }
    }
    // 파이프라인
    for (id, pl) in &to.pipelines {
        match from.pipelines.get(id) {
            None => ops.extend(restore_pipeline(pl)),
            Some(old) => {
                if old.name != pl.name || old.tick_hz != pl.tick_hz {
                    ops.push(Op::UpsertPipelineMeta { id: *id, name: pl.name.clone(), tick_hz: pl.tick_hz });
                }
                for (nid, n) in &pl.nodes {
                    if old.nodes.get(nid) != Some(n) {
                        ops.push(Op::UpsertPNode { pipeline: *id, node: n.clone() });
                    }
                }
                for nid in old.nodes.keys() {
                    if !pl.nodes.contains_key(nid) {
                        ops.push(Op::DeletePNode { pipeline: *id, id: *nid });
                    }
                }
                for (lid, l) in &pl.links {
                    if old.links.get(lid) != Some(l) {
                        ops.push(Op::UpsertLink { pipeline: *id, link: l.clone() });
                    }
                }
                for lid in old.links.keys() {
                    if !pl.links.contains_key(lid) {
                        ops.push(Op::DeleteLink { pipeline: *id, id: *lid });
                    }
                }
            }
        }
    }
    for id in from.pipelines.keys() {
        if !to.pipelines.contains_key(id) {
            ops.push(Op::DeletePipeline { id: *id });
        }
    }
    // GUI
    if from.gui.window != to.gui.window {
        ops.push(Op::SetGuiWindow { window: to.gui.window.clone() });
    }
    for (id, w) in &to.gui.widgets {
        if from.gui.widgets.get(id) != Some(w) {
            ops.push(Op::UpsertWidget { widget: w.clone() });
        }
    }
    for id in from.gui.widgets.keys() {
        if !to.gui.widgets.contains_key(id) {
            ops.push(Op::DeleteWidget { id: *id });
        }
    }
    // 실행 기록
    for (id, r) in &to.runs {
        if from.runs.get(id) != Some(r) {
            ops.push(Op::UpsertRun { run: r.clone() });
        }
    }
    for id in from.runs.keys() {
        if !to.runs.contains_key(id) {
            ops.push(Op::DeleteRun { id: *id });
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LayerKind, Port};

    fn sample() -> (Project, ModelId, NodeId, NodeId, EdgeId) {
        let mut p = Project::new("p");
        let m = p.add_model("m");
        let g = &mut p.models.get_mut(&m).unwrap().graph;
        let a = g.add_node(Node::new(LayerKind::Input { shape: vec![4] }, [0.0, 0.0]));
        let b = g.add_node(Node::new(LayerKind::Linear { out_features: 2, bias: true }, [100.0, 0.0]));
        let e = g.add_edge(a, Port::new(b, 0)).unwrap();
        (p, m, a, b, e)
    }

    #[test]
    fn delete_node_inverse_restores_edges() {
        let (p, m, a, _b, e) = sample();
        let ops = vec![Op::DeleteNode { model: m, id: a }];
        let inv = inverse_ops(&p, &ops);
        let mut q = p.clone();
        apply_ops(&mut q, &ops);
        assert!(!q.models[&m].graph.edges.contains_key(&e));
        apply_ops(&mut q, &inv);
        assert_eq!(q, p);
    }

    #[test]
    fn delete_model_inverse_restores_whole_model() {
        let (p, m, _, _, _) = sample();
        let ops = vec![Op::DeleteModel { id: m }];
        let inv = inverse_ops(&p, &ops);
        let mut q = p.clone();
        apply_ops(&mut q, &ops);
        assert!(q.models.is_empty());
        apply_ops(&mut q, &inv);
        assert_eq!(q, p);
    }

    #[test]
    fn diff_then_apply_converges() {
        let (p, m, _a, b, _e) = sample();
        let mut q = p.clone();
        q.name = "renamed".into();
        q.models.get_mut(&m).unwrap().graph.nodes.get_mut(&b).unwrap().pos = [50.0, 50.0];
        q.models.get_mut(&m).unwrap().train.epochs = 99;
        let ds = DatasetSpec::new("d", crate::dataset::DataSource::Synthetic { kind: crate::dataset::SyntheticKind::Xor, samples: 10 });
        q.datasets.insert(ds.id, ds);
        let ops = diff_ops(&p, &q);
        assert!(!ops.is_empty());
        let mut r = p.clone();
        apply_ops(&mut r, &ops);
        assert_eq!(r, q);
        assert!(diff_ops(&q, &r).is_empty());
    }

    #[test]
    fn inverse_of_diff_round_trips() {
        let (p, m, a, _b, _e) = sample();
        let mut q = p.clone();
        q.models.get_mut(&m).unwrap().graph.remove_node(a);
        let ops = diff_ops(&p, &q);
        let inv = inverse_ops(&p, &ops);
        let mut r = p.clone();
        apply_ops(&mut r, &ops);
        assert_eq!(r, q);
        apply_ops(&mut r, &inv);
        assert_eq!(r, p);
    }

    #[test]
    fn ops_are_idempotent_and_order_tolerant() {
        let (p, m, _, _, _) = sample();
        let n = Node::new(LayerKind::Flatten, [0.0, 0.0]);
        let op = Op::UpsertNode { model: m, node: n.clone() };
        let mut q = p.clone();
        apply_op(&mut q, &op);
        apply_op(&mut q, &op);
        assert_eq!(q.models[&m].graph.nodes.len(), 3);
        // 없는 모델에 노드 upsert → 빈 모델이 생기고 노드가 들어간다
        let other = ModelId::new();
        let mut r = p.clone();
        apply_op(&mut r, &Op::UpsertNode { model: other, node: n.clone() });
        assert!(r.models[&other].graph.nodes.contains_key(&n.id));
        apply_op(&mut r, &Op::DeleteNode { model: ModelId::new(), id: n.id });
    }

    #[test]
    fn op_json_round_trip() {
        let (p, m, a, _, _) = sample();
        let op = Op::UpsertNode { model: m, node: p.models[&m].graph.nodes[&a].clone() };
        let s = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&s).unwrap();
        assert_eq!(back, op);
    }
}
