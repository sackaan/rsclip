//! Audio capture and mixing.
//!
//! Each audio source (game, microphone) gets its own [`AudioChannel`]
//! backed by a ring buffer. At save time the [`AudioMixer`] blends all
//! channels into a single WAV that the encoder remuxes into the final MP4.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{info, warn};
use parking_lot::Mutex;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use super::wasapi;

pub use wasapi::MicDevice;

/// Gaps in delivery shorter than this are treated as timestamp jitter;
/// longer ones (a silent source that stopped delivering, a capture overrun)
/// are filled with silence so the sample timeline keeps matching wall time.
const GAP_TOLERANCE: Duration = Duration::from_millis(30);

/// Fixed-capacity ring of canonical f32 interleaved PCM, anchored to wall
/// time: `end_time` is when the newest sample ends, and samples run back
/// from there at the nominal rate.
pub(super) struct AudioRingBuffer {
    data: Vec<u8>,
    write_pos: usize,
    filled: usize,
    sample_rate: u32,
    channels: u16,
    block_align: u16,
    /// Wall-clock time at which the newest buffered sample ends.
    end_time: Option<Instant>,
}

impl AudioRingBuffer {
    fn new(max_seconds: u32, sample_rate: u32, channels: u16) -> Self {
        let block_align = channels * 4;
        let bytes_per_sec = sample_rate as usize * block_align as usize;
        // +2 seconds margin (same as video buffer).
        let max_bytes = bytes_per_sec * (max_seconds as usize + 2);

        Self {
            data: vec![0u8; max_bytes],
            write_pos: 0,
            filled: 0,
            sample_rate,
            channels,
            block_align,
            end_time: None,
        }
    }

    fn bytes_to_duration(&self, bytes: usize) -> Duration {
        let bytes_per_sec = self.sample_rate as f64 * self.block_align as f64;
        Duration::from_secs_f64(bytes as f64 / bytes_per_sec)
    }

    /// Append PCM whose first sample was captured at `start`.
    pub(super) fn push_at(&mut self, pcm: &[u8], start: Instant) {
        self.fill_gap(start);
        self.write(Some(pcm), pcm.len());
        self.end_time = Some(start + self.bytes_to_duration(pcm.len()));
    }

    /// Append `count` bytes of silence starting at `start`.
    pub(super) fn push_silence_at(&mut self, count: usize, start: Instant) {
        self.fill_gap(start);
        self.write(None, count);
        self.end_time = Some(start + self.bytes_to_duration(count));
    }

    fn fill_gap(&mut self, start: Instant) {
        let Some(end) = self.end_time else {
            return;
        };
        let gap = start.saturating_duration_since(end);
        if gap <= GAP_TOLERANCE {
            return;
        }
        let bytes_per_sec = self.sample_rate as f64 * self.block_align as f64;
        let block = self.block_align as usize;
        let bytes = (gap.as_secs_f64() * bytes_per_sec) as usize / block * block;
        self.write(None, bytes);
    }

    /// Write `len` bytes of `pcm` (or zeros when `None`) at the head.
    fn write(&mut self, pcm: Option<&[u8]>, len: usize) {
        let cap = self.data.len();
        if cap == 0 || len == 0 {
            return;
        }
        // Only the newest `cap` bytes can be kept.
        let skip = len.saturating_sub(cap);
        let len = len - skip;
        let src = pcm.map(|p| &p[skip..skip + len]);

        let first = len.min(cap - self.write_pos);
        let (a, b) = (self.write_pos..self.write_pos + first, 0..len - first);
        match src {
            Some(src) => {
                self.data[a].copy_from_slice(&src[..first]);
                self.data[b].copy_from_slice(&src[first..]);
            }
            None => {
                self.data[a].fill(0);
                self.data[b].fill(0);
            }
        }
        self.write_pos = (self.write_pos + len) % cap;
        self.filled = (self.filled + len).min(cap);
    }

    /// Interleaved samples covering exactly `[t0, t1)` of wall time at the
    /// nominal rate. Moments the ring doesn't cover come back as silence.
    fn span_f32(&self, t0: Instant, t1: Instant) -> Vec<f32> {
        let rate = self.sample_rate as f64;
        let ch = self.channels as usize;
        let n = (t1.saturating_duration_since(t0).as_secs_f64() * rate).round() as usize;
        let mut out = vec![0.0f32; n * ch];
        let Some(end) = self.end_time else {
            return out;
        };
        let block = self.block_align as usize;
        let cap_frames = self.data.len() / block;
        let filled_frames = (self.filled / block) as i64;
        let head = self.write_pos / block;
        // Frames between t0 and the end of the newest sample (may be < 0).
        let lead = if end >= t0 {
            (end - t0).as_secs_f64() * rate
        } else {
            -(t0 - end).as_secs_f64() * rate
        };
        for (j, frame) in out.chunks_exact_mut(ch).enumerate() {
            // How many frames back from the newest one covers this moment.
            let back = (lead - j as f64).ceil() as i64;
            if back < 1 || back > filled_frames {
                continue;
            }
            let fi = (head + cap_frames - back as usize) % cap_frames;
            let bytes = &self.data[fi * block..fi * block + block];
            for (s, b) in frame.iter_mut().zip(bytes.chunks_exact(4)) {
                *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
        }
        out
    }
}

fn build_wav(
    pcm: &[u8],
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    is_float: bool,
) -> Vec<u8> {
    let data_size = pcm.len() as u32;
    let block_align = channels * (bits_per_sample / 8);
    let byte_rate = sample_rate * block_align as u32;
    let audio_format: u16 = if is_float { 3 } else { 1 };

    let mut wav = Vec::with_capacity(44 + pcm.len());
    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&audio_format.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

pub(super) type SharedAudioBuffer = Arc<Mutex<AudioRingBuffer>>;

fn new_canonical_buffer(buffer_seconds: u32) -> SharedAudioBuffer {
    Arc::new(Mutex::new(AudioRingBuffer::new(buffer_seconds, 48000, 2)))
}

/// The kind of audio source backing a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelKind {
    /// Game audio captured via WASAPI process loopback (auto-follows RuneLite).
    Game,
    /// Microphone input captured via WASAPI device capture.
    Microphone,
    /// Arbitrary user-picked application captured via WASAPI process loopback.
    Application {
        /// PID at the time the channel was created. May refer to a dead
        /// process if the app exits mid-recording — the capture loop just
        /// stops producing samples in that case.
        pid: u32,
        /// Lowercased exe basename, used as the stable identity across
        /// process restarts (see [`Settings::app_channels`]).
        exe_name: String,
    },
}

/// A single audio source with its own background capture thread and ring
/// buffer.
pub struct AudioChannel {
    /// Source kind (game loopback or microphone).
    pub kind: ChannelKind,
    /// Linear scaling factor applied when mixing (1.0 = unity, 0.0 = mute).
    pub volume: f32,
    running: Arc<AtomicBool>,
    buffer: SharedAudioBuffer,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioChannel {
    fn start_game(buffer_seconds: u32, target_pid: u32) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let buffer = new_canonical_buffer(buffer_seconds);

        let thread = spawn_game_thread(&running, &buffer, target_pid)?;

        Ok(Self {
            kind: ChannelKind::Game,
            volume: 1.0,
            running,
            buffer,
            thread: Some(thread),
        })
    }

    fn start_application(buffer_seconds: u32, pid: u32, exe_name: String) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let buffer = new_canonical_buffer(buffer_seconds);

        // Application channels reuse the per-process loopback thread that
        // the game channel uses — only difference is the PID source.
        let thread = spawn_game_thread(&running, &buffer, pid)?;

        Ok(Self {
            kind: ChannelKind::Application { pid, exe_name },
            volume: 1.0,
            running,
            buffer,
            thread: Some(thread),
        })
    }

    fn start_mic(buffer_seconds: u32, device_id: &str) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let buffer = new_canonical_buffer(buffer_seconds);

        let thread = spawn_mic_thread(&running, &buffer, device_id)?;

        Ok(Self {
            kind: ChannelKind::Microphone,
            volume: 1.0,
            running,
            buffer,
            thread: Some(thread),
        })
    }

    fn switch_pid(&mut self, new_pid: u32) -> Result<()> {
        debug_assert_eq!(self.kind, ChannelKind::Game);

        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }

        self.running = Arc::new(AtomicBool::new(true));
        self.thread = Some(spawn_game_thread(&self.running, &self.buffer, new_pid)?);
        Ok(())
    }

    fn span_f32(&self, t0: Instant, t1: Instant) -> Vec<f32> {
        self.buffer.lock().span_f32(t0, t1)
    }

    fn has_audio(&self) -> bool {
        self.buffer.lock().end_time.is_some()
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for AudioChannel {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_game_thread(
    running: &Arc<AtomicBool>,
    buffer: &SharedAudioBuffer,
    target_pid: u32,
) -> Result<std::thread::JoinHandle<()>> {
    let running = running.clone();
    let buffer = buffer.clone();
    let handle = std::thread::Builder::new()
        .name("audio-game".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            match unsafe { wasapi::process_loopback_loop(&running, &buffer, target_pid) } {
                Ok(()) => info!("Game audio thread stopped"),
                Err(e) => warn!("Game audio error: {}", e),
            }
            unsafe {
                CoUninitialize();
            }
        })?;
    Ok(handle)
}

fn spawn_mic_thread(
    running: &Arc<AtomicBool>,
    buffer: &SharedAudioBuffer,
    device_id: &str,
) -> Result<std::thread::JoinHandle<()>> {
    let running = running.clone();
    let buffer = buffer.clone();
    let device_id = device_id.to_string();
    let handle = std::thread::Builder::new()
        .name("audio-mic".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            match unsafe { wasapi::device_capture_loop(&running, &buffer, &device_id) } {
                Ok(()) => info!("Mic capture thread stopped"),
                Err(e) => warn!("Mic capture error: {}", e),
            }
            unsafe {
                CoUninitialize();
            }
        })?;
    Ok(handle)
}

/// Manages multiple audio sources and mixes them on demand.
pub struct AudioMixer {
    channels: Vec<AudioChannel>,
    buffer_seconds: u32,
}

impl AudioMixer {
    /// Create an empty mixer sized for `buffer_seconds` of buffered audio
    /// per channel.
    pub fn new(buffer_seconds: u32) -> Self {
        Self {
            channels: Vec::new(),
            buffer_seconds,
        }
    }

    /// Start capturing game audio for `pid` via WASAPI process loopback.
    pub fn add_game_channel(&mut self, pid: u32) -> Result<()> {
        let mut ch = AudioChannel::start_game(self.buffer_seconds, pid)?;
        ch.volume = 1.0;
        self.channels.push(ch);
        Ok(())
    }

    /// Start capturing microphone audio from the given WASAPI endpoint id.
    pub fn add_mic_channel(&mut self, device_id: &str) -> Result<()> {
        let mut ch = AudioChannel::start_mic(self.buffer_seconds, device_id)?;
        ch.volume = 1.0;
        self.channels.push(ch);
        Ok(())
    }

    /// Stop and remove the microphone channel, if any.
    pub fn remove_mic_channel(&mut self) {
        self.channels
            .retain(|ch| ch.kind != ChannelKind::Microphone);
    }

    /// Start capturing audio from an arbitrary application via WASAPI
    /// process loopback. `exe_name` is the lowercased exe basename used as
    /// the stable identity (e.g. for persistence and removal).
    pub fn add_application_channel(&mut self, pid: u32, exe_name: String) -> Result<()> {
        let ch = AudioChannel::start_application(self.buffer_seconds, pid, exe_name)?;
        self.channels.push(ch);
        Ok(())
    }

    /// Remove the application channel matching `pid`, stopping its thread.
    pub fn remove_application_channel(&mut self, pid: u32) {
        self.channels.retain(|ch| match &ch.kind {
            ChannelKind::Application { pid: p, .. } => *p != pid,
            _ => true,
        });
    }

    /// Find the live PID of the application channel whose exe matches
    /// `exe_name`. Returns `None` when no such channel is attached.
    pub fn application_pid_for_exe(&self, exe_name: &str) -> Option<u32> {
        self.channels.iter().find_map(|ch| match &ch.kind {
            ChannelKind::Application { pid, exe_name: e } if e == exe_name => Some(*pid),
            _ => None,
        })
    }

    /// Borrow an application channel mutably for volume / state tweaks.
    pub fn application_channel_mut(&mut self, pid: u32) -> Option<&mut AudioChannel> {
        self.channels.iter_mut().find(|ch| match &ch.kind {
            ChannelKind::Application { pid: p, .. } => *p == pid,
            _ => false,
        })
    }

    /// Repoint the game-audio channel at a new process. Creates the channel
    /// if it doesn't exist yet.
    pub fn switch_game_pid(&mut self, new_pid: u32) -> Result<()> {
        if let Some(ch) = self
            .channels
            .iter_mut()
            .find(|c| c.kind == ChannelKind::Game)
        {
            ch.switch_pid(new_pid)
        } else {
            self.add_game_channel(new_pid)
        }
    }

    /// Borrow the game channel mutably for volume tweaks.
    pub fn game_channel_mut(&mut self) -> Option<&mut AudioChannel> {
        self.channels
            .iter_mut()
            .find(|c| c.kind == ChannelKind::Game)
    }

    /// Borrow the microphone channel mutably for volume tweaks.
    pub fn mic_channel_mut(&mut self) -> Option<&mut AudioChannel> {
        self.channels
            .iter_mut()
            .find(|c| c.kind == ChannelKind::Microphone)
    }

    /// Mix all active channels over the wall-clock interval `[t0, t1)` into
    /// a single WAV, so the audio lines up with video covering the same
    /// moments. Parts a channel didn't capture are silent. Returns an empty
    /// vector if no active channel has captured anything.
    pub fn snapshot_mixed_wav_span(&self, t0: Instant, t1: Instant) -> Vec<u8> {
        let active: Vec<_> = self
            .channels
            .iter()
            .filter(|c| c.volume > 0.0 && c.has_audio())
            .collect();
        if active.is_empty() {
            return Vec::new();
        }

        let mut mixed: Vec<f32> = Vec::new();
        for ch in active {
            let pcm = ch.span_f32(t0, t1);
            if mixed.is_empty() {
                mixed = pcm.iter().map(|&s| s * ch.volume).collect();
            } else {
                for (m, s) in mixed.iter_mut().zip(&pcm) {
                    *m += s * ch.volume;
                }
            }
        }
        for s in &mut mixed {
            *s = s.clamp(-1.0, 1.0);
        }

        let bytes = f32_slice_to_bytes(&mixed);
        build_wav(&bytes, 48000, 2, 32, true)
    }

    /// Stop every channel's capture thread. Idempotent.
    pub fn stop(&self) {
        for ch in &self.channels {
            ch.stop();
        }
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Enumerate active WASAPI capture endpoints (microphones / line-ins).
pub fn enumerate_mic_devices() -> Result<Vec<MicDevice>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    wasapi::enumerate_mic_devices()
}

fn f32_slice_to_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48000;

    /// Stereo f32 PCM of `frames` frames, every sample = `v`.
    fn pcm(frames: usize, v: f32) -> Vec<u8> {
        f32_slice_to_bytes(&vec![v; frames * 2])
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn test_build_wav_header_pcm() {
        let pcm = vec![0u8; 100];
        let wav = build_wav(&pcm, 44100, 2, 16, false);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        // audio_format = 1 (PCM)
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        // channels = 2
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 2);
        // sample_rate = 44100
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            44100
        );
        // data chunk
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]),
            100
        );
        assert_eq!(wav.len(), 44 + 100);
    }

    #[test]
    fn test_build_wav_header_float() {
        let pcm = vec![0u8; 200];
        let wav = build_wav(&pcm, 48000, 2, 32, true);
        // audio_format = 3 (IEEE float)
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 3);
        assert_eq!(wav.len(), 44 + 200);
    }

    #[test]
    fn span_returns_exact_length_and_content() {
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(5, RATE, 2);
        buf.push_at(&pcm(48_000, 0.5), t0); // 1 s ending at t0 + 1 s
        let out = buf.span_f32(t0 + ms(250), t0 + ms(750));
        assert_eq!(out.len(), 24_000 * 2);
        assert!(out.iter().all(|&s| s == 0.5));
    }

    #[test]
    fn span_pads_uncovered_time_with_silence() {
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(5, RATE, 2);
        buf.push_at(&pcm(48_000, 0.5), t0 + ms(1000)); // covers 1 s..2 s
        let out = buf.span_f32(t0, t0 + ms(3000));
        assert_eq!(out.len(), 144_000 * 2);
        let sec = |i: usize| &out[i * 96_000..(i + 1) * 96_000];
        assert!(sec(0).iter().all(|&s| s == 0.0), "before capture");
        assert!(sec(1).iter().all(|&s| s == 0.5), "captured");
        assert!(sec(2).iter().all(|&s| s == 0.0), "after capture stopped");
    }

    #[test]
    fn delivery_gap_is_filled_so_later_audio_keeps_its_time() {
        // A source that goes quiet and stops delivering for 2 s must not
        // pull the audio after the gap 2 s earlier.
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(10, RATE, 2);
        buf.push_at(&pcm(48_000, 0.25), t0);
        buf.push_at(&pcm(48_000, 0.75), t0 + ms(3000));
        let out = buf.span_f32(t0, t0 + ms(4000));
        let at = |sec: f64| out[(sec * 48_000.0) as usize * 2];
        assert_eq!(at(0.5), 0.25);
        assert_eq!(at(2.0), 0.0);
        assert_eq!(at(3.5), 0.75);
    }

    #[test]
    fn small_jitter_does_not_insert_silence() {
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(5, RATE, 2);
        for i in 0..100u64 {
            // 10 ms packets, stamped up to 5 ms late.
            buf.push_at(&pcm(480, 1.0), t0 + ms(i * 10 + (i % 3) * 2));
        }
        assert_eq!(buf.filled, 48_000 * 8);
    }

    #[test]
    fn ring_wraps_and_keeps_newest() {
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(1, RATE, 2); // 3 s capacity
        for i in 0..5u64 {
            buf.push_at(&pcm(48_000, i as f32), t0 + ms(i * 1000));
        }
        assert_eq!(buf.filled, buf.data.len());
        let out = buf.span_f32(t0 + ms(2000), t0 + ms(5000));
        assert_eq!(out[0], 2.0);
        assert_eq!(out[96_000], 3.0);
        assert_eq!(out[out.len() - 1], 4.0);
    }

    #[test]
    fn silence_packets_advance_time() {
        let t0 = Instant::now();
        let mut buf = AudioRingBuffer::new(5, RATE, 2);
        buf.push_silence_at(48_000 * 8, t0);
        buf.push_at(&pcm(48_000, 0.5), t0 + ms(1000));
        let out = buf.span_f32(t0, t0 + ms(2000));
        assert_eq!(out[0], 0.0);
        assert_eq!(out[out.len() - 1], 0.5);
        assert_eq!(buf.filled, 2 * 48_000 * 8);
    }

    #[test]
    fn mixer_aligns_channels_by_wall_time() {
        let t0 = Instant::now();
        let mut mixer = AudioMixer::new(5);
        for (vol, start, v) in [(1.0, 0u64, 0.25f32), (0.5, 1000, 0.5)] {
            let buffer = new_canonical_buffer(5);
            buffer.lock().push_at(&pcm(48_000, v), t0 + ms(start));
            mixer.channels.push(AudioChannel {
                kind: ChannelKind::Microphone,
                volume: vol,
                running: Arc::new(AtomicBool::new(false)),
                buffer,
                thread: None,
            });
        }
        let wav = mixer.snapshot_mixed_wav_span(t0, t0 + ms(2000));
        let samples: Vec<f32> = wav[44..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(samples.len(), 96_000 * 2);
        assert_eq!(samples[0], 0.25); // first channel only
        assert_eq!(samples[samples.len() - 1], 0.25); // second, at half volume
    }

    #[test]
    fn mixer_without_captured_audio_returns_empty() {
        let mixer = AudioMixer::new(5);
        let now = Instant::now();
        assert!(mixer
            .snapshot_mixed_wav_span(now, now + ms(1000))
            .is_empty());
    }

    /// Whole save pipeline, A/V sync: a white flash in the video and a click
    /// in the audio happen at the same wall-clock moment; in the saved MP4
    /// they must land within ~2 frames of each other. Uses the real encoder,
    /// TS ring and remux. Opt-in: `cargo test -- --ignored av_sync`.
    #[test]
    #[ignore]
    fn av_sync_flash_and_click_line_up_in_saved_clip() {
        use crate::encoding::config::{EncoderConfig, RateControl, ScaleFilter, X264Preset};
        use crate::encoding::encoder::{LiveEncoder, RawFrame};
        use crate::encoding::hw::ffmpeg_cmd;

        const FPS: u32 = 60;
        let enc = LiveEncoder::start(EncoderConfig {
            width: 320,
            height: 240,
            fps: FPS,
            buffer_seconds: 30,
            output_width: 320,
            output_height: 240,
            rate_control: RateControl::Quality,
            crf: 23,
            video_bitrate_kbps: 6000,
            x264_preset: X264Preset::Veryfast,
            audio_bitrate_kbps: 128,
            scale_filter: ScaleFilter::Lanczos,
        })
        .expect("ffmpeg available");

        let mut mixer = AudioMixer::new(30);
        let ring = new_canonical_buffer(30);
        mixer.channels.push(AudioChannel {
            kind: ChannelKind::Game,
            volume: 1.0,
            running: Arc::new(AtomicBool::new(false)),
            buffer: ring.clone(),
            thread: None,
        });

        let t0 = Instant::now();
        let flash = t0 + ms(1500);
        let flash_end = flash + ms(500);
        let lit = |t: Instant| t >= flash && t < flash_end;
        let tx = enc.frame_sender();
        let mut audio_end = t0;
        while audio_end < t0 + ms(4000) {
            std::thread::sleep(Duration::from_secs_f64(1.0 / FPS as f64));
            let now = Instant::now();
            let level = if lit(now) { 255 } else { 0 };
            let _ = tx.try_send(RawFrame {
                data: vec![level; 320 * 240 * 4],
                width: 320,
                height: 240,
            });
            let frames = ((now - audio_end).as_secs_f64() * RATE as f64) as usize;
            let samples: Vec<f32> = (0..frames)
                .flat_map(|i| {
                    let t = audio_end + Duration::from_secs_f64(i as f64 / RATE as f64);
                    let v = if lit(t) { 0.8 } else { 0.0 };
                    [v, v]
                })
                .collect();
            ring.lock()
                .push_at(&f32_slice_to_bytes(&samples), audio_end);
            audio_end += Duration::from_secs_f64(frames as f64 / RATE as f64);
        }

        let video = enc.snapshot().expect("encoded video");
        let wav = mixer.snapshot_mixed_wav_span(video.start, video.end);
        let dir = std::env::temp_dir().join(format!("rsclip_avsync_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let request = enc.save_request(video, &dir.to_string_lossy(), Some(wav));
        crate::encoding::remux::remux_to_mp4(&request).expect("remux");
        let clip = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();

        let out = ffmpeg_cmd()
            .args(["-hide_banner", "-i"])
            .arg(&clip)
            .args([
                "-vf",
                "blackdetect=d=0.01:pix_th=0.5",
                "-af",
                "silencedetect=n=-30dB:d=0.01",
            ])
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stderr);
        let first = |key: &str| -> f64 {
            let i = log.find(key).unwrap_or_else(|| {
                panic!(
                    "no {key} in:
{log}"
                )
            });
            log[i + key.len()..]
                .split(|c: char| c.is_whitespace() || c == '|')
                .find(|s| !s.is_empty())
                .and_then(|v| v.parse().ok())
                .unwrap()
        };
        let video_flash = first("black_end:");
        let audio_click = first("silence_end:");
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("flash at {video_flash:.3}s, click at {audio_click:.3}s");
        assert!(
            (video_flash - audio_click).abs() <= 2.0 / FPS as f64 + 0.005,
            "A/V offset {:.1} ms",
            (audio_click - video_flash) * 1000.0
        );
    }

    #[test]
    fn test_f32_slice_to_bytes_roundtrip() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.0];
        let bytes = f32_slice_to_bytes(&samples);
        assert_eq!(bytes.len(), 16);
        let round: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(samples, round);
    }
}
