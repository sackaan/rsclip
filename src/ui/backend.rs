use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui;
use log::{error, info, warn};

use crate::capture::audio::AudioMixer;
use crate::encoding::encoder::{self, LiveEncoder};
use crate::platform::hotkeys;
use crate::platform::runelite::{self, RuneLiteWindow};
use crate::utils::{RecordingStatus, Severity, StatusMessage};

use super::app::ClipperApp;

impl ClipperApp {
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
        self.handle_focus_switch_recording();
        self.handle_focus_switch_preview();
        self.update_idle_window_detection();
    }

    /// Verify the active window (and its canvas) still exist.
    /// Returns `false` if the window was lost and recording was stopped.
    fn check_active_window_alive(&mut self) -> bool {
        let win = match &self.active_window {
            Some(w) => w,
            None => return true,
        };
        if !runelite::is_window_alive(win.hwnd) {
            warn!("Active RuneLite window lost");
            self.active_window = None;
            self.stop_recording_internal();
            self.status_msg = StatusMessage::warning("RuneLite window closed");
            return false;
        }
        if let Some(canvas) = win.canvas_hwnd {
            if !runelite::is_window_alive(canvas) {
                warn!("Game canvas child lost, re-scanning");
                self.active_window = None;
                self.stop_recording_internal();
                self.status_msg = StatusMessage::warning("Game canvas lost");
                return false;
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
    /// encoder alive if the dimensions are compatible.
    fn switch_capture(&mut self, new_win: RuneLiteWindow) {
        // Stop old capture (but keep encoder + audio buffer).
        if let Some(handle) = self.capture_handle.take() {
            handle.stop();
        }

        // Check if we can reuse the existing encoder (same dimensions).
        let can_reuse = self.live_encoder.as_ref().is_some_and(|_| {
            let old = self.active_window.as_ref().unwrap();
            old.width == new_win.width && old.height == new_win.height
        });

        if !can_reuse {
            // Dimensions differ — full restart.
            if let Some(ref enc) = self.live_encoder {
                enc.stop();
            }
            self.live_encoder = None;

            let encoder =
                match LiveEncoder::start(self.encoder_config(new_win.width, new_win.height)) {
                    Ok(e) => e,
                    Err(e) => {
                        error!("Failed to restart encoder on switch: {}", e);
                        self.status = RecordingStatus::Error;
                        self.status_msg = StatusMessage::error(format!("Encoder error: {}", e));
                        self.active_window = Some(new_win);
                        return;
                    }
                };
            self.live_encoder = Some(encoder);
        }

        let frame_tx = self.live_encoder.as_ref().unwrap().frame_sender();
        match crate::capture::screen::start_capture(
            new_win.capture_hwnd(),
            frame_tx,
            new_win.canvas_crop_rect(),
            self.dropped_frames.clone(),
        ) {
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

        let encoder = match LiveEncoder::start(self.encoder_config(win.width, win.height)) {
            Ok(e) => e,
            Err(e) => {
                error!("Failed to start live encoder: {}", e);
                self.status = RecordingStatus::Error;
                self.status_msg = StatusMessage::error(format!("Encoder error: {}", e));
                return;
            }
        };

        let frame_tx = encoder.frame_sender();

        match crate::capture::screen::start_capture(
            win.capture_hwnd(),
            frame_tx,
            win.canvas_crop_rect(),
            self.dropped_frames.clone(),
        ) {
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

    fn stop_recording_internal(&mut self) {
        if let Some(handle) = self.capture_handle.take() {
            handle.stop();
        }
        if let Some(ref mixer) = self.audio_mixer {
            mixer.stop();
        }
        self.audio_mixer = None;
        if let Some(ref encoder) = self.live_encoder {
            encoder.stop();
        }
        // Keep the encoder alive while a save is in flight so we can poll results.
        if !self.is_saving.load(Ordering::SeqCst) {
            self.live_encoder = None;
        }
        self.status = RecordingStatus::Idle;

        // If preview is open, restart standalone preview capture.
        if self.show_preview {
            self.start_preview_capture();
        }
    }

    /// Pull the latest frame from encoder or standalone capture into the preview texture.
    pub(super) fn update_preview_texture(&mut self, ctx: &egui::Context) {
        if !self.show_preview {
            return;
        }
        let maybe_frame = if let Some(ref enc) = self.live_encoder {
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

        match crate::capture::screen::start_capture(
            win.capture_hwnd(),
            frame_tx,
            win.canvas_crop_rect(),
            self.dropped_frames.clone(),
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
        let video_secs = encoder.encoded_buffer().read().duration_secs();
        let audio_wav = self
            .audio_mixer
            .as_ref()
            .map(|m| m.snapshot_mixed_wav(video_secs))
            .filter(|w| !w.is_empty());

        if encoder.save_clip(&self.settings.output_dir, audio_wav, video_secs) {
            self.is_saving.store(true, Ordering::SeqCst);
            self.status = RecordingStatus::Saving;
            self.status_msg = StatusMessage::info("Saving...");
        } else {
            self.status_msg = StatusMessage::warning("Busy, retry");
        }
    }

    pub(super) fn poll_encode_results(&mut self) {
        if let Some(ref encoder) = self.live_encoder {
            while let Some(result) = encoder.poll_result() {
                self.is_saving.store(false, Ordering::SeqCst);
                self.status_msg = match result {
                    Ok(msg) => StatusMessage::success(msg),
                    Err(e) => StatusMessage::error(format!("Failed: {}", e)),
                };
                self.status = if self.capture_handle.is_some() {
                    RecordingStatus::Recording
                } else {
                    RecordingStatus::Idle
                };
            }
        }
        // If we were keeping the encoder alive for a save, drop it now.
        if !self.is_saving.load(Ordering::SeqCst) && self.capture_handle.is_none() {
            self.live_encoder = None;
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
                if self.capture_handle.is_some() {
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
