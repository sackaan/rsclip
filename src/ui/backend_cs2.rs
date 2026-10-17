//! CS2 (GPU zero-copy) recording lifecycle.
//!
//! Parallel to [`super::backend`], which drives the RuneLite FFmpeg pipeline.
//! These methods are selected when [`GameTarget::Cs2`] is active; the RuneLite
//! path is left completely untouched.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use log::{error, info, warn};
use parking_lot::Mutex;
use std::sync::Arc;

use crate::capture::audio::AudioMixer;
use crate::capture::cs2_capture::start_cs2_capture;
use crate::encoding::gpu_buffer::EncodedAuBuffer;
use crate::encoding::gpu_save::{save_cs2_clip, Cs2SaveRequest};
use crate::platform::cs2;
use crate::utils::{RecordingStatus, Severity, StatusMessage};

use super::app::ClipperApp;

impl ClipperApp {
    /// Periodic poll for the CS2 window (mirrors `refresh_runelite_window`).
    pub(super) fn refresh_cs2_window(&mut self) {
        if self.last_window_check.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_window_check = Instant::now();

        self.cs2_window = cs2::find_cs2_window();

        // If recording and the window vanished, stop.
        if self.cs2_capture.is_some() && self.cs2_window.is_none() {
            warn!("CS2 window lost while recording");
            self.stop_recording_cs2_internal();
            self.status_msg = StatusMessage::warning("CS2 window closed");
            return;
        }

        if self.cs2_capture.is_none() {
            if let Some(ref win) = self.cs2_window {
                if self.status_msg.severity == Severity::Info {
                    self.status_msg =
                        StatusMessage::info(format!("Detected CS2: {}x{}", win.width, win.height));
                }
            }
        }
    }

    pub(super) fn start_recording_cs2(&mut self) {
        if self.cs2_capture.is_some() {
            return;
        }
        let win = match self.cs2_window.clone().or_else(cs2::find_cs2_window) {
            Some(w) => w,
            None => {
                self.status_msg = StatusMessage::warning("CS2 not found");
                return;
            }
        };
        if !self.ffmpeg_available {
            self.status_msg = StatusMessage::warning("FFmpeg not found");
            return;
        }

        // Fresh buffer + state for this session.
        self.cs2_buffer = None;
        self.audio_mixer = None;
        self.dropped_frames.store(0, Ordering::Relaxed);

        let fps = self.settings.cs2_capture_fps;
        let bitrate = self.settings.cs2_bitrate_kbps;
        let buffer = Arc::new(Mutex::new(EncodedAuBuffer::new(self.settings.buffer_seconds)));

        match start_cs2_capture(
            win.capture_hwnd(),
            win.width,
            win.height,
            fps,
            bitrate,
            buffer.clone(),
            self.dropped_frames.clone(),
        ) {
            Ok(handle) => {
                self.cs2_capture = Some(handle);
                self.cs2_buffer = Some(buffer);
                self.cs2_window = Some(win.clone());

                self.setup_cs2_audio(win.pid);

                self.status = RecordingStatus::Recording;
                self.status_msg = StatusMessage::success(format!(
                    "Recording CS2 @ {} fps → {} fps blur",
                    fps, self.settings.cs2_output_fps
                ));
                info!("CS2 recording started");
            }
            Err(e) => {
                error!("Failed to start CS2 capture: {e:#}");
                self.status = RecordingStatus::Error;
                self.status_msg = StatusMessage::error(format!("CS2 capture error: {e}"));
            }
        }
    }

    /// Audio mixer setup (game loopback + mic + configured app channels),
    /// matching the RuneLite path but keyed on the CS2 process.
    fn setup_cs2_audio(&mut self, game_pid: u32) {
        let mut mixer = AudioMixer::new(self.settings.buffer_seconds);
        match mixer.add_game_channel(game_pid) {
            Ok(()) => info!("CS2 game audio capture started"),
            Err(e) => warn!("No CS2 game audio: {e}"),
        }
        if let Some(ch) = mixer.game_channel_mut() {
            ch.volume = self.settings.game_volume;
        }
        if self.settings.mic_enabled && !self.settings.mic_device_id.is_empty() {
            match mixer.add_mic_channel(&self.settings.mic_device_id) {
                Ok(()) => {
                    if let Some(ch) = mixer.mic_channel_mut() {
                        ch.volume = self.settings.mic_volume;
                    }
                    info!("Mic capture started");
                }
                Err(e) => warn!("No mic audio: {e}"),
            }
        }
        super::backend::attach_configured_app_channels(
            &mut mixer,
            &self.settings.app_channels,
            game_pid,
        );
        self.audio_mixer = Some(mixer);
    }

    pub(super) fn stop_recording_cs2(&mut self) {
        self.stop_recording_cs2_internal();
        self.status_msg = StatusMessage::info("Stopped");
    }

    fn stop_recording_cs2_internal(&mut self) {
        if let Some(mut handle) = self.cs2_capture.take() {
            handle.stop();
        }
        if let Some(ref mixer) = self.audio_mixer {
            mixer.stop();
        }
        self.audio_mixer = None;
        self.status = RecordingStatus::Idle;
        // Keep `cs2_buffer` so the user can still save after stopping.
    }

    pub(super) fn save_clip_cs2(&mut self) {
        if self.is_saving.load(Ordering::SeqCst) {
            self.status_msg = StatusMessage::warning("Already saving...");
            return;
        }
        let snapshot = match self.cs2_buffer.as_ref().and_then(|b| b.lock().snapshot()) {
            Some(s) => s,
            None => {
                self.status_msg = StatusMessage::warning("Buffer empty");
                return;
            }
        };

        let audio_wav = self
            .audio_mixer
            .as_ref()
            .map(|m| m.snapshot_mixed_wav(snapshot.duration_secs))
            .filter(|w| !w.is_empty());

        let blur_pct = if self.settings.cs2_blur_enabled {
            self.settings.cs2_blur_pct
        } else {
            0
        };

        // Use the *measured* capture rate (real frames / real duration), not
        // the requested rate — the encoder may not sustain the full rate, and
        // the elementary stream has no timestamps, so this is what makes the
        // saved clip play at the correct speed.
        let measured_fps = if snapshot.duration_secs > 0.1 {
            ((snapshot.frame_count as f64 / snapshot.duration_secs).round() as u32).max(1)
        } else {
            self.settings.cs2_capture_fps
        };
        info!(
            "CS2 save: {} frames over {:.2}s = {} fps measured",
            snapshot.frame_count, snapshot.duration_secs, measured_fps
        );

        let request = Cs2SaveRequest {
            annexb: snapshot.annexb,
            intermediate_codec: snapshot.codec,
            output_dir: self.settings.output_dir.clone(),
            audio_wav,
            video_duration_secs: snapshot.duration_secs,
            audio_bitrate_kbps: self.settings.audio_bitrate_kbps,
            capture_fps: measured_fps,
            output_fps: self.settings.cs2_output_fps,
            blur_pct,
            rate_control: self.settings.rate_control,
            crf: self.settings.crf,
            video_bitrate_kbps: self.settings.video_bitrate_kbps,
            x264_preset: self.settings.x264_preset,
        };

        let tx = self.cs2_result_tx.clone();
        std::thread::Builder::new()
            .name("cs2-save".into())
            .spawn(move || {
                let _ = tx.send(save_cs2_clip(&request));
            })
            .ok();

        self.is_saving.store(true, Ordering::SeqCst);
        self.status = RecordingStatus::Saving;
        self.status_msg = StatusMessage::info("Rendering blur...");
        info!("CS2 clip render queued ({} frames)", snapshot.frame_count);
    }

    pub(super) fn poll_cs2_results(&mut self) {
        while let Ok(result) = self.cs2_result_rx.try_recv() {
            self.is_saving.store(false, Ordering::SeqCst);
            self.status_msg = match result {
                Ok(msg) => StatusMessage::success(msg),
                Err(e) => StatusMessage::error(format!("Failed: {e}")),
            };
            self.status = if self.cs2_capture.is_some() {
                RecordingStatus::Recording
            } else {
                RecordingStatus::Idle
            };
        }
    }
}
