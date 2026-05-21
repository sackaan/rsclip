//! RuneLite window and game canvas detection via Win32 APIs.
//!
//! RuneLite is a Java/Swing app. The game renders in a `java.awt.Canvas`
//! child window (class `SunAwtCanvas`). We capture the full top-level window
//! and compute a crop rectangle for the canvas so the saved clip contains
//! only the game viewport — no chat, side panels, or window chrome.

use anyhow::{Context, Result};
use log::{debug, warn};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClassNameW, GetClientRect, GetForegroundWindow,
    GetWindowTextLengthW, GetWindowTextW, IsWindow,
};

/// Sub-rectangle of a captured frame to retain (game canvas inside the
/// RuneLite window).
#[derive(Debug, Clone, Copy)]
pub struct CropRect {
    /// Left edge offset within the source frame, in pixels.
    pub x: u32,
    /// Top edge offset within the source frame, in pixels.
    pub y: u32,
    /// Crop width in pixels.
    pub width: u32,
    /// Crop height in pixels.
    pub height: u32,
}

/// A detected RuneLite client window.
#[derive(Debug, Clone)]
pub struct RuneLiteWindow {
    /// Top-level window handle.
    pub hwnd: isize,
    /// Handle of the `SunAwtCanvas` child window, if found.
    pub canvas_hwnd: Option<isize>,
    /// Effective width: canvas size when the canvas was detected, otherwise
    /// the full window client size.
    pub width: u32,
    /// Effective height (see [`width`](Self::width)).
    pub height: u32,
    /// Whether canvas detection succeeded (used to label clips and pick a
    /// crop rectangle).
    pub canvas_found: bool,
    /// Process id of the RuneLite instance — used by per-process audio capture.
    pub pid: u32,
}

impl RuneLiteWindow {
    /// HWND to feed to Windows Graphics Capture (always the top-level
    /// window — WGC cannot capture child windows directly).
    pub fn capture_hwnd(&self) -> isize {
        self.hwnd
    }

    /// Compute the canvas crop rectangle relative to the captured top-level
    /// frame, if a canvas was found.
    pub fn canvas_crop_rect(&self) -> Option<CropRect> {
        let canvas_hwnd = self.canvas_hwnd?;
        compute_crop_rect(self.hwnd, canvas_hwnd).ok()
    }
}

/// Enumerate every visible RuneLite top-level window, excluding our own
/// process.
pub fn find_all_runelite_windows() -> Vec<RuneLiteWindow> {
    let mut candidates: Vec<(HWND, String)> = Vec::new();

    unsafe {
        let candidates_ptr: *mut Vec<(HWND, String)> = &mut candidates;
        let _ = EnumWindows(Some(enum_windows_callback), LPARAM(candidates_ptr as isize));
    }

    let own_pid = unsafe { GetCurrentProcessId() };
    candidates.retain(|(hwnd, _)| {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(*hwnd, Some(&mut pid)) };
        pid != own_pid
    });

    let mut windows = Vec::new();
    for (hwnd, _title) in &candidates {
        if let Some(win) = build_window_info(*hwnd) {
            windows.push(win);
        }
    }
    windows
}

/// Return the handle of the currently focused RuneLite window, if any of
/// `windows` is foregrounded.
pub fn focused_runelite_hwnd(windows: &[RuneLiteWindow]) -> Option<isize> {
    let fg = unsafe { GetForegroundWindow() };
    let fg_isize = fg.0 as isize;
    windows.iter().find(|w| w.hwnd == fg_isize).map(|w| w.hwnd)
}

/// Whether the given HWND still refers to a live window.
pub fn is_window_alive(hwnd: isize) -> bool {
    let h = HWND(hwnd as *mut _);
    unsafe { IsWindow(h).as_bool() }
}

/// Client-area size of a window, in pixels.
pub fn get_window_size(hwnd: isize) -> Result<(u32, u32)> {
    let h = HWND(hwnd as *mut _);
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(h, &mut rect).context("GetClientRect failed")?;
    }
    let w = (rect.right - rect.left).max(0) as u32;
    let h = (rect.bottom - rect.top).max(0) as u32;
    Ok((w, h))
}

/// EnumWindows callback: collects Java/AWT windows titled "RuneLite".
unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let candidates = &mut *(lparam.0 as *mut Vec<(HWND, String)>);

    let len = GetWindowTextLengthW(hwnd);
    if len > 0 {
        let mut buf = vec![0u16; (len + 1) as usize];
        let read = GetWindowTextW(hwnd, &mut buf);
        if read > 0 {
            let title = String::from_utf16_lossy(&buf[..read as usize]);
            // Title must be exactly "RuneLite" or start with "RuneLite - ".
            if title == "RuneLite" || title.starts_with("RuneLite - ") {
                // Verify it's a Java/AWT window, not a browser or random app.
                let mut cls_buf = [0u16; 64];
                let cls_len = GetClassNameW(hwnd, &mut cls_buf);
                if cls_len > 0 {
                    let class = String::from_utf16_lossy(&cls_buf[..cls_len as usize]);
                    if class.starts_with("SunAwt") {
                        candidates.push((hwnd, title));
                    }
                }
            }
        }
    }

    BOOL(1)
}

fn build_window_info(hwnd: HWND) -> Option<RuneLiteWindow> {
    let parent_hwnd = hwnd.0 as isize;

    match get_window_size(parent_hwnd) {
        Ok((w, h)) if w == 0 || h == 0 => {
            warn!("RuneLite window has zero size, skipping");
            return None;
        }
        Err(e) => {
            warn!("Failed to query RuneLite window size: {}", e);
            return None;
        }
        _ => {}
    }

    let canvas = find_game_canvas(hwnd);

    let (canvas_hwnd, canvas_found, width, height) = match canvas {
        Some(child) => {
            let child_isize = child.0 as isize;
            match get_window_size(child_isize) {
                Ok((w, h)) if w > 0 && h > 0 => {
                    debug!("Found game canvas child window inside RuneLite");
                    (Some(child_isize), true, w, h)
                }
                _ => {
                    warn!("Canvas child found but has invalid size, using full window");
                    let (w, h) = get_window_size(parent_hwnd).unwrap_or((0, 0));
                    (None, false, w, h)
                }
            }
        }
        None => {
            debug!("Game canvas child not found, using full RuneLite window");
            let (w, h) = get_window_size(parent_hwnd).unwrap_or((0, 0));
            (None, false, w, h)
        }
    };

    if width == 0 || height == 0 {
        return None;
    }

    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };

    Some(RuneLiteWindow {
        hwnd: parent_hwnd,
        canvas_hwnd,
        width,
        height,
        canvas_found,
        pid,
    })
}

/// Find the largest "SunAwtCanvas" child window (the game viewport).
fn find_game_canvas(parent: HWND) -> Option<HWND> {
    let mut children: Vec<HWND> = Vec::new();

    unsafe {
        let children_ptr: *mut Vec<HWND> = &mut children;
        let _ = EnumChildWindows(
            parent,
            Some(enum_child_callback),
            LPARAM(children_ptr as isize),
        );
    }

    let mut best_canvas: Option<(HWND, u32)> = None;

    for child in &children {
        let class = get_window_class(*child);
        if class.contains("Canvas") {
            if let Ok((w, h)) = get_window_size(child.0 as isize) {
                let area = w * h;
                if area > best_canvas.map_or(0, |(_, a)| a) {
                    best_canvas = Some((*child, area));
                }
            }
        }
    }

    if let Some((hwnd, _)) = best_canvas {
        let class = get_window_class(hwnd);
        if let Ok((w, h)) = get_window_size(hwnd.0 as isize) {
            debug!("Game canvas: class=\"{}\" size={}x{}", class, w, h);
        }
        Some(hwnd)
    } else {
        None
    }
}

unsafe extern "system" fn enum_child_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let children = &mut *(lparam.0 as *mut Vec<HWND>);
    children.push(hwnd);
    BOOL(1)
}

fn get_window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Compute the canvas position within the captured window image.
fn compute_crop_rect(parent_hwnd: isize, canvas_hwnd: isize) -> Result<CropRect> {
    let parent_h = HWND(parent_hwnd as *mut _);
    let canvas_h = HWND(canvas_hwnd as *mut _);

    let mut parent_rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            parent_h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut parent_rect as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .context("DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS) failed")?;
    }

    let mut canvas_origin = POINT { x: 0, y: 0 };
    unsafe {
        if !ClientToScreen(canvas_h, &mut canvas_origin).as_bool() {
            anyhow::bail!("ClientToScreen(canvas) failed");
        }
    }

    let mut canvas_client = RECT::default();
    unsafe {
        GetClientRect(canvas_h, &mut canvas_client).context("GetClientRect(canvas) failed")?;
    }

    let canvas_w = (canvas_client.right - canvas_client.left).max(0) as u32;
    let canvas_h_px = (canvas_client.bottom - canvas_client.top).max(0) as u32;

    let offset_x = (canvas_origin.x - parent_rect.left).max(0) as u32;
    let offset_y = (canvas_origin.y - parent_rect.top).max(0) as u32;

    debug!(
        "Canvas crop: offset=({},{}) size={}x{} parent_origin=({},{})",
        offset_x, offset_y, canvas_w, canvas_h_px, parent_rect.left, parent_rect.top
    );

    Ok(CropRect {
        x: offset_x,
        y: offset_y,
        width: canvas_w,
        height: canvas_h_px,
    })
}
