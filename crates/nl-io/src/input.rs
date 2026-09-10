//! 마우스·키보드 시뮬레이션 (enigo). **안전장치**: [`InputSim`] 은 `armed` 가 켜진 동안만 실제 입력을 보낸다.
//!
//! `armed == false` 면 액션을 `log::info!` 로 남기기만 한다. 빌더에서 파이프라인을 시험 실행할 때
//! 실수로 마우스가 움직이는 일을 막기 위한 기본값이다.
//!
//! enigo 연결은 **지연 초기화**한다. 헤드리스 환경(입력 백엔드 없음)에서도 `InputSim::new()` 는 성공하고,
//! 실제로 무장한 채 액션을 보낼 때만 초기화 실패가 오류로 올라온다.
//!
//! ## 배포판 요구 사항 (Linux)
//! 실행에 필요한 시스템 라이브러리는 `libxkbcommon.so.0` 하나뿐이고, 이것은 모든 데스크톱 Linux 에 기본 포함된다
//! (Fedora `libxkbcommon`, Debian/Ubuntu `libxkbcommon0`). **개발 패키지는 필요 없다** — 빌드할 때만 필요한
//! `libxkbcommon.so` 링크 이름은 `crates/nl-io/build.rs` 가 `OUT_DIR` 안에 심볼릭 링크로 만들어 준다.

use anyhow::{anyhow, bail, Context};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use nl_core::pipeline::{InputAction, MouseButton};

pub struct InputSim {
    /// 켜져 있을 때만 실제 입력을 보낸다.
    pub armed: bool,
    enigo: Option<Enigo>,
}

impl InputSim {
    /// 항상 성공한다(`armed = false`, enigo 미초기화). 반환형은 호출부 계약 유지를 위해 `Result` 로 둔다.
    #[allow(clippy::unnecessary_wraps)]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { armed: false, enigo: None })
    }

    /// 무장 상태를 지정해 만든다.
    #[allow(clippy::unnecessary_wraps)]
    pub fn with_armed(armed: bool) -> anyhow::Result<Self> {
        Ok(Self { armed, enigo: None })
    }

    /// enigo 연결을 확보한다. 실패하면 원인을 그대로 올린다.
    fn enigo(&mut self) -> anyhow::Result<&mut Enigo> {
        if self.enigo.is_none() {
            let e = Enigo::new(&Settings::default()).map_err(|e| anyhow!("{e}")).context(
                "입력 시뮬레이션 백엔드를 열지 못했다 (Linux 면 X11 DISPLAY 또는 Wayland 가상 입력 프로토콜이 필요하다)",
            )?;
            self.enigo = Some(e);
        }
        Ok(self.enigo.as_mut().expect("바로 위에서 채웠다"))
    }

    /// `armed` 가 꺼져 있으면 로그만 남기고 아무것도 하지 않는다.
    pub fn perform(&mut self, action: &InputAction) -> anyhow::Result<()> {
        if !self.armed {
            log::info!("입력 시뮬레이션(비무장, 실행 안 함): {}", describe(action));
            return Ok(());
        }
        self.exec(action)
    }

    fn exec(&mut self, action: &InputAction) -> anyhow::Result<()> {
        match action {
            InputAction::None => Ok(()),
            InputAction::MoveTo { x, y } => {
                let e = self.enigo()?;
                e.move_mouse(*x, *y, Coordinate::Abs).map_err(|e| anyhow!("커서 이동 실패: {e}"))
            }
            InputAction::MoveBy { dx, dy } => {
                let e = self.enigo()?;
                e.move_mouse(*dx, *dy, Coordinate::Rel).map_err(|e| anyhow!("커서 상대 이동 실패: {e}"))
            }
            InputAction::Click { button } => {
                let b = to_button(*button);
                let e = self.enigo()?;
                e.button(b, Direction::Click).map_err(|e| anyhow!("클릭 실패: {e}"))
            }
            InputAction::KeyTap { key } => self.key(key, Direction::Click),
            InputAction::KeyDown { key } => self.key(key, Direction::Press),
            InputAction::KeyUp { key } => self.key(key, Direction::Release),
            InputAction::TypeText { text } => {
                if text.is_empty() {
                    return Ok(());
                }
                let e = self.enigo()?;
                e.text(text).map_err(|e| anyhow!("텍스트 입력 실패: {e}"))
            }
            InputAction::Scroll { dx, dy } => {
                let e = self.enigo()?;
                if *dx != 0 {
                    e.scroll(*dx, Axis::Horizontal).map_err(|e| anyhow!("가로 스크롤 실패: {e}"))?;
                }
                if *dy != 0 {
                    e.scroll(*dy, Axis::Vertical).map_err(|e| anyhow!("세로 스크롤 실패: {e}"))?;
                }
                Ok(())
            }
            InputAction::Sequence { steps } => {
                for s in steps {
                    self.exec(s)?;
                }
                Ok(())
            }
        }
    }

    fn key(&mut self, name: &str, dir: Direction) -> anyhow::Result<()> {
        let k = parse_key(name)?;
        let e = self.enigo()?;
        e.key(k, dir).map_err(|e| anyhow!("키 '{name}' {dir:?} 실패: {e}"))
    }
}

fn to_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// 로그·오류 메시지용 한 줄 설명.
pub fn describe(action: &InputAction) -> String {
    match action {
        InputAction::None => "없음".into(),
        InputAction::MoveTo { x, y } => format!("커서 이동 → ({x}, {y})"),
        InputAction::MoveBy { dx, dy } => format!("커서 상대 이동 ({dx:+}, {dy:+})"),
        InputAction::Click { button } => format!("{button:?} 클릭"),
        InputAction::KeyTap { key } => format!("키 '{key}' 누름/뗌"),
        InputAction::KeyDown { key } => format!("키 '{key}' 누름"),
        InputAction::KeyUp { key } => format!("키 '{key}' 뗌"),
        InputAction::TypeText { text } => format!("텍스트 입력 {text:?}"),
        InputAction::Scroll { dx, dy } => format!("스크롤 ({dx:+}, {dy:+})"),
        InputAction::Sequence { steps } => {
            format!("순서 {}개: [{}]", steps.len(), steps.iter().map(describe).collect::<Vec<_>>().join(", "))
        }
    }
}

/// 키 이름 문자열 → [`enigo::Key`].
///
/// - 이름은 대소문자·앞뒤 공백을 무시한다. `"ctrl"`, `"Control"`, `" CTRL "` 모두 같다.
/// - 한 글자면 그 문자를 그대로 입력하는 `Key::Unicode` 가 된다 (`"a"`, `"1"`, `"가"`).
/// - 그 밖에는 오류. 오류 메시지에 들어온 이름을 담는다.
pub fn parse_key(name: &str) -> anyhow::Result<Key> {
    let raw = name.trim();
    if raw.is_empty() {
        bail!("키 이름이 비어 있다");
    }
    let n = raw.to_ascii_lowercase();
    let k = match n.as_str() {
        "enter" | "return" | "\n" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "space" | "spacebar" => Key::Space,
        "tab" => Key::Tab,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "up" | "uparrow" | "arrowup" => Key::UpArrow,
        "down" | "downarrow" | "arrowdown" => Key::DownArrow,
        "left" | "leftarrow" | "arrowleft" => Key::LeftArrow,
        "right" | "rightarrow" | "arrowright" => Key::RightArrow,
        "ctrl" | "control" => Key::Control,
        "lctrl" | "leftctrl" => Key::LControl,
        "rctrl" | "rightctrl" => Key::RControl,
        "shift" => Key::Shift,
        "lshift" | "leftshift" => Key::LShift,
        "rshift" | "rightshift" => Key::RShift,
        "alt" | "option" => Key::Alt,
        "meta" | "super" | "win" | "windows" | "cmd" | "command" => Key::Meta,
        "capslock" | "caps" => Key::CapsLock,
        "help" => Key::Help,
        "add" => Key::Add,
        "subtract" => Key::Subtract,
        "multiply" => Key::Multiply,
        "divide" => Key::Divide,
        "decimal" => Key::Decimal,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "f13" => Key::F13,
        "f14" => Key::F14,
        "f15" => Key::F15,
        "f16" => Key::F16,
        "f17" => Key::F17,
        "f18" => Key::F18,
        "f19" => Key::F19,
        "f20" => Key::F20,
        "numpad0" | "kp0" => Key::Numpad0,
        "numpad1" | "kp1" => Key::Numpad1,
        "numpad2" | "kp2" => Key::Numpad2,
        "numpad3" | "kp3" => Key::Numpad3,
        "numpad4" | "kp4" => Key::Numpad4,
        "numpad5" | "kp5" => Key::Numpad5,
        "numpad6" | "kp6" => Key::Numpad6,
        "numpad7" | "kp7" => Key::Numpad7,
        "numpad8" | "kp8" => Key::Numpad8,
        "numpad9" | "kp9" => Key::Numpad9,
        _ => {
            // 한 글자면 그 문자를 그대로 (원본 대소문자 유지: 'A' 와 'a' 는 다르다).
            let mut it = raw.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => bail!("모르는 키 이름: {raw:?}"),
            }
        }
    };
    Ok(k)
}

/// 일회성 실행 (시험 버튼). 이 경로는 사용자가 명시적으로 누른 것이므로 무장 상태로 실행한다.
pub fn perform(action: &InputAction) -> anyhow::Result<()> {
    let mut sim = InputSim::with_armed(true)?;
    sim.perform(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_map_to_enigo_keys() {
        assert_eq!(parse_key("enter").unwrap(), Key::Return);
        assert_eq!(parse_key("Return").unwrap(), Key::Return);
        assert_eq!(parse_key("esc").unwrap(), Key::Escape);
        assert_eq!(parse_key("ESCAPE").unwrap(), Key::Escape);
        assert_eq!(parse_key("space").unwrap(), Key::Space);
        assert_eq!(parse_key("tab").unwrap(), Key::Tab);
        assert_eq!(parse_key("ctrl").unwrap(), Key::Control);
        assert_eq!(parse_key("control").unwrap(), Key::Control);
        assert_eq!(parse_key("shift").unwrap(), Key::Shift);
        assert_eq!(parse_key("alt").unwrap(), Key::Alt);
        assert_eq!(parse_key("f1").unwrap(), Key::F1);
        assert_eq!(parse_key("F12").unwrap(), Key::F12);
        assert_eq!(parse_key("pgup").unwrap(), Key::PageUp);
        assert_eq!(parse_key("numpad7").unwrap(), Key::Numpad7);
        assert_eq!(parse_key("super").unwrap(), Key::Meta);
    }

    #[test]
    fn whitespace_is_trimmed() {
        assert_eq!(parse_key("  enter  ").unwrap(), Key::Return);
    }

    #[test]
    fn single_characters_become_unicode() {
        assert_eq!(parse_key("a").unwrap(), Key::Unicode('a'));
        assert_eq!(parse_key("A").unwrap(), Key::Unicode('A'));
        assert_eq!(parse_key("1").unwrap(), Key::Unicode('1'));
        assert_eq!(parse_key("가").unwrap(), Key::Unicode('가'));
        assert_eq!(parse_key("+").unwrap(), Key::Unicode('+'));
    }

    #[test]
    fn unknown_names_are_errors() {
        assert!(parse_key("").is_err());
        assert!(parse_key("   ").is_err());
        assert!(parse_key("hyperspace").is_err());
        let e = parse_key("nope").unwrap_err().to_string();
        assert!(e.contains("nope"), "오류 메시지에 키 이름이 없다: {e}");
    }

    /// 비무장 상태에서는 enigo 를 아예 열지 않으므로 헤드리스에서도 성공해야 한다.
    #[test]
    fn disarmed_sim_never_touches_the_backend() {
        let mut sim = InputSim::new().unwrap();
        assert!(!sim.armed);
        sim.perform(&InputAction::MoveTo { x: 10, y: 10 }).unwrap();
        sim.perform(&InputAction::TypeText { text: "무장 안 됨".into() }).unwrap();
        sim.perform(&InputAction::Sequence {
            steps: vec![InputAction::KeyTap { key: "enter".into() }, InputAction::Click { button: MouseButton::Left }],
        })
        .unwrap();
        assert!(sim.enigo.is_none(), "비무장인데 백엔드가 열렸다");
    }

    #[test]
    fn describe_covers_every_action() {
        let all = [
            InputAction::None,
            InputAction::MoveTo { x: 1, y: 2 },
            InputAction::MoveBy { dx: -1, dy: 2 },
            InputAction::Click { button: MouseButton::Middle },
            InputAction::KeyTap { key: "a".into() },
            InputAction::KeyDown { key: "ctrl".into() },
            InputAction::KeyUp { key: "ctrl".into() },
            InputAction::TypeText { text: "hi".into() },
            InputAction::Scroll { dx: 0, dy: 3 },
            InputAction::Sequence { steps: vec![InputAction::None] },
        ];
        for a in &all {
            assert!(!describe(a).is_empty());
        }
    }
}
