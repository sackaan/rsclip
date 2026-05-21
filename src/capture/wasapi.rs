//! Low-level WASAPI internals.
//!
//! - Per-process audio capture (Windows 10 build 20348+) for game audio.
//! - Standard device capture for microphones.
//! - A stateful linear resampler that carries fractional sample position
//!   across chunk boundaries (otherwise long sessions drift audibly — the
//!   same class of bug OBS avoids).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use log::{info, warn};

use windows::core::Interface;
use windows::core::PROPVARIANT;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::GetCurrentProcessId;

use super::audio::SharedAudioBuffer;

pub(super) const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

/// Completion handler for ActivateAudioInterfaceAsync.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: Arc<(StdMutex<bool>, Condvar)>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _activateoperation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let (lock, cvar) = &*self.done;
        let mut done = lock.lock().unwrap();
        *done = true;
        cvar.notify_all();
        Ok(())
    }
}

/// Activate an `IAudioClient` that captures only audio from `target_pid`.
unsafe fn activate_process_loopback(target_pid: u32) -> Result<IAudioClient> {
    let loopback_params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: target_pid,
        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    };

    let activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: loopback_params,
        },
    };

    let propvariant = create_blob_propvariant(&activation_params);

    let done = Arc::new((StdMutex::new(false), Condvar::new()));
    let handler = ActivationHandler { done: done.clone() };
    let handler: IActivateAudioInterfaceCompletionHandler = handler.into();

    let operation = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&propvariant),
        &handler,
    )
    .context("ActivateAudioInterfaceAsync failed")?;

    {
        let (lock, cvar) = &*done;
        let mut done_flag = lock.lock().unwrap();
        while !*done_flag {
            let result = cvar
                .wait_timeout(done_flag, Duration::from_secs(5))
                .unwrap();
            done_flag = result.0;
            if result.1.timed_out() {
                anyhow::bail!("Audio activation timed out after 5 seconds");
            }
        }
    }

    let mut hr = windows::core::HRESULT(0);
    let mut activated_interface: Option<windows::core::IUnknown> = None;
    operation
        .GetActivateResult(&mut hr, &mut activated_interface)
        .context("GetActivateResult failed")?;
    hr.ok().context("Audio activation returned error HRESULT")?;

    let unknown =
        activated_interface.context("ActivateAudioInterfaceAsync returned null interface")?;
    let audio_client: IAudioClient = unknown.cast().context("Failed to cast to IAudioClient")?;

    Ok(audio_client)
}

/// Pack `AUDIOCLIENT_ACTIVATION_PARAMS` into a `VT_BLOB` PROPVARIANT.
///
/// Safety: manually writes into the PROPVARIANT union. The blob data is
/// allocated with `CoTaskMemAlloc` as required by the Windows API.
unsafe fn create_blob_propvariant(params: &AUDIOCLIENT_ACTIVATION_PARAMS) -> PROPVARIANT {
    let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    let params_ptr = params as *const _ as *const u8;

    let alloc = CoTaskMemAlloc(params_size);
    std::ptr::copy_nonoverlapping(params_ptr, alloc as *mut u8, params_size);

    // PROPVARIANT layout: [u16 vt][u16 r1][u16 r2][u16 r3][union...]
    // VT_BLOB union: { u32 cbSize, *u8 pBlobData }
    let mut pv = PROPVARIANT::default();
    let pv_ptr = &mut pv as *mut _ as *mut u8;

    let vt: u16 = 0x0041; // VT_BLOB
    std::ptr::copy_nonoverlapping(&vt as *const u16 as *const u8, pv_ptr, 2);

    let cb_size = params_size as u32;
    std::ptr::copy_nonoverlapping(&cb_size as *const u32 as *const u8, pv_ptr.add(8), 4);

    let ptr_offset = if std::mem::size_of::<usize>() == 8 {
        16
    } else {
        12
    };
    let blob_ptr = alloc as usize;
    std::ptr::copy_nonoverlapping(
        &blob_ptr as *const usize as *const u8,
        pv_ptr.add(ptr_offset),
        std::mem::size_of::<usize>(),
    );

    pv
}

/// Run the WASAPI capture loop. Blocks until `running` is set to `false`.
pub(super) unsafe fn process_loopback_loop(
    running: &AtomicBool,
    buffer: &SharedAudioBuffer,
    target_pid: u32,
) -> Result<()> {
    info!(
        "Starting per-process audio capture for PID {} (our PID: {})",
        target_pid,
        GetCurrentProcessId()
    );

    let audio_client = activate_process_loopback(target_pid)
        .context("Failed to activate process loopback audio")?;

    // Process loopback always uses 48 kHz stereo float.
    let sample_rate: u32 = 48000;
    let channels: u16 = 2;
    let bits_per_sample: u16 = 32;
    let block_align: u16 = channels * (bits_per_sample / 8); // 8
    let avg_bytes_per_sec: u32 = sample_rate * block_align as u32;

    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: avg_bytes_per_sec,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: 0,
    };

    info!(
        "Process audio capture: {} Hz, {} ch, {} bit, float",
        sample_rate, channels, bits_per_sample
    );

    audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            200_000, // 20 ms in 100-ns units
            0,
            &format as *const _,
            None,
        )
        .context("Failed to initialize process loopback audio client")?;

    let capture_client: IAudioCaptureClient = audio_client
        .GetService()
        .context("Failed to get IAudioCaptureClient")?;

    audio_client
        .Start()
        .context("Failed to start process audio capture")?;

    info!("Process audio capture started for PID {}", target_pid);

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));

        loop {
            let packet_size = match capture_client.GetNextPacketSize() {
                Ok(s) => s,
                Err(_) => break,
            };
            if packet_size == 0 {
                break;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;

            if capture_client
                .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                .is_err()
            {
                break;
            }

            let byte_count = num_frames as usize * block_align as usize;

            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                buffer.lock().push_silence(byte_count);
            } else if !data_ptr.is_null() && byte_count > 0 {
                let slice = std::slice::from_raw_parts(data_ptr, byte_count);
                buffer.lock().push(slice);
            }

            let _ = capture_client.ReleaseBuffer(num_frames);
        }
    }

    let _ = audio_client.Stop();

    Ok(())
}

/// A discovered microphone / audio input device.
#[derive(Clone, Debug)]
pub struct MicDevice {
    /// WASAPI endpoint id (stable across reboots; suitable for persistence).
    pub id: String,
    /// Human-friendly device name as shown in the OS sound settings.
    pub name: String,
}

pub(super) fn enumerate_mic_devices() -> Result<Vec<MicDevice>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = windows::Win32::System::Com::CoCreateInstance(
            &MMDeviceEnumerator,
            None,
            windows::Win32::System::Com::CLSCTX_ALL,
        )
        .context("Failed to create IMMDeviceEnumerator")?;

        let collection = enumerator
            .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
            .context("Failed to enumerate capture endpoints")?;

        let count = collection.GetCount().context("GetCount failed")?;
        let mut devices = Vec::with_capacity(count as usize);

        for i in 0..count {
            let device = match collection.Item(i) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let id = match device.GetId() {
                Ok(pwstr) => {
                    let s = pwstr.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(pwstr.0 as *const _));
                    s
                }
                Err(_) => continue,
            };

            let name = match device.OpenPropertyStore(STGM_READ) {
                Ok(store) => match store.GetValue(&PKEY_Device_FriendlyName) {
                    Ok(val) => val.to_string(),
                    Err(_) => id.clone(),
                },
                Err(_) => id.clone(),
            };

            devices.push(MicDevice { id, name });
        }

        Ok(devices)
    }
}

/// Source format description extracted from WAVEFORMATEX.
struct SrcFormat {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    is_float: bool,
    block_align: u16,
}

/// Stateful linear resampler that carries the fractional sample position
/// across chunk boundaries. Without this, each chunk independently truncates
/// the fractional part, losing up to one output sample per chunk. Over long
/// sessions (100 chunks/sec * hours) this drift becomes audible — the same
/// class of bug OBS avoids by using persistent resampler state.
struct ResamplerState {
    frac: f64,
}

impl ResamplerState {
    fn new() -> Self {
        Self { frac: 0.0 }
    }

    fn resample(&mut self, input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
        if src_rate == dst_rate {
            return input.to_vec();
        }

        let src_frames = input.len() / 2;
        if src_frames == 0 {
            return Vec::new();
        }

        let ratio = src_rate as f64 / dst_rate as f64;
        let mut out = Vec::new();
        let mut src_pos = self.frac;

        while (src_pos as usize) < src_frames {
            let idx = src_pos as usize;
            let frac = (src_pos - idx as f64) as f32;

            let idx0 = idx.min(src_frames - 1);
            let idx1 = (idx + 1).min(src_frames - 1);

            // Left channel
            let l0 = input[idx0 * 2];
            let l1 = input[idx1 * 2];
            out.push(l0 + (l1 - l0) * frac);

            // Right channel
            let r0 = input[idx0 * 2 + 1];
            let r1 = input[idx1 * 2 + 1];
            out.push(r0 + (r1 - r0) * frac);

            src_pos += ratio;
        }

        // Carry the fractional remainder to the next call.
        self.frac = src_pos - src_frames as f64;

        out
    }
}

/// WASAPI capture loop for a microphone. Converts to 48 kHz stereo f32.
pub(super) unsafe fn device_capture_loop(
    running: &AtomicBool,
    buffer: &SharedAudioBuffer,
    device_id: &str,
) -> Result<()> {
    info!("Starting mic capture for device: {}", device_id);

    let enumerator: IMMDeviceEnumerator = windows::Win32::System::Com::CoCreateInstance(
        &MMDeviceEnumerator,
        None,
        windows::Win32::System::Com::CLSCTX_ALL,
    )
    .context("Failed to create IMMDeviceEnumerator")?;

    let wide_id: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
    let device = enumerator
        .GetDevice(windows::core::PCWSTR(wide_id.as_ptr()))
        .context("Failed to get mic device by ID")?;

    let audio_client: IAudioClient = device
        .Activate(windows::Win32::System::Com::CLSCTX_ALL, None)
        .context("Failed to activate IAudioClient for mic")?;

    let format_ptr = audio_client
        .GetMixFormat()
        .context("GetMixFormat failed for mic")?;
    let fmt = &*format_ptr;

    let src_format = SrcFormat {
        sample_rate: fmt.nSamplesPerSec,
        channels: fmt.nChannels,
        bits_per_sample: fmt.wBitsPerSample,
        is_float: fmt.wFormatTag == 3
            || (fmt.wFormatTag == 0xFFFE && fmt.wBitsPerSample == 32 && fmt.cbSize >= 22),
        block_align: fmt.nBlockAlign,
    };

    info!(
        "Mic native format: {} Hz, {} ch, {} bit, float={}",
        src_format.sample_rate,
        src_format.channels,
        src_format.bits_per_sample,
        src_format.is_float
    );

    audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_NOPERSIST,
            200_000, // 20 ms in 100-ns units
            0,
            format_ptr,
            None,
        )
        .context("Failed to initialize mic audio client")?;

    let capture_client: IAudioCaptureClient = audio_client
        .GetService()
        .context("Failed to get IAudioCaptureClient for mic")?;

    audio_client
        .Start()
        .context("Failed to start mic capture")?;

    info!("Mic capture started");

    let mut resampler = ResamplerState::new();

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));

        loop {
            let packet_size = match capture_client.GetNextPacketSize() {
                Ok(s) => s,
                Err(_) => break,
            };
            if packet_size == 0 {
                break;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;

            if capture_client
                .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                .is_err()
            {
                break;
            }

            let byte_count = num_frames as usize * src_format.block_align as usize;

            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                // Push silence in canonical format: num_frames * 2ch * 4 bytes/sample = 8 bytes/frame
                let canonical_bytes = num_frames as usize * 8;
                buffer.lock().push_silence(canonical_bytes);
            } else if !data_ptr.is_null() && byte_count > 0 {
                let slice = std::slice::from_raw_parts(data_ptr, byte_count);
                let converted =
                    convert_to_canonical(slice, num_frames, &src_format, &mut resampler);
                buffer.lock().push(&converted);
            }

            let _ = capture_client.ReleaseBuffer(num_frames);
        }
    }

    let _ = audio_client.Stop();

    Ok(())
}

/// Convert raw PCM from the source format to canonical 48 kHz stereo f32.
fn convert_to_canonical(
    data: &[u8],
    num_frames: u32,
    src: &SrcFormat,
    resampler: &mut ResamplerState,
) -> Vec<u8> {
    let src_samples = decode_to_f32(data, num_frames, src);
    let stereo = to_stereo(&src_samples, src.channels, num_frames as usize);
    let resampled = if src.sample_rate == 48000 {
        stereo
    } else {
        resampler.resample(&stereo, src.sample_rate, 48000)
    };

    let mut out = Vec::with_capacity(resampled.len() * 4);
    for sample in &resampled {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

fn decode_to_f32(data: &[u8], num_frames: u32, src: &SrcFormat) -> Vec<f32> {
    let total_samples = num_frames as usize * src.channels as usize;
    let mut out = Vec::with_capacity(total_samples);

    if src.is_float && src.bits_per_sample == 32 {
        for chunk in data.chunks_exact(4) {
            out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    } else if !src.is_float && src.bits_per_sample == 16 {
        for chunk in data.chunks_exact(2) {
            let s = i16::from_le_bytes([chunk[0], chunk[1]]);
            out.push(s as f32 / 32768.0);
        }
    } else if !src.is_float && src.bits_per_sample == 24 {
        for chunk in data.chunks_exact(3) {
            let s = ((chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16)) << 8
                >> 8; // sign-extend
            out.push(s as f32 / 8388608.0);
        }
    } else if !src.is_float && src.bits_per_sample == 32 {
        for chunk in data.chunks_exact(4) {
            let s = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            out.push(s as f32 / 2147483648.0);
        }
    } else {
        // Unknown format — fill with silence.
        warn!(
            "Unknown mic format: {} bit, float={}",
            src.bits_per_sample, src.is_float
        );
        out.resize(total_samples, 0.0);
    }

    out
}

fn to_stereo(samples: &[f32], src_channels: u16, num_frames: usize) -> Vec<f32> {
    let ch = src_channels as usize;
    if ch == 2 {
        return samples.to_vec();
    }

    let mut out = Vec::with_capacity(num_frames * 2);
    if ch == 1 {
        for &s in samples {
            out.push(s);
            out.push(s);
        }
    } else {
        for frame in samples.chunks_exact(ch) {
            out.push(frame[0]);
            out.push(frame[1]);
        }
    }
    out
}
