//! Counter-Strike 2 window detection via Win32 APIs.
//!
//! Unlike RuneLite, CS2 (Source 2) renders the whole game into a single
//! top-level window — there is no child canvas and no chrome to crop out, so
//! we capture the full client area. We match the window by its exact title
//! (`Counter-Strike 2`) and confirm the owning process is `cs2.exe` to avoid
//! grabbing an unrelated window (browser tab, chat, etc.).
//!
//! CS2 must run in **borderless / fullscreen-windowed** mode. Windows Graphics
//! Capture cannot capture a window held in *exclusive* fullscreen; that case
//! is detected later when the capture item reports a zero size.

use log::{debug, warn};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::platform::runelite::get_window_size;

/// Exact CS2 window title (Source 2 sets this on the main game window).
const CS2_WINDOW_TITLE: &str = "Counter-Strike 2";
/// Executable name we expect to own the CS2 window.
const CS2_EXE_NAME: &str = "cs2.exe";

/// A detected Counter-Strike 2 game window.
#[derive(Debug, Clone)]
pub struct Cs2Window {
    /// Top-level window handle (fed directly to Windows Graphics Capture).
    pub hwnd: isize,
    /// Client-area width in pixels.
    pub width: u32,
    /// Client-area height in pixels.
    pub height: u32,
    /// Process id of the `cs2.exe` instance — used by per-process audio capture.
    pub pid: u32,
}

impl Cs2Window {
    /// HWND to feed to Windows Graphics Capture (CS2 is a single top-level
    /// window, so this is just the window itself — no canvas crop).
    pub fn capture_hwnd(&self) -> isize {
        self.hwnd
    }
}

/// Find the running CS2 game window, if any. Returns the first visible
/// top-level window titled "Counter-Strike 2" whose process is `cs2.exe`.
pub fn find_cs2_window() -> Option<Cs2Window> {
    let mut candidates: Vec<HWND> = Vec::new();
    unsafe {
        let ptr: *mut Vec<HWND> = &mut candidates;
        let _ = EnumWindows(Some(enum_windows_callback), LPARAM(ptr as isize));
    }

    for hwnd in candidates {
        let hwnd_isize = hwnd.0 as isize;

        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            continue;
        }

        // Confirm the owning process is actually cs2.exe — the title match
        // alone could collide with a stream overlay or browser tab.
        if !process_is_cs2(pid) {
            debug!("Window titled CS2 but process is not cs2.exe (pid {pid}), skipping");
            continue;
        }

        match get_window_size(hwnd_isize) {
            Ok((w, h)) if w > 0 && h > 0 => {
                debug!("Found CS2 window: {w}x{h} (pid {pid})");
                return Some(Cs2Window {
                    hwnd: hwnd_isize,
                    width: w,
                    height: h,
                    pid,
                });
            }
            _ => {
                warn!("CS2 window found but has zero/invalid size — exclusive fullscreen?");
                continue;
            }
        }
    }

    None
}

/// EnumWindows callback: collect visible top-level windows titled exactly
/// "Counter-Strike 2".
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let candidates = &mut *(lparam.0 as *mut Vec<HWND>);

    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let len = GetWindowTextLengthW(hwnd);
    if len > 0 {
        let mut buf = vec![0u16; (len + 1) as usize];
        let read = GetWindowTextW(hwnd, &mut buf);
        if read > 0 {
            let title = String::from_utf16_lossy(&buf[..read as usize]);
            if title == CS2_WINDOW_TITLE {
                candidates.push(hwnd);
            }
        }
    }

    BOOL(1)
}

/// Whether the given process id resolves to an executable named `cs2.exe`.
fn process_is_cs2(pid: u32) -> bool {
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => return false,
    };

    let mut buf = vec![0u16; MAX_PATH as usize];
    let mut size = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }

    if ok.is_err() || size == 0 {
        return false;
    }

    let path = String::from_utf16_lossy(&buf[..size as usize]);
    path.rsplit(['\\', '/'])
        .next()
        .map(|name| name.eq_ignore_ascii_case(CS2_EXE_NAME))
        .unwrap_or(false)
}
