//! In-process GPU H.264 encoder for the CS2 capture path.
//!
//! This is the zero-copy alternative to the FFmpeg-subprocess pipeline used
//! for RuneLite. A captured BGRA frame never leaves the GPU:
//!
//! 1. A D3D11 [`VideoProcessor`](ID3D11VideoProcessor) converts the WGC BGRA
//!    surface to NV12 (the format hardware H.264 encoders want), entirely on
//!    the GPU.
//! 2. The NV12 texture is wrapped in an [`IMFSample`] (no copy — it references
//!    the D3D11 surface) and fed to the hardware H.264 encoder MFT, which runs
//!    on the GPU's dedicated encode silicon (NVENC on NVIDIA).
//! 3. The MFT emits an H.264 Annex-B access unit, which is the *only* thing
//!    that reaches the CPU — already compressed.
//!
//! There is no `Map`/readback of full frames, so capturing CS2 at the panel
//! refresh rate (240 Hz) costs the game almost nothing.
//!
//! The encoder MFT is an **async hardware MFT**, so it is driven by the
//! `METransformNeedInput` / `METransformHaveOutput` event model rather than
//! synchronous `ProcessInput`/`ProcessOutput` calls.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use log::{debug, info, warn};

use super::gpu_buffer::SharedAuBuffer;
use crate::metrics::{self, Stage, Timestamp};

use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL};
use windows::Win32::Media::MediaFoundation::*;

/// Which codec the GPU encoder settled on. HEVC is preferred because it can
/// represent very high resolution × frame-rate combinations (e.g. 2560×1600 @
/// 240) that exceed every H.264 level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    fn subtype(&self) -> GUID {
        match self {
            VideoCodec::H264 => MFVideoFormat_H264,
            VideoCodec::Hevc => MFVideoFormat_HEVC,
        }
    }

    /// FFmpeg raw-bitstream demuxer name for the intermediate elementary
    /// stream held in the ring buffer.
    pub fn ffmpeg_demuxer(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Hevc => "hevc",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
        }
    }
}

/// A captured frame in flight to the encoder: the GPU-resident BGRA texture,
/// its capture timestamp in 100-ns units, and the instrumentation stamp used
/// to measure pipeline latency (zero-sized unless built with `--features
/// metrics`).
pub type CapturedFrame = (ID3D11Texture2D, i64, Timestamp);

/// One encoded access unit produced by the GPU encoder.
pub struct EncodedAu {
    /// Annex-B bytes for this access unit (one frame).
    pub data: Vec<u8>,
    /// Presentation timestamp in 100-ns units (Media Foundation native).
    pub pts_100ns: i64,
    /// True if this is an IDR / clean seek point — clips must start here.
    pub keyframe: bool,
}

/// Number of NV12 scratch textures cycled through so the encoder can read a
/// frame while the capture thread converts the next one.
const NV12_POOL_SIZE: usize = 6;

/// Upper bound on unmatched in-flight latency stamps. Comfortably above the
/// encoder's real pipeline depth; only exists so a lost output cannot grow the
/// queue without limit.
const IN_FLIGHT_CAP: usize = 64;

/// GPU H.264 encoder built around a hardware Media Foundation MFT.
///
/// Not `Send` by itself; intended to be created and driven from a single
/// encoder thread that owns the shared D3D11 device alongside WGC capture.
pub struct GpuH264Encoder {
    // Retained so the D3D11 device/context outlive the MFT, which references
    // them through the DXGI device manager. Not read directly after setup.
    #[allow(dead_code)]
    device: ID3D11Device,
    #[allow(dead_code)]
    context: ID3D11DeviceContext,

    // Video processor (BGRA -> NV12).
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    vp_enum: ID3D11VideoProcessorEnumerator,
    video_processor: ID3D11VideoProcessor,

    // NV12 scratch pool.
    nv12_pool: Vec<ID3D11Texture2D>,
    nv12_index: usize,

    // Encoder MFT (async hardware).
    transform: IMFTransform,
    event_gen: IMFMediaEventGenerator,

    codec: VideoCodec,
    sample_duration_100ns: i64,

    /// SPS/PPS sequence header, captured once the output type is negotiated.
    sequence_header: Vec<u8>,
}

impl GpuH264Encoder {
    /// Create the encoder against an existing D3D11 device (shared with WGC
    /// capture so textures need no cross-device copy).
    pub fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        unsafe {
            // MF must be running; safe to call repeatedly (refcounted).
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup failed")?;
        }

        // D3D11 immediate context is not thread-safe; the MFT will touch the
        // device from its own worker, so enable multithread protection.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            unsafe { let _ = mt.SetMultithreadProtected(true); };
        }

        let (video_device, video_context, vp_enum, video_processor) =
            create_video_processor(&device, &context, width, height)
                .context("creating BGRA->NV12 video processor")?;

        let nv12_pool = create_nv12_pool(&device, width, height)?;

        let (codec, transform, event_gen) =
            create_encoder_mft(&device, width, height, fps, bitrate_kbps)
                .context("creating hardware video encoder MFT")?;

        let sequence_header = read_sequence_header(&transform).unwrap_or_default();
        if sequence_header.is_empty() {
            warn!("Encoder did not expose an SPS/PPS sequence header up front");
        }

        let sample_duration_100ns = 10_000_000i64 / fps.max(1) as i64;

        info!(
            "GPU encoder ready: {}x{} @ {} fps, ~{} kbps, codec {} (Media Foundation HW MFT)",
            width,
            height,
            fps,
            bitrate_kbps,
            codec.label()
        );

        Ok(Self {
            device,
            context,
            video_device,
            video_context,
            vp_enum,
            video_processor,
            nv12_pool,
            nv12_index: 0,
            transform,
            event_gen,
            codec,
            sample_duration_100ns,
            sequence_header,
        })
    }

    /// SPS/PPS (or VPS/SPS/PPS for HEVC) bytes to prepend before the first
    /// access unit when saving a clip (so a cut at an IDR decodes standalone).
    #[allow(dead_code)] // CS2 path reads the header via the AU buffer today.
    pub fn sequence_header(&self) -> &[u8] {
        &self.sequence_header
    }

    /// Which codec the encoder settled on (H.264 or HEVC).
    #[allow(dead_code)] // CS2 path reads the codec via the AU buffer today.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Re-read the sequence header from the negotiated output type. Some
    /// encoders (notably HEVC) only populate `MF_MT_MPEG_SEQUENCE_HEADER`
    /// after the first frame has been processed.
    pub fn refresh_sequence_header(&mut self) -> &[u8] {
        if self.sequence_header.is_empty() {
            if let Ok(hdr) = read_sequence_header(&self.transform) {
                self.sequence_header = hdr;
            }
        }
        &self.sequence_header
    }

    /// Drive the async MFT to completion. Runs a single event loop: on each
    /// `METransformNeedInput` it pulls the next captured BGRA frame from
    /// `frame_rx`, converts it to NV12 and submits it; on each
    /// `METransformHaveOutput` it pulls an access unit and pushes it into
    /// `buffer`. Returns when `running` is cleared or the frame channel
    /// disconnects, after draining the encoder.
    pub fn run(
        &mut self,
        frame_rx: Receiver<CapturedFrame>,
        buffer: SharedAuBuffer,
        running: Arc<AtomicBool>,
    ) {
        {
            let mut b = buffer.lock();
            b.set_codec(self.codec);
            if !self.sequence_header.is_empty() {
                b.set_sequence_header(self.sequence_header.clone());
            }
        }

        let mut frames_in: u64 = 0;
        let mut aus_out: u64 = 0;
        let mut header_captured = !self.sequence_header.is_empty();
        let mut last_report = Instant::now();

        // Capture/submit stamps for frames handed to the MFT but not yet
        // returned. The MFT emits access units in submission order, so a FIFO
        // pairing gives each output the latency of its own input.
        let mut in_flight: VecDeque<(Timestamp, Timestamp)> = VecDeque::new();

        while running.load(Ordering::Relaxed) {
            let Some(event) = self.next_event(&running) else {
                break;
            };
            let evt_type = MF_EVENT_TYPE(unsafe { event.GetType().unwrap_or(0) } as i32);

            if evt_type == METransformNeedInput {
                // Block (briefly) for the next captured frame; a disconnect or
                // stop request ends the stream.
                let frame = loop {
                    match frame_rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(f) => break Some(f),
                        Err(RecvTimeoutError::Timeout) => {
                            if !running.load(Ordering::Relaxed) {
                                break None;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break None,
                    }
                };
                match frame {
                    Some((bgra, pts, captured)) => {
                        frames_in += 1;
                        metrics::record_since(Stage::CaptureToSubmit, captured);
                        let submitted = Timestamp::now();
                        if let Err(e) = self.process_input(&bgra, pts) {
                            warn!("CS2 ProcessInput failed on frame {frames_in}: {e:#}");
                        } else {
                            // Bounded: a dropped output would otherwise leak a
                            // stamp per frame for the life of the session.
                            if in_flight.len() >= IN_FLIGHT_CAP {
                                in_flight.pop_front();
                            }
                            in_flight.push_back((captured, submitted));
                        }
                    }
                    None => break,
                }
            } else if evt_type == METransformHaveOutput {
                match self.pull_output() {
                    Ok(Some(au)) => {
                        aus_out += 1;
                        let stamps = in_flight.pop_front();
                        if let Some((_, submitted)) = stamps {
                            metrics::record(Stage::Encode, submitted.elapsed());
                        }
                        {
                            // Timed at the call site, not inside `push`, so the
                            // buffer lock acquisition counts too — that lock is
                            // shared with the UI thread's memory readout.
                            let _t = metrics::Timer::start(Stage::BufferPush);
                            buffer.lock().push(au);
                        }
                        if let Some((captured, _)) = stamps {
                            metrics::record_since(Stage::EndToEnd, captured);
                        }
                        metrics::mark_frame();
                        if !header_captured {
                            let hdr = self.refresh_sequence_header().to_vec();
                            if !hdr.is_empty() {
                                buffer.lock().set_sequence_header(hdr);
                                header_captured = true;
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!("CS2 ProcessOutput failed: {e:#}"),
                }
            }

            if last_report.elapsed() >= Duration::from_secs(2) {
                last_report = Instant::now();
                info!(
                    "CS2 encode: {frames_in} frames in, {aus_out} access units out (header: {header_captured})"
                );
            }
        }

        aus_out += self.drain_remaining(&buffer);
        info!("CS2 encoder stopped: {frames_in} frames in, {aus_out} access units out");
    }

    /// Block for the next MFT event. Blocking (not polling) is essential for
    /// throughput — a `sleep`-based poll is throttled by Windows' coarse timer
    /// resolution. Shutdown is handled by the frame channel disconnecting,
    /// which the `NeedInput` path observes. Returns `None` to end the loop.
    fn next_event(&self, running: &AtomicBool) -> Option<IMFMediaEvent> {
        if !running.load(Ordering::Relaxed) {
            return None;
        }
        match unsafe {
            self.event_gen
                .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
        } {
            Ok(e) => Some(e),
            Err(e) => {
                warn!("CS2 encoder GetEvent failed: {e}");
                None
            }
        }
    }

    /// Convert one BGRA frame to NV12 and submit it to the encoder, stamping it
    /// with the real capture timestamp (`pts_100ns`) so playback speed is
    /// correct regardless of the actual capture rate.
    fn process_input(&mut self, bgra: &ID3D11Texture2D, pts_100ns: i64) -> Result<()> {
        let nv12 = self.convert_to_nv12(bgra)?;
        let sample = wrap_nv12_sample(&nv12, pts_100ns, self.sample_duration_100ns)?;
        unsafe {
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("ProcessInput")?;
        }
        Ok(())
    }

    /// Flush the encoder at end of stream, collecting trailing access units
    /// into `buffer`. Returns how many were produced.
    fn drain_remaining(&mut self, buffer: &SharedAuBuffer) -> u64 {
        if unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .is_err()
        } {
            return 0;
        }
        let mut n = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let event = match unsafe {
                self.event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT)
            } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(_) => break,
            };
            let evt_type = MF_EVENT_TYPE(unsafe { event.GetType().unwrap_or(0) } as i32);
            if evt_type == METransformHaveOutput {
                if let Ok(Some(au)) = self.pull_output() {
                    n += 1;
                    buffer.lock().push(au);
                }
            } else if evt_type == METransformDrainComplete {
                break;
            }
        }
        n
    }

    /// BGRA -> NV12 colour conversion via the D3D11 video processor.
    fn convert_to_nv12(&mut self, bgra: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        let nv12 = self.nv12_pool[self.nv12_index].clone();
        self.nv12_index = (self.nv12_index + 1) % self.nv12_pool.len();

        unsafe {
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(bgra, &self.vp_enum, &in_desc, Some(&mut input_view))
                .context("CreateVideoProcessorInputView failed")?;
            let input_view = input_view.context("null input view")?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_VPOV {
                        MipSlice: 0,
                    },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &nv12,
                    &self.vp_enum,
                    &out_desc,
                    Some(&mut output_view),
                )
                .context("CreateVideoProcessorOutputView failed")?;
            let output_view = output_view.context("null output view")?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view.clone())),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            self.video_context
                .VideoProcessorBlt(&self.video_processor, &output_view, 0, &[stream])
                .context("VideoProcessorBlt (BGRA->NV12) failed")?;
        }

        Ok(nv12)
    }

    /// `ProcessOutput` one access unit and copy its bytes out of the GPU
    /// staging buffer the MFT allocated.
    fn pull_output(&mut self) -> Result<Option<EncodedAu>> {
        unsafe {
            // Hardware MFTs allocate their own output samples.
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let hr = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            if let Err(e) = hr {
                // Need more input is normal between frames.
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Ok(None);
                }
                return Err(e.into());
            }

            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            let sample = match sample {
                Some(s) => s,
                None => return Ok(None),
            };

            let pts = sample.GetSampleTime().unwrap_or(0);
            let keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false);

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            let mut cur_len = 0u32;
            buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            let data = std::slice::from_raw_parts(ptr, cur_len as usize).to_vec();
            buffer.Unlock()?;

            Ok(Some(EncodedAu {
                data,
                pts_100ns: pts,
                keyframe,
            }))
        }
    }
}

impl Drop for GpuH264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        debug!("GPU H.264 encoder dropped");
    }
}

/// Build the D3D11 video processor used for BGRA->NV12 conversion.
fn create_video_processor(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    width: u32,
    height: u32,
) -> Result<(
    ID3D11VideoDevice,
    ID3D11VideoContext,
    ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessor,
)> {
    let video_device: ID3D11VideoDevice = device
        .cast()
        .context("device does not expose ID3D11VideoDevice (need D3D11_CREATE_DEVICE_VIDEO_SUPPORT)")?;
    let video_context: ID3D11VideoContext = context.cast().context("no ID3D11VideoContext")?;

    let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: 240,
            Denominator: 1,
        },
        InputWidth: width,
        InputHeight: height,
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: 240,
            Denominator: 1,
        },
        OutputWidth: width,
        OutputHeight: height,
        Usage: windows::Win32::Graphics::Direct3D11::D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };

    let vp_enum = unsafe {
        video_device
            .CreateVideoProcessorEnumerator(&content_desc)
            .context("CreateVideoProcessorEnumerator failed")?
    };
    let video_processor = unsafe {
        video_device
            .CreateVideoProcessor(&vp_enum, 0)
            .context("CreateVideoProcessor failed")?
    };

    Ok((video_device, video_context, vp_enum, video_processor))
}

/// Allocate the NV12 scratch texture pool the encoder reads from.
fn create_nv12_pool(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut pool = Vec::with_capacity(NV12_POOL_SIZE);
    for _ in 0..NV12_POOL_SIZE {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        pool.push(tex.context("CreateTexture2D(NV12) returned null")?);
    }
    Ok(pool)
}

/// Enumerate, activate, and configure the hardware H.264 encoder MFT for
/// D3D11 NV12 input. Returns the transform and its event generator.
fn create_encoder_mft(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(VideoCodec, IMFTransform, IMFMediaEventGenerator)> {
    unsafe {
        let mut device_manager: Option<IMFDXGIDeviceManager> = None;
        let mut reset_token = 0u32;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)
            .context("MFCreateDXGIDeviceManager failed")?;
        let device_manager = device_manager.context("null DXGI device manager")?;
        device_manager
            .ResetDevice(device, reset_token)
            .context("DXGI device manager ResetDevice failed")?;

        // Prefer HEVC: it covers high resolution × frame-rate combos that
        // exceed every H.264 level (e.g. 2560×1600 @ 240). Fall back to H.264.
        let mut last_err: Option<anyhow::Error> = None;
        for codec in [VideoCodec::Hevc, VideoCodec::H264] {
            match try_codec(codec, &device_manager, width, height, fps, bitrate_kbps) {
                Ok((transform, event_gen)) => return Ok((codec, transform, event_gen)),
                Err(e) => {
                    warn!("No usable {} hardware encoder: {:#}", codec.label(), e);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no usable hardware video encoder MFT")))
    }
}

/// Enumerate hardware encoders for one codec and return the first candidate
/// that fully activates and configures. On laptops the enumeration returns
/// BOTH the NVIDIA dGPU encoder and the Intel/AMD iGPU encoder; the first is
/// not always usable (the NVIDIA H.264 MFT often fails `ActivateObject` with
/// E_UNEXPECTED), so we try each in turn.
unsafe fn try_codec(
    codec: VideoCodec,
    device_manager: &IMFDXGIDeviceManager,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(IMFTransform, IMFMediaEventGenerator)> {
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: codec.subtype(),
    };
    let mut activate_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut activate_count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(&output_type as *const _),
        &mut activate_ptr,
        &mut activate_count,
    )?;
    if activate_count == 0 || activate_ptr.is_null() {
        bail!("no hardware {} encoder MFT enumerated", codec.label());
    }
    let activates = std::slice::from_raw_parts(activate_ptr, activate_count as usize);

    let mut last_err: Option<anyhow::Error> = None;
    let mut result = None;
    for activate in activates.iter().flatten() {
        let name = friendly_name(activate);
        match configure_encoder(activate, codec, device_manager, width, height, fps, bitrate_kbps) {
            Ok(pair) => {
                info!("Using {} encoder MFT: {}", codec.label(), name);
                result = Some(pair);
                break;
            }
            Err(e) => {
                warn!("Encoder candidate '{}' unusable: {:#}", name, e);
                let _ = activate.ShutdownObject();
                last_err = Some(e);
            }
        }
    }

    windows::Win32::System::Com::CoTaskMemFree(Some(activate_ptr as *const _));
    result.ok_or_else(|| {
        last_err.unwrap_or_else(|| anyhow::anyhow!("no usable {} encoder", codec.label()))
    })
}

/// Friendly display name of an encoder activate (for logging).
fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut pw = windows::core::PWSTR::null();
        let mut len = 0u32;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut len)
            .is_ok()
        {
            let s = pw.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(pw.0 as *const _));
            s
        } else {
            "<unknown>".into()
        }
    }
}

/// Activate and fully configure one encoder candidate. Returns the running
/// transform + its event generator, or an error naming the failing step.
unsafe fn configure_encoder(
    activate: &IMFActivate,
    codec: VideoCodec,
    device_manager: &IMFDXGIDeviceManager,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(IMFTransform, IMFMediaEventGenerator)> {
    let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;

    // Unlock async MFT.
    let attrs = transform.GetAttributes().context("GetAttributes")?;
    attrs
        .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
        .context("set MF_TRANSFORM_ASYNC_UNLOCK")?;

    // Output type: compressed codec with bitrate / size / frame rate.
    let out_type = MFCreateMediaType().context("MFCreateMediaType(out)")?;
    out_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .context("out major type")?;
    out_type
        .SetGUID(&MF_MT_SUBTYPE, &codec.subtype())
        .context("out subtype")?;
    out_type
        .SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))
        .context("out bitrate")?;
    out_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .context("out interlace")?;
    if codec == VideoCodec::H264 {
        // HEVC encoders pick their own profile; forcing the H.264 profile
        // attribute on them is rejected.
        out_type
            .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
            .context("out profile")?;
    }
    set_attr_ratio(&out_type, &MF_MT_FRAME_RATE, fps, 1).context("out frame rate")?;
    set_attr_size(&out_type, &MF_MT_FRAME_SIZE, width, height).context("out frame size")?;
    set_attr_ratio(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1).context("out PAR")?;
    let _ = out_type.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, fps);
    transform
        .SetOutputType(0, &out_type, 0)
        .context("SetOutputType (H264)")?;

    // Input type: NV12 matching frame size / rate.
    let in_type = MFCreateMediaType().context("MFCreateMediaType(in)")?;
    in_type
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .context("in major type")?;
    in_type
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .context("in subtype NV12")?;
    in_type
        .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .context("in interlace")?;
    set_attr_ratio(&in_type, &MF_MT_FRAME_RATE, fps, 1).context("in frame rate")?;
    set_attr_size(&in_type, &MF_MT_FRAME_SIZE, width, height).context("in frame size")?;
    set_attr_ratio(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1).context("in PAR")?;
    transform
        .SetInputType(0, &in_type, 0)
        .context("SetInputType (NV12)")?;

    // Bind the D3D11 device manager (after types are negotiated).
    transform
        .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, device_manager.as_raw() as usize)
        .context("MFT_MESSAGE_SET_D3D_MANAGER")?;

    let event_gen: IMFMediaEventGenerator =
        transform.cast().context("MFT is not an event generator")?;

    transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        .context("BEGIN_STREAMING")?;
    transform
        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        .context("START_OF_STREAM")?;

    Ok((transform, event_gen))
}

/// Read the SPS/PPS sequence header from the negotiated output type, if any.
fn read_sequence_header(transform: &IMFTransform) -> Result<Vec<u8>> {
    unsafe {
        let out_type = transform.GetOutputCurrentType(0)?;
        // Query blob size first.
        let size = match out_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
            Ok(s) if s > 0 => s,
            _ => return Ok(Vec::new()),
        };
        let mut buf = vec![0u8; size as usize];
        let mut size = size;
        out_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, Some(&mut size))?;
        Ok(buf)
    }
}

/// Wrap an NV12 D3D11 texture in an `IMFSample` without copying the surface.
fn wrap_nv12_sample(
    nv12: &ID3D11Texture2D,
    pts_100ns: i64,
    duration_100ns: i64,
) -> Result<IMFSample> {
    unsafe {
        let buffer: IMFMediaBuffer =
            MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;

        // A freshly wrapped DXGI surface buffer has current length 0; encoders
        // reject a zero-length input sample (E_INVALIDARG). Set it from the
        // contiguous NV12 size reported by the 2D buffer.
        let len = buffer
            .cast::<IMF2DBuffer>()
            .context("buffer is not IMF2DBuffer")?
            .GetContiguousLength()
            .context("GetContiguousLength")?;
        buffer
            .SetCurrentLength(len)
            .context("SetCurrentLength on input buffer")?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(pts_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
    }
}

fn set_attr_ratio(attrs: &IMFMediaType, key: &GUID, num: u32, den: u32) -> Result<()> {
    let packed = ((num as u64) << 32) | den as u64;
    unsafe { attrs.SetUINT64(key, packed)? };
    Ok(())
}

fn set_attr_size(attrs: &IMFMediaType, key: &GUID, w: u32, h: u32) -> Result<()> {
    let packed = ((w as u64) << 32) | h as u64;
    unsafe { attrs.SetUINT64(key, packed)? };
    Ok(())
}
