//! 샘플 프로젝트. 정의는 `nl_core::sample` 에 있고 여기서는 앱이 쓰는 이름만 다시 내보낸다.
//!
//! 예전에는 같은 프로젝트를 이 크레이트에서 한 번 더 만들었다. 배포 런타임과 CLI 도 같은 샘플을
//! 열 수 있어야 해서 정의를 nl-core 로 옮겼고, 여기 남은 것은 이름 맞춤뿐이다.
//! core 에서 CNN 샘플의 이름은 `quadrants_cnn_project` 다.

pub use nl_core::sample::{new_project, quadrants_cnn_project as cnn_project, xor_project, SampleFactory, SAMPLES};

#[cfg(test)]
mod tests {
    use super::*;
    use nl_core::gui::{Binding, BuiltinAction};
    use nl_core::pipeline::{PNodeKind, Sink};

    /// 샘플의 링크가 캔버스 규칙(`pcanvas::link_check`)도 지키는지.
    ///
    /// core 의 `Pipeline::add_link` 만 통과하면 파일은 열리지만, 캔버스가 같은 연결을 거부하면
    /// 사용자가 지울 수는 있어도 다시 만들 수는 없는 모양이 된다.
    #[test]
    fn sample_links_pass_the_canvas_rules() {
        for (name, make) in SAMPLES {
            let p = make();
            for pl in p.pipelines.values() {
                for l in pl.links.values() {
                    let mut copy = pl.clone();
                    copy.links.clear();
                    assert!(
                        crate::pcanvas::link_check(&copy, l.from, l.to, None).is_none(),
                        "{name}: 샘플 링크가 캔버스 규칙을 어긴다: {l:?}"
                    );
                }
            }
        }
    }

    /// GUI 위젯의 바인딩이 실제로 존재하는 대상을 가리키는지 — 끊긴 바인딩은 화면에서만 드러난다.
    #[test]
    fn sample_gui_bindings_point_at_real_things() {
        let p = xor_project();
        let actions: Vec<_> = p
            .gui
            .widgets
            .values()
            .filter_map(|w| match &w.binding {
                Some(Binding::Action { action }) => Some(*action),
                _ => None,
            })
            .collect();
        assert!(actions.contains(&BuiltinAction::StartPipeline));
        assert!(actions.contains(&BuiltinAction::StopPipeline));

        for pl in p.pipelines.values() {
            for n in pl.nodes.values() {
                if let PNodeKind::Sink { sink: Sink::GuiWidget { widget } } = &n.kind {
                    assert!(p.gui.widgets.contains_key(widget), "싱크가 없는 위젯을 가리킨다: {widget:?}");
                }
            }
        }
    }

    /// 앱 메뉴가 쓰는 목록 — 이름이 비어 있으면 메뉴에 빈 줄이 생긴다.
    #[test]
    fn sample_menu_entries_have_names() {
        assert_eq!(SAMPLES.len(), 2);
        for (name, make) in SAMPLES {
            assert!(!name.trim().is_empty());
            assert!(!make().models.is_empty(), "{name}: 모델이 없다");
        }
    }
}
