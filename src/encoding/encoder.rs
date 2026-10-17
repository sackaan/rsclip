//! Live encoding pipeline.
//!
//! A persistent FFmpeg subprocess encodes raw BGRA frames to H.264 MPEG-TS
//! in real time. Saving a clip snapshots the encoded buffer into a
//! [`SaveRequest`] that is remuxed to MP4 (stream copy — no re-encode), with
//! the mixed audio muxed in.

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};

use parking_lot::RwLock;

use crate::encoding::buffer::{EncodedReplayBuffer, SharedEncodedBuffer, PTS_HZ};
use crate::encoding::config::EncoderConfig;

use super::hw::ffmpeg_cmd;
use super::types::SaveRequest;

pub use super::hw::{detect_best_encoder, is_ffmpeg_available, HwEncoder};
pub use super::types::{build_preview_frame, RawFrame, SharedPreviewFrame};

/// Spare frame buffers kept for reuse by the capture side.
const RECYCLE_DEPTH: usize = 4;

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
    /// Audio bitrate for AAC encoding during remux.
    audio_bitrate_kbps: u32,
    /// Which encoder backend is active.
    hw_encoder: HwEncoder,
    /// Frame size the encoder was started with (always even).
    frame_size: (u32, u32),
    /// Output frame rate (constant).
    fps: u32,
    /// Origin of the writer's frame clock, set when the first frame is
    /// written: frame `n` is written at `writer_start + n / fps`.
    writer_start: Arc<OnceLock<Instant>>,
}

/// Encoded video taken for a save, with the wall-clock interval it shows.
pub(crate) struct VideoSnapshot {
    /// MPEG-TS bytes, starting on a keyframe.
    pub data: Vec<u8>,
    /// When the first frame's content was captured.
    pub start: Instant,
    /// When the last frame stops being displayed.
    pub end: Instant,
}

impl VideoSnapshot {
    /// Length of the clip in seconds.
    pub fn duration_secs(&self) -> f64 {
        (self.end - self.start).as_secs_f64()
    }
}

impl LiveEncoder {
    /// Spawn FFmpeg and the supporting threads. Returns once the child is
    /// running and ready to receive frames on the [`frame_sender`].
    ///
    /// [`frame_sender`]: LiveEncoder::frame_sender
    pub fn start(cfg: EncoderConfig) -> anyhow::Result<Self> {
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
        let writer_start = Arc::new(OnceLock::new());
        let writer_start_w = writer_start.clone();
        let audio_bitrate_kbps = cfg.audio_bitrate_kbps;
        let frame_len = enc_width as usize * enc_height as usize * 4;

        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<RawFrame>(2);
        let (recycle_tx, recycle_rx) = crossbeam_channel::bounded::<Vec<u8>>(RECYCLE_DEPTH);

        let mut hw_encoder = detect_best_encoder();
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
        let running_w = running.clone();
        let failed_w = failed.clone();
        let preview_clone = preview_frame.clone();
        std::thread::Builder::new()
            .name("encoder-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut last_preview = Instant::now();
                let mut last_frame_data: Option<Vec<u8>> = None;
                // The frame clock starts when the first frame is written, so
                // frame numbers (and therefore encoder PTS) map exactly onto
                // wall time: frame `n` goes out at `start_time + n / fps`.
                let mut start_time: Option<Instant> = None;
                let mut frames_written: u64 = 0;

                loop {
                    if !running_w.load(Ordering::Relaxed) {
                        break;
                    }

                    let got_frame = match start_time {
                        // Nothing to pace (or repeat) until the first frame.
                        None => match frame_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(raw_frame) => Some(raw_frame),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        },
                        Some(start) => {
                            let next_deadline = start
                                + Duration::from_nanos(frames_written * 1_000_000_000 / fps as u64);
                            let wait = next_deadline.saturating_duration_since(Instant::now());
                            if wait > Duration::ZERO {
                                match frame_rx.recv_timeout(wait) {
                                    Ok(raw_frame) => Some(raw_frame),
                                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                                }
                            } else {
                                frame_rx.try_recv().ok()
                            }
                        }
                    };

                    // Every frame — fresh or repeated — is a packed buffer of
                    // exactly the encoder's size, written in one call.
                    let data = match got_frame {
                        Some(raw_frame) => {
                            if raw_frame.width != enc_width
                                || raw_frame.height != enc_height
                                || raw_frame.data.len() != frame_len
                            {
                                continue;
                            }
                            if last_preview.elapsed() >= Duration::from_millis(250) {
                                last_preview = Instant::now();
                                let pf = build_preview_frame(&raw_frame);
                                *preview_clone.lock() = Some(pf);
                            }
                            if let Some(old) = last_frame_data.replace(raw_frame.data) {
                                let _ = recycle_tx.try_send(old);
                            }
                            last_frame_data.as_deref()
                        }
                        None => last_frame_data.as_deref(),
                    };

                    if let Some(data) = data {
                        if start_time.is_none() {
                            let now = Instant::now();
                            start_time = Some(now);
                            let _ = writer_start_w.set(now);
                        }
                        if stdin.write_all(data).is_err() {
                            if running_w.load(Ordering::Relaxed) {
                                warn!("FFmpeg stdin write failed — encoder may have crashed");
                                failed_w.store(true, Ordering::SeqCst);
                            }
                            break;
                        }
                    }

                    frames_written += 1;
                }

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
            audio_bitrate_kbps,
            hw_encoder,
            frame_size: (enc_width, enc_height),
            fps,
            writer_start,
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

    /// Snapshot the encoded buffer together with the wall-clock interval
    /// it covers, so audio can be cut to exactly the same moments. Returns
    /// `None` until the first keyframe has been encoded.
    pub(crate) fn snapshot(&self) -> Option<VideoSnapshot> {
        let writer_start = *self.writer_start.get()?;
        let snap = self.encoded_buffer.read().snapshot()?;
        let (start, end) = frame_span_to_wall(
            writer_start,
            self.fps,
            snap.base_pts,
            snap.start_pts,
            snap.end_pts,
        );
        Some(VideoSnapshot {
            data: snap.data,
            start,
            end,
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

/// Map the PTS range of buffered frames to the wall-clock interval they
/// show. The writer emits frame `n` at `writer_start + n / fps` carrying
/// the newest captured image, and the encoder stamps it
/// `base_pts + n * 90000 / fps`; the last frame stays on screen for one
/// more frame period.
fn frame_span_to_wall(
    writer_start: Instant,
    fps: u32,
    base_pts: i64,
    start_pts: i64,
    end_pts: i64,
) -> (Instant, Instant) {
    let fps = fps.max(1) as f64;
    let at = |pts: i64| {
        let frame = ((pts - base_pts) as f64 * fps / PTS_HZ as f64)
            .round()
            .max(0.0);
        writer_start + Duration::from_secs_f64(frame / fps)
    };
    let end = at(end_pts) + Duration::from_secs_f64(1.0 / fps);
    (at(start_pts), end)
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

    #[test]
    fn frame_span_maps_pts_to_writer_clock() {
        let t0 = Instant::now();
        // 60 fps: 1500 ticks per frame. Frames 120..=299 buffered.
        let (start, end) =
            frame_span_to_wall(t0, 60, 126_000, 126_000 + 120 * 1500, 126_000 + 299 * 1500);
        let ms = |i: Instant| (i - t0).as_secs_f64() * 1000.0;
        assert!((ms(start) - 2000.0).abs() < 0.01);
        assert!((ms(end) - 5000.0).abs() < 0.01);
        // 180 frames -> exactly 3 s of clip.
        assert!(((end - start).as_secs_f64() - 3.0).abs() < 1e-6);
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
}
