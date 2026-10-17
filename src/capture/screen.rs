//! Screen capture via the Windows Graphics Capture API.
//!
//! [`start_capture`] takes a window handle and forwards captured frames to
//! the encoder over a [`crossbeam_channel::Sender`]. The `FrameArrived`
//! callback runs on a WGC-owned thread; we throttle to the output frame
//! rate, copy the GPU texture to a staging texture, map it, and write the
//! crop region into a packed buffer of exactly the encoder's frame size. If
//! the channel is full the frame is counted as dropped rather than blocking
//! the capture thread.
//!
//! The output size is fixed for the lifetime of the capture so the encoder
//! never has to restart. When the source region changes size (window
//! resized, RuneLite sidebar toggled, client switched) it is scaled to fit
//! and letterboxed instead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{debug, info};
use parking_lot::Mutex;

use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};

use crossbeam_channel::{Receiver, Sender};

use crate::encoding::encoder::RawFrame;
use crate::platform::runelite::CropRect;

const BYTES_PER_PIXEL: usize = 4;

/// Parameters for a capture session.
pub struct CaptureConfig {
    /// Width of every frame sent downstream. Must be even and non-zero.
    pub out_width: u32,
    /// Height of every frame sent downstream. Must be even and non-zero.
    pub out_height: u32,
    /// Initial sub-rectangle of the window to keep (the game canvas), or
    /// `None` for the whole window. Can be changed later with
    /// [`CaptureHandle::set_crop`].
    pub crop: Option<CropRect>,
    /// Upper bound on frames processed per second. Frames arriving faster
    /// are skipped *before* the GPU readback. `0` disables the limit.
    pub max_fps: u32,
}

/// Handle to a running capture session. Drop or call `stop()` to terminate.
pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    session: Arc<Mutex<Option<GraphicsCaptureSession>>>,
    frame_pool: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
    crop: Arc<Mutex<Option<CropRect>>>,
}

impl CaptureHandle {
    /// Signal the capture loop to stop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        // Close session and frame pool
        if let Some(session) = self.session.lock().take() {
            let _ = session.Close();
        }
        if let Some(pool) = self.frame_pool.lock().take() {
            let _ = pool.Close();
        }
        info!("Capture stopped");
    }

    /// Replace the crop rectangle used for subsequent frames (e.g. after the
    /// game canvas moved or was resized).
    pub fn set_crop(&self, crop: Option<CropRect>) {
        *self.crop.lock() = crop;
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mutable per-session state touched only by the `FrameArrived` handler.
/// Kept behind one mutex so handler invocations are serialized: the D3D11
/// immediate context and the staging texture are not thread-safe.
struct FrameState {
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    pool_size: SizeInt32,
    throttle: FrameThrottle,
}

/// Start capturing the given window.
///
/// `recycle`, when given, supplies previously used frame buffers so the
/// steady state does not allocate a new frame-sized `Vec` per frame.
pub fn start_capture(
    hwnd: isize,
    frame_tx: Sender<RawFrame>,
    cfg: CaptureConfig,
    dropped_frames: Arc<AtomicU64>,
    recycle: Option<Receiver<Vec<u8>>>,
) -> Result<CaptureHandle> {
    anyhow::ensure!(
        cfg.out_width > 0
            && cfg.out_height > 0
            && cfg.out_width.is_multiple_of(2)
            && cfg.out_height.is_multiple_of(2),
        "invalid capture output size {}x{}",
        cfg.out_width,
        cfg.out_height
    );
    let running = Arc::new(AtomicBool::new(true));

    // --- Create D3D11 device ---
    let (d3d_device, d3d_context) = create_d3d11_device()?;
    let device = winrt_device(&d3d_device)?;

    // --- Create capture item from HWND ---
    let item = create_capture_item(hwnd)?;
    let size = item.Size()?;
    info!(
        "Capture item created: {}x{} -> output {}x{}",
        size.Width, size.Height, cfg.out_width, cfg.out_height
    );

    // --- Frame pool (free-threaded, BGRA8, 2 buffers) ---
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;

    // --- Session ---
    let session = frame_pool.CreateCaptureSession(&item)?;

    // Try to hide the yellow capture border (Windows 11+, silently ignored on older builds).
    let _ = session.SetIsBorderRequired(false);

    // --- Shared state for the event handler ---
    let crop = Arc::new(Mutex::new(cfg.crop));
    let state = Mutex::new(FrameState {
        staging: None,
        pool_size: size,
        throttle: FrameThrottle::new(cfg.max_fps),
    });
    let out_size = (cfg.out_width, cfg.out_height);
    let running_clone = running.clone();
    let crop_clone = crop.clone();

    // --- FrameArrived handler ---
    frame_pool.FrameArrived(&TypedEventHandler::new(
        move |pool: &Option<Direct3D11CaptureFramePool>, _| {
            if !running_clone.load(Ordering::SeqCst) {
                return Ok(());
            }
            let Some(pool) = pool else {
                return Ok(());
            };
            let Ok(frame) = pool.TryGetNextFrame() else {
                return Ok(());
            };
            let mut st = state.lock();

            // Follow window resizes: without this the pool keeps the
            // original size and new content is clipped.
            let content = frame.ContentSize().unwrap_or(st.pool_size);
            if content.Width > 0
                && content.Height > 0
                && (content.Width != st.pool_size.Width || content.Height != st.pool_size.Height)
            {
                debug!(
                    "Capture content resized to {}x{}",
                    content.Width, content.Height
                );
                st.pool_size = content;
                // The WinRT device wrapper is not Send, so rebuild it here
                // from the D3D11 device; resizes are rare.
                if let Ok(device) = winrt_device(&d3d_device) {
                    let _ = pool.Recreate(
                        &device,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        content,
                    );
                }
            }

            if !st.throttle.accept(Instant::now()) {
                return Ok(());
            }

            let crop = *crop_clone.lock();
            if let Err(e) = process_frame(
                &frame,
                content,
                &d3d_device,
                &d3d_context,
                &mut st.staging,
                &frame_tx,
                crop.as_ref(),
                out_size,
                recycle.as_ref(),
                &dropped_frames,
            ) {
                debug!("Frame processing error: {}", e);
            }
            Ok(())
        },
    ))?;

    // --- Start ---
    session.StartCapture()?;
    info!("Capture started for HWND {:?}", hwnd);

    Ok(CaptureHandle {
        running,
        session: Arc::new(Mutex::new(Some(session))),
        frame_pool: Arc::new(Mutex::new(Some(frame_pool))),
        crop,
    })
}

/// Create a D3D11 device suitable for Graphics Capture.
fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice failed")?;
    }
    let device = device.context("D3D11CreateDevice returned a null device")?;
    let context = context.context("D3D11CreateDevice returned a null context")?;
    Ok((device, context))
}

/// Wrap a D3D11 device as the WinRT `IDirect3DDevice` WGC expects.
fn winrt_device(d3d_device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi_device: IDXGIDevice = d3d_device.cast()?;
    let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
    Ok(inspectable.cast()?)
}

/// Create a GraphicsCaptureItem from a window handle.
fn create_capture_item(hwnd: isize) -> Result<GraphicsCaptureItem> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
    Ok(item)
}

/// Map a captured texture, write the crop region into an output-sized
/// buffer, and send it to the encoder. The staging texture is cached and
/// reused across frames.
#[allow(clippy::too_many_arguments)]
fn process_frame(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    content: SizeInt32,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    staging_cache: &mut Option<(ID3D11Texture2D, u32, u32)>,
    frame_tx: &Sender<RawFrame>,
    crop: Option<&CropRect>,
    (out_w, out_h): (u32, u32),
    recycle: Option<&Receiver<Vec<u8>>>,
    dropped_frames: &AtomicU64,
) -> Result<()> {
    // This path does a GPU→CPU readback, so `CaptureToSubmit` here measures
    // the map/copy cost that the CS2 GPU path avoids entirely.
    let captured = crate::metrics::Timestamp::now();
    crate::metrics::note_capture(captured);

    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };

    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };

    let width = desc.Width;
    let height = desc.Height;

    if width == 0 || height == 0 {
        return Ok(());
    }

    let staging = match staging_cache {
        Some((tex, w, h)) if *w == width && *h == height => tex.clone(),
        _ => {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: desc.SampleDesc,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: Default::default(),
            };
            let mut new_staging: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut new_staging))? };
            let new_staging = new_staging.context("CreateTexture2D returned None")?;
            *staging_cache = Some((new_staging.clone(), width, height));
            new_staging
        }
    };

    unsafe {
        context.CopyResource(&staging, &texture);
    }

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .context("Failed to map staging texture")?;
    }

    // Before the pool is recreated after a shrink, the surface is larger
    // than the live content; never read past the content.
    let src_w = width.min(content.Width.max(1) as u32);
    let src_h = height.min(content.Height.max(1) as u32);

    let out_len = out_w as usize * out_h as usize * BYTES_PER_PIXEL;
    let mut out = match recycle.and_then(|r| r.try_recv().ok()) {
        Some(v) if v.len() == out_len => v,
        _ => vec![0u8; out_len],
    };

    let row_pitch = mapped.RowPitch as usize;
    // SAFETY: a successful Map of a `height`-row staging texture yields
    // `RowPitch` bytes per row, the last row holding at least
    // `width * 4` bytes. The slice is only used before `Unmap` below.
    let src = unsafe {
        std::slice::from_raw_parts(
            mapped.pData as *const u8,
            row_pitch * (height as usize - 1) + width as usize * BYTES_PER_PIXEL,
        )
    };
    let region = resolve_region(crop, src_w, src_h);
    blit_fit(src, row_pitch, region, &mut out, out_w, out_h);

    unsafe {
        context.Unmap(&staging, 0);
    }

    if frame_tx
        .try_send(RawFrame {
            data: out,
            width: out_w,
            height: out_h,
        })
        .is_err()
    {
        dropped_frames.fetch_add(1, Ordering::Relaxed);
        crate::metrics::mark_dropped();
    } else {
        crate::metrics::record_since(crate::metrics::Stage::CaptureToSubmit, captured);
        // Counted at handoff rather than at buffer insert: this path buffers
        // opaque MPEG-TS chunks, so there is no per-frame insert downstream.
        crate::metrics::mark_frame();
    }

    Ok(())
}

/// Clamp `crop` to a `src_w`×`src_h` surface. A missing or degenerate crop
/// selects the whole surface.
fn resolve_region(crop: Option<&CropRect>, src_w: u32, src_h: u32) -> CropRect {
    let full = CropRect {
        x: 0,
        y: 0,
        width: src_w,
        height: src_h,
    };
    let Some(cr) = crop else {
        return full;
    };
    let x = cr.x.min(src_w);
    let y = cr.y.min(src_h);
    let w = cr.width.min(src_w - x);
    let h = cr.height.min(src_h - y);
    if w == 0 || h == 0 {
        full
    } else {
        CropRect {
            x,
            y,
            width: w,
            height: h,
        }
    }
}

/// Write `region` of the BGRA image `src` (row stride `src_pitch`) into the
/// packed `dst_w`×`dst_h` BGRA buffer `dst`.
///
/// When the region matches the output size (ignoring the odd pixel dropped
/// to keep encoder dimensions even) rows are copied straight across.
/// Otherwise the region is scaled to fit with nearest-neighbour sampling,
/// preserving aspect ratio, and centred on black.
fn blit_fit(
    src: &[u8],
    src_pitch: usize,
    region: CropRect,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;
    let (rx, ry) = (region.x as usize, region.y as usize);
    let (rw, rh) = (region.width as usize, region.height as usize);
    let dst_row = dst_w * BYTES_PER_PIXEL;
    debug_assert_eq!(dst.len(), dst_row * dst_h);

    if rw & !1 == dst_w && rh & !1 == dst_h {
        for (row, out) in dst.chunks_exact_mut(dst_row).enumerate() {
            let start = (ry + row) * src_pitch + rx * BYTES_PER_PIXEL;
            out.copy_from_slice(&src[start..start + dst_row]);
        }
        return;
    }

    // Fit inside the output, keeping aspect ratio.
    let (fit_w, fit_h) = if rw * dst_h <= rh * dst_w {
        (((rw * dst_h) / rh).clamp(1, dst_w), dst_h)
    } else {
        (dst_w, ((rh * dst_w) / rw).clamp(1, dst_h))
    };
    let off_x = (dst_w - fit_w) / 2;
    let off_y = (dst_h - fit_h) / 2;

    for px in dst.chunks_exact_mut(BYTES_PER_PIXEL) {
        px.copy_from_slice(&[0, 0, 0, 255]);
    }
    for dy in 0..fit_h {
        let sy = ry + dy * rh / fit_h;
        let src_row = sy * src_pitch;
        let out_row = (off_y + dy) * dst_row;
        for dx in 0..fit_w {
            let sx = rx + dx * rw / fit_w;
            let s = src_row + sx * BYTES_PER_PIXEL;
            let d = out_row + (off_x + dx) * BYTES_PER_PIXEL;
            dst[d..d + BYTES_PER_PIXEL].copy_from_slice(&src[s..s + BYTES_PER_PIXEL]);
        }
    }
}

/// Rate limiter applied before the expensive GPU readback.
///
/// Keeps the long-run accepted rate at `fps` while tolerating up to half an
/// interval of arrival jitter, so a source running at exactly `fps` is not
/// thinned out by timer noise.
struct FrameThrottle {
    interval: Option<Duration>,
    next_due: Option<Instant>,
}

impl FrameThrottle {
    fn new(fps: u32) -> Self {
        Self {
            interval: (fps > 0).then(|| Duration::from_secs(1) / fps),
            next_due: None,
        }
    }

    fn accept(&mut self, now: Instant) -> bool {
        let Some(interval) = self.interval else {
            return true;
        };
        let half = interval / 2;
        let due = match self.next_due {
            Some(due) if now + half < due => return false,
            Some(due) => due,
            None => now,
        };
        // Keep the schedule's phase while arrivals are merely jittery; after
        // a real gap restart from `now` so missed slots don't allow a burst.
        let base = if due + half < now { now } else { due };
        self.next_due = Some(base + interval);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `w`×`h` BGRA image with row padding where each pixel encodes
    /// its own coordinates: B = x, G = y.
    fn coord_image(w: usize, h: usize, pitch: usize) -> Vec<u8> {
        let mut img = vec![0u8; pitch * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * pitch + x * 4;
                img[i] = x as u8;
                img[i + 1] = y as u8;
                img[i + 3] = 255;
            }
        }
        img
    }

    fn px(buf: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn odd_crop_is_packed_without_shear() {
        // Regression: a 7-px-wide region used to be written with a stride
        // that did not match its buffer, skewing every row.
        let pitch = 64;
        let src = coord_image(16, 12, pitch);
        let region = CropRect {
            x: 3,
            y: 2,
            width: 7,
            height: 5,
        };
        let mut dst = vec![0u8; 6 * 4 * 4];
        blit_fit(&src, pitch, region, &mut dst, 6, 4);
        for y in 0..4 {
            for x in 0..6 {
                assert_eq!(px(&dst, 6, x, y), [3 + x as u8, 2 + y as u8, 0, 255]);
            }
        }
    }

    #[test]
    fn smaller_region_is_letterboxed_not_stretched() {
        let pitch = 32;
        let src = coord_image(8, 8, pitch);
        let region = CropRect {
            x: 0,
            y: 0,
            width: 4,
            height: 8,
        };
        // 4x8 into 8x8: scale 1, centred with 2-px bars left and right.
        let mut dst = vec![7u8; 8 * 8 * 4];
        blit_fit(&src, pitch, region, &mut dst, 8, 8);
        assert_eq!(px(&dst, 8, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 8, 7, 7), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 8, 2, 0), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 8, 2, 5), [0, 5, 0, 255]);
        assert_eq!(px(&dst, 8, 5, 3), [3, 3, 0, 255]);
    }

    #[test]
    fn larger_region_is_downscaled_to_fit() {
        let pitch = 16 * 4;
        let src = coord_image(16, 16, pitch);
        let region = CropRect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        };
        let mut dst = vec![0u8; 4 * 4 * 4];
        blit_fit(&src, pitch, region, &mut dst, 4, 4);
        assert_eq!(px(&dst, 4, 0, 0), [0, 0, 0, 255]);
        assert_eq!(px(&dst, 4, 1, 2), [4, 8, 0, 255]);
        assert_eq!(px(&dst, 4, 3, 3), [12, 12, 0, 255]);
    }

    #[test]
    fn resolve_region_clamps_and_falls_back() {
        let r = resolve_region(None, 100, 50);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 100, 50));

        let crop = CropRect {
            x: 90,
            y: 10,
            width: 40,
            height: 100,
        };
        let r = resolve_region(Some(&crop), 100, 50);
        assert_eq!((r.x, r.y, r.width, r.height), (90, 10, 10, 40));

        let off = CropRect {
            x: 200,
            y: 0,
            width: 10,
            height: 10,
        };
        let r = resolve_region(Some(&off), 100, 50);
        assert_eq!((r.width, r.height), (100, 50));
    }

    fn accepted(fps: u32, source_hz: f64, secs: f64) -> usize {
        let mut t = FrameThrottle::new(fps);
        let start = Instant::now();
        let n = (source_hz * secs) as usize;
        (0..n)
            .filter(|&i| t.accept(start + Duration::from_secs_f64(i as f64 / source_hz)))
            .count()
    }

    #[test]
    fn throttle_limits_fast_source_to_target_rate() {
        let n = accepted(60, 240.0, 10.0);
        assert!((595..=605).contains(&n), "accepted {n} of 2400");
        let n = accepted(60, 144.0, 10.0);
        assert!((595..=605).contains(&n), "accepted {n} of 1440");
    }

    #[test]
    fn throttle_passes_slower_and_equal_sources() {
        assert_eq!(accepted(60, 50.0, 10.0), 500);
        assert_eq!(accepted(60, 60.0, 10.0), 600);
    }

    #[test]
    fn throttle_tolerates_jitter_at_matching_rate() {
        let mut t = FrameThrottle::new(60);
        let start = Instant::now();
        let ok = (0..600)
            .filter(|&i| {
                let jitter = if i % 2 == 0 { 3.0 } else { -3.0 };
                let ms = i as f64 * 1000.0 / 60.0 + jitter;
                t.accept(start + Duration::from_secs_f64(ms.max(0.0) / 1000.0))
            })
            .count();
        assert_eq!(ok, 600);
    }

    #[test]
    fn throttle_does_not_burst_after_idle_gap() {
        let mut t = FrameThrottle::new(60);
        let start = Instant::now();
        assert!(t.accept(start));
        let later = start + Duration::from_secs(5);
        assert!(t.accept(later));
        // A 240 Hz burst right after the gap must not all pass.
        let burst = (1..=4)
            .filter(|&i| t.accept(later + Duration::from_micros(4167 * i)))
            .count();
        assert!(burst <= 1, "burst of {burst}");
    }

    #[test]
    fn throttle_zero_means_unlimited() {
        assert_eq!(accepted(0, 240.0, 1.0), 240);
    }
}
