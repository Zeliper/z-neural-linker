//! 우측 인스펙터. 선택된 대상의 속성을 편집한다.
//!
//! 타이핑·드래그는 `&mut` 로 문서를 직접 고치고 `DocState::note_edited` 로 burst 에 모은다
//! (1초 무입력 → undo 한 항목). 구조가 바뀌는 편집만 op 로 보낸다.

use crate::app::NlApp;
use crate::canvas::Selection;
use crate::views::{self, parse_shape_text, shape_text, COL_ERROR, COL_OK, COL_WARN, COL_WEAK};
use eframe::egui::{self, DragValue, RichText};
use nl_core::dataset::{DataSource, Split, SyntheticKind};
use nl_core::train::{Loss, Metric, Optimizer};
use nl_core::{Act, DevicePref, LayerKind, Op, PayloadId};

impl NlApp {
    pub(crate) fn inspector(&mut self, ui: &mut egui::Ui, now: f64) {
        let selection = self.canvas.selection;
        egui::ScrollArea::vertical().id_salt("inspector-scroll").show(ui, |ui| match selection {
            Selection::None => {
                ui.label(RichText::new("선택된 항목이 없습니다").color(COL_WEAK));
                ui.add_space(6.0);
                ui.label(RichText::new("좌측 아웃라인이나 캔버스에서 대상을 고르세요.").color(COL_WEAK).size(11.5));
            }
            Selection::Project => self.inspect_project(ui, now),
            Selection::Model(id) => self.inspect_model(ui, id, now),
            Selection::Node(m, n) => self.inspect_node(ui, m, n, now),
            Selection::Edge(m, e) => self.inspect_edge(ui, m, e),
            Selection::Dataset(id) => self.inspect_dataset(ui, id, now),
            Selection::Payload(id) => self.inspect_payload(ui, id, now),
            Selection::Pipeline(id) => self.inspect_pipeline(ui, id, now),
            Selection::Run(id) => self.inspect_run(ui, id, now),
        });
    }

    // ── 프로젝트 ────────────────────────────────────────────────

    fn inspect_project(&mut self, ui: &mut egui::Ui, now: f64) {
        ui.label(RichText::new("프로젝트").size(15.0).strong());
        ui.separator();
        let devices = self.devices_snapshot();
        let doc = &mut self.doc;
        let mut changed = false;
        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut doc.project.name).desired_width(f32::INFINITY)).changed();
        ui.label(RichText::new("설명").color(COL_WEAK).size(11.5));
        changed |= ui
            .add(egui::TextEdit::multiline(&mut doc.project.description).desired_rows(3).desired_width(f32::INFINITY))
            .changed();
        ui.add_space(6.0);
        ui.label(RichText::new("기본 장치").color(COL_WEAK).size(11.5));
        let current = doc.project.settings.default_device;
        let label = devices
            .iter()
            .find(|(pref, _)| *pref == current)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| current.label());
        egui::ComboBox::from_id_salt("insp-device").selected_text(label).show_ui(ui, |ui| {
            if ui.selectable_label(current == DevicePref::Auto, "자동").clicked() && current != DevicePref::Auto {
                doc.project.settings.default_device = DevicePref::Auto;
                changed = true;
            }
            for (pref, name) in &devices {
                if ui.selectable_label(current == *pref, name).clicked() && current != *pref {
                    doc.project.settings.default_device = *pref;
                    changed = true;
                }
            }
        });
        ui.add_space(6.0);
        views::kv(ui, "모델", doc.project.models.len().to_string());
        views::kv(ui, "데이터셋", doc.project.datasets.len().to_string());
        views::kv(ui, "페이로드", doc.project.payloads.len().to_string());
        views::kv(ui, "실행 기록", doc.project.runs.len().to_string());
        if changed {
            doc.note_edited(now);
        }
    }

    /// 장치 목록의 (선호값, 표시 이름) — 문서를 빌린 채 쓰기 위해 미리 복사한다.
    fn devices_snapshot(&self) -> Vec<(DevicePref, String)> {
        self.devices.iter().map(|d| (d.pref, views::device_label(d))).collect()
    }

    // ── 모델 ────────────────────────────────────────────────────

    fn inspect_model(&mut self, ui: &mut egui::Ui, id: nl_core::ModelId, now: f64) {
        let payloads: Vec<(PayloadId, String)> =
            self.doc.project.payloads.iter().map(|(k, v)| (*k, v.name.clone())).collect();
        let datasets: Vec<(nl_core::DatasetId, String)> =
            self.doc.project.datasets.iter().map(|(k, v)| (*k, v.name.clone())).collect();
        let devices = self.devices_snapshot();
        let doc = &mut self.doc;
        let Some(model) = doc.project.models.get_mut(&id) else {
            ui.label(RichText::new("모델이 없습니다").color(COL_ERROR));
            return;
        };
        let mut changed = false;
        ui.label(RichText::new("모델").size(15.0).strong());
        ui.separator();
        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut model.name).desired_width(f32::INFINITY)).changed();
        ui.label(RichText::new("설명").color(COL_WEAK).size(11.5));
        changed |= ui
            .add(egui::TextEdit::multiline(&mut model.description).desired_rows(2).desired_width(f32::INFINITY))
            .changed();

        ui.add_space(4.0);
        ui.label(RichText::new("페이로드").color(COL_WEAK).size(11.5));
        let cur = model.payload;
        let label = cur.and_then(|p| payloads.iter().find(|(k, _)| *k == p)).map(|(_, n)| n.clone()).unwrap_or_else(|| "(없음)".into());
        egui::ComboBox::from_id_salt("insp-model-payload").selected_text(label).show_ui(ui, |ui| {
            if ui.selectable_label(cur.is_none(), "(없음)").clicked() && cur.is_some() {
                model.payload = None;
                changed = true;
            }
            for (pid, name) in &payloads {
                if ui.selectable_label(cur == Some(*pid), name).clicked() && cur != Some(*pid) {
                    model.payload = Some(*pid);
                    changed = true;
                }
            }
        });
        if let Some(w) = &model.weights {
            ui.label(RichText::new(format!("가중치: {}", views::short_path(w))).color(COL_OK).size(11.0)).on_hover_text(w);
        } else {
            ui.label(RichText::new("학습된 가중치 없음").color(COL_WARN).size(11.0));
        }

        ui.add_space(8.0);
        ui.label(RichText::new("학습 설정").size(13.5).strong());
        ui.separator();
        let c = &mut model.train;

        ui.label(RichText::new("데이터셋").color(COL_WEAK).size(11.5));
        let cur_ds = c.dataset;
        let ds_label =
            cur_ds.and_then(|d| datasets.iter().find(|(k, _)| *k == d)).map(|(_, n)| n.clone()).unwrap_or_else(|| "(없음)".into());
        egui::ComboBox::from_id_salt("insp-train-dataset").selected_text(ds_label).show_ui(ui, |ui| {
            if ui.selectable_label(cur_ds.is_none(), "(없음)").clicked() && cur_ds.is_some() {
                c.dataset = None;
                changed = true;
            }
            for (did, name) in &datasets {
                if ui.selectable_label(cur_ds == Some(*did), name).clicked() && cur_ds != Some(*did) {
                    c.dataset = Some(*did);
                    changed = true;
                }
            }
        });

        ui.label(RichText::new("옵티마이저").color(COL_WEAK).size(11.5));
        egui::ComboBox::from_id_salt("insp-optim").selected_text(c.optimizer.label()).show_ui(ui, |ui| {
            for preset in [Optimizer::default_sgd(), Optimizer::default_adam(), Optimizer::default_adamw()] {
                let same = c.optimizer.label() == preset.label();
                if ui.selectable_label(same, preset.label()).clicked() && !same {
                    // 종류를 바꿔도 학습률은 이어 간다.
                    let lr = c.optimizer.lr();
                    c.optimizer = preset;
                    c.optimizer.set_lr(lr);
                    changed = true;
                }
            }
        });
        changed |= optimizer_params(ui, &mut c.optimizer);

        ui.horizontal(|ui| {
            ui.label(RichText::new("손실").color(COL_WEAK).size(11.5));
            egui::ComboBox::from_id_salt("insp-loss").selected_text(c.loss.label()).show_ui(ui, |ui| {
                for l in Loss::ALL {
                    if ui.selectable_label(c.loss == l, l.label()).clicked() && c.loss != l {
                        c.loss = l;
                        changed = true;
                    }
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("지표").color(COL_WEAK).size(11.5));
            egui::ComboBox::from_id_salt("insp-metric").selected_text(c.metric.label()).show_ui(ui, |ui| {
                for m in Metric::ALL {
                    if ui.selectable_label(c.metric == m, m.label()).clicked() && c.metric != m {
                        c.metric = m;
                        changed = true;
                    }
                }
            });
        });

        egui::Grid::new("insp-train-grid").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label(RichText::new("에포크").color(COL_WEAK).size(11.5));
            changed |= ui.add(DragValue::new(&mut c.epochs).range(1..=100_000)).changed();
            ui.end_row();
            ui.label(RichText::new("배치 크기").color(COL_WEAK).size(11.5));
            changed |= ui.add(DragValue::new(&mut c.batch_size).range(1..=65_536)).changed();
            ui.end_row();
            ui.label(RichText::new("검증 비율").color(COL_WEAK).size(11.5));
            changed |= ui.add(DragValue::new(&mut c.val_split).range(0.0..=0.9).speed(0.01)).changed();
            ui.end_row();
            ui.label(RichText::new("시드").color(COL_WEAK).size(11.5));
            changed |= ui.add(DragValue::new(&mut c.seed)).changed();
            ui.end_row();
            ui.label(RichText::new("체크포인트 주기").color(COL_WEAK).size(11.5));
            changed |= ui
                .add(DragValue::new(&mut c.checkpoint_every).range(0..=10_000).suffix(" 에포크"))
                .on_hover_text("0 이면 마지막만")
                .changed();
            ui.end_row();
            ui.label(RichText::new("그래디언트 클리핑").color(COL_WEAK).size(11.5));
            changed |= ui
                .add(DragValue::new(&mut c.grad_clip).range(0.0..=1000.0).speed(0.05))
                .on_hover_text("0 이면 없음")
                .changed();
            ui.end_row();
        });

        ui.label(RichText::new("장치").color(COL_WEAK).size(11.5));
        let cur_dev = c.device;
        let dev_label = devices
            .iter()
            .find(|(p, _)| *p == cur_dev)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| cur_dev.label());
        egui::ComboBox::from_id_salt("insp-train-device").selected_text(dev_label).show_ui(ui, |ui| {
            if ui.selectable_label(cur_dev == DevicePref::Auto, "자동").clicked() && cur_dev != DevicePref::Auto {
                c.device = DevicePref::Auto;
                changed = true;
            }
            for (pref, name) in &devices {
                if ui.selectable_label(cur_dev == *pref, name).clicked() && cur_dev != *pref {
                    c.device = *pref;
                    changed = true;
                }
            }
        });

        if changed {
            doc.note_edited(now);
        }
    }

    // ── 노드 ────────────────────────────────────────────────────

    fn inspect_node(&mut self, ui: &mut egui::Ui, model: nl_core::ModelId, node_id: nl_core::NodeId, now: f64) {
        let stale = self.buffers_stale();
        let report = self.shapes();
        // 서로 다른 필드라 동시에 빌릴 수 있다.
        let shape_buf = &mut self.shape_buf;
        let doc = &mut self.doc;
        let Some(node) = doc.project.models.get_mut(&model).and_then(|m| m.graph.nodes.get_mut(&node_id)) else {
            ui.label(RichText::new("노드가 없습니다").color(COL_ERROR));
            return;
        };
        let spec = node.kind.spec();
        let mut changed = false;

        ui.label(RichText::new(spec.label).size(15.0).strong());
        ui.label(RichText::new(node.kind.summary()).color(COL_WEAK).size(11.5));
        ui.separator();

        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut node.name).desired_width(f32::INFINITY)).changed();

        // 형상 텍스트 필드는 파싱이 실패해도 타이핑을 끊지 않도록 버퍼를 따로 둔다.
        if stale {
            *shape_buf = match &node.kind {
                LayerKind::Input { shape } | LayerKind::Reshape { shape } => shape_text(shape),
                _ => String::new(),
            };
        }

        ui.add_space(4.0);
        match &mut node.kind {
            LayerKind::Input { shape } | LayerKind::Reshape { shape } => {
                ui.label(RichText::new("형상 (배치 제외)").color(COL_WEAK).size(11.5));
                let resp = ui.add(egui::TextEdit::singleline(shape_buf).desired_width(f32::INFINITY).hint_text("3×28×28"));
                if resp.changed() {
                    if let Some(v) = parse_shape_text(shape_buf) {
                        if *shape != v {
                            *shape = v;
                            changed = true;
                        }
                    }
                }
                if parse_shape_text(shape_buf).is_none() {
                    ui.label(RichText::new("숫자를 × 로 잇습니다 (예: 3×28×28)").color(COL_WARN).size(11.0));
                }
            }
            LayerKind::Output => {
                ui.label(RichText::new("모델의 출력입니다. 설정이 없습니다.").color(COL_WEAK).size(11.5));
            }
            LayerKind::Linear { out_features, bias } => {
                ui.horizontal(|ui| {
                    ui.label("출력 크기");
                    changed |= ui.add(DragValue::new(out_features).range(1..=1_000_000)).changed();
                });
                changed |= ui.checkbox(bias, "바이어스").changed();
            }
            LayerKind::Conv2d { out_channels, kernel, stride, padding, bias } => {
                ui.horizontal(|ui| {
                    ui.label("출력 채널");
                    changed |= ui.add(DragValue::new(out_channels).range(1..=100_000)).changed();
                });
                changed |= pair(ui, "커널", kernel, 1..=64);
                changed |= pair(ui, "스트라이드", stride, 1..=32);
                changed |= pair(ui, "패딩", padding, 0..=32);
                changed |= ui.checkbox(bias, "바이어스").changed();
            }
            LayerKind::MaxPool2d { kernel, stride } | LayerKind::AvgPool2d { kernel, stride } => {
                changed |= pair(ui, "커널", kernel, 1..=64);
                changed |= pair(ui, "스트라이드", stride, 1..=32);
            }
            LayerKind::Activation { act } => {
                ui.horizontal(|ui| {
                    ui.label("함수");
                    egui::ComboBox::from_id_salt("insp-act").selected_text(act.label()).show_ui(ui, |ui| {
                        for a in Act::ALL {
                            let same = a.label() == act.label();
                            if ui.selectable_label(same, a.label()).clicked() && !same {
                                *act = a;
                                changed = true;
                            }
                        }
                    });
                });
                if let Act::LeakyRelu { slope } = act {
                    ui.horizontal(|ui| {
                        ui.label("기울기");
                        changed |= ui.add(DragValue::new(slope).speed(0.005).range(0.0..=1.0)).changed();
                    });
                }
            }
            LayerKind::Dropout { p } => {
                ui.horizontal(|ui| {
                    ui.label("비율 p");
                    changed |= ui.add(DragValue::new(p).range(0.0..=0.95).speed(0.01)).changed();
                });
            }
            LayerKind::BatchNorm { eps, momentum } => {
                ui.horizontal(|ui| {
                    ui.label("eps");
                    changed |= ui.add(DragValue::new(eps).speed(1e-6)).changed();
                    ui.label("momentum");
                    changed |= ui.add(DragValue::new(momentum).range(0.0..=1.0).speed(0.01)).changed();
                });
            }
            LayerKind::LayerNorm { eps } => {
                ui.horizontal(|ui| {
                    ui.label("eps");
                    changed |= ui.add(DragValue::new(eps).speed(1e-6)).changed();
                });
            }
            LayerKind::Concat { dim } => {
                ui.horizontal(|ui| {
                    ui.label("이어붙일 차원");
                    changed |= ui.add(DragValue::new(dim).range(0..=7)).changed();
                });
            }
            LayerKind::Embedding { vocab, dim } => {
                ui.horizontal(|ui| {
                    ui.label("어휘 수");
                    changed |= ui.add(DragValue::new(vocab).range(1..=10_000_000)).changed();
                    ui.label("차원");
                    changed |= ui.add(DragValue::new(dim).range(1..=100_000)).changed();
                });
            }
            LayerKind::GlobalAvgPool | LayerKind::Flatten | LayerKind::Add | LayerKind::Mul => {
                ui.label(RichText::new("설정이 없는 레이어입니다.").color(COL_WEAK).size(11.5));
            }
        }

        ui.add_space(8.0);
        ui.label(RichText::new("위치").color(COL_WEAK).size(11.5));
        ui.horizontal(|ui| {
            changed |= ui.add(DragValue::new(&mut node.pos[0]).prefix("x ").speed(1.0)).changed();
            changed |= ui.add(DragValue::new(&mut node.pos[1]).prefix("y ").speed(1.0)).changed();
        });

        ui.add_space(8.0);
        ui.separator();
        match report.errors.get(&node_id) {
            Some(e) => {
                ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(12.0));
            }
            None => {
                let out = report.shape(node_id).map(|s| s.to_string()).unwrap_or_else(|| "?".into());
                ui.label(RichText::new(format!("출력 형상 {out}")).color(COL_OK).size(12.0));
            }
        }
        views::kv(ui, "입력 슬롯", spec.inputs.to_string());
        views::kv(ui, "학습 파라미터", if spec.has_params { "있음" } else { "없음" });

        if changed {
            doc.note_edited(now);
        }
    }

    fn inspect_edge(&mut self, ui: &mut egui::Ui, model: nl_core::ModelId, edge: nl_core::EdgeId) {
        let Some(m) = self.doc.project.models.get(&model) else { return };
        let Some(e) = m.graph.edges.get(&edge) else {
            ui.label(RichText::new("연결이 없습니다").color(COL_ERROR));
            return;
        };
        let name = |id: nl_core::NodeId| m.graph.nodes.get(&id).map(|n| n.display_name()).unwrap_or_else(|| "?".into());
        ui.label(RichText::new("연결").size(15.0).strong());
        ui.separator();
        views::kv(ui, "출발", name(e.from));
        views::kv(ui, "도착", format!("{} · 입력 {}", name(e.to.node), e.to.slot + 1));
        ui.add_space(6.0);
        ui.label(RichText::new("Del 로 지웁니다.").color(COL_WEAK).size(11.5));
    }

    // ── 데이터셋 · 페이로드 ─────────────────────────────────────

    fn inspect_dataset(&mut self, ui: &mut egui::Ui, id: nl_core::DatasetId, now: f64) {
        let payloads: Vec<(PayloadId, String)> =
            self.doc.project.payloads.iter().map(|(k, v)| (*k, v.name.clone())).collect();
        let doc = &mut self.doc;
        let Some(d) = doc.project.datasets.get_mut(&id) else {
            ui.label(RichText::new("데이터셋이 없습니다").color(COL_ERROR));
            return;
        };
        let mut changed = false;
        ui.label(RichText::new("데이터셋").size(15.0).strong());
        ui.separator();
        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(f32::INFINITY)).changed();

        ui.add_space(4.0);
        ui.label(RichText::new("소스").color(COL_WEAK).size(11.5));
        match &mut d.source {
            DataSource::Synthetic { kind, samples } => {
                egui::ComboBox::from_id_salt("insp-syn").selected_text(kind.label()).show_ui(ui, |ui| {
                    for k in SyntheticKind::ALL {
                        if ui.selectable_label(*kind == k, k.label()).clicked() && *kind != k {
                            *kind = k;
                            changed = true;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("샘플 수");
                    changed |= ui.add(DragValue::new(samples).range(2..=1_000_000)).changed();
                });
                let (i, t, c) = kind.shapes();
                views::kv(ui, "입력", shape_text(&i));
                views::kv(ui, "타깃", shape_text(&t));
                if let Some(c) = c {
                    views::kv(ui, "클래스", c.to_string());
                }
            }
            DataSource::Csv { path, input_cols, target_cols, header } => {
                changed |= ui.add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY)).changed();
                changed |= ui.checkbox(header, "첫 줄이 헤더").changed();
                let mut inp = input_cols.join(", ");
                let mut tgt = target_cols.join(", ");
                ui.label(RichText::new("입력 열").color(COL_WEAK).size(11.0));
                if ui.add(egui::TextEdit::singleline(&mut inp).desired_width(f32::INFINITY)).changed() {
                    *input_cols = split_list(&inp);
                    changed = true;
                }
                ui.label(RichText::new("타깃 열").color(COL_WEAK).size(11.0));
                if ui.add(egui::TextEdit::singleline(&mut tgt).desired_width(f32::INFINITY)).changed() {
                    *target_cols = split_list(&tgt);
                    changed = true;
                }
            }
            DataSource::ImageFolder { path } | DataSource::Recorded { path } => {
                changed |= ui.add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY)).changed();
                ui.label(RichText::new("하위 폴더 이름이 클래스가 됩니다").color(COL_WEAK).size(11.0));
            }
        }

        ui.add_space(6.0);
        changed |= ui.checkbox(&mut d.shuffle, "섞기").changed();
        let ratio = matches!(d.split, Split::Ratio);
        ui.label(RichText::new("검증 분할").color(COL_WEAK).size(11.5));
        egui::ComboBox::from_id_salt("insp-split")
            .selected_text(if ratio { "학습 설정의 비율" } else { "별도 소스" })
            .show_ui(ui, |ui| {
                if ui.selectable_label(ratio, "학습 설정의 비율").clicked() && !ratio {
                    d.split = Split::Ratio;
                    changed = true;
                }
                if ui.selectable_label(!ratio, "별도 소스").clicked() && ratio {
                    d.split = Split::Separate { validation: Box::new(d.source.clone()) };
                    changed = true;
                }
            });

        ui.add_space(6.0);
        ui.label(RichText::new("페이로드").color(COL_WEAK).size(11.5));
        let cur = d.payload;
        let label =
            cur.and_then(|p| payloads.iter().find(|(k, _)| *k == p)).map(|(_, n)| n.clone()).unwrap_or_else(|| "(없음)".into());
        egui::ComboBox::from_id_salt("insp-ds-payload").selected_text(label).show_ui(ui, |ui| {
            if ui.selectable_label(cur.is_none(), "(없음)").clicked() && cur.is_some() {
                d.payload = None;
                changed = true;
            }
            for (pid, name) in &payloads {
                if ui.selectable_label(cur == Some(*pid), name).clicked() && cur != Some(*pid) {
                    d.payload = Some(*pid);
                    changed = true;
                }
            }
        });

        if let Some(info) = &d.cached_info {
            ui.add_space(6.0);
            ui.separator();
            views::kv(ui, "샘플", info.samples.to_string());
            views::kv(ui, "입력 형상", shape_text(&info.input_shape));
            views::kv(ui, "타깃 형상", shape_text(&info.target_shape));
            if !info.classes.is_empty() {
                views::kv(ui, "클래스", info.classes.join(", "));
            }
        }
        if changed {
            doc.note_edited(now);
        }
    }

    fn inspect_payload(&mut self, ui: &mut egui::Ui, id: PayloadId, now: f64) {
        let doc = &mut self.doc;
        let Some(p) = doc.project.payloads.get_mut(&id) else {
            ui.label(RichText::new("페이로드가 없습니다").color(COL_ERROR));
            return;
        };
        let mut changed = false;
        ui.label(RichText::new("페이로드").size(15.0).strong());
        ui.separator();
        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut p.name).desired_width(f32::INFINITY)).changed();
        ui.add_space(6.0);
        for (title, fields) in [("입력", &p.inputs), ("출력", &p.outputs)] {
            ui.label(RichText::new(title).strong());
            if fields.is_empty() {
                ui.label(RichText::new("필드 없음").color(COL_WEAK).size(11.0));
            }
            for f in fields {
                let shape = f.tensor_shape().map(|s| shape_text(&s)).unwrap_or_else(|| "-".into());
                views::kv(ui, &f.name, format!("{} · {}", crate::views::data::field_kind_label(&f.kind), shape));
            }
            ui.add_space(4.0);
        }
        ui.label(RichText::new("필드와 Transform 체인은 데이터 뷰에서 편집합니다.").color(COL_WEAK).size(11.0));
        if changed {
            doc.note_edited(now);
        }
    }

    fn inspect_pipeline(&mut self, ui: &mut egui::Ui, id: nl_core::PipelineId, now: f64) {
        let doc = &mut self.doc;
        let Some(pl) = doc.project.pipelines.get_mut(&id) else {
            ui.label(RichText::new("파이프라인이 없습니다").color(COL_ERROR));
            return;
        };
        let mut changed = false;
        ui.label(RichText::new("파이프라인").size(15.0).strong());
        ui.separator();
        ui.label(RichText::new("이름").color(COL_WEAK).size(11.5));
        changed |= ui.add(egui::TextEdit::singleline(&mut pl.name).desired_width(f32::INFINITY)).changed();
        ui.horizontal(|ui| {
            ui.label("틱 속도");
            changed |= ui.add(DragValue::new(&mut pl.tick_hz).range(0.1..=240.0).suffix(" Hz")).changed();
        });
        views::kv(ui, "노드", pl.nodes.len().to_string());
        views::kv(ui, "연결", pl.links.len().to_string());
        ui.add_space(6.0);
        ui.label(RichText::new("노드 편집기는 M0 2차분에서 구현합니다.").color(COL_WARN).size(11.5));
        if changed {
            doc.note_edited(now);
        }
    }

    fn inspect_run(&mut self, ui: &mut egui::Ui, id: nl_core::RunId, now: f64) {
        let model_name = {
            let run = self.doc.project.runs.get(&id);
            run.and_then(|r| self.doc.project.models.get(&r.model)).map(|m| m.name.clone()).unwrap_or_else(|| "(삭제됨)".into())
        };
        let doc = &mut self.doc;
        let Some(r) = doc.project.runs.get_mut(&id) else {
            ui.label(RichText::new("실행 기록이 없습니다").color(COL_ERROR));
            return;
        };
        ui.label(RichText::new("실행 기록").size(15.0).strong());
        ui.separator();
        views::kv(ui, "모델", model_name);
        views::kv(ui, "상태", crate::views::train::status_label(r.status));
        views::kv(ui, "시작", r.started.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string());
        if let Some(f) = r.finished {
            views::kv(ui, "종료", f.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string());
        }
        views::kv(ui, "장치", if r.device_name.is_empty() { "-".to_string() } else { r.device_name.clone() });
        views::kv(ui, "에포크", format!("{}/{}", r.epochs.len(), r.config.epochs));
        if let Some(e) = r.last() {
            views::kv(ui, "최종 학습 손실", views::fmt_metric(e.train_loss));
        }
        if let Some(v) = r.best_val_loss() {
            views::kv(ui, "최고 검증 손실", views::fmt_metric(v));
        }
        if let Some(c) = &r.checkpoint {
            views::kv(ui, "체크포인트", views::short_path(c));
        }
        if let Some(e) = &r.error {
            ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
        }
        ui.add_space(6.0);
        ui.label(RichText::new("메모").color(COL_WEAK).size(11.5));
        let changed =
            ui.add(egui::TextEdit::multiline(&mut r.note).desired_rows(3).desired_width(f32::INFINITY)).changed();
        if changed {
            doc.note_edited(now);
        }
    }
}

/// `[a, b]` 형태의 커널·스트라이드·패딩 두 칸.
fn pair(ui: &mut egui::Ui, label: &str, v: &mut [usize; 2], range: std::ops::RangeInclusive<usize>) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        changed |= ui.add(DragValue::new(&mut v[0]).range(range.clone())).changed();
        ui.label("×");
        changed |= ui.add(DragValue::new(&mut v[1]).range(range)).changed();
    });
    changed
}

fn optimizer_params(ui: &mut egui::Ui, opt: &mut Optimizer) -> bool {
    let mut changed = false;
    match opt {
        Optimizer::Sgd { lr, momentum } => {
            ui.horizontal(|ui| {
                ui.label("lr");
                changed |= ui.add(DragValue::new(lr).speed(1e-4).range(1e-8..=10.0)).changed();
                ui.label("모멘텀");
                changed |= ui.add(DragValue::new(momentum).speed(0.01).range(0.0..=1.0)).changed();
            });
        }
        Optimizer::Adam { lr, beta1, beta2, eps } => {
            ui.horizontal(|ui| {
                ui.label("lr");
                changed |= ui.add(DragValue::new(lr).speed(1e-5).range(1e-8..=10.0)).changed();
                ui.label("eps");
                changed |= ui.add(DragValue::new(eps).speed(1e-9)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("β1");
                changed |= ui.add(DragValue::new(beta1).speed(0.001).range(0.0..=0.9999)).changed();
                ui.label("β2");
                changed |= ui.add(DragValue::new(beta2).speed(0.001).range(0.0..=0.9999)).changed();
            });
        }
        Optimizer::AdamW { lr, beta1, beta2, eps, weight_decay } => {
            ui.horizontal(|ui| {
                ui.label("lr");
                changed |= ui.add(DragValue::new(lr).speed(1e-5).range(1e-8..=10.0)).changed();
                ui.label("eps");
                changed |= ui.add(DragValue::new(eps).speed(1e-9)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("β1");
                changed |= ui.add(DragValue::new(beta1).speed(0.001).range(0.0..=0.9999)).changed();
                ui.label("β2");
                changed |= ui.add(DragValue::new(beta2).speed(0.001).range(0.0..=0.9999)).changed();
            });
            ui.horizontal(|ui| {
                ui.label("가중치 감쇠");
                changed |= ui.add(DragValue::new(weight_decay).speed(1e-4).range(0.0..=1.0)).changed();
            });
        }
    }
    changed
}

/// "a, b , c" → ["a", "b", "c"] (빈 항목 제거).
pub fn split_list(text: &str) -> Vec<String> {
    text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// 인스펙터가 op 로 보내야 하는 편집 (아웃라인의 이름 변경과 공유).
pub fn rename_op(project: &nl_core::Project, sel: Selection, name: String) -> Option<Op> {
    match sel {
        Selection::Model(id) => {
            let m = project.models.get(&id)?;
            Some(Op::UpsertModelMeta {
                id,
                name,
                description: m.description.clone(),
                payload: m.payload,
                weights: m.weights.clone(),
            })
        }
        Selection::Node(model, id) => {
            let mut n = project.models.get(&model)?.graph.nodes.get(&id)?.clone();
            n.name = name;
            Some(Op::UpsertNode { model, node: n })
        }
        Selection::Dataset(id) => {
            let mut d = project.datasets.get(&id)?.clone();
            d.name = name;
            Some(Op::UpsertDataset { dataset: d })
        }
        Selection::Payload(id) => {
            let mut p = project.payloads.get(&id)?.clone();
            p.name = name;
            Some(Op::UpsertPayload { payload: p })
        }
        Selection::Pipeline(id) => {
            let pl = project.pipelines.get(&id)?;
            Some(Op::UpsertPipelineMeta { id, name, tick_hz: pl.tick_hz })
        }
        Selection::Project => Some(Op::SetProjectMeta { name, description: project.description.clone() }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;

    #[test]
    fn split_list_trims_and_drops_empties() {
        assert_eq!(split_list("a, b ,, c"), vec!["a", "b", "c"]);
        assert!(split_list("  ,  ").is_empty());
    }

    #[test]
    fn rename_op_covers_every_named_object() {
        let p = sample::xor_project();
        let model = *p.models.keys().next().unwrap();
        let node = *p.models[&model].graph.nodes.keys().next().unwrap();
        let dataset = *p.datasets.keys().next().unwrap();
        let payload = *p.payloads.keys().next().unwrap();
        for sel in [
            Selection::Project,
            Selection::Model(model),
            Selection::Node(model, node),
            Selection::Dataset(dataset),
            Selection::Payload(payload),
        ] {
            let op = rename_op(&p, sel, "새 이름".into()).expect("이름 변경 op");
            let mut q = p.clone();
            nl_core::apply_ops(&mut q, &[op]);
            assert_ne!(q, p, "{sel:?} 이름이 바뀌지 않았다");
        }
        // 연결·실행 기록은 이름이 없다.
        assert!(rename_op(&p, Selection::None, "x".into()).is_none());
    }
}
