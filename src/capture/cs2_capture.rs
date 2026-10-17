//! CS2 GPU zero-copy capture orchestrator.
//!
//! Ties Windows Graphics Capture to the in-process [`GpuH264Encoder`]. One
//! D3D11 device is shared between WGC and the encoder so a captured frame
//! never crosses the GPU/CPU boundary as raw pixels:
//!
//! - The WGC `FrameArrived` callback copies the live BGRA surface into a small
//!   GPU-resident pool texture (a cheap on-GPU copy, no readback) and forwards
//!   it to the encoder thread.
//! - The encoder thread converts BGRA→NV12 and feeds the hardware H.264 MFT,
//!   pushing each compressed access unit into the [`SharedAuBuffer`].
//!
//! If the encoder falls behind, frames are dropped at the channel rather than
//! blocking the capture callback — keeping the game's frame times flat.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use crossbeam_channel::{Sender, TrySendError};
use log::{debug, info, warn};
use parking_lot::Mutex;

use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};

use crate::encoding::gpu_buffer::SharedAuBuffer;
use crate::encoding::gpu_encoder::{CapturedFrame, GpuH264Encoder};
use crate::metrics::{self, Timestamp};

/// Number of BGRA pool textures cycled between capture and encoder threads.
const BGRA_POOL_SIZE: usize = 4;
/// Bounded queue depth between capture and encoder. Small so we drop rather
/// than let the encoder lag the game.
const FRAME_QUEUE_DEPTH: usize = 2;
// Queued textures + the one the encoder is converting must never include
// the next slot `forward_frame` copies into.
const _: () = assert!(BGRA_POOL_SIZE > FRAME_QUEUE_DEPTH + 1);

/// Handle to a running CS2 capture+encode session. Drop or call `stop()`.
pub struct Cs2CaptureHandle {
    running: Arc<AtomicBool>,
    session: Arc<Mutex<Option<windows::Graphics::Capture::GraphicsCaptureSession>>>,
    frame_pool: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
    encoder_thread: Option<std::thread::JoinHandle<()>>,
    frame_tx: Option<Sender<CapturedFrame>>,
}

impl Cs2CaptureHandle {
    /// Stop capture and wind down the encoder thread.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(session) = self.session.lock().take() {
            let _ = session.Close();
        }
        if let Some(pool) = self.frame_pool.lock().take() {
            let _ = pool.Close();
        }
        // Dropping the sender unblocks the encoder thread's recv loop.
        self.frame_tx.take();
        if let Some(handle) = self.encoder_thread.take() {
            let _ = handle.join();
        }
        info!("CS2 capture stopped");
    }
}

impl Drop for Cs2CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start capturing + GPU-encoding the CS2 window `hwnd` at `fps`, pushing
/// access units into `buffer`.
pub fn start_cs2_capture(
    hwnd: isize,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    buffer: SharedAuBuffer,
    dropped_frames: Arc<AtomicU64>,
) -> Result<Cs2CaptureHandle> {
    let running = Arc::new(AtomicBool::new(true));

    let (device, context) = create_d3d11_device()?;
    let dxgi_device: IDXGIDevice = device.cast()?;
    let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
    let wgc_device: IDirect3DDevice = inspectable.cast()?;

    let item = create_capture_item(hwnd)?;
    let size = item.Size()?;
    if size.Width == 0 || size.Height == 0 {
        anyhow::bail!("CS2 capture item has zero size — is CS2 in exclusive fullscreen? Use borderless / fullscreen-windowed.");
    }
    info!("CS2 capture item: {}x{}", size.Width, size.Height);

    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &wgc_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = frame_pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsBorderRequired(false);

    // BGRA scratch pool the capture thread copies live frames into.
    let bgra_pool = create_bgra_pool(&device, width, height)?;
    let bgra_pool = Arc::new(bgra_pool);

    let (frame_tx, frame_rx) = crossbeam_channel::bounded::<CapturedFrame>(FRAME_QUEUE_DEPTH);

    // --- Encoder thread ---
    let encoder_thread = {
        let device = device.clone();
        let context = context.clone();
        let buffer = buffer.clone();
        let running = running.clone();
        let dropped = dropped_frames.clone();
        std::thread::Builder::new()
            .name("cs2-encoder".into())
            .spawn(move || {
                // Media Foundation requires an MTA apartment on this thread.
                let _com = ComGuard::new();

                let mut encoder =
                    match GpuH264Encoder::new(device, context, width, height, fps, bitrate_kbps) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!("CS2 GPU encoder init failed: {e:#}");
                            return;
                        }
                    };

                // Drive the encoder's event loop until capture stops.
                encoder.run(frame_rx, buffer, running);

                info!(
                    "CS2 capture dropped {} frames (encoder couldn't keep up with capture rate)",
                    dropped.load(Ordering::Relaxed)
                );
            })
            .context("spawning CS2 encoder thread")?
    };

    // --- FrameArrived handler ---
    let running_cb = running.clone();
    let context_cb = context.clone();
    let pool_cb = bgra_pool.clone();
    let tx_cb = frame_tx.clone();
    let dropped_cb = dropped_frames.clone();
    // Next pool slot. Held across copy + send so overlapping callbacks
    // can't pick the same texture.
    let next_slot = Arc::new(Mutex::new(0usize));

    frame_pool.FrameArrived(&TypedEventHandler::new(
        move |pool: &Option<Direct3D11CaptureFramePool>, _| {
            if !running_cb.load(Ordering::SeqCst) {
                return Ok(());
            }
            let Some(pool) = pool.as_ref() else {
                return Ok(());
            };
            let Ok(frame) = pool.TryGetNextFrame() else {
                return Ok(());
            };

            if let Err(e) = forward_frame(
                &frame,
                &context_cb,
                &pool_cb,
                &next_slot,
                &tx_cb,
                &dropped_cb,
            ) {
                debug!("CS2 frame forward error: {e}");
            }
            Ok(())
        },
    ))?;

    session.StartCapture()?;
    info!("CS2 capture started for HWND {:X}", hwnd);

    Ok(Cs2CaptureHandle {
        running,
        session: Arc::new(Mutex::new(Some(session))),
        frame_pool: Arc::new(Mutex::new(Some(frame_pool))),
        encoder_thread: Some(encoder_thread),
        frame_tx: Some(frame_tx),
    })
}

/// Copy the live WGC surface into a pool texture and forward it to the encoder.
fn forward_frame(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    context: &ID3D11DeviceContext,
    bgra_pool: &[ID3D11Texture2D],
    next_slot: &Mutex<usize>,
    tx: &Sender<CapturedFrame>,
    dropped: &AtomicU64,
) -> Result<()> {
    // Stamped at the very top of the callback so the GPU copy below is inside
    // the measured window. Free (zero-sized) in non-instrumented builds.
    let captured = Timestamp::now();
    metrics::note_capture(captured);

    let surface = frame.Surface()?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    let src: ID3D11Texture2D = unsafe { access.GetInterface()? };

    // Real capture timestamp (QPC, 100-ns units) — drives correct playback
    // speed downstream. Only the differences between frames matter.
    let pts = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);

    // Up to FRAME_QUEUE_DEPTH textures sit in the queue and one is being
    // converted by the encoder. The slot only advances when a frame is
    // actually handed over, so the next slot is never one of those; and a
    // full queue drops the frame *before* copying over a pool texture.
    let mut slot = next_slot.lock();
    if tx.is_full() {
        dropped.fetch_add(1, Ordering::Relaxed);
        metrics::mark_dropped();
        return Ok(());
    }
    let dst = bgra_pool[*slot % bgra_pool.len()].clone();

    // GPU→GPU copy; the source surface is only valid during this callback.
    unsafe { context.CopyResource(&dst, &src) };

    match tx.try_send((dst, pts, captured)) {
        Ok(()) => *slot += 1,
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            metrics::mark_dropped();
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
    Ok(())
}

/// Create a D3D11 device with BGRA + video support (needed for the video
/// processor colour conversion).
fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice (CS2) failed")?;
    }
    Ok((
        device.context("null D3D11 device")?,
        context.context("null D3D11 context")?,
    ))
}

/// Allocate the BGRA pool the capture thread copies frames into. Bound as
/// shader resource + render target so the video processor accepts it as input.
fn create_bgra_pool(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut pool = Vec::with_capacity(BGRA_POOL_SIZE);
    for _ in 0..BGRA_POOL_SIZE {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        pool.push(tex.context("CreateTexture2D(BGRA pool) returned null")?);
    }
    Ok(pool)
}

/// Create a GraphicsCaptureItem from a top-level window handle.
fn create_capture_item(hwnd: isize) -> Result<GraphicsCaptureItem> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
    Ok(item)
}

/// RAII guard initialising/uninitialising an MTA COM apartment for the
/// encoder thread (Media Foundation requires MTA).
struct ComGuard;

impl ComGuard {
    fn new() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        ComGuard
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
