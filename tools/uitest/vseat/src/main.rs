//! 헤드리스 sway(입력 장치 없음)의 시트에 가상 포인터와 가상 키보드를 만들어 **붙들고 있는다**.
//! 장치가 하나도 없으면 시트에 pointer/keyboard capability 가 없어 앱이 `wl_pointer`/`wl_keyboard` 를
//! 받지 못하고, `swaymsg seat cursor …` 로 넣는 클릭이 아무 데도 닿지 않는다. wtype 은 실행 중에만
//! 키보드를 만들었다 없애므로 포인터 쪽이 특히 비어 있다. 이 프로세스가 살아 있는 동안 두 장치가 유지된다.
//!
//! 사용: `WAYLAND_DISPLAY=wayland-N vseat` (uitest.sh 가 start 때 띄운다).

use std::io::Write;
use std::os::fd::AsFd;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    pointer_mgr: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" => st.seat = Some(registry.bind(name, version.min(7), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => {
                    st.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    st.keyboard_mgr = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(State: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

/// 가상 키보드는 키맵을 줘야 "완성"된다. us 배열 최소 키맵.
const KEYMAP: &str = r#"xkb_keymap {
    xkb_keycodes { include "evdev+aliases(qwerty)" };
    xkb_types { include "complete" };
    xkb_compat { include "complete" };
    xkb_symbols { include "pc+us+inet(evdev)" };
};
"#;

fn main() {
    let conn = Connection::connect_to_env().expect("WAYLAND_DISPLAY 에 연결할 수 없습니다");
    let display = conn.display();
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st).expect("registry roundtrip");

    let seat = st.seat.clone().expect("wl_seat 없음");
    let pm = st
        .pointer_mgr
        .clone()
        .expect("zwlr_virtual_pointer_manager_v1 없음 (wlroots 컴포지터가 아닌가?)");
    let km = st.keyboard_mgr.clone().expect("zwp_virtual_keyboard_manager_v1 없음");

    let _pointer = pm.create_virtual_pointer(Some(&seat), &qh, ());
    let keyboard = km.create_virtual_keyboard(&seat, &qh, ());
    // 키맵은 파일 디스크립터로 넘긴다(memfd).
    let mut f = tempfile_memfd();
    f.write_all(KEYMAP.as_bytes()).unwrap();
    f.write_all(b"\0").unwrap();
    keyboard.keymap(1 /* xkb_v1 */, f.as_fd(), KEYMAP.len() as u32 + 1);
    queue.roundtrip(&mut st).expect("device roundtrip");
    println!("vseat: 가상 포인터·키보드 붙임 — 종료하면 사라진다");

    loop {
        queue.blocking_dispatch(&mut st).expect("dispatch");
    }
}

fn tempfile_memfd() -> std::fs::File {
    // memfd 가 없는 환경을 대비해 임시 파일로도 충분하다(읽기만 하면 된다).
    let path = std::env::temp_dir().join(format!("vseat-keymap-{}", std::process::id()));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    let _ = std::fs::remove_file(&path);
    f
}
