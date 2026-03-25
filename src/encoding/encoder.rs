// Live encoding pipeline.
//
// A persistent FFmpeg subprocess encodes raw BGRA frames to H.264 MPEG-TS in
// real time. Saving a clip snapshots the encoded buffer and remuxes to MP4
// (stream copy, no re-encode).

use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use log::{info, warn};

use parking_lot::RwLock;

use crate::encoding::buffer::{EncodedReplayBuffer, SharedEncodedBuffer};
use crate::encoding::config::EncoderConfig;

use super::hw::ffmpeg_cmd;
use super::remux::remux_to_mp4;
use super::types::SaveRequest;

pub use super::hw::{detect_best_encoder, is_ffmpeg_available, HwEncoder};
pub use super::types::{build_preview_frame, RawFrame, SharedPreviewFrame};

/// Manages a persistent FFmpeg process for real-time H.264 encoding.
///
/// Three background threads: writer (feeds frames to stdin), reader (captures
/// encoded TS from stdout), and save worker (remuxes TS to MP4 on demand).
pub struct LiveEncoder {
    /// Send raw frames here for encoding.
    frame_tx: Sender<RawFrame>,
    /// The encoded output buffer (shared with reader thread).
    encoded_buffer: SharedEncodedBuffer,
    /// Latest frame snapshot for UI preview.
    preview_frame: SharedPreviewFrame,
    /// Signal threads to stop.
    running: Arc<AtomicBool>,
    /// Receive save results (polled by the app).
    result_rx: Receiver<anyhow::Result<String>>,
    /// Send save requests to the save worker.
    save_tx: Sender<SaveRequest>,
    /// Audio bitrate for AAC encoding during remux.
    audio_bitrate_kbps: u32,
    /// Which encoder backend is active.
    hw_encoder: HwEncoder,
}

impl LiveEncoder {
    pub fn start(cfg: EncoderConfig) -> anyhow::Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let encoded_buffer: SharedEncodedBuffer =
            Arc::new(RwLock::new(EncodedReplayBuffer::new(cfg.buffer_seconds)));
        let preview_frame: SharedPreviewFrame = Arc::new(parking_lot::Mutex::new(None));

        let enc_width = cfg.width & !1;
        let enc_height = cfg.height & !1;
        let fps = cfg.fps;
        let audio_bitrate_kbps = cfg.audio_bitrate_kbps;

        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<RawFrame>(2);
        let (save_tx, save_rx) = crossbeam_channel::bounded::<SaveRequest>(2);
        let (result_tx, result_rx) = crossbeam_channel::bounded::<anyhow::Result<String>>(4);

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

        let stdin = child.stdin.take().expect("FFmpeg stdin");
        let stdout = child.stdout.take().expect("FFmpeg stdout");
        let stderr = child.stderr.take().expect("FFmpeg stderr");

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
        let preview_clone = preview_frame.clone();
        std::thread::Builder::new()
            .name("encoder-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let row_bytes = (enc_width * 4) as usize;
                let mut last_preview = Instant::now();
                let mut last_frame_data: Option<Vec<u8>> = None;
                let start_time = Instant::now();
                let mut frames_written: u64 = 0;

                let mut write_frame = |data: &[u8], src_stride: usize| -> bool {
                    for y in 0..enc_height as usize {
                        let offset = y * src_stride;
                        let end = offset + row_bytes;
                        if end > data.len() || stdin.write_all(&data[offset..end]).is_err() {
                            return false;
                        }
                    }
                    true
                };

                loop {
                    if !running_w.load(Ordering::Relaxed) {
                        break;
                    }

                    let next_deadline = start_time
                        + Duration::from_nanos((frames_written + 1) * 1_000_000_000 / fps as u64);
                    let now = Instant::now();

                    let wait = if now < next_deadline {
                        next_deadline - now
                    } else {
                        Duration::ZERO
                    };

                    let got_frame = if wait > Duration::ZERO {
                        match frame_rx.recv_timeout(wait) {
                            Ok(raw_frame) => Some(raw_frame),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                        }
                    } else {
                        frame_rx.try_recv().ok()
                    };

                    if let Some(raw_frame) = got_frame {
                        let fw = raw_frame.width & !1;
                        let fh = raw_frame.height & !1;
                        if fw != enc_width || fh != enc_height {
                            continue;
                        }

                        if last_preview.elapsed() >= Duration::from_millis(250) {
                            last_preview = Instant::now();
                            let pf = build_preview_frame(&raw_frame);
                            *preview_clone.lock() = Some(pf);
                        }

                        let src_stride = (raw_frame.width * 4) as usize;
                        if !write_frame(&raw_frame.data, src_stride) {
                            warn!("FFmpeg stdin write failed — encoder may have crashed");
                            break;
                        }
                        last_frame_data = Some(raw_frame.data);
                    } else {
                        if let Some(ref data) = last_frame_data {
                            if !write_frame(data, row_bytes) {
                                warn!("FFmpeg stdin write failed — encoder may have crashed");
                                break;
                            }
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
                            buffer_clone.write().push(buf[..n].to_vec());
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            warn!("Encoder reader error: {}", e);
                            break;
                        }
                    }
                }
                info!("Encoder reader thread stopped");
            })?;

        std::thread::Builder::new()
            .name("save-worker".into())
            .spawn(move || {
                for request in save_rx.iter() {
                    let result = remux_to_mp4(&request);
                    let _ = result_tx.send(result);
                }
                info!("Save worker thread stopped");
            })?;

        info!(
            "Live encoder started: {}x{} @ {} fps, codec: {:?}",
            enc_width, enc_height, fps, hw_encoder
        );

        Ok(Self {
            frame_tx,
            encoded_buffer,
            preview_frame,
            running,
            result_rx,
            save_tx,
            audio_bitrate_kbps,
            hw_encoder,
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

        // Only apply scaling if input and output dimensions differ.
        if enc_width != cfg.output_width || enc_height != cfg.output_height {
            cmd.args([
                "-vf",
                &format!(
                    "scale={}:{}:flags={}",
                    cfg.output_width,
                    cfg.output_height,
                    cfg.scale_filter.as_str()
                ),
            ]);
        }

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

    pub fn frame_sender(&self) -> Sender<RawFrame> {
        self.frame_tx.clone()
    }

    pub fn encoded_buffer(&self) -> &SharedEncodedBuffer {
        &self.encoded_buffer
    }

    pub fn preview_frame(&self) -> &SharedPreviewFrame {
        &self.preview_frame
    }

    pub fn hw_encoder(&self) -> HwEncoder {
        self.hw_encoder
    }

    /// Queue a clip save request. Returns `true` if accepted.
    pub fn save_clip(
        &self,
        output_dir: &str,
        audio_wav: Option<Vec<u8>>,
        video_duration_secs: f64,
    ) -> bool {
        let encoded_data = self.encoded_buffer.read().snapshot();
        if encoded_data.is_empty() {
            return false;
        }
        self.save_tx
            .try_send(SaveRequest {
                encoded_data,
                output_dir: output_dir.to_string(),
                audio_wav,
                video_duration_secs,
                audio_bitrate_kbps: self.audio_bitrate_kbps,
            })
            .is_ok()
    }

    pub fn poll_result(&self) -> Option<anyhow::Result<String>> {
        self.result_rx.try_recv().ok()
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for LiveEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}
