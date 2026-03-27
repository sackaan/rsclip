// Audio capture and mixing.
//
// Each audio source (game, microphone) gets its own `AudioChannel` with a
// ring buffer. At save time the `AudioMixer` blends all channels into a
// single WAV.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use log::{info, warn};
use parking_lot::Mutex;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use super::wasapi;

pub use wasapi::MicDevice;

pub(super) struct AudioRingBuffer {
    data: Vec<u8>,
    write_pos: usize,
    filled: usize,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    is_float: bool,
    /// Monotonic count of all bytes ever pushed (including silence).
    /// Used with `capture_start` to compute the effective byte rate,
    /// compensating for audio hardware clock drift — the same approach
    /// OBS uses (sample-count based timing instead of system clock).
    total_bytes_written: u64,
    /// Wall-clock instant when the first audio data arrived.
    capture_start: Option<Instant>,
}

impl AudioRingBuffer {
    fn new(
        max_seconds: u32,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        is_float: bool,
    ) -> Self {
        let block_align = channels * (bits_per_sample / 8);
        let bytes_per_sec = sample_rate as usize * block_align as usize;
        // +2 seconds margin (same as video buffer).
        let max_bytes = bytes_per_sec * (max_seconds as usize + 2);

        Self {
            data: vec![0u8; max_bytes],
            write_pos: 0,
            filled: 0,
            sample_rate,
            channels,
            bits_per_sample,
            block_align,
            is_float,
            total_bytes_written: 0,
            capture_start: None,
        }
    }

    /// Effective byte rate derived from actual capture timing.
    ///
    /// Audio hardware clocks drift from their nominal rate (e.g. a "48 kHz"
    /// device might actually run at 47 999 Hz). Over long sessions this causes
    /// audio/video desync. By dividing the total bytes the hardware delivered
    /// by the wall-clock time elapsed we get the *real* rate, which we then
    /// use when extracting a time-based snapshot from the ring buffer.
    fn effective_bytes_per_sec(&self) -> f64 {
        let nominal = self.sample_rate as f64 * self.block_align as f64;
        if let Some(start) = self.capture_start {
            let elapsed = start.elapsed().as_secs_f64();
            // Only trust the measurement after enough data has accumulated.
            if elapsed >= 1.0 && self.total_bytes_written > nominal as u64 {
                return self.total_bytes_written as f64 / elapsed;
            }
        }
        nominal
    }

    pub(super) fn push(&mut self, pcm: &[u8]) {
        let cap = self.data.len();
        if cap == 0 || pcm.is_empty() {
            return;
        }

        if self.capture_start.is_none() {
            self.capture_start = Some(Instant::now());
        }
        self.total_bytes_written += pcm.len() as u64;

        let len = pcm.len();
        if len >= cap {
            // Data larger than buffer — keep only the last `cap` bytes.
            self.data.copy_from_slice(&pcm[len - cap..]);
            self.write_pos = 0;
            self.filled = cap;
            return;
        }

        let space_to_end = cap - self.write_pos;
        if len <= space_to_end {
            self.data[self.write_pos..self.write_pos + len].copy_from_slice(pcm);
        } else {
            self.data[self.write_pos..].copy_from_slice(&pcm[..space_to_end]);
            self.data[..len - space_to_end].copy_from_slice(&pcm[space_to_end..]);
        }
        self.write_pos = (self.write_pos + len) % cap;
        self.filled = (self.filled + len).min(cap);
    }

    pub(super) fn push_silence(&mut self, count: usize) {
        let cap = self.data.len();
        if cap == 0 || count == 0 {
            return;
        }

        if self.capture_start.is_none() {
            self.capture_start = Some(Instant::now());
        }
        self.total_bytes_written += count as u64;

        let len = count.min(cap);
        let space_to_end = cap - self.write_pos;
        if len <= space_to_end {
            self.data[self.write_pos..self.write_pos + len].fill(0);
        } else {
            self.data[self.write_pos..].fill(0);
            self.data[..len - space_to_end].fill(0);
        }
        self.write_pos = (self.write_pos + len) % cap;
        self.filled = (self.filled + len).min(cap);
    }

    fn snapshot_pcm_f32(&self, seconds: f64) -> Vec<f32> {
        if self.block_align == 0 || self.sample_rate == 0 {
            return Vec::new();
        }

        let bytes_per_sec = self.effective_bytes_per_sec();
        let requested = (seconds * bytes_per_sec) as usize;
        let available = requested.min(self.filled);
        let available = (available / self.block_align as usize) * self.block_align as usize;
        if available == 0 {
            return Vec::new();
        }

        let cap = self.data.len();
        let start = if self.write_pos >= available {
            self.write_pos - available
        } else {
            cap - (available - self.write_pos)
        };

        let mut pcm_bytes = vec![0u8; available];
        if start + available <= cap {
            pcm_bytes.copy_from_slice(&self.data[start..start + available]);
        } else {
            let first = cap - start;
            pcm_bytes[..first].copy_from_slice(&self.data[start..]);
            pcm_bytes[first..].copy_from_slice(&self.data[..available - first]);
        }

        pcm_bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    fn snapshot_wav(&self, seconds: f64) -> Vec<u8> {
        if self.block_align == 0 || self.sample_rate == 0 {
            return Vec::new();
        }

        let bytes_per_sec = self.effective_bytes_per_sec();
        let requested = (seconds * bytes_per_sec) as usize;
        let available = requested.min(self.filled);
        let available = (available / self.block_align as usize) * self.block_align as usize;
        if available == 0 {
            return Vec::new();
        }

        let cap = self.data.len();
        let start = if self.write_pos >= available {
            self.write_pos - available
        } else {
            cap - (available - self.write_pos)
        };

        let mut pcm = vec![0u8; available];
        if start + available <= cap {
            pcm.copy_from_slice(&self.data[start..start + available]);
        } else {
            let first = cap - start;
            pcm[..first].copy_from_slice(&self.data[start..]);
            pcm[first..].copy_from_slice(&self.data[..available - first]);
        }

        build_wav(
            &pcm,
            self.sample_rate,
            self.channels,
            self.bits_per_sample,
            self.is_float,
        )
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
    Arc::new(Mutex::new(AudioRingBuffer::new(
        buffer_seconds,
        48000,
        2,
        32,
        true,
    )))
}

/// The kind of audio source backing a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// Game audio captured via WASAPI process loopback.
    Game,
    /// Microphone input captured via WASAPI device capture.
    Microphone,
}

pub struct AudioChannel {
    pub kind: ChannelKind,
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
        assert_eq!(self.kind, ChannelKind::Game);

        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }

        self.running = Arc::new(AtomicBool::new(true));
        self.thread = Some(spawn_game_thread(&self.running, &self.buffer, new_pid)?);
        Ok(())
    }

    fn snapshot_pcm_f32(&self, seconds: f64) -> Vec<f32> {
        self.buffer.lock().snapshot_pcm_f32(seconds)
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

/// Manages multiple audio sources and mixes them at save time.
pub struct AudioMixer {
    channels: Vec<AudioChannel>,
    buffer_seconds: u32,
}

impl AudioMixer {
    pub fn new(buffer_seconds: u32) -> Self {
        Self {
            channels: Vec::new(),
            buffer_seconds,
        }
    }

    pub fn add_game_channel(&mut self, pid: u32) -> Result<()> {
        let mut ch = AudioChannel::start_game(self.buffer_seconds, pid)?;
        ch.volume = 1.0;
        self.channels.push(ch);
        Ok(())
    }

    pub fn add_mic_channel(&mut self, device_id: &str) -> Result<()> {
        let mut ch = AudioChannel::start_mic(self.buffer_seconds, device_id)?;
        ch.volume = 1.0;
        self.channels.push(ch);
        Ok(())
    }

    pub fn remove_mic_channel(&mut self) {
        self.channels
            .retain(|ch| ch.kind != ChannelKind::Microphone);
    }

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

    pub fn game_channel_mut(&mut self) -> Option<&mut AudioChannel> {
        self.channels
            .iter_mut()
            .find(|c| c.kind == ChannelKind::Game)
    }

    pub fn mic_channel_mut(&mut self) -> Option<&mut AudioChannel> {
        self.channels
            .iter_mut()
            .find(|c| c.kind == ChannelKind::Microphone)
    }

    pub fn has_mic(&self) -> bool {
        self.channels
            .iter()
            .any(|c| c.kind == ChannelKind::Microphone)
    }

    /// Mix all active channels into a single WAV. Shorter channels are
    /// right-aligned so audio aligns to "now" at the end.
    pub fn snapshot_mixed_wav(&self, seconds: f64) -> Vec<u8> {
        let active: Vec<_> = self.channels.iter().filter(|c| c.volume > 0.0).collect();

        if active.is_empty() {
            return Vec::new();
        }

        if active.len() == 1 {
            let ch = active[0];
            let pcm = ch.snapshot_pcm_f32(seconds);
            if pcm.is_empty() {
                return Vec::new();
            }
            let bytes = if (ch.volume - 1.0).abs() < f32::EPSILON {
                f32_slice_to_bytes(&pcm)
            } else {
                let scaled: Vec<f32> = pcm
                    .iter()
                    .map(|&s| (s * ch.volume).clamp(-1.0, 1.0))
                    .collect();
                f32_slice_to_bytes(&scaled)
            };
            return build_wav(&bytes, 48000, 2, 32, true);
        }

        let channel_pcms: Vec<(f32, Vec<f32>)> = active
            .iter()
            .map(|ch| (ch.volume, ch.snapshot_pcm_f32(seconds)))
            .collect();

        let max_len = channel_pcms.iter().map(|(_, p)| p.len()).max().unwrap_or(0);
        if max_len == 0 {
            return Vec::new();
        }

        let mut mixed = vec![0.0f32; max_len];

        for (volume, pcm) in &channel_pcms {
            let offset = max_len - pcm.len();
            for (i, &sample) in pcm.iter().enumerate() {
                mixed[offset + i] += sample * volume;
            }
        }

        for s in &mut mixed {
            *s = s.clamp(-1.0, 1.0);
        }

        let bytes = f32_slice_to_bytes(&mixed);
        build_wav(&bytes, 48000, 2, 32, true)
    }

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
    fn test_ring_buffer_push_and_snapshot() {
        let mut buf = AudioRingBuffer::new(1, 48000, 2, 32, true);
        let block = vec![1u8; 384]; // 1 frame = 8 bytes, 48 frames
        buf.push(&block);
        let wav = buf.snapshot_wav(0.01);
        assert!(!wav.is_empty());
        assert_eq!(&wav[0..4], b"RIFF");
    }

    #[test]
    fn test_ring_buffer_wraps() {
        // Small buffer: 1 second at 100 Hz, mono, 8-bit = ~(100+2)*1 = 102 bytes
        let mut buf = AudioRingBuffer::new(1, 100, 1, 8, false);
        let cap = buf.data.len();
        // Fill past capacity
        let data = vec![42u8; cap + 10];
        buf.push(&data);
        assert_eq!(buf.filled, cap);
    }

    #[test]
    fn test_ring_buffer_push_silence() {
        let mut buf = AudioRingBuffer::new(1, 48000, 2, 32, true);
        buf.push_silence(384);
        assert_eq!(buf.filled, 384);
        // All pushed bytes should be zero
        let wav = buf.snapshot_wav(0.01);
        // Data portion (after 44-byte header) should be all zeros
        for &b in &wav[44..] {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn test_snapshot_pcm_f32() {
        let mut buf = AudioRingBuffer::new(1, 48000, 2, 32, true);
        // Push a known f32 value (0.5) as 1 stereo frame = 8 bytes.
        let val: f32 = 0.5;
        let bytes = val.to_le_bytes();
        let mut frame = Vec::new();
        frame.extend_from_slice(&bytes); // L
        frame.extend_from_slice(&bytes); // R
        buf.push(&frame);

        let pcm = buf.snapshot_pcm_f32(0.01);
        assert_eq!(pcm.len(), 2); // 1 frame = 2 samples
        assert!((pcm[0] - 0.5).abs() < f32::EPSILON);
        assert!((pcm[1] - 0.5).abs() < f32::EPSILON);
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
