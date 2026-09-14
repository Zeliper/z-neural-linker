//! 빌드 뷰: 배포 설정 · 도구 상태 · 산출물.
//!
//! 산출물은 `nl-runtime` 실행 파일 뒤에 `.nlapp` 번들을 꼬리표와 함께 붙인 것이다
//! (`docs/ARCHITECTURE.md` "번들"). 사용자 PC 에 Rust 툴체인이 필요 없다.
//! 실제 빌드는 [`run_build`] 가 별도 스레드에서 돌고 UI 는 채널만 읽는다.

use super::{fmt_bytes, ViewAction, ViewCtx, COL_ERROR, COL_OK, COL_SELECT, COL_SURFACE, COL_WARN, COL_WEAK};
use crate::tools::{ToolKind, ToolState};
use eframe::egui::{self, RichText};
use nl_bundle::Bundle;
use nl_core::bundle::{BundledModel, DEFAULT_OUTPUT_DIR};
use nl_core::{BuildSpec, BuildTarget, BundleManifest, DevicePref, Op, Project, Severity};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

// ───────────────────────────── 빌드 작업 ─────────────────────────────

#[derive(Clone, Debug)]
pub struct BuildArtifact {
    pub target: BuildTarget,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub enum BuildEvent {
    Log(String),
    /// 0.0..=1.0.
    Progress(f32),
    Artifact(BuildArtifact),
    Done,
    Failed(String),
}

/// 스레드에 넘기는 빌드 입력. UI 상태를 참조하지 않도록 전부 소유한다.
pub struct BuildRequest {
    pub project: Project,
    pub spec: BuildSpec,
    /// 가중치 상대 경로의 기준(프로젝트 폴더).
    pub base_dir: PathBuf,
    pub out_dir: PathBuf,
    /// 대상별 런타임 실행 파일.
    pub runtimes: BTreeMap<BuildTarget, PathBuf>,
    /// 빌더 버전 (매니페스트 진단용).
    pub built_with: String,
    /// 앱 아이콘 PNG (이미 절대 경로로 풀린 것).
    pub icon: Option<PathBuf>,
    /// Windows 설치 프로그램에 적을 발행자.
    pub publisher: String,
}

/// 번들을 만들고 대상마다 런타임에 붙여 배포 아카이브까지 만든다.
pub fn run_build(req: BuildRequest, tx: &Sender<BuildEvent>) {
    let send = |e: BuildEvent| {
        let _ = tx.send(e);
    };
    if let Err(e) = build_inner(req, &send) {
        send(BuildEvent::Failed(e));
    } else {
        send(BuildEvent::Done);
    }
}

fn build_inner(req: BuildRequest, send: &dyn Fn(BuildEvent)) -> Result<(), String> {
    let BuildRequest {
        project,
        spec,
        base_dir,
        out_dir,
        runtimes,
        built_with,
        icon,
        publisher,
    } = req;

    // 1. 검증. 오류가 하나라도 있으면 만들지 않는다 — 깨진 앱을 배포하는 것이 더 나쁘다.
    send(BuildEvent::Log("검증 중…".into()));
    let issues = nl_core::validate(&project);
    let errors: Vec<String> = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .map(|i| i.message.clone())
        .collect();
    if !errors.is_empty() {
        return Err(format!("검증 오류 {}개: {}", errors.len(), errors.join(" / ")));
    }
    // 프로젝트 폴더 밖 파일은 번들에 넣지 않는다. 남에게 받은 프로젝트가 남의 파일을 실어 나르는
    // 통로가 되면 안 된다 (보안 리뷰 H9). 경로를 고치면 그대로 빌드된다.
    let blockers = crate::paths::build_blockers(&project);
    if !blockers.is_empty() {
        return Err(format!(
            "프로젝트 폴더 밖 경로 {}개: {}",
            blockers.len(),
            blockers.join(" / ")
        ));
    }
    if spec.targets.is_empty() {
        return Err("대상 플랫폼을 하나 이상 고르세요".into());
    }
    send(BuildEvent::Progress(0.1));

    // 2. 번들에 넣을 가중치를 읽고 프로젝트 사본의 경로를 번들 기준으로 다시 쓴다.
    let (bundle_project, models, weights) = collect_models(&project, &spec, &base_dir, send)?;
    send(BuildEvent::Progress(0.3));

    let manifest = BundleManifest {
        format: BundleManifest::FORMAT,
        app_name: spec.app_name.clone(),
        app_version: spec.app_version.clone(),
        built_with,
        entry_pipeline: spec.entry_pipeline,
        models,
        default_device: spec.default_device,
        autostart: spec.autostart,
        update_url: spec.update_url.clone().filter(|u| !u.trim().is_empty()),
        update_public_key: spec.update_public_key.clone().filter(|k| !k.trim().is_empty()),
        auto_update: spec.auto_update,
        arm_input: spec.arm_input,
        extra: Default::default(),
    };
    let mut bundle = Bundle::new(manifest, bundle_project);
    bundle.weights = weights;
    let zip = bundle
        .to_zip()
        .map_err(|e| format!("번들을 만들지 못했습니다: {e:#}"))?;
    send(BuildEvent::Log(format!(
        "번들 {} ({} 모델)",
        fmt_bytes(zip.len() as u64),
        bundle.weights.len()
    )));
    send(BuildEvent::Progress(0.45));

    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let staging = out_dir.join(".staging");
    std::fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

    let slug = nl_bundle::slugify(&spec.app_name);
    let icon = icon.as_deref();
    let mut artifacts: Vec<(BuildArtifact, nl_bundle::AssetKind)> = Vec::new();
    let step = 0.5 / spec.targets.len() as f32;
    for (i, target) in spec.targets.iter().enumerate() {
        let runtime = runtimes
            .get(target)
            .ok_or_else(|| format!("{} 런타임 바이너리가 없습니다 — 도구 상태를 확인하세요", target.label()))?;
        send(BuildEvent::Log(format!(
            "{} · 런타임 {}",
            target.label(),
            runtime.display()
        )));

        let exe_name = match target {
            BuildTarget::LinuxX64 => slug.clone(),
            BuildTarget::WindowsX64 => format!("{slug}.exe"),
        };
        let staged = staging.join(&exe_name);
        nl_bundle::attach(runtime, &zip, &staged).map_err(|e| format!("번들을 붙이지 못했습니다: {e:#}"))?;

        // Windows 는 설치 프로그램을 먼저 시도한다. 컴파일러가 없으면 zip 으로 떨어진다.
        let mut made: Option<(BuildArtifact, nl_bundle::AssetKind)> = None;
        if *target == BuildTarget::WindowsX64 {
            match nl_bundle::windows_installer(&staged, &spec.app_name, &spec.app_version, &publisher, &out_dir, icon) {
                Ok(Some(art)) => {
                    send(BuildEvent::Log(format!("설치 프로그램 → {}", art.path.display())));
                    made = Some((
                        BuildArtifact {
                            target: *target,
                            path: art.path,
                            size: art.size,
                            sha256: art.sha256,
                        },
                        nl_bundle::AssetKind::Installer,
                    ));
                }
                Ok(None) => send(BuildEvent::Log(
                    "Inno Setup 컴파일러가 없어 zip 으로 만듭니다 (.iss 스크립트는 남겨 뒀습니다)".into(),
                )),
                Err(e) => send(BuildEvent::Log(format!(
                    "설치 프로그램을 만들지 못해 zip 으로 갑니다: {e:#}"
                ))),
            }
        }
        let (artifact, kind) = match made {
            Some(x) => x,
            None => {
                let opts = nl_bundle::ArchiveOptions::new(
                    crate::tools::bundle_target(*target),
                    &staged,
                    &spec.app_name,
                    &spec.app_version,
                    &out_dir,
                )
                .icon(icon);
                let art = nl_bundle::archive_with(opts).map_err(|e| format!("아카이브를 만들지 못했습니다: {e:#}"))?;
                send(BuildEvent::Log(format!("{} → {}", target.label(), art.path.display())));
                let kind = match target {
                    BuildTarget::LinuxX64 => nl_bundle::AssetKind::Binary,
                    BuildTarget::WindowsX64 => nl_bundle::AssetKind::Installer,
                };
                (
                    BuildArtifact {
                        target: *target,
                        path: art.path,
                        size: art.size,
                        sha256: art.sha256,
                    },
                    kind,
                )
            }
        };
        artifacts.push((artifact, kind));
        send(BuildEvent::Progress(0.45 + step * (i + 1) as f32));
    }
    let _ = std::fs::remove_dir_all(&staging);

    // 3. 배포 매니페스트. 형식은 nl-update 가 읽는 것과 같아야 하므로 nl-bundle 에 맡긴다.
    let base_url = spec.update_base_url.clone().unwrap_or_default();
    let entries: Vec<(String, nl_bundle::Artifact, nl_bundle::AssetKind)> = artifacts
        .iter()
        .map(|(a, kind)| {
            (
                crate::tools::short_key(a.target).to_string(),
                nl_bundle::Artifact {
                    path: a.path.clone(),
                    sha256: a.sha256.clone(),
                    size: a.size,
                },
                *kind,
            )
        })
        .collect();
    match nl_bundle::write_manifest(&spec.app_version, "", &entries, &base_url, &out_dir) {
        Ok(p) => {
            send(BuildEvent::Log(format!("매니페스트 {}", super::tilde(&p))));
            if base_url.trim().is_empty() {
                send(BuildEvent::Log(
                    "자산 기본 주소가 비어 있어 latest.json 의 URL 이 파일 이름뿐입니다 — 올릴 때 앞에 주소를 붙이세요"
                        .into(),
                ));
            }
        }
        Err(e) => send(BuildEvent::Log(format!("매니페스트를 쓰지 못했습니다: {e:#}"))),
    }

    for (a, _) in artifacts {
        send(BuildEvent::Artifact(a));
    }
    send(BuildEvent::Progress(1.0));
    Ok(())
}

/// 번들에 담을 것: 경로를 다시 쓴 프로젝트 사본 · 매니페스트의 모델 목록 · 가중치 바이트.
type Collected = (Project, Vec<BundledModel>, BTreeMap<String, Vec<u8>>);

/// 번들에 들어갈 모델·가중치를 모으고, 프로젝트 사본의 `weights` 경로를 번들 기준으로 바꾼다.
fn collect_models(
    project: &Project,
    spec: &BuildSpec,
    base_dir: &Path,
    send: &dyn Fn(BuildEvent),
) -> Result<Collected, String> {
    let mut out = project.clone();
    let mut models = Vec::new();
    let mut weights = BTreeMap::new();
    for (id, m) in &project.models {
        let included = spec.models.contains(id);
        let Some(rel) = m.weights.as_deref().filter(|_| included) else {
            // 넣지 않는 모델은 가중치 경로를 지운다 — 런타임이 없는 파일을 찾지 않도록.
            if let Some(mm) = out.models.get_mut(id) {
                mm.weights = None;
            }
            if included {
                send(BuildEvent::Log(format!(
                    "모델 '{}' 은 가중치가 없어 건너뜁니다",
                    m.name
                )));
            }
            continue;
        };
        let path = base_dir.join(rel);
        let bytes = std::fs::read(&path).map_err(|e| {
            format!(
                "모델 '{}' 의 가중치를 읽지 못했습니다 ({}): {e}",
                m.name,
                path.display()
            )
        })?;
        let file = format!("{}.{}", id.short(), weights_ext(rel));
        send(BuildEvent::Log(format!(
            "모델 '{}' 가중치 {} → weights/{file}",
            m.name,
            fmt_bytes(bytes.len() as u64)
        )));
        weights.insert(file.clone(), bytes);
        models.push(BundledModel {
            model: *id,
            weights_file: file.clone(),
        });
        if let Some(mm) = out.models.get_mut(id) {
            mm.weights = Some(format!("weights/{file}"));
        }
    }
    if models.is_empty() {
        send(BuildEvent::Log(
            "번들에 담을 가중치가 없습니다 — 모델은 무작위 초기값으로 돕니다".into(),
        ));
    }
    Ok((out, models, weights))
}

fn weights_ext(rel: &str) -> String {
    Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("safetensors")
        .to_string()
}

// ───────────────────────────── 뷰 ─────────────────────────────

#[derive(Default)]
pub struct BuildViewState {
    pub log: Vec<String>,
    pub artifacts: Vec<BuildArtifact>,
    pub progress: Option<f32>,
    pub running: bool,
    pub error: Option<String>,
    /// 런타임 매니페스트 주소 (설정에 저장).
    pub manifest_url: String,
    /// 아이콘 미리보기 텍스처와 다시 읽어야 하는지.
    pub icon_preview: Option<egui::TextureHandle>,
    pub icon_dirty: bool,
    pub icon_error: Option<String>,
    /// 미리보기를 만든 경로 (바뀌면 다시 읽는다).
    pub icon_path: Option<PathBuf>,
}

impl BuildViewState {
    pub fn log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > 400 {
            self.log.remove(0);
        }
    }
}

/// 아이콘 PNG 를 읽어 미리보기 텍스처를 갱신한다. 경로가 그대로면 아무 일도 하지 않는다.
fn refresh_icon(ui: &egui::Ui, ctx: &ViewCtx, state: &mut BuildViewState, spec: &BuildSpec) {
    let resolved = spec.icon.as_deref().map(|rel| resolve_path(ctx.base_dir, rel));
    if !state.icon_dirty && state.icon_path == resolved {
        return;
    }
    state.icon_dirty = false;
    state.icon_path = resolved.clone();
    state.icon_error = None;
    state.icon_preview = None;
    let Some(path) = resolved else { return };
    match std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| image::load_from_memory(&b).map_err(|e| format!("PNG 를 읽지 못했습니다: {e}")))
    {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            state.icon_preview = Some(ui.ctx().load_texture("build-icon", color, egui::TextureOptions::LINEAR));
        }
        Err(e) => state.icon_error = Some(format!("{}: {e}", path.display())),
    }
}

/// 프로젝트 폴더 기준 상대 경로를 푼다.
pub fn resolve_path(base: Option<&Path>, rel: &str) -> PathBuf {
    let p = PathBuf::from(rel);
    match base {
        Some(b) if p.is_relative() => b.join(p),
        _ => p,
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &ViewCtx, state: &mut BuildViewState, tools: &[ToolState]) -> Vec<ViewAction> {
    let mut actions = Vec::new();
    let spec = ctx
        .project
        .settings
        .build
        .clone()
        .unwrap_or_else(|| BuildSpec::from_project(ctx.project));

    refresh_icon(ui, ctx, state, &spec);
    egui::ScrollArea::vertical().id_salt("build-scroll").show(ui, |ui| {
        spec_editor(ui, ctx, &spec, state, &mut actions);
        ui.add_space(8.0);
        tools_table(ui, tools, state, &mut actions);
        ui.add_space(8.0);
        run_section(ui, ctx, &spec, tools, state, &mut actions);
        ui.add_space(8.0);
        artifacts_table(ui, state, &mut actions);
        if !state.log.is_empty() {
            ui.add_space(8.0);
            log_section(ui, state);
        }
    });
    actions
}

fn spec_editor(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    spec: &BuildSpec,
    state: &BuildViewState,
    actions: &mut Vec<ViewAction>,
) {
    let mut next = spec.clone();
    let mut changed = false;
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.label(RichText::new("배포 설정").size(15.0).strong());
            ui.separator();
            egui::Grid::new("build-spec")
                .num_columns(2)
                .spacing([12.0, 5.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("앱 이름").color(COL_WEAK));
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut next.app_name).desired_width(260.0))
                        .changed();
                    ui.end_row();
                    ui.label(RichText::new("버전").color(COL_WEAK));
                    ui.vertical(|ui| {
                        changed |= ui
                            .add(egui::TextEdit::singleline(&mut next.app_version).desired_width(120.0))
                            .changed();
                        // 번들 만들기가 semver 를 강제한다. 빌드를 눌러 실패하기 전에 여기서 알려 준다.
                        if let Err(e) = semver::Version::parse(next.app_version.trim()) {
                            ui.label(
                                RichText::new(format!("✖ semver 가 아닙니다 ({e}) — 예: 0.1.0"))
                                    .color(COL_ERROR)
                                    .size(11.0),
                            );
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("대상").color(COL_WEAK));
                    ui.horizontal(|ui| {
                        for t in BuildTarget::ALL {
                            let mut on = next.targets.contains(&t);
                            if ui.checkbox(&mut on, t.label()).changed() {
                                if on {
                                    next.targets.push(t);
                                    next.targets.sort();
                                    next.targets.dedup();
                                } else {
                                    next.targets.retain(|x| *x != t);
                                }
                                changed = true;
                            }
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("진입 파이프라인").color(COL_WEAK));
                    let label = next
                        .entry_pipeline
                        .and_then(|p| ctx.project.pipelines.get(&p))
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| "(없음)".into());
                    egui::ComboBox::from_id_salt("build-entry")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(next.entry_pipeline.is_none(), "(없음)").clicked() {
                                next.entry_pipeline = None;
                                changed = true;
                            }
                            for (id, p) in &ctx.project.pipelines {
                                if ui.selectable_label(next.entry_pipeline == Some(*id), &p.name).clicked() {
                                    next.entry_pipeline = Some(*id);
                                    changed = true;
                                }
                            }
                        });
                    // 파이프라인이 없으면 콤보에 고를 것이 없다. 왜 비었는지 알려 준다.
                    if ctx.project.pipelines.is_empty() {
                        ui.label(
                            RichText::new("파이프라인이 없습니다 — 파이프라인 뷰에서 먼저 만드세요")
                                .color(COL_WARN)
                                .size(11.0),
                        );
                    }
                    ui.end_row();

                    ui.label(RichText::new("시작 동작").color(COL_WEAK));
                    changed |= ui
                        .checkbox(&mut next.autostart, "실행하면 파이프라인 자동 시작")
                        .on_hover_text("끄면 GUI 의 시작 버튼이나 제어 바로 시작합니다")
                        .changed();
                    ui.end_row();

                    ui.label(RichText::new("기본 장치").color(COL_WEAK));
                    let dev_label = ctx
                        .devices
                        .iter()
                        .find(|d| d.pref == next.default_device)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| next.default_device.label());
                    egui::ComboBox::from_id_salt("build-device")
                        .selected_text(dev_label)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(next.default_device == DevicePref::Auto, "자동")
                                .clicked()
                            {
                                next.default_device = DevicePref::Auto;
                                changed = true;
                            }
                            for d in ctx.devices {
                                if ui
                                    .selectable_label(next.default_device == d.pref, super::device_label(d))
                                    .clicked()
                                {
                                    next.default_device = d.pref;
                                    changed = true;
                                }
                            }
                        });
                    ui.end_row();

                    ui.label(RichText::new("산출물 폴더").color(COL_WEAK));
                    let mut dir = next
                        .output_dir
                        .clone()
                        .unwrap_or_else(|| DEFAULT_OUTPUT_DIR.to_string());
                    if ui
                        .add(egui::TextEdit::singleline(&mut dir).desired_width(260.0))
                        .changed()
                    {
                        next.output_dir = Some(dir);
                        changed = true;
                    }
                    ui.end_row();

                    // 왼쪽 칸은 비운다 — 체크박스 자신이 "입력 무장" 이라 두 번 쓰면 찾을 때 걸린다.
                    ui.label("");
                    ui.vertical(|ui| {
                        changed |= ui
                            .checkbox(&mut next.arm_input, "입력 무장")
                            .on_hover_text(
                                "켜면 배포한 앱이 마우스·키보드를 실제로 움직입니다. 꺼 두면 로그만 남깁니다",
                            )
                            .changed();
                        if next.arm_input {
                            ui.label(
                                RichText::new(
                                    "⚠ 받은 사람이 실행하자마자 커서와 키 입력이 움직입니다. 꼭 필요할 때만 켜세요.",
                                )
                                .color(COL_WARN)
                                .size(11.0),
                            );
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("아이콘").color(COL_WEAK));
                    ui.horizontal(|ui| {
                        if let Some(t) = &state.icon_preview {
                            ui.add(egui::Image::new(t).fit_to_exact_size(egui::Vec2::splat(40.0)));
                        }
                        match &next.icon {
                            Some(p) => {
                                ui.label(RichText::new(super::short_path(p)).size(11.0))
                                    .on_hover_text(p);
                            }
                            None => {
                                ui.label(RichText::new("(없음)").color(COL_WEAK).size(11.0));
                            }
                        }
                        if ui.small_button("PNG 고르기…").clicked() {
                            actions.push(ViewAction::PickIcon);
                        }
                        if next.icon.is_some() && ui.small_button("지우기").clicked() {
                            next.icon = None;
                            changed = true;
                        }
                    });
                    ui.end_row();
                });
            if let Some(e) = &state.icon_error {
                ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.0));
            }

            ui.add_space(6.0);
            ui.label(RichText::new("배포 앱 자동 업데이트").strong());
            egui::Grid::new("build-update")
                .num_columns(2)
                .spacing([12.0, 5.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("자산 기본 주소").color(COL_WEAK));
                    let mut base = next.update_base_url.clone().unwrap_or_default();
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut base)
                                .desired_width(320.0)
                                .hint_text("https://example.com/앱/0.1.0"),
                        )
                        .on_hover_text("latest.json 의 자산 주소는 여기에 파일 이름을 붙여 만듭니다")
                        .changed()
                    {
                        next.update_base_url = (!base.trim().is_empty()).then_some(base);
                        changed = true;
                    }
                    ui.end_row();

                    ui.label(RichText::new("매니페스트 주소").color(COL_WEAK));
                    let mut url = next.update_url.clone().unwrap_or_default();
                    ui.vertical(|ui| {
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut url)
                                    .desired_width(320.0)
                                    .hint_text("https://example.com/앱/latest.json"),
                            )
                            .on_hover_text("비우면 배포 앱의 자동 업데이트가 꺼집니다")
                            .changed()
                        {
                            next.update_url = (!url.trim().is_empty()).then_some(url.clone());
                            changed = true;
                        }
                        // 배포 앱은 https 가 아닌 주소를 거부한다. 여기서 먼저 알려 주지 않으면
                        // 받은 사람만 "업데이트 사용 불가" 를 보게 된다.
                        if let Some(u) = next.update_url.as_deref() {
                            if let Err(e) = nl_update::require_https(u) {
                                ui.label(RichText::new(format!("✖ {e:#}")).color(COL_ERROR).size(11.0));
                            }
                        }
                    });
                    ui.end_row();

                    ui.label(RichText::new("서명 공개키").color(COL_WEAK));
                    let mut key = next.update_public_key.clone().unwrap_or_default();
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut key)
                                .desired_width(320.0)
                                .hint_text("minisign 공개키 (RWQ…)"),
                        )
                        .on_hover_text("비우면 배포 앱이 매니페스트 서명을 검증하지 않습니다")
                        .changed()
                    {
                        next.update_public_key = (!key.trim().is_empty()).then_some(key);
                        changed = true;
                    }
                    ui.end_row();

                    ui.label(RichText::new("자동 내려받기").color(COL_WEAK));
                    changed |= ui
                        .checkbox(&mut next.auto_update, "새 버전을 알아서 내려받기 (적용은 사용자 확인)")
                        .changed();
                    ui.end_row();
                });

            ui.add_space(4.0);
            ui.label(RichText::new("포함할 모델").color(COL_WEAK).size(11.5));
            if ctx.project.models.is_empty() {
                ui.label(RichText::new("모델이 없습니다").color(COL_WEAK).size(11.0));
            }
            for (id, m) in &ctx.project.models {
                let has_weights = m.weights.is_some();
                let mut on = next.models.contains(id) && has_weights;
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(has_weights, |ui| {
                        if ui.checkbox(&mut on, &m.name).changed() {
                            if on {
                                next.models.push(*id);
                                next.models.dedup();
                            } else {
                                next.models.retain(|x| x != id);
                            }
                            changed = true;
                        }
                    });
                    if has_weights {
                        ui.label(
                            RichText::new(super::short_path(m.weights.as_deref().unwrap_or("")))
                                .color(COL_OK)
                                .size(11.0),
                        );
                    } else {
                        ui.label(
                            RichText::new("가중치 없음 — 먼저 학습하세요")
                                .color(COL_WARN)
                                .size(11.0),
                        );
                    }
                });
            }
        });
    if changed {
        let mut settings = ctx.project.settings.clone();
        settings.build = Some(next);
        actions.push(ViewAction::Edit(vec![Op::SetSettings { settings }]));
    }
}

fn tools_table(ui: &mut egui::Ui, tools: &[ToolState], state: &mut BuildViewState, actions: &mut Vec<ViewAction>) {
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("도구 상태").size(15.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("↻ 다시 검사").clicked() {
                        actions.push(ViewAction::RecheckTools);
                    }
                });
            });
            ui.separator();
            if tools.is_empty() {
                ui.label(RichText::new("대상을 고르면 필요한 도구를 검사합니다").color(COL_WEAK));
                return;
            }
            for t in tools {
                ui.horizontal(|ui| {
                    let (icon, color) = if t.ok() {
                        ("✔", COL_OK)
                    } else if t.kind.optional() {
                        ("⚠", COL_WARN)
                    } else {
                        ("✖", COL_ERROR)
                    };
                    ui.label(RichText::new(icon).color(color));
                    ui.label(t.kind.label());
                    ui.label(RichText::new(&t.note).color(COL_WEAK).size(11.0));
                    if !t.ok() {
                        match t.kind {
                            ToolKind::Runtime(target) => {
                                if ui
                                    .small_button("설치…")
                                    .on_hover_text("무엇을 어디서 받아 어디에 놓는지 먼저 보여 줍니다")
                                    .clicked()
                                {
                                    actions.push(ViewAction::ToolPlan(target));
                                }
                            }
                            ToolKind::InnoSetup => {
                                if ui
                                    .small_button("설치…")
                                    .on_hover_text("jrsoftware.org 에서 Inno Setup 6 을 내려받아 설치합니다")
                                    .clicked()
                                {
                                    actions.push(ViewAction::ToolPlanInno);
                                }
                                ui.label(RichText::new(ToolKind::InnoSetup.why()).color(COL_WEAK).size(11.0));
                            }
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("런타임 매니페스트").color(COL_WEAK).size(11.0));
                ui.add(
                    egui::TextEdit::singleline(&mut state.manifest_url)
                        .desired_width(f32::INFINITY)
                        .hint_text(crate::tools::DEFAULT_MANIFEST_URL),
                )
                .on_hover_text("NL_RUNTIME_MANIFEST 환경 변수가 있으면 그것이 우선합니다");
            });
            ui.label(
                RichText::new("Inno Setup 이 없으면 Windows 산출물은 zip 으로만 만듭니다.")
                    .color(COL_WEAK)
                    .size(11.0),
            );
        });
}

fn run_section(
    ui: &mut egui::Ui,
    ctx: &ViewCtx,
    spec: &BuildSpec,
    tools: &[ToolState],
    state: &mut BuildViewState,
    actions: &mut Vec<ViewAction>,
) {
    let issues = nl_core::validate(ctx.project);
    let errors = issues.iter().filter(|i| i.severity == Severity::Error).count();
    let missing: Vec<&ToolState> = tools.iter().filter(|t| !t.ok() && !t.kind.optional()).collect();
    let ready = errors == 0 && !spec.targets.is_empty() && missing.is_empty() && !state.running && ctx.saved();

    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled_ui(ready, |ui| {
                    if ui.button(RichText::new("🔨 빌드").color(COL_OK)).clicked() {
                        actions.push(ViewAction::BuildStart);
                    }
                });
                if state.running {
                    ui.add(
                        egui::ProgressBar::new(state.progress.unwrap_or(0.0))
                            .desired_width(200.0)
                            .show_percentage(),
                    );
                }
                if !ctx.saved() {
                    ui.label(
                        RichText::new("프로젝트를 먼저 저장하세요 (산출물 폴더 기준)")
                            .color(COL_WARN)
                            .size(11.5),
                    );
                } else if errors > 0 {
                    ui.label(
                        RichText::new(format!("검증 오류 {errors}개를 먼저 고치세요"))
                            .color(COL_ERROR)
                            .size(11.5),
                    );
                } else if spec.targets.is_empty() {
                    ui.label(RichText::new("대상 플랫폼을 고르세요").color(COL_WARN).size(11.5));
                } else if let Some(t) = missing.first() {
                    ui.label(
                        RichText::new(format!("{} 이(가) 없습니다", t.kind.label()))
                            .color(COL_ERROR)
                            .size(11.5),
                    );
                }
            });
            if let Some(e) = &state.error {
                ui.label(RichText::new(format!("✖ {e}")).color(COL_ERROR).size(11.5));
            }
            if let Some(dir) = out_dir_display(ctx, spec) {
                ui.label(
                    RichText::new(format!("산출물 폴더: {}", super::tilde(&dir)))
                        .color(COL_WEAK)
                        .size(11.0),
                );
            }
        });
}

/// 실제로 쓰일 산출물 폴더 (프로젝트가 저장돼 있을 때만).
pub fn out_dir_display(ctx: &ViewCtx, spec: &BuildSpec) -> Option<PathBuf> {
    let base = ctx.base_dir?;
    Some(resolve_out_dir(base, spec))
}

/// 상대 경로는 프로젝트 폴더 기준, 절대 경로는 그대로.
pub fn resolve_out_dir(base: &Path, spec: &BuildSpec) -> PathBuf {
    let raw = spec
        .output_dir
        .clone()
        .unwrap_or_else(|| DEFAULT_OUTPUT_DIR.to_string());
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

fn artifacts_table(ui: &mut egui::Ui, state: &mut BuildViewState, actions: &mut Vec<ViewAction>) {
    if state.artifacts.is_empty() {
        return;
    }
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.label(RichText::new("산출물").size(15.0).strong());
            ui.separator();
            egui::Grid::new("build-artifacts")
                .num_columns(5)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for h in ["대상", "파일", "크기", "sha256", ""] {
                        ui.label(RichText::new(h).color(COL_WEAK).size(11.0));
                    }
                    ui.end_row();
                    for a in &state.artifacts {
                        ui.label(a.target.label());
                        let name = a
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        ui.label(RichText::new(name).color(COL_SELECT))
                            .on_hover_text(a.path.display().to_string());
                        ui.label(fmt_bytes(a.size));
                        ui.label(RichText::new(&a.sha256[..a.sha256.len().min(12)]).size(11.0))
                            .on_hover_text(&a.sha256);
                        ui.horizontal(|ui| {
                            if ui.small_button("폴더 열기").clicked() {
                                if let Some(d) = a.path.parent() {
                                    actions.push(ViewAction::OpenPath(d.to_path_buf()));
                                }
                            }
                            let runnable_here = a.target == BuildTarget::LinuxX64
                                && crate::tools::host_target() == Some(BuildTarget::LinuxX64);
                            if runnable_here
                                && ui
                                    .small_button("지금 실행")
                                    .on_hover_text("아카이브를 풀어 앱을 띄웁니다")
                                    .clicked()
                            {
                                actions.push(ViewAction::RunArtifact(a.path.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
        });
}

fn log_section(ui: &mut egui::Ui, state: &BuildViewState) {
    egui::Frame::NONE
        .fill(COL_SURFACE)
        .inner_margin(10)
        .corner_radius(5)
        .show(ui, |ui| {
            ui.label(RichText::new("빌드 로그").size(13.0).strong());
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("build-log")
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &state.log {
                        ui.label(RichText::new(line).size(11.0));
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(t: BuildTarget, name: &str) -> (nl_bundle::Artifact, nl_bundle::AssetKind) {
        let kind = match t {
            BuildTarget::LinuxX64 => nl_bundle::AssetKind::Binary,
            BuildTarget::WindowsX64 => nl_bundle::AssetKind::Installer,
        };
        (
            nl_bundle::Artifact {
                path: PathBuf::from("/out").join(name),
                sha256: "a".repeat(64),
                size: 1234,
            },
            kind,
        )
    }

    /// 빌더가 만드는 매니페스트는 배포 앱(`nl_update`)이 그대로 읽는 형식이어야 한다.
    #[test]
    fn manifest_is_what_the_deployed_app_reads() {
        let (art, kind) = artifact(BuildTarget::LinuxX64, "앱-0.2.0-linux-x86_64.tar.gz");
        let entries = vec![(crate::tools::short_key(BuildTarget::LinuxX64).to_string(), art, kind)];
        let m = nl_bundle::build_manifest("0.2.0", "메모", &entries, "https://example.com/앱/0.2.0").unwrap();
        assert_eq!(m.version, "0.2.0");
        let asset = m.assets.get("linux-x86_64").expect("리눅스 자산");
        assert_eq!(asset.url, "https://example.com/앱/0.2.0/앱-0.2.0-linux-x86_64.tar.gz");
        assert_eq!(asset.kind, nl_bundle::AssetKind::Binary);
        assert_eq!(asset.size, 1234);
        // 배포 앱 쪽 판정도 같은 결론을 내야 한다.
        let current = semver::Version::new(0, 1, 0);
        assert!(m.newer_for(&current, "linux-x86_64").is_some());
        assert!(m.newer_for(&semver::Version::new(9, 0, 0), "linux-x86_64").is_none());
    }

    #[test]
    fn windows_artifacts_are_installers_in_the_manifest() {
        let (art, kind) = artifact(BuildTarget::WindowsX64, "앱-0.2.0-setup.exe");
        let entries = vec![(crate::tools::short_key(BuildTarget::WindowsX64).to_string(), art, kind)];
        let m = nl_bundle::build_manifest("0.2.0", "", &entries, "https://example.com").unwrap();
        assert_eq!(m.assets["windows-x86_64"].kind, nl_bundle::AssetKind::Installer);
    }

    #[test]
    fn output_dir_resolves_relative_against_the_project_folder() {
        let base = Path::new("/proj");
        let mut spec = BuildSpec::default();
        assert_eq!(resolve_out_dir(base, &spec), PathBuf::from("/proj/dist"));
        spec.output_dir = Some("배포".into());
        assert_eq!(resolve_out_dir(base, &spec), PathBuf::from("/proj/배포"));
        spec.output_dir = Some("/tmp/out".into());
        assert_eq!(resolve_out_dir(base, &spec), PathBuf::from("/tmp/out"));
    }

    #[test]
    fn build_spec_defaults_come_from_the_project() {
        let p = crate::sample::xor_project();
        let spec = BuildSpec::from_project(&p);
        assert_eq!(spec.app_name, p.name);
        assert!(spec.autostart);
        assert!(spec.targets.is_empty(), "대상은 사용자가 고른다");
        // 가중치가 없으면 포함 목록도 비어 있다.
        assert!(spec.models.is_empty());
    }

    #[test]
    fn collect_models_rewrites_weight_paths_into_the_bundle() {
        let dir = std::env::temp_dir().join(format!("nl-build-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("runs")).unwrap();
        std::fs::write(dir.join("runs/w.safetensors"), b"weights").unwrap();

        let mut p = crate::sample::xor_project();
        let mid = *p.models.keys().next().unwrap();
        p.models.get_mut(&mid).unwrap().weights = Some("runs/w.safetensors".into());
        let spec = BuildSpec {
            models: vec![mid],
            ..BuildSpec::from_project(&p)
        };

        let (out, models, weights) = collect_models(&p, &spec, &dir, &|_| {}).unwrap();
        assert_eq!(models.len(), 1);
        let file = models[0].weights_file.clone();
        assert_eq!(weights.get(&file).map(|b| b.as_slice()), Some(&b"weights"[..]));
        // 런타임이 찾는 경로 규약: project.json 의 weights 는 `weights/<file>`.
        assert_eq!(
            out.models[&mid].weights.as_deref(),
            Some(format!("weights/{file}").as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn excluded_models_lose_their_weight_path() {
        let mut p = crate::sample::xor_project();
        let mid = *p.models.keys().next().unwrap();
        p.models.get_mut(&mid).unwrap().weights = Some("runs/w.safetensors".into());
        let spec = BuildSpec {
            models: vec![],
            ..BuildSpec::from_project(&p)
        };
        let (out, models, weights) = collect_models(&p, &spec, Path::new("/nope"), &|_| {}).unwrap();
        assert!(models.is_empty() && weights.is_empty());
        assert_eq!(out.models[&mid].weights, None, "번들에 없는 가중치를 가리키면 안 된다");
    }
}
