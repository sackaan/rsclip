//! Live encoding pipeline.
//!
//! A persistent FFmpeg subprocess encodes raw BGRA frames to H.264 MPEG-TS
//! in real time. Saving a clip snapshots the encoded buffer into a
//! [`SaveRequest`] that is remuxed to MP4 (stream copy — no re-encode), with
//! the mixed audio muxed in.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};

use parking_lot::{Mutex, RwLock};

use crate::encoding::buffer::{EncodedReplayBuffer, SharedEncodedBuffer, PTS_HZ};
use crate::encoding::config::EncoderConfig;

use super::hw::ffmpeg_cmd;
use super::types::SaveRequest;

pub use super::hw::{detect_best_encoder, is_ffmpeg_available, HwEncoder};
pub use super::types::{build_preview_frame, RawFrame, SharedPreviewFrame};

/// Spare frame buffers kept for reuse by the capture side.
const RECYCLE_DEPTH: usize = 4;

/// A frame written up to this late is stamped with its scheduled time, so
/// OS timer jitter doesn't disturb the exact frame cadence.
const WRITE_JITTER: Duration = Duration::from_millis(20);

/// Falling further behind schedule than this means FFmpeg can't encode in
/// real time. The writer then resyncs to now instead of bursting repeated
/// frames to catch up, and frames are stamped with when they were written.
const MAX_WRITE_LAG: Duration = Duration::from_millis(100);

/// How long frame write times are kept: the longest buffer the settings
/// allow (300 s) plus a GOP and the encoder's latency.
const CLOCK_HISTORY_SECS: u64 = 330;

/// Above this relative difference between a clip's wall-clock span and its
/// encoded duration, the remux retimes the video to the wall-clock span.
const RETIME_TOLERANCE: f64 = 0.001;

/// Manages a persistent FFmpeg process for real-time H.264 encoding.
///
/// Three background threads: writer (feeds frames to stdin), reader
/// (captures encoded TS from stdout), and a stderr logger. The encoded
/// buffer outlives the FFmpeg process, so footage stays savable after
/// [`stop`](LiveEncoder::stop) or an encoder crash.
pub struct LiveEncoder {
    /// Send raw frames here for encoding.
    frame_tx: Sender<RawFrame>,
    /// Frame buffers handed back by the writer for the capture to reuse.
    recycle_rx: Receiver<Vec<u8>>,
    /// The encoded output buffer (shared with reader thread).
    encoded_buffer: SharedEncodedBuffer,
    /// Latest frame snapshot for UI preview.
    preview_frame: SharedPreviewFrame,
    /// Signal threads to stop.
    running: Arc<AtomicBool>,
    /// Set when FFmpeg dies or its pipes break while still running.
    failed: Arc<AtomicBool>,
    /// Set by the writer when FFmpeg falls behind real time.
    overloaded: Arc<AtomicBool>,
    /// Whether [`take_overload_notice`](LiveEncoder::take_overload_notice)
    /// has already reported an overload.
    overload_noticed: AtomicBool,
    /// Audio bitrate for AAC encoding during remux.
    audio_bitrate_kbps: u32,
    /// Which encoder backend is active.
    hw_encoder: HwEncoder,
    /// Frame size the encoder was started with (always even).
    frame_size: (u32, u32),
    /// Output frame rate (constant).
    fps: u32,
    /// When each frame number was written to FFmpeg.
    frame_clock: SharedFrameClock,
}

/// Encoded video taken for a save, with the wall-clock interval it shows.
pub(crate) struct VideoSnapshot {
    /// MPEG-TS bytes, starting on a keyframe.
    pub data: Vec<u8>,
    /// When the first frame's content was captured.
    pub start: Instant,
    /// When the last frame stops being displayed.
    pub end: Instant,
    /// Duration of the encoded stream (frame count / fps). Differs from
    /// the wall-clock span when FFmpeg couldn't keep up.
    pub stream_secs: f64,
}

impl VideoSnapshot {
    /// Length of the clip in seconds.
    pub fn duration_secs(&self) -> f64 {
        (self.end - self.start).as_secs_f64()
    }

    /// Factor that stretches the encoded stream onto the wall-clock span it
    /// shows, or 1.0 when they already agree.
    pub fn time_scale(&self) -> f64 {
        if self.stream_secs <= 0.0 {
            return 1.0;
        }
        let scale = self.duration_secs() / self.stream_secs;
        if (scale - 1.0).abs() > RETIME_TOLERANCE {
            scale
        } else {
            1.0
        }
    }
}

/// When each frame number was written to FFmpeg. The encoder stamps frame
/// `n` with PTS `n / fps`; this maps it back to the moment it shows.
#[derive(Default)]
struct FrameClock {
    /// Frame number of `times[0]`.
    first: u64,
    times: VecDeque<Instant>,
}

type SharedFrameClock = Arc<Mutex<FrameClock>>;

impl FrameClock {
    /// Record the next frame's time, keeping at most `keep` entries.
    fn push(&mut self, at: Instant, keep: usize) {
        self.times.push_back(at);
        while self.times.len() > keep.max(1) {
            self.times.pop_front();
            self.first += 1;
        }
    }

    /// Time of frame `n`, clamped to the frames still on record.
    fn at(&self, n: u64) -> Option<Instant> {
        let last = self.times.len().checked_sub(1)?;
        let i = n.saturating_sub(self.first).min(last as u64) as usize;
        Some(self.times[i])
    }
}

impl LiveEncoder {
    /// Spawn FFmpeg with the best detected encoder. See [`start_with`].
    ///
    /// [`start_with`]: LiveEncoder::start_with
    pub fn start(cfg: EncoderConfig) -> anyhow::Result<Self> {
        Self::start_with(cfg, detect_best_encoder())
    }

    /// Spawn FFmpeg using `encoder` and the supporting threads. Returns once
    /// the child is running and ready to receive frames on the
    /// [`frame_sender`].
    ///
    /// [`frame_sender`]: LiveEncoder::frame_sender
    pub fn start_with(cfg: EncoderConfig, encoder: HwEncoder) -> anyhow::Result<Self> {
        let enc_width = cfg.width & !1;
        let enc_height = cfg.height & !1;
        anyhow::ensure!(
            enc_width > 0 && enc_height > 0,
            "capture area too small ({}x{})",
            cfg.width,
            cfg.height
        );

        let running = Arc::new(AtomicBool::new(true));
        let failed = Arc::new(AtomicBool::new(false));
        let encoded_buffer: SharedEncodedBuffer =
            Arc::new(RwLock::new(EncodedReplayBuffer::new(cfg.buffer_seconds)));
        let preview_frame: SharedPreviewFrame = Arc::new(parking_lot::Mutex::new(None));

        let fps = cfg.fps;
        let overloaded = Arc::new(AtomicBool::new(false));
        let frame_clock: SharedFrameClock = Arc::default();
        let audio_bitrate_kbps = cfg.audio_bitrate_kbps;

        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<RawFrame>(2);
        let (recycle_tx, recycle_rx) = crossbeam_channel::bounded::<Vec<u8>>(RECYCLE_DEPTH);

        let mut hw_encoder = encoder;
        let mut child = match Self::start_ffmpeg(enc_width, enc_height, &cfg, &hw_encoder) {
            Ok(c) => c,
            Err(e) if hw_encoder != HwEncoder::Software => {
                warn!("Hardware encoder failed ({}), trying libx264", e);
                hw_encoder = HwEncoder::Software;
                Self::start_ffmpeg(enc_width, enc_height, &cfg, &hw_encoder)?
            }
            Err(e) => return Err(e),
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("FFmpeg child did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("FFmpeg child did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("FFmpeg child did not expose stderr"))?;

        std::thread::Builder::new()
            .name("encoder-stderr".into())
            .spawn(move || {
                use std::io::BufRead;
                let reader = std::io::BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) if !l.is_empty() => warn!("FFmpeg: {}", l),
                        Err(_) => break,
                        _ => {}
                    }
                }
            })?;

        // Writer thread: clock-disciplined pacing to maintain constant frame rate.
        let writer = FrameWriter {
            frame_rx,
            recycle_tx,
            preview: preview_frame.clone(),
            running: running.clone(),
            failed: failed.clone(),
            overloaded: overloaded.clone(),
            clock: frame_clock.clone(),
            frame_size: (enc_width, enc_height),
            fps,
        };
        std::thread::Builder::new()
            .name("encoder-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                writer.run(&mut stdin);
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                info!("Encoder writer thread stopped");
            })?;

        let running_r = running.clone();
        let failed_r = failed.clone();
        let buffer_clone = encoded_buffer.clone();
        std::thread::Builder::new()
            .name("encoder-reader".into())
            .spawn(move || {
                let mut stdout = stdout;
                let mut buf = vec![0u8; 65536];
                loop {
                    if !running_r.load(Ordering::Relaxed) {
                        break;
                    }
                    match stdout.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let _t =
                                crate::metrics::Timer::start(crate::metrics::Stage::BufferPush);
                            buffer_clone.write().push(&buf[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            warn!("Encoder reader error: {}", e);
                            break;
                        }
                    }
                }
                if running_r.load(Ordering::Relaxed) {
                    warn!("FFmpeg output ended unexpectedly");
                    failed_r.store(true, Ordering::SeqCst);
                }
                info!("Encoder reader thread stopped");
            })?;

        info!(
            "Live encoder started: {}x{} @ {} fps, codec: {:?}",
            enc_width, enc_height, fps, hw_encoder
        );

        Ok(Self {
            frame_tx,
            recycle_rx,
            encoded_buffer,
            preview_frame,
            running,
            failed,
            overloaded,
            overload_noticed: AtomicBool::new(false),
            audio_bitrate_kbps,
            hw_encoder,
            frame_size: (enc_width, enc_height),
            fps,
            frame_clock,
        })
    }

    fn start_ffmpeg(
        enc_width: u32,
        enc_height: u32,
        cfg: &EncoderConfig,
        encoder: &HwEncoder,
    ) -> anyhow::Result<Child> {
        let gop = cfg.fps.to_string();
        let mut cmd = ffmpeg_cmd();

        cmd.arg("-y")
            .arg("-hide_banner")
            .args(["-loglevel", "warning"])
            .args(["-f", "rawvideo"])
            .args(["-pixel_format", "bgra"])
            .args(["-video_size", &format!("{}x{}", enc_width, enc_height)])
            .args(["-framerate", &cfg.fps.to_string()])
            .args(["-i", "pipe:0"])
            .args(["-c:v", encoder.codec_name()]);

        for arg in encoder.quality_args(
            cfg.rate_control,
            cfg.crf,
            cfg.video_bitrate_kbps,
            cfg.x264_preset,
        ) {
            cmd.arg(arg);
        }

        cmd.args(["-vf", &video_filter(enc_width, enc_height, cfg)]);

        cmd.args(["-pix_fmt", encoder.pix_fmt()])
            .args(["-g", &gop])
            .args(["-f", "mpegts"])
            .args(["-flush_packets", "1"])
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("Failed to start FFmpeg: {}", e))
    }

    /// Clone the channel used to feed raw frames into the encoder.
    pub fn frame_sender(&self) -> Sender<RawFrame> {
        self.frame_tx.clone()
    }

    /// Clone the channel of spent frame buffers the capture should reuse.
    pub fn frame_recycler(&self) -> Receiver<Vec<u8>> {
        self.recycle_rx.clone()
    }

    /// Frame size (always even) every submitted frame must have.
    pub fn frame_size(&self) -> (u32, u32) {
        self.frame_size
    }

    /// Borrow the shared encoded ring buffer (read by the GUI for status,
    /// snapshotted on save).
    pub fn encoded_buffer(&self) -> &SharedEncodedBuffer {
        &self.encoded_buffer
    }

    /// Borrow the latest downscaled preview frame (refreshed ~4× per second).
    pub fn preview_frame(&self) -> &SharedPreviewFrame {
        &self.preview_frame
    }

    /// Which hardware encoder ended up active (`Software` = libx264).
    pub fn hw_encoder(&self) -> HwEncoder {
        self.hw_encoder
    }

    /// Whether FFmpeg stopped on its own (crash, broken pipe) rather than
    /// through [`stop`](LiveEncoder::stop).
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// `true` the first time FFmpeg is seen falling behind real time, so
    /// the UI can warn once per recording.
    pub fn take_overload_notice(&self) -> bool {
        self.overloaded.load(Ordering::Relaxed)
            && !self.overload_noticed.swap(true, Ordering::Relaxed)
    }

    /// Snapshot the encoded buffer together with the wall-clock interval
    /// it covers, so audio can be cut to exactly the same moments. Returns
    /// `None` until the first keyframe has been encoded.
    pub(crate) fn snapshot(&self) -> Option<VideoSnapshot> {
        let snap = self.encoded_buffer.read().snapshot()?;
        let span = frame_span(
            &self.frame_clock.lock(),
            self.fps,
            snap.base_pts,
            snap.start_pts,
            snap.end_pts,
        )?;
        Some(VideoSnapshot {
            data: snap.data,
            start: span.start,
            end: span.end,
            stream_secs: span.stream_secs,
        })
    }

    /// Package a snapshot and its matching audio for the save worker.
    pub(crate) fn save_request(
        &self,
        video: VideoSnapshot,
        output_dir: &str,
        audio_wav: Option<Vec<u8>>,
    ) -> SaveRequest {
        SaveRequest {
            video_duration_secs: video.duration_secs(),
            video_time_scale: video.time_scale(),
            encoded_data: video.data,
            output_dir: output_dir.to_string(),
            audio_wav,
            audio_bitrate_kbps: self.audio_bitrate_kbps,
        }
    }

    /// Signal all worker threads to wind down. Idempotent. The encoded
    /// buffer is kept.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for LiveEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Writer-thread state: paces frames into FFmpeg's stdin at a constant
/// rate, repeating the newest captured frame when no new one arrived.
struct FrameWriter {
    frame_rx: Receiver<RawFrame>,
    recycle_tx: Sender<Vec<u8>>,
    preview: SharedPreviewFrame,
    running: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    overloaded: Arc<AtomicBool>,
    clock: SharedFrameClock,
    frame_size: (u32, u32),
    fps: u32,
}

impl FrameWriter {
    /// Write frames to `out` until stopped or the pipe breaks.
    ///
    /// The encoder numbers frames in the order they are written, so every
    /// write records the moment that frame shows in [`FrameClock`]. On
    /// schedule that is the scheduled time, keeping the cadence exact. If
    /// FFmpeg can't keep up, the writer stamps frames with when they were
    /// actually written and resyncs instead of bursting repeats; otherwise
    /// the encoded timeline would drift ever further behind wall time.
    fn run(self, out: &mut impl Write) {
        let (width, height) = self.frame_size;
        let frame_len = width as usize * height as usize * 4;
        let fps = self.fps.max(1);
        let period = Duration::from_nanos(1_000_000_000 / fps as u64);
        let keep = (CLOCK_HISTORY_SECS * fps as u64) as usize;
        let mut last_preview = Instant::now();
        let mut last_frame_data: Option<Vec<u8>> = None;
        // When the next frame is due; `None` until the first frame.
        let mut next_due: Option<Instant> = None;

        loop {
            if !self.running.load(Ordering::Relaxed) {
                break;
            }

            let got_frame = match next_due {
                // Nothing to pace (or repeat) until the first frame.
                None => match self.frame_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(raw_frame) => Some(raw_frame),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                },
                Some(due) => {
                    let wait = due.saturating_duration_since(Instant::now());
                    if wait > Duration::ZERO {
                        match self.frame_rx.recv_timeout(wait) {
                            Ok(raw_frame) => Some(raw_frame),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        self.frame_rx.try_recv().ok()
                    }
                }
            };

            // Every frame — fresh or repeated — is a packed buffer of
            // exactly the encoder's size, written in one call.
            let data = match got_frame {
                Some(raw_frame) => {
                    if raw_frame.width != width
                        || raw_frame.height != height
                        || raw_frame.data.len() != frame_len
                    {
                        continue;
                    }
                    if last_preview.elapsed() >= Duration::from_millis(250) {
                        last_preview = Instant::now();
                        let pf = build_preview_frame(&raw_frame);
                        *self.preview.lock() = Some(pf);
                    }
                    if let Some(old) = last_frame_data.replace(raw_frame.data) {
                        let _ = self.recycle_tx.try_send(old);
                    }
                    last_frame_data.as_deref()
                }
                None => last_frame_data.as_deref(),
            };
            let Some(data) = data else {
                continue;
            };

            let now = Instant::now();
            let due = next_due.unwrap_or(now);
            let shown_at = if now.saturating_duration_since(due) <= WRITE_JITTER {
                due
            } else {
                now
            };
            self.clock.lock().push(shown_at, keep);

            if out.write_all(data).is_err() {
                if self.running.load(Ordering::Relaxed) {
                    warn!("FFmpeg stdin write failed — encoder may have crashed");
                    self.failed.store(true, Ordering::SeqCst);
                }
                break;
            }

            let after = Instant::now();
            let following = due + period;
            next_due = Some(
                if after.saturating_duration_since(following) > MAX_WRITE_LAG {
                    if !self.overloaded.swap(true, Ordering::Relaxed) {
                        warn!(
                            "Encoder can't keep up with {} fps; clips are retimed to real time",
                            fps
                        );
                    }
                    after
                } else {
                    following
                },
            );
        }
    }
}

/// Wall-clock interval shown by a run of buffered frames.
struct FrameSpan {
    start: Instant,
    end: Instant,
    /// Duration of those frames in the encoded stream.
    stream_secs: f64,
}

/// Map the PTS range of buffered frames to the wall-clock interval they
/// show. The encoder stamps frame `n` with `base_pts + n * 90000 / fps`;
/// `clock` says when frame `n` was written. The last frame stays on screen
/// for one more frame period. `None` if no frame has been written.
fn frame_span(
    clock: &FrameClock,
    fps: u32,
    base_pts: i64,
    start_pts: i64,
    end_pts: i64,
) -> Option<FrameSpan> {
    let fps = fps.max(1) as f64;
    let frame = |pts: i64| {
        ((pts - base_pts) as f64 * fps / PTS_HZ as f64)
            .round()
            .max(0.0) as u64
    };
    let (first, last) = (frame(start_pts), frame(end_pts).max(frame(start_pts)));
    let period = Duration::from_secs_f64(1.0 / fps);
    Some(FrameSpan {
        start: clock.at(first)?,
        end: clock.at(last)? + period,
        stream_secs: (last + 1 - first) as f64 / fps,
    })
}

/// Colour conversion applied to every frame: BGRA is converted to
/// limited-range BT.709 YUV and the stream is tagged as such. Without this
/// swscale uses BT.601 and leaves the stream untagged, so players that
/// assume BT.709 for HD content shift hues.
const BT709: &str = "out_color_matrix=bt709:out_range=tv";
const BT709_TAGS: &str =
    "setparams=colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv";

/// FFmpeg `-vf` chain for an `in_w`×`in_h` input: optional scaling to the
/// configured output size (aspect ratio kept, padded with black instead of
/// stretched) plus BT.709 colour conversion and tagging.
fn video_filter(in_w: u32, in_h: u32, cfg: &EncoderConfig) -> String {
    let out_w = cfg.output_width & !1;
    let out_h = cfg.output_height & !1;
    if out_w == 0 || out_h == 0 || (out_w == in_w && out_h == in_h) {
        return format!("scale={BT709},{BT709_TAGS}");
    }
    let (w, h, f) = (out_w, out_h, cfg.scale_filter.as_str());
    format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags={f}:{BT709},\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,{BT709_TAGS}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::config::{RateControl, ScaleFilter, X264Preset};

    fn cfg(out_w: u32, out_h: u32) -> EncoderConfig {
        EncoderConfig {
            width: 765,
            height: 503,
            fps: 60,
            buffer_seconds: 30,
            output_width: out_w,
            output_height: out_h,
            rate_control: RateControl::Quality,
            crf: 23,
            video_bitrate_kbps: 6000,
            x264_preset: X264Preset::Veryfast,
            audio_bitrate_kbps: 128,
            scale_filter: ScaleFilter::Lanczos,
        }
    }

    /// A clock with frame `n` written at `t0 + n * spacing`.
    fn clock_with_spacing(t0: Instant, frames: u64, spacing: Duration) -> FrameClock {
        let mut clock = FrameClock::default();
        for n in 0..frames {
            clock.push(t0 + spacing * n as u32, usize::MAX);
        }
        clock
    }

    #[test]
    fn frame_span_maps_pts_to_write_times() {
        let t0 = Instant::now();
        let clock = clock_with_spacing(t0, 400, Duration::from_secs(1) / 60);
        // 60 fps: 1500 ticks per frame. Frames 120..=299 buffered.
        let span = frame_span(
            &clock,
            60,
            126_000,
            126_000 + 120 * 1500,
            126_000 + 299 * 1500,
        )
        .unwrap();
        let ms = |i: Instant| (i - t0).as_secs_f64() * 1000.0;
        assert!((ms(span.start) - 2000.0).abs() < 0.01);
        assert!((ms(span.end) - 5000.0).abs() < 0.01);
        // 180 frames -> exactly 3 s of clip, and no retiming needed.
        assert!((span.stream_secs - 3.0).abs() < 1e-9);
        let snap = VideoSnapshot {
            data: Vec::new(),
            start: span.start,
            end: span.end,
            stream_secs: span.stream_secs,
        };
        assert_eq!(snap.time_scale(), 1.0);
    }

    #[test]
    fn frame_span_follows_a_writer_that_fell_behind() {
        // FFmpeg only took a frame every 1.5 periods: 180 frames (3 s of
        // stream) were written over 4.5 s of wall time.
        let t0 = Instant::now();
        let clock = clock_with_spacing(t0, 400, Duration::from_secs(1) / 40);
        let span = frame_span(&clock, 60, 0, 120 * 1500, 299 * 1500).unwrap();
        let ms = |i: Instant| (i - t0).as_secs_f64() * 1000.0;
        assert!((ms(span.start) - 3000.0).abs() < 0.01);
        assert!((ms(span.end) - (299.0 * 25.0 + 1000.0 / 60.0)).abs() < 0.01);
        assert!((span.stream_secs - 3.0).abs() < 1e-9);
        let snap = VideoSnapshot {
            data: Vec::new(),
            start: span.start,
            end: span.end,
            stream_secs: span.stream_secs,
        };
        let expected = (179.0 * 25.0 + 1000.0 / 60.0) / 3000.0;
        assert!((snap.time_scale() - expected).abs() < 1e-6);
    }

    #[test]
    fn frame_clock_clamps_to_retained_frames() {
        let t0 = Instant::now();
        let mut clock = FrameClock::default();
        assert!(clock.at(0).is_none());
        for n in 0..10u32 {
            clock.push(t0 + Duration::from_millis(10) * n, 4);
        }
        // Frames 6..=9 are kept.
        assert_eq!(clock.at(0), Some(t0 + Duration::from_millis(60)));
        assert_eq!(clock.at(7), Some(t0 + Duration::from_millis(70)));
        assert_eq!(clock.at(99), Some(t0 + Duration::from_millis(90)));
    }

    /// Sink that takes `delay` per frame, like an FFmpeg that encodes
    /// slower than real time, and records when each write began.
    struct SlowSink {
        delay: Duration,
        starts: Arc<Mutex<Vec<Instant>>>,
    }

    impl Write for SlowSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.starts.lock().push(Instant::now());
            std::thread::sleep(self.delay);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Feed frames at `fps` for `run_for` into a writer whose sink takes
    /// `delay` per frame. Returns the recorded frame times, when each write
    /// actually began, and whether the writer reported an overload.
    fn drive_writer(
        fps: u32,
        delay: Duration,
        run_for: Duration,
    ) -> (Vec<Instant>, Vec<Instant>, bool) {
        let (frame_tx, frame_rx) = crossbeam_channel::bounded(2);
        let (recycle_tx, _recycle_rx) = crossbeam_channel::bounded(RECYCLE_DEPTH);
        let running = Arc::new(AtomicBool::new(true));
        let overloaded = Arc::new(AtomicBool::new(false));
        let clock: SharedFrameClock = Arc::default();
        let writer = FrameWriter {
            frame_rx,
            recycle_tx,
            preview: Arc::new(Mutex::new(None)),
            running: running.clone(),
            failed: Arc::new(AtomicBool::new(false)),
            overloaded: overloaded.clone(),
            clock: clock.clone(),
            frame_size: (4, 2),
            fps,
        };
        let starts = Arc::new(Mutex::new(Vec::new()));
        let mut sink = SlowSink {
            delay,
            starts: starts.clone(),
        };
        let handle = std::thread::spawn(move || writer.run(&mut sink));

        let begin = Instant::now();
        while begin.elapsed() < run_for {
            let _ = frame_tx.try_send(RawFrame {
                data: vec![0; 4 * 2 * 4],
                width: 4,
                height: 2,
            });
            std::thread::sleep(Duration::from_secs(1) / fps);
        }
        running.store(false, Ordering::SeqCst);
        handle.join().unwrap();

        let times = clock.lock().times.iter().copied().collect();
        let starts = starts.lock().clone();
        (times, starts, overloaded.load(Ordering::SeqCst))
    }

    #[test]
    fn writer_stamps_frames_with_real_time_when_ffmpeg_is_slow() {
        // Each write takes 1.5 frame periods. The old writer stamped frame
        // `n` at `n / fps` regardless, drifting ~0.5 s behind after 1.5 s.
        let (times, starts, overloaded) =
            drive_writer(60, Duration::from_millis(25), Duration::from_millis(1500));
        assert!(overloaded, "a writer that can't keep up must say so");
        assert_eq!(times.len(), starts.len());
        assert!(times.len() > 30, "only {} frames written", times.len());
        let worst = times
            .iter()
            .zip(&starts)
            .map(|(t, s)| {
                s.saturating_duration_since(*t)
                    .max(t.saturating_duration_since(*s))
            })
            .max()
            .unwrap();
        assert!(
            worst <= WRITE_JITTER + Duration::from_millis(5),
            "frame time off by {worst:?}"
        );
    }

    #[test]
    fn writer_keeps_exact_cadence_when_on_time() {
        let (times, _, overloaded) = drive_writer(60, Duration::ZERO, Duration::from_millis(500));
        assert!(times.len() > 10);
        // A heavily loaded machine may legitimately stall the writer; only
        // check the cadence when it stayed on schedule.
        if !overloaded {
            let period = Duration::from_nanos(1_000_000_000 / 60);
            let exact = times.windows(2).filter(|w| w[1] - w[0] == period).count();
            assert!(
                exact * 10 >= (times.len() - 1) * 9,
                "{exact} of {} intervals on cadence",
                times.len() - 1
            );
        }
    }

    #[test]
    fn native_output_only_converts_colour() {
        let expected = format!("scale={BT709},{BT709_TAGS}");
        assert_eq!(video_filter(764, 502, &cfg(764, 502)), expected);
        // Odd configured sizes round down to the even encoder size.
        assert_eq!(video_filter(764, 502, &cfg(765, 503)), expected);
    }

    #[test]
    fn scaled_output_preserves_aspect() {
        let f = video_filter(764, 502, &cfg(1920, 1080));
        assert!(f.starts_with("scale=1920:1080:force_original_aspect_ratio=decrease"));
        assert!(f.contains("flags=lanczos"));
        assert!(f.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2"));
        assert!(f.contains(BT709) && f.ends_with(BT709_TAGS));
        assert!(!f.contains(char::is_whitespace), "{f}");
    }

    /// A hardware encoder that cannot open (NVENC rejects frames below its
    /// minimum size) must surface as "failed with nothing buffered", the
    /// signal the UI uses to fall back to libx264. NVIDIA-only; opt-in:
    /// `cargo test -- --ignored hw_open_failure`.
    #[test]
    #[ignore]
    fn hw_open_failure_is_reported_with_empty_buffer() {
        if detect_best_encoder() != HwEncoder::Nvenc {
            eprintln!("skipped: NVENC not available");
            return;
        }
        let mut c = cfg(64, 48);
        c.width = 64;
        c.height = 48;
        let enc = LiveEncoder::start_with(c, HwEncoder::Nvenc).unwrap();
        let tx = enc.frame_sender();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !enc.has_failed() && Instant::now() < deadline {
            let _ = tx.try_send(RawFrame {
                data: vec![0; 64 * 48 * 4],
                width: 64,
                height: 48,
            });
            std::thread::sleep(Duration::from_millis(16));
        }
        assert!(enc.has_failed());
        assert!(enc.encoded_buffer().read().is_empty());

        // The fallback the UI performs: same size, libx264.
        let mut c = cfg(64, 48);
        c.width = 64;
        c.height = 48;
        let sw = LiveEncoder::start_with(c, HwEncoder::Software).unwrap();
        let tx = sw.frame_sender();
        let deadline = Instant::now() + Duration::from_secs(10);
        while sw.encoded_buffer().read().is_empty() && Instant::now() < deadline {
            let _ = tx.try_send(RawFrame {
                data: vec![0; 64 * 48 * 4],
                width: 64,
                height: 48,
            });
            std::thread::sleep(Duration::from_millis(16));
        }
        assert!(!sw.has_failed());
        assert!(!sw.encoded_buffer().read().is_empty());
    }

    /// End-to-end check against a real FFmpeg: feed frames, confirm encoded
    /// output lands in the ring and survives `stop`. Needs FFmpeg on PATH or
    /// beside the test binary, so it is opt-in:
    /// `cargo test -- --ignored live_encoder`.
    #[test]
    #[ignore]
    fn live_encoder_buffers_and_keeps_output_after_stop() {
        crate::logging::init_gui_logger(log::LevelFilter::Info);
        let dump_log = || {
            let mut gen = 0;
            for e in crate::logging::gui_logger()
                .and_then(|l| l.entries_if_changed(&mut gen))
                .unwrap_or_default()
            {
                eprintln!("[{}] {}", e.level, e.message);
            }
        };
        // Large enough for every hardware encoder's minimum frame size.
        let mut c = cfg(320, 240);
        c.width = 321; // odd, like the RuneLite fixed-mode canvas
        c.height = 240;
        let enc = LiveEncoder::start(c).expect("ffmpeg available");
        assert_eq!(enc.frame_size(), (320, 240));
        let tx = enc.frame_sender();
        for i in 0..90u8 {
            let _ = tx.try_send(RawFrame {
                data: vec![i; 320 * 240 * 4],
                width: 320,
                height: 240,
            });
            std::thread::sleep(Duration::from_millis(16));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while enc.encoded_buffer().read().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if enc.has_failed() || enc.encoded_buffer().read().is_empty() {
            dump_log();
            panic!("encoder produced no output (failed: {})", enc.has_failed());
        }
        enc.stop();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!enc.has_failed(), "a requested stop is not a failure");
        let snap = enc.snapshot().expect("buffered output");
        assert_eq!(snap.data[0], 0x47, "MPEG-TS sync byte");
        // ~1.5 s of frames were fed; the snapshot must start on a keyframe
        // and its wall span must match the frames it holds.
        let d = snap.duration_secs();
        assert!(d > 0.5 && d < 3.0, "span {d}");
    }

    /// libx264 `slower` at 1440p60 can't encode in real time on most
    /// machines. The saved span must still follow wall time instead of
    /// drifting behind it (the old frame clock lost ~10 s in 20 s here).
    /// Opt-in: `cargo test --release -- --ignored overloaded_encoder`.
    #[test]
    #[ignore]
    fn overloaded_encoder_snapshot_follows_wall_time() {
        let (w, h) = (2560u32, 1440u32);
        let mut c = cfg(w, h);
        c.width = w;
        c.height = h;
        c.x264_preset = X264Preset::Slower;
        let enc = LiveEncoder::start_with(c, HwEncoder::Software).unwrap();
        let tx = enc.frame_sender();
        let begin = Instant::now();
        let mut i = 0u8;
        while begin.elapsed() < Duration::from_secs(20) {
            // Changing content so x264 does real work.
            let mut data = vec![0u8; (w * h * 4) as usize];
            for (k, b) in data.iter_mut().enumerate().step_by(97) {
                *b = (k as u8).wrapping_add(i);
            }
            let _ = tx.try_send(RawFrame {
                data,
                width: w,
                height: h,
            });
            i = i.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(16));
        }
        if !enc.take_overload_notice() {
            eprintln!("skipped: libx264 kept up on this machine");
            return;
        }
        let snap = enc.snapshot().expect("buffered output");
        let start_off = snap.start.saturating_duration_since(begin);
        let end_lag = Instant::now().saturating_duration_since(snap.end);
        eprintln!(
            "span {:.2}s wall / {:.2}s stream, start +{:.2}s, end lags {:.2}s",
            snap.duration_secs(),
            snap.stream_secs,
            start_off.as_secs_f64(),
            end_lag.as_secs_f64()
        );
        enc.stop();
        assert!(
            start_off < Duration::from_millis(500),
            "start {start_off:?}"
        );
        assert!(snap.time_scale() > 1.2, "scale {}", snap.time_scale());
        // Only the encoder's own latency separates the newest encoded
        // frame from now.
        assert!(end_lag < Duration::from_secs(6), "end lags {end_lag:?}");
    }
}
