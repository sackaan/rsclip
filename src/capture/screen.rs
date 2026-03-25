// Screen capture via the Windows Graphics Capture API.
//
// Captures the RuneLite window by HWND. Frames are pushed to the encoder
// pipeline via a channel from the WGC FrameArrived callback.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

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

use crossbeam_channel::Sender;

use crate::encoding::encoder::RawFrame;
use crate::platform::runelite::CropRect;

/// Handle to a running capture session. Drop or call `stop()` to terminate.
pub struct CaptureHandle {
    running: Arc<AtomicBool>,
    session: Arc<Mutex<Option<GraphicsCaptureSession>>>,
    frame_pool: Arc<Mutex<Option<Direct3D11CaptureFramePool>>>,
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
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start capturing the given window. If `crop` is `Some`, each frame is
/// cropped to that sub-rectangle (used to isolate the game canvas).
pub fn start_capture(
    hwnd: isize,
    frame_tx: Sender<RawFrame>,
    crop: Option<CropRect>,
    dropped_frames: Arc<AtomicU64>,
) -> Result<CaptureHandle> {
    let running = Arc::new(AtomicBool::new(true));

    // --- Create D3D11 device ---
    let (d3d_device, d3d_context) = create_d3d11_device()?;
    let dxgi_device: IDXGIDevice = d3d_device.cast()?;
    let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
    let device: IDirect3DDevice = inspectable.cast()?;

    // --- Create capture item from HWND ---
    let item = create_capture_item(hwnd)?;
    let size = item.Size()?;
    info!("Capture item created: {}x{}", size.Width, size.Height);

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
    let _ = set_border_required_false(&session);

    // --- Shared state for the event handler ---
    let running_clone = running.clone();
    let d3d_device_clone = d3d_device.clone();
    let d3d_context_clone = d3d_context.clone();
    let frame_tx_clone = frame_tx.clone();
    let dropped_clone = dropped_frames.clone();
    let crop_clone = crop;
    let frame_pool_arc = Arc::new(Mutex::new(Some(frame_pool)));
    let session_arc = Arc::new(Mutex::new(Some(session)));
    let staging_cache: Arc<Mutex<Option<(ID3D11Texture2D, u32, u32)>>> = Arc::new(Mutex::new(None));

    // We need a reference to frame_pool inside the handler, but also return it.
    let fp_for_handler = frame_pool_arc.clone();

    // --- FrameArrived handler ---
    {
        let fp_guard = fp_for_handler.lock();
        if let Some(ref fp) = *fp_guard {
            fp.FrameArrived(&TypedEventHandler::new(
                move |pool: &Option<Direct3D11CaptureFramePool>, _| {
                    if !running_clone.load(Ordering::SeqCst) {
                        return Ok(());
                    }
                    if let Some(ref pool) = pool {
                        if let Ok(frame) = pool.TryGetNextFrame() {
                            if let Err(e) = process_frame(
                                &frame,
                                &d3d_device_clone,
                                &d3d_context_clone,
                                &frame_tx_clone,
                                crop_clone.as_ref(),
                                &staging_cache,
                                &dropped_clone,
                            ) {
                                debug!("Frame processing error: {}", e);
                            }
                        }
                    }
                    Ok(())
                },
            ))?;
        }
    }

    // --- Start ---
    {
        let session_guard = session_arc.lock();
        if let Some(ref s) = *session_guard {
            s.StartCapture()?;
        }
    }
    info!("Capture started for HWND {:?}", hwnd);

    Ok(CaptureHandle {
        running,
        session: session_arc,
        frame_pool: frame_pool_arc,
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
    Ok((device.unwrap(), context.unwrap()))
}

/// Create a GraphicsCaptureItem from a window handle.
fn create_capture_item(hwnd: isize) -> Result<GraphicsCaptureItem> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
    Ok(item)
}

/// Map a captured texture, optionally crop, and send to the encoder.
/// The staging texture is cached and reused across frames.
fn process_frame(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame_tx: &Sender<RawFrame>,
    crop: Option<&CropRect>,
    staging_cache: &Mutex<Option<(ID3D11Texture2D, u32, u32)>>,
    dropped_frames: &Arc<AtomicU64>,
) -> Result<()> {
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

    let staging = {
        let mut cache = staging_cache.lock();
        match *cache {
            Some((ref tex, w, h)) if w == width && h == height => tex.clone(),
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
                *cache = Some((new_staging.clone(), width, height));
                new_staging
            }
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

    let row_pitch = mapped.RowPitch as usize;
    let bpp = 4usize;

    let (final_data, final_w, final_h) = if let Some(cr) = crop {
        let x = cr.x.min(width);
        let y = cr.y.min(height);
        let w = cr.width.min(width.saturating_sub(x));
        let h = cr.height.min(height.saturating_sub(y));

        if w == 0 || h == 0 {
            // Degenerate crop — read full frame.
            read_mapped_region(mapped.pData, row_pitch, 0, 0, width, height, bpp)
        } else {
            read_mapped_region(mapped.pData, row_pitch, x, y, w, h, bpp)
        }
    } else {
        read_mapped_region(mapped.pData, row_pitch, 0, 0, width, height, bpp)
    };

    unsafe {
        context.Unmap(&staging, 0);
    }

    if frame_tx
        .try_send(RawFrame {
            data: final_data,
            width: final_w,
            height: final_h,
        })
        .is_err()
    {
        dropped_frames.fetch_add(1, Ordering::Relaxed);
    }

    Ok(())
}

fn read_mapped_region(
    data_ptr: *mut std::ffi::c_void,
    row_pitch: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    bpp: usize,
) -> (Vec<u8>, u32, u32) {
    let dst_row_bytes = w as usize * bpp;
    let mut dst = vec![0u8; dst_row_bytes * h as usize];

    unsafe {
        let src = data_ptr as *const u8;
        for row in 0..h as usize {
            let src_offset = (y as usize + row) * row_pitch + x as usize * bpp;
            let dst_offset = row * dst_row_bytes;
            std::ptr::copy_nonoverlapping(
                src.add(src_offset),
                dst.as_mut_ptr().add(dst_offset),
                dst_row_bytes,
            );
        }
    }

    (dst, w, h)
}

fn set_border_required_false(session: &GraphicsCaptureSession) -> Result<()> {
    session.SetIsBorderRequired(false)?;
    Ok(())
}
