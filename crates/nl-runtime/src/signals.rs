//! Ctrl+C 처리. 헤드리스 실행이 임시 폴더를 지우고 끝날 수 있도록 신호를 플래그로 받는다.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

#[cfg(unix)]
pub fn install() {
    extern "C" fn on_signal(_sig: libc::c_int) {
        // 신호 처리기 안에서는 원자적 저장만 한다.
        INTERRUPTED.store(true, Ordering::SeqCst);
    }
    // SAFETY: 처리기는 원자적 저장만 하므로 비동기 신호 안전하다.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

/// Windows 에서는 기본 동작(Ctrl+C 로 프로세스 종료)을 그대로 둔다.
/// 이때 임시 폴더가 남을 수 있어 `WorkDir::create` 가 시작할 때 같은 pid 의 폴더를 비운다.
#[cfg(not(unix))]
pub fn install() {}
