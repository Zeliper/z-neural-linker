//! Windows 콘솔 붙잡기.
//!
//! 배포 런타임은 GUI 앱이라 릴리스 빌드에서 `windows_subsystem = "windows"` 로 만든다. 그래야 창만 뜨고
//! 검은 콘솔 창이 같이 뜨지 않는다. 그 대가로 **표준 출력이 갈 곳이 없어진다** — 명령 프롬프트에서
//! `nl-runtime.exe --version` 을 쳐도 아무것도 안 보인다.
//!
//! `AttachConsole(ATTACH_PARENT_PROCESS)` 는 **이미 있는** 부모 콘솔에 붙는다. 없으면 실패할 뿐 새로 만들지 않는다.
//! 그래서 탐색기에서 더블클릭한 경우에는 조용히 지나가고, 명령 프롬프트에서 부른 경우에만 출력이 보인다.
//!
//! 붙은 뒤에는 표준 핸들을 직접 `CONOUT$`/`CONIN$` 로 열어 꽂아 준다. 프로세스가 콘솔 없이 시작했으면
//! 표준 핸들이 비어 있고, `AttachConsole` 만으로는 채워지지 않는 경우가 있다.
//!
//! Windows 가 아니면 전부 아무 일도 하지 않는다.

/// 부모 콘솔에 붙어 `println!`·`eprintln!` 이 보이게 한다. 콘솔이 없으면 아무 일도 하지 않는다.
///
/// 창을 띄우는 경로에서는 부를 필요가 없다. `--version`·`--help`·`--headless` 처럼
/// 터미널에서 부른 것이 분명한 경우와 오류 메시지에만 쓴다.
pub fn attach_parent() {
    imp::attach_parent();
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::RawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    pub fn attach_parent() {
        // SAFETY: 인자 없는 커널 호출이고 실패해도 값으로만 알려 준다.
        let attached = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } != 0;
        if !attached {
            // 부모에 콘솔이 없다 (탐색기에서 띄웠거나 이미 붙어 있다). 그대로 둔다.
            return;
        }
        redirect(STD_OUTPUT_HANDLE, "CONOUT$", true);
        redirect(STD_ERROR_HANDLE, "CONOUT$", true);
        redirect(STD_INPUT_HANDLE, "CONIN$", false);
    }

    /// 표준 핸들이 비어 있으면 콘솔 장치를 열어 꽂는다. 이미 리다이렉트돼 있으면(`> out.txt`) 건드리지 않는다.
    fn redirect(which: STD_HANDLE, device: &str, write: bool) {
        // SAFETY: 커널 호출. 돌려받은 핸들은 아래에서 유효성을 확인한다.
        let current = unsafe { GetStdHandle(which) };
        if !is_empty(current) {
            return;
        }
        let name: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let access = if write { FILE_GENERIC_WRITE } else { FILE_GENERIC_READ };
        // SAFETY: `name` 은 NUL 로 끝나는 UTF-16 이고 보안 서술자는 기본값(null)을 쓴다.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return;
        }
        // SAFETY: 바로 위에서 연 유효한 핸들이다.
        let ok = unsafe { SetStdHandle(which, handle) } != 0;
        if !ok {
            // SAFETY: 우리가 연 핸들이고 아무 곳에도 넘기지 않았다.
            unsafe { CloseHandle(handle) };
        }
    }

    fn is_empty(h: RawHandle) -> bool {
        h.is_null() || h == INVALID_HANDLE_VALUE
    }
}

#[cfg(not(windows))]
mod imp {
    /// Linux·macOS 는 GUI 앱도 표준 출력을 그대로 쓴다.
    pub fn attach_parent() {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn attaching_is_harmless_without_a_console() {
        // Windows 가 아니면 빈 함수, Windows 면 부모 콘솔이 없을 때 조용히 지나간다.
        super::attach_parent();
        super::attach_parent();
    }
}
