//! 실제 실행 파일을 돌려 보는 통합 테스트: 버전 출력, 번들 없음(코드 2), `.nlapp` 헤드리스 실행,
//! 실행 파일에 번들을 붙인 뒤 그 복사본 실행.

use nl_bundle::Bundle;
use nl_core::gui::{Widget, WidgetKind};
use nl_core::{BundleManifest, GuiLayout, PNode, PNodeKind, Pipeline, Project, Sink};
use std::process::Command;

const EXE: &str = env!("CARGO_BIN_EXE_nl-runtime");

fn demo_zip() -> Vec<u8> {
    let mut project = Project::new("스모크");
    let mut pipeline = Pipeline::new("스모크 파이프라인");
    pipeline.add_node(PNode::new(PNodeKind::Sink { sink: Sink::Log }, [0.0, 0.0]));
    let pid = pipeline.id;
    project.pipelines.insert(pid, pipeline);

    let mut gui = GuiLayout::default();
    gui.window.title = "스모크 앱".into();
    gui.add(Widget::new(WidgetKind::Label { text: "안녕".into() }, [10.0, 10.0, 100.0, 20.0]));
    project.gui = gui;

    let mut manifest = BundleManifest::new("스모크 앱", "9.9.9");
    manifest.entry_pipeline = Some(pid);
    manifest.autostart = true;
    let mut bundle = Bundle::new(manifest, project);
    bundle.weights.insert("w.safetensors".into(), vec![1, 2, 3, 4]);
    bundle.to_zip().unwrap()
}

#[test]
fn version_flag_prints_version() {
    let out = Command::new(EXE).arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("nl-runtime "), "{text}");
}

#[test]
fn missing_bundle_exits_with_two() {
    // 테스트가 쓰는 실행 파일에는 번들이 붙어 있지 않다.
    let out = Command::new(EXE).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("사용법"));
}

#[test]
fn unknown_option_exits_with_two() {
    let out = Command::new(EXE).arg("--nope").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn headless_runs_a_nlapp_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("스모크.nlapp");
    std::fs::write(&file, demo_zip()).unwrap();

    let out = Command::new(EXE).arg("--headless").arg("--device").arg("cpu").arg(&file).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "종료 코드 {:?}\n{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("스모크 앱 9.9.9"), "{stdout}");
    assert!(stdout.contains("스모크 파이프라인"), "{stdout}");
}

/// 실행 파일에 번들을 붙이면 인자 없이도 그 번들이 실행된다.
#[cfg(unix)]
#[test]
fn attached_bundle_runs_without_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let out_exe = dir.path().join("스모크앱");
    nl_bundle::attach(std::path::Path::new(EXE), &demo_zip(), &out_exe).unwrap();

    let out = Command::new(&out_exe).arg("--headless").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "종료 코드 {:?}\n{}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("스모크 앱 9.9.9"), "{stdout}");
}
