use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui;
use log::{error, info, warn};

use crate::capture::audio::AudioMixer;
use crate::capture::screen::{self, CaptureConfig, CaptureHandle};
use crate::capture::sessions;
use crate::encoding::encoder::{self, LiveEncoder};
use crate::encoding::remux::remux_to_mp4;
use crate::platform::hotkeys;
use crate::platform::runelite::{self, RuneLiteWindow};
use crate::utils::{RecordingStatus, Severity, StatusMessage};

use super::app::ClipperApp;

impl ClipperApp {
    /// Whether a capture session is currently running.
    pub(super) fn is_recording(&self) -> bool {
        self.capture_handle.is_some()
    }

    /// Whether a RuneLite window has been detected.
    pub(super) fn game_window_found(&self) -> bool {
        self.active_window.is_some()
    }

    /// Whether there is buffered footage that can be saved right now.
    pub(super) fn can_save(&self) -> bool {
        if self.is_saving.load(Ordering::SeqCst) {
            return false;
        }
        self.live_encoder
            .as_ref()
            .is_some_and(|e| !e.encoded_buffer().read().is_empty())
    }

    /// One-line description of the detected RuneLite window for the main
    /// screen, or `None` if no window is detected.
    pub(super) fn detected_window_line(&self) -> Option<String> {
        let win = self.active_window.as_ref()?;
        let canvas_tag = if win.canvas_found { " (canvas)" } else { "" };
        let n = self.runelite_windows.len();
        Some(if n > 1 {
            format!(
                "{}x{}{}  ·  {} clients",
                win.width, win.height, canvas_tag, n
            )
        } else {
            format!("{}x{}{}", win.width, win.height, canvas_tag)
        })
    }

    /// One-line buffer duration + memory readout, or `None` when no buffer
    /// is live.
    pub(super) fn buffer_stats_line(&self) -> Option<String> {
        let enc = self.live_encoder.as_ref()?;
        let buf = enc.encoded_buffer().read();
        let (dur, bytes) = (buf.duration_secs(), buf.memory_usage_bytes());
        let mins = dur as u32 / 60;
        let secs = dur as u32 % 60;
        let mb = bytes as f64 / (1024.0 * 1024.0);
        Some(format!("{}:{:02} · {:.1} MB", mins, secs, mb))
    }

    /// Periodic poll: rescan RuneLite windows, validate the active window,
    /// and handle focus-based capture switching.
    pub(super) fn refresh_runelite_window(&mut self) {
        if self.last_window_check.elapsed() < std::time::Duration::from_millis(500) {
            return;
        }
        self.last_window_check = Instant::now();

        self.runelite_windows = runelite::find_all_runelite_windows();

        if !self.check_active_window_alive() {
            return;
        }
        self.update_active_dimensions();
        self.update_capture_crop();
        self.handle_focus_switch_recording();
        self.handle_focus_switch_preview();
        self.update_idle_window_detection();
    }

    /// Verify the active window still exists. If only its game canvas went
    /// away (RuneLite can recreate it), adopt the freshly scanned canvas
    /// instead of stopping. Returns `false` if the window was lost and
    /// recording was stopped; the buffered footage stays savable.
    fn check_active_window_alive(&mut self) -> bool {
        let win = match &self.active_window {
            Some(w) => w,
            None => return true,
        };
        if !runelite::is_window_alive(win.hwnd) {
            warn!("Active RuneLite window lost");
            let was_recording = self.capture_handle.is_some();
            self.active_window = None;
            self.stop_recording_internal();
            self.status_msg = if was_recording {
                StatusMessage::warning("RuneLite closed — buffer kept, save still works")
            } else {
                StatusMessage::warning("RuneLite window closed")
            };
            return false;
        }
        if let Some(canvas) = win.canvas_hwnd {
            if !runelite::is_window_alive(canvas) {
                let hwnd = win.hwnd;
                let rescanned = self
                    .runelite_windows
                    .iter()
                    .find(|w| w.hwnd == hwnd)
                    .cloned();
                info!(
                    "Game canvas child lost, re-resolved: {}",
                    rescanned.as_ref().is_some_and(|w| w.canvas_found)
                );
                if let Some(w) = rescanned {
                    self.active_window = Some(w);
                } else if let Some(ref mut active) = self.active_window {
                    active.canvas_hwnd = None;
                    active.canvas_found = false;
                }
            }
        }
        true
    }

    /// Refresh the cached width/height of the active window.
    fn update_active_dimensions(&mut self) {
        let win = match &self.active_window {
            Some(w) => w.clone(),
            None => return,
        };
        let size_hwnd = win.canvas_hwnd.unwrap_or_else(|| win.capture_hwnd());
        if let Ok((w, h)) = runelite::get_window_size(size_hwnd) {
            if let Some(ref mut active) = self.active_window {
                active.width = w;
                active.height = h;
            }
        }
    }

    /// Push the current canvas position/size to running captures so a moved
    /// or resized canvas (sidebar toggled, window resized) keeps being
    /// cropped correctly.
    fn update_capture_crop(&self) {
        let Some(ref win) = self.active_window else {
            return;
        };
        if self.capture_handle.is_none() && self.preview_capture.is_none() {
            return;
        }
        let crop = win.canvas_crop_rect();
        for handle in [&self.capture_handle, &self.preview_capture]
            .into_iter()
            .flatten()
        {
            handle.set_crop(crop);
        }
    }

    /// Start a capture of `win` feeding `encoder`, at the encoder's fixed
    /// frame size.
    fn start_encoder_capture(
        &self,
        win: &RuneLiteWindow,
        encoder: &LiveEncoder,
    ) -> anyhow::Result<CaptureHandle> {
        let (out_width, out_height) = encoder.frame_size();
        screen::start_capture(
            win.capture_hwnd(),
            encoder.frame_sender(),
            CaptureConfig {
                out_width,
                out_height,
                crop: win.canvas_crop_rect(),
                max_fps: self.settings.fps,
            },
            self.dropped_frames.clone(),
            Some(encoder.frame_recycler()),
        )
    }

    /// While recording, switch capture if a different RuneLite client gains focus.
    fn handle_focus_switch_recording(&mut self) {
        if self.capture_handle.is_none() {
            return;
        }
        if let Some(focused_hwnd) = runelite::focused_runelite_hwnd(&self.runelite_windows) {
            let active_hwnd = self.active_window.as_ref().map(|w| w.hwnd);
            if Some(focused_hwnd) != active_hwnd {
                if let Some(new_win) = self
                    .runelite_windows
                    .iter()
                    .find(|w| w.hwnd == focused_hwnd)
                    .cloned()
                {
                    info!("Focus switched to different RuneLite client, switching capture");
                    self.switch_capture(new_win);
                }
            }
        }
    }

    /// While previewing (not recording), switch preview when focus changes.
    fn handle_focus_switch_preview(&mut self) {
        if self.capture_handle.is_some()
            || self.preview_capture.is_none()
            || self.runelite_windows.len() <= 1
        {
            return;
        }
        if let Some(focused_hwnd) = runelite::focused_runelite_hwnd(&self.runelite_windows) {
            let active_hwnd = self.active_window.as_ref().map(|w| w.hwnd);
            if Some(focused_hwnd) != active_hwnd {
                if let Some(new_win) = self
                    .runelite_windows
                    .iter()
                    .find(|w| w.hwnd == focused_hwnd)
                    .cloned()
                {
                    self.stop_preview_capture();
                    self.active_window = Some(new_win);
                    self.start_preview_capture();
                }
            }
        }
    }

    /// When idle (not recording, no preview), track which RuneLite window is focused
    /// and update the status message.
    fn update_idle_window_detection(&mut self) {
        if self.capture_handle.is_some() || self.preview_capture.is_some() {
            return;
        }
        let prev_hwnd = self.active_window.as_ref().map(|w| w.hwnd);
        if let Some(focused) = runelite::focused_runelite_hwnd(&self.runelite_windows) {
            if Some(focused) != prev_hwnd {
                self.active_window = self
                    .runelite_windows
                    .iter()
                    .find(|w| w.hwnd == focused)
                    .cloned();
            }
        } else if self.active_window.is_none() {
            self.active_window = self.runelite_windows.first().cloned();
        }
        if let Some(ref win) = self.active_window {
            if self.status_msg.severity == Severity::Info {
                self.status_msg =
                    StatusMessage::info(format!("Detected: {}x{}", win.width, win.height));
            }
        }
    }

    /// Switch the capture to a different RuneLite window while keeping the
    /// encoder and its buffer alive. A client of a different size is scaled
    /// to fit the existing encoder size.
    fn switch_capture(&mut self, new_win: RuneLiteWindow) {
        // Stop old capture (but keep encoder + audio buffer).
        if let Some(handle) = self.capture_handle.take() {
            handle.stop();
        }

        let result = match self.live_encoder.as_ref() {
            Some(enc) => self.start_encoder_capture(&new_win, enc),
            None => Err(anyhow::anyhow!("no encoder running")),
        };
        match result {
            Ok(handle) => {
                self.capture_handle = Some(handle);

                // Switch audio to the new process, keeping the ring buffer.
                if let Some(ref mut mixer) = self.audio_mixer {
                    if let Err(e) = mixer.switch_game_pid(new_win.pid) {
                        warn!("Failed to switch audio PID: {}", e);
                    }
                } else {
                    let mut mixer = AudioMixer::new(self.settings.buffer_seconds);
                    match mixer.add_game_channel(new_win.pid) {
                        Ok(()) => self.audio_mixer = Some(mixer),
                        Err(e) => warn!("No audio after switch: {}", e),
                    }
                }

                let mode = if new_win.canvas_found {
                    "canvas"
                } else {
                    "window"
                };
                self.status_msg =
                    StatusMessage::success(format!("Switched → {} ({})", new_win.width, mode));
                info!("Capture switched to HWND {:X}", new_win.hwnd);
            }
            Err(e) => {
                error!("Failed to start capture after switch: {}", e);
                self.status = RecordingStatus::Error;
                self.status_msg = StatusMessage::error(format!("Capture error: {}", e));
            }
        }
        self.active_window = Some(new_win);
    }

    pub(super) fn start_recording(&mut self) {
        if self.capture_handle.is_some() {
            return;
        }
        // Stop standalone preview capture — the encoder will provide preview.
        self.stop_preview_capture();

        let win = match &self.active_window {
            Some(w) => w.clone(),
            None => {
                self.status_msg = StatusMessage::warning("RuneLite not found");
                return;
            }
        };
        if !self.ffmpeg_available {
            self.status_msg = StatusMessage::warning("FFmpeg not found");
            return;
        }

        self.live_encoder = None;
        self.audio_mixer = None;
        self.dropped_frames.store(0, Ordering::Relaxed);

        let encoder = match self.start_encoder(win.width, win.height) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to start live encoder: {}", e);
                self.status = RecordingStatus::Error;
                self.status_msg = StatusMessage::error(format!("Encoder error: {}", e));
                return;
            }
        };

        match self.start_encoder_capture(&win, &encoder) {
            Ok(handle) => {
                let mode = if win.canvas_found { "canvas" } else { "window" };
                self.capture_handle = Some(handle);
                self.live_encoder = Some(encoder);

                // Set up audio mixer with game channel + optional mic.
                let mut mixer = AudioMixer::new(self.settings.buffer_seconds);
                match mixer.add_game_channel(win.pid) {
                    Ok(()) => info!("Game audio capture started"),
                    Err(e) => warn!("No game audio: {}", e),
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
                        Err(e) => warn!("No mic audio: {}", e),
                    }
                }
                attach_configured_app_channels(&mut mixer, &self.settings.app_channels, win.pid);
                self.audio_mixer = Some(mixer);

                self.status = RecordingStatus::Recording;
                self.status_msg = StatusMessage::success(format!(
                    "Recording ({}) @ {} fps",
                    mode, self.settings.fps
                ));
                info!("Recording started ({})", mode);
            }
            Err(e) => {
                error!("Failed to start capture: {}", e);
                self.status = RecordingStatus::Error;
                self.status_msg = StatusMessage::error(format!("Capture error: {}", e));
            }
        }
    }

    pub(super) fn stop_recording(&mut self) {
        self.stop_recording_internal();
        self.status_msg = StatusMessage::info("Stopped");
    }

    /// Stop capture, audio, and the FFmpeg process. The encoded buffer and
    /// the audio rings are kept so the footage can still be saved until the
    /// next recording starts.
    fn stop_recording_internal(&mut self) {
        if let Some(handle) = self.capture_handle.take() {
            handle.stop();
        }
        if let Some(ref mixer) = self.audio_mixer {
            mixer.stop();
        }
        if let Some(ref encoder) = self.live_encoder {
            encoder.stop();
        }
        self.status = if self.is_saving.load(Ordering::SeqCst) {
            RecordingStatus::Saving
        } else {
            RecordingStatus::Idle
        };

        // If preview is open, restart standalone preview capture.
        if self.show_preview {
            self.start_preview_capture();
        }
    }

    /// Start an encoder for a `width`×`height` source, honouring a
    /// session-wide fallback to libx264.
    fn start_encoder(&self, width: u32, height: u32) -> anyhow::Result<LiveEncoder> {
        let cfg = self.encoder_config(width, height);
        if self.force_software_encoder {
            LiveEncoder::start_with(cfg, encoder::HwEncoder::Software)
        } else {
            LiveEncoder::start(cfg)
        }
    }

    /// Detect an FFmpeg crash while recording. A hardware encoder that dies
    /// before producing any video (driver too old, unsupported size) is
    /// replaced by libx264 transparently; any other crash stops capturing
    /// into the dead encoder and tells the user, keeping what was buffered.
    pub(super) fn check_encoder_health(&mut self) {
        if self.capture_handle.is_none() {
            return;
        }
        let Some(enc) = self.live_encoder.as_ref() else {
            return;
        };
        if !enc.has_failed() {
            return;
        }
        let hw = enc.hw_encoder();
        let (width, height) = enc.frame_size();
        if hw != encoder::HwEncoder::Software && enc.encoded_buffer().read().is_empty() {
            warn!(
                "{} failed to start; falling back to libx264",
                hw.display_name()
            );
            self.force_software_encoder = true;
            if let Some(handle) = self.capture_handle.take() {
                handle.stop();
            }
            let restarted = self
                .active_window
                .clone()
                .ok_or_else(|| anyhow::anyhow!("RuneLite window lost"))
                .and_then(|win| {
                    let enc = self.start_encoder(width, height)?;
                    let handle = self.start_encoder_capture(&win, &enc)?;
                    Ok((enc, handle))
                });
            match restarted {
                Ok((enc, handle)) => {
                    self.live_encoder = Some(enc);
                    self.capture_handle = Some(handle);
                    self.status_msg = StatusMessage::warning(format!(
                        "{} unavailable — recording with x264",
                        hw.display_name()
                    ));
                    return;
                }
                Err(e) => error!("libx264 fallback failed: {}", e),
            }
        }
        error!("Encoder stopped unexpectedly; recording halted");
        self.stop_recording_internal();
        self.status = RecordingStatus::Error;
        self.status_msg = StatusMessage::error("Encoder stopped — buffer kept; press ▶ to restart");
    }

    /// Pull the latest frame from encoder or standalone capture into the preview texture.
    pub(super) fn update_preview_texture(&mut self, ctx: &egui::Context) {
        if !self.show_preview {
            return;
        }
        // A stopped encoder keeps its buffer but no longer produces preview
        // frames; use the standalone preview capture then.
        let maybe_frame = if let (Some(_), Some(enc)) = (&self.capture_handle, &self.live_encoder) {
            enc.preview_frame().lock().take()
        } else {
            self.preview_frame.lock().take()
        };
        if let Some(pf) = maybe_frame {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [pf.width as usize, pf.height as usize],
                &pf.rgba,
            );
            let tex = ctx.load_texture("preview", image, egui::TextureOptions::LINEAR);
            self.preview_texture = Some(tex);
        }
    }

    /// Start a lightweight capture that only produces preview thumbnails.
    /// Used when the preview window is open but we're not recording.
    pub(super) fn start_preview_capture(&mut self) {
        // Don't start if already running or if recording (encoder provides preview).
        if self.preview_capture.is_some() || self.capture_handle.is_some() {
            return;
        }
        let win = match &self.active_window {
            Some(w) => w.clone(),
            None => return,
        };

        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<encoder::RawFrame>(1);
        let preview_frame = self.preview_frame.clone();

        std::thread::Builder::new()
            .name("preview-worker".into())
            .spawn(move || {
                use std::time::{Duration, Instant};
                let mut last = Instant::now();
                loop {
                    match frame_rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(raw) => {
                            // ~4 fps throttle
                            if last.elapsed() >= Duration::from_millis(250) {
                                last = Instant::now();
                                let pf = encoder::build_preview_frame(&raw);
                                *preview_frame.lock() = Some(pf);
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();

        match screen::start_capture(
            win.capture_hwnd(),
            frame_tx,
            CaptureConfig {
                out_width: (win.width & !1).max(2),
                out_height: (win.height & !1).max(2),
                crop: win.canvas_crop_rect(),
                // The preview refreshes ~4x per second; don't read back more.
                max_fps: 4,
            },
            self.dropped_frames.clone(),
            None,
        ) {
            Ok(handle) => self.preview_capture = Some(handle),
            Err(e) => warn!("Preview capture failed: {}", e),
        }
    }

    /// Stop the standalone preview capture.
    pub(super) fn stop_preview_capture(&mut self) {
        if let Some(handle) = self.preview_capture.take() {
            handle.stop();
        }
    }

    pub(super) fn save_clip(&mut self) {
        if self.is_saving.load(Ordering::SeqCst) {
            self.status_msg = StatusMessage::warning("Already saving...");
            return;
        }
        let encoder = match &self.live_encoder {
            Some(e) => e,
            None => {
                self.status_msg = StatusMessage::warning("No data");
                return;
            }
        };
        if encoder.encoded_buffer().read().is_empty() {
            self.status_msg = StatusMessage::warning("Buffer empty");
            return;
        }

        info!("Saving clip");
        let Some(video) = encoder.snapshot() else {
            self.status_msg = StatusMessage::warning("Buffer empty");
            return;
        };
        // Cut audio to exactly the wall-clock moments the video shows.
        let audio_wav = self
            .audio_mixer
            .as_ref()
            .map(|m| m.snapshot_mixed_wav_span(video.start, video.end))
            .filter(|w| !w.is_empty());
        info!("Clip spans {:.2}s of video", video.duration_secs());
        let request = encoder.save_request(video, &self.settings.output_dir, audio_wav);

        let tx = self.save_result_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("save-worker".into())
            .spawn(move || {
                let _ = tx.send(remux_to_mp4(&request));
            });
        if let Err(e) = spawned {
            self.status_msg = StatusMessage::error(format!("Save failed: {}", e));
            return;
        }
        self.is_saving.store(true, Ordering::SeqCst);
        self.status = RecordingStatus::Saving;
        self.status_msg = StatusMessage::info("Saving...");
    }

    /// Drain finished saves and update the status line.
    pub(super) fn poll_save_results(&mut self) {
        while let Ok(result) = self.save_result_rx.try_recv() {
            self.is_saving.store(false, Ordering::SeqCst);
            self.status_msg = match result {
                Ok(msg) => StatusMessage::success(msg),
                Err(e) => StatusMessage::error(format!("Failed: {}", e)),
            };
            self.status = if self.is_recording() {
                RecordingStatus::Recording
            } else {
                RecordingStatus::Idle
            };
        }
    }

    pub(super) fn poll_hotkeys(&mut self) {
        let (save_id, record_id) = match &self.hotkey_manager {
            Some(hm) => (hm.save_hotkey_id(), hm.record_hotkey_id()),
            None => return,
        };
        while let Some(event) = hotkeys::poll_hotkey_event() {
            if event.state() != global_hotkey::HotKeyState::Pressed {
                continue;
            }
            if event.id() == save_id {
                info!("Hotkey: Save Clip");
                self.save_clip();
            } else if record_id == Some(event.id()) {
                if self.is_recording() {
                    info!("Hotkey: Stop Recording");
                    self.stop_recording();
                } else {
                    info!("Hotkey: Start Recording");
                    self.start_recording();
                }
            }
        }
    }
}

/// Re-resolve each persisted `AppChannelConfig` to a live PID and attach it
/// to the mixer. Apps that are not currently producing audio (no WASAPI
/// session) or were filtered out (system sounds, the game PID, our own PID)
/// are skipped silently — they will pick up audio on their next recording
/// session if they're running by then.
pub(super) fn attach_configured_app_channels(
    mixer: &mut AudioMixer,
    configs: &[super::settings::AppChannelConfig],
    game_pid: u32,
) {
    if configs.is_empty() {
        return;
    }

    let sessions = match sessions::enumerate_audio_sessions(&[game_pid]) {
        Ok(s) => s,
        Err(e) => {
            warn!("Could not enumerate audio sessions: {}", e);
            return;
        }
    };

    for cfg in configs {
        let exe = cfg.exe_name.to_lowercase();
        let Some(session) = sessions.iter().find(|s| s.exe_name == exe) else {
            info!("App audio source '{}' not running — skipping", cfg.exe_name);
            continue;
        };
        match mixer.add_application_channel(session.pid, session.exe_name.clone()) {
            Ok(()) => {
                if let Some(ch) = mixer.application_channel_mut(session.pid) {
                    ch.volume = cfg.volume;
                }
                info!(
                    "App audio capture started for {} (PID {})",
                    cfg.display_name, session.pid
                );
            }
            Err(e) => warn!("Failed to capture {}: {}", cfg.display_name, e),
        }
    }
}
