use eframe::egui;

use log::warn;

use super::app::ClipperApp;
use super::settings::{AppChannelConfig, GameTarget};
use super::theme;
use crate::capture::audio;
use crate::capture::sessions;
use crate::encoding::config::{RateControl, ScaleFilter, X264Preset};
use crate::platform::hotkeys;

const AUDIO_BITRATE_OPTIONS: &[u32] = &[64, 96, 128, 160, 192, 256, 320];

/// (width, height) pairs covering common monitor resolutions.
const RESOLUTION_PRESETS: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1024, 768),
    (1152, 864),
    (1280, 720),
    (1280, 800),
    (1280, 960),
    (1280, 1024),
    (1360, 768),
    (1366, 768),
    (1440, 900),
    (1440, 1080),
    (1600, 900),
    (1600, 1024),
    (1600, 1200),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (2560, 1600),
    (3440, 1440),
    (3840, 1080),
    (3840, 2160),
];

/// Aspect ratio labels and their (numerator, denominator) for filtering.
const ASPECT_RATIOS: &[(&str, (u32, u32))] = &[
    ("All", (0, 0)),
    ("4:3", (4, 3)),
    ("5:4", (5, 4)),
    ("16:9", (16, 9)),
    ("16:10", (16, 10)),
    ("21:9", (21, 9)),
    ("32:9", (32, 9)),
];

fn matches_aspect(w: u32, h: u32, num: u32, den: u32) -> bool {
    if num == 0 {
        return true;
    }
    w * den == h * num
}

fn abbreviate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        format!("…{}", &path[path.len() - max_len..])
    }
}

/// Press-to-bind hotkey widget. Returns `true` if a new hotkey was bound.
fn hotkey_bind_widget(
    ui: &mut egui::Ui,
    listening: &mut bool,
    combo: &mut String,
    display: &mut String,
) -> bool {
    let mut changed = false;
    if *listening {
        ui.add(
            egui::Button::new(
                egui::RichText::new("press a key\u{2026}")
                    .size(12.0)
                    .color(theme::orange()),
            )
            .fill(theme::bg_active())
            .min_size(egui::vec2(80.0, 0.0)),
        );

        let events: Vec<_> = ui.input(|i| i.events.clone());
        for event in &events {
            if let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            {
                if *key == egui::Key::Escape {
                    *listening = false;
                    break;
                }
                if let Some(combo_str) = hotkeys::build_hotkey_string(modifiers, key) {
                    let disp = hotkeys::display_hotkey_string(modifiers, key)
                        .unwrap_or_else(|| combo_str.clone());
                    *combo = combo_str;
                    *display = disp;
                    *listening = false;
                    changed = true;
                    break;
                }
            }
        }
    } else {
        let label = if display.is_empty() {
            "none"
        } else {
            display.as_str()
        };
        let color = if display.is_empty() {
            theme::dim()
        } else {
            theme::white()
        };
        let btn = egui::Button::new(egui::RichText::new(label).size(12.0).color(color))
            .fill(theme::bg_widget())
            .min_size(egui::vec2(80.0, 0.0));
        if ui.add(btn).clicked() {
            *listening = true;
        }
    }
    changed
}

fn section(
    ui: &mut egui::Ui,
    label: &str,
    default_open: bool,
    add_body: impl FnOnce(&mut egui::Ui),
) {
    egui::CollapsingHeader::new(egui::RichText::new(label).size(12.0).color(theme::white()))
        .default_open(default_open)
        .show(ui, |ui| {
            egui::Grid::new(label)
                .num_columns(2)
                .spacing([12.0, 4.0])
                .striped(true)
                .show(ui, add_body);
        });
}

impl ClipperApp {
    pub(super) fn draw_settings_screen(&mut self, ui: &mut egui::Ui, content_rect: egui::Rect) {
        let pad = 14.0;
        let inset_rect = content_rect.shrink2(egui::vec2(pad, pad));

        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .layout(egui::Layout::top_down(egui::Align::LEFT))
                .max_rect(inset_rect),
        );

        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(&mut content_ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 3.0);
                ui.spacing_mut().interact_size.y = 16.0;
                ui.spacing_mut().button_padding = egui::vec2(6.0, 2.0);

                self.section_game(ui);
                self.section_general(ui);
                self.section_audio(ui);
                self.section_encoding(ui);
                self.section_output(ui);
                self.section_hotkeys(ui);

                ui.add_space(8.0);
                ui.colored_label(
                    theme::dim(),
                    egui::RichText::new("fps / resolution apply on next recording start").size(9.0),
                );
            });
    }

    fn section_audio(&mut self, ui: &mut egui::Ui) {
        section(ui, "Audio / Mixer", true, |ui| {
            // ---- game volume ----
            ui.label(
                egui::RichText::new("game volume")
                    .size(12.0)
                    .color(theme::white()),
            );
            ui.horizontal(|ui| {
                let mut vol = self.settings.game_volume;
                if ui
                    .add(
                        egui::Slider::new(&mut vol, 0.0..=1.0)
                            .text("")
                            .custom_formatter(|v, _| format!("{}%", (v * 100.0) as u32))
                            .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                {
                    self.settings.game_volume = vol;
                    if let Some(ref mut mixer) = self.audio_mixer {
                        if let Some(ch) = mixer.game_channel_mut() {
                            ch.volume = vol;
                        }
                    }
                }
            });
            ui.end_row();

            // ---- mic enabled ----
            ui.label(
                egui::RichText::new("microphone")
                    .size(12.0)
                    .color(theme::white()),
            );
            let mut mic_on = self.settings.mic_enabled;
            if ui
                .checkbox(&mut mic_on, egui::RichText::new("enabled").size(10.0))
                .changed()
            {
                self.settings.mic_enabled = mic_on;
                if mic_on {
                    // Refresh device list when enabling.
                    match audio::enumerate_mic_devices() {
                        Ok(devs) => self.mic_devices = devs,
                        Err(e) => warn!("Failed to enumerate mic devices: {}", e),
                    }
                } else {
                    // Remove mic channel if recording.
                    if let Some(ref mut mixer) = self.audio_mixer {
                        mixer.remove_mic_channel();
                    }
                }
            }
            ui.end_row();

            if self.settings.mic_enabled {
                // ---- mic device picker ----
                ui.label(
                    egui::RichText::new("mic device")
                        .size(12.0)
                        .color(theme::white()),
                );
                ui.horizontal(|ui| {
                    let current_label = if self.settings.mic_device_name.is_empty() {
                        "select a device".to_string()
                    } else {
                        abbreviate_path(&self.settings.mic_device_name, 28)
                    };

                    let mut changed = false;
                    egui::ComboBox::from_id_salt("mic_device")
                        .selected_text(egui::RichText::new(&current_label).size(12.0))
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.spacing_mut().interact_size.y = 14.0;
                            for dev in &self.mic_devices {
                                let selected = dev.id == self.settings.mic_device_id;
                                if ui
                                    .selectable_label(
                                        selected,
                                        egui::RichText::new(&dev.name).size(10.0),
                                    )
                                    .clicked()
                                {
                                    self.settings.mic_device_id = dev.id.clone();
                                    self.settings.mic_device_name = dev.name.clone();
                                    changed = true;
                                }
                            }
                        });

                    // Refresh button.
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new("↻").size(12.0))
                                .fill(theme::bg_widget()),
                        )
                        .on_hover_text("Refresh device list")
                        .clicked()
                    {
                        match audio::enumerate_mic_devices() {
                            Ok(devs) => self.mic_devices = devs,
                            Err(e) => warn!("Failed to enumerate mic devices: {}", e),
                        }
                    }

                    // If device changed while recording, restart mic channel.
                    if changed {
                        if let Some(ref mut mixer) = self.audio_mixer {
                            mixer.remove_mic_channel();
                            if !self.settings.mic_device_id.is_empty() {
                                match mixer.add_mic_channel(&self.settings.mic_device_id) {
                                    Ok(()) => {
                                        if let Some(ch) = mixer.mic_channel_mut() {
                                            ch.volume = self.settings.mic_volume;
                                        }
                                    }
                                    Err(e) => warn!("Failed to start mic: {}", e),
                                }
                            }
                        }
                    }
                });
                ui.end_row();

                // ---- mic volume ----
                ui.label(
                    egui::RichText::new("mic volume")
                        .size(12.0)
                        .color(theme::white()),
                );
                ui.horizontal(|ui| {
                    let mut vol = self.settings.mic_volume;
                    if ui
                        .add(
                            egui::Slider::new(&mut vol, 0.0..=1.0)
                                .text("")
                                .custom_formatter(|v, _| format!("{}%", (v * 100.0) as u32))
                                .clamping(egui::SliderClamping::Always),
                        )
                        .changed()
                    {
                        self.settings.mic_volume = vol;
                        if let Some(ref mut mixer) = self.audio_mixer {
                            if let Some(ch) = mixer.mic_channel_mut() {
                                ch.volume = vol;
                            }
                        }
                    }
                });
                ui.end_row();
            }

            // ---- application audio sources ----
            self.draw_app_audio_rows(ui);

            // ---- audio bitrate ----
            ui.label(
                egui::RichText::new("audio bitrate")
                    .size(12.0)
                    .color(theme::white()),
            );
            {
                let current = self.settings.audio_bitrate_kbps;
                let label = format!("{} kbps", current);
                egui::ComboBox::from_id_salt("audio_bitrate")
                    .selected_text(egui::RichText::new(&label).size(12.0))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.spacing_mut().interact_size.y = 14.0;
                        for &br in AUDIO_BITRATE_OPTIONS {
                            let lbl = format!("{} kbps", br);
                            ui.selectable_value(
                                &mut self.settings.audio_bitrate_kbps,
                                br,
                                egui::RichText::new(&lbl).size(10.0),
                            );
                        }
                    });
            }
            ui.end_row();
        });
    }

    /// Renders the list of configured application audio channels plus the
    /// "+ add application" picker. Called from inside the Audio grid, so it
    /// must close every row with `ui.end_row()`.
    fn draw_app_audio_rows(&mut self, ui: &mut egui::Ui) {
        // Header row: title in label cell, "add" button in control cell.
        ui.label(
            egui::RichText::new("application audio")
                .size(12.0)
                .color(theme::white()),
        );
        ui.horizontal(|ui| {
            let btn_label = if self.app_picker_open {
                "cancel"
            } else {
                "+ add application"
            };
            if ui
                .add(
                    egui::Button::new(egui::RichText::new(btn_label).size(10.0))
                        .fill(theme::bg_widget()),
                )
                .clicked()
            {
                self.app_picker_open = !self.app_picker_open;
                if self.app_picker_open {
                    self.refresh_app_audio_sessions();
                }
            }
        });
        ui.end_row();

        // Picker row (only when open).
        if self.app_picker_open {
            ui.label("");
            ui.horizontal(|ui| {
                let already: std::collections::HashSet<String> = self
                    .settings
                    .app_channels
                    .iter()
                    .map(|c| c.exe_name.clone())
                    .collect();

                let selectable: Vec<_> = self
                    .app_audio_sessions
                    .iter()
                    .filter(|s| !already.contains(&s.exe_name))
                    .cloned()
                    .collect();

                let selected_label = if selectable.is_empty() {
                    "no apps producing sound"
                } else {
                    "pick application\u{2026}"
                };
                let mut picked: Option<sessions::AudioSession> = None;
                egui::ComboBox::from_id_salt("app_audio_picker")
                    .selected_text(egui::RichText::new(selected_label).size(10.0))
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.spacing_mut().interact_size.y = 14.0;
                        for sess in &selectable {
                            let label = format!("{} (PID {})", sess.display_name, sess.pid);
                            if ui
                                .selectable_label(false, egui::RichText::new(&label).size(10.0))
                                .clicked()
                            {
                                picked = Some(sess.clone());
                            }
                        }
                    });
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("\u{21bb}").size(10.0))
                            .fill(theme::bg_widget()),
                    )
                    .on_hover_text("refresh")
                    .clicked()
                {
                    self.refresh_app_audio_sessions();
                }
                if let Some(sess) = picked {
                    self.add_app_channel_from_session(sess);
                    self.app_picker_open = false;
                }
            });
            ui.end_row();
        }

        // One row per configured channel. Collect removals to apply after the loop.
        let mut to_remove: Option<String> = None;
        let mut volume_changes: Vec<(String, f32)> = Vec::new();

        for cfg in &mut self.settings.app_channels {
            ui.label(
                egui::RichText::new(&cfg.display_name)
                    .size(11.0)
                    .color(theme::white()),
            );
            ui.horizontal(|ui| {
                let mut vol = cfg.volume;
                if ui
                    .add(
                        egui::Slider::new(&mut vol, 0.0..=1.0)
                            .text("")
                            .custom_formatter(|v, _| format!("{}%", (v * 100.0) as u32))
                            .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                {
                    cfg.volume = vol;
                    volume_changes.push((cfg.exe_name.clone(), vol));
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("\u{00d7}")
                                .size(11.0)
                                .color(theme::white()),
                        )
                        .fill(theme::bg_widget()),
                    )
                    .on_hover_text("remove")
                    .clicked()
                {
                    to_remove = Some(cfg.exe_name.clone());
                }
            });
            ui.end_row();
        }

        // Apply live-mixer updates after the borrow loop.
        for (exe, vol) in volume_changes {
            if let Some(ref mut mixer) = self.audio_mixer {
                if let Some(pid) = mixer.application_pid_for_exe(&exe) {
                    if let Some(ch) = mixer.application_channel_mut(pid) {
                        ch.volume = vol;
                    }
                }
            }
        }
        if let Some(exe) = to_remove {
            if let Some(ref mut mixer) = self.audio_mixer {
                if let Some(pid) = mixer.application_pid_for_exe(&exe) {
                    mixer.remove_application_channel(pid);
                }
            }
            self.settings.app_channels.retain(|c| c.exe_name != exe);
        }
    }

    fn refresh_app_audio_sessions(&mut self) {
        let game_pid = match self.settings.game_target {
            GameTarget::RuneLite => self.active_window.as_ref().map(|w| w.pid),
            GameTarget::Cs2 => self.cs2_window.as_ref().map(|w| w.pid),
        }
        .unwrap_or(0);
        match sessions::enumerate_audio_sessions(&[game_pid]) {
            Ok(s) => self.app_audio_sessions = s,
            Err(e) => warn!("Audio session enumeration failed: {}", e),
        }
    }

    fn add_app_channel_from_session(&mut self, sess: sessions::AudioSession) {
        self.settings.app_channels.push(AppChannelConfig {
            exe_name: sess.exe_name.clone(),
            display_name: sess.display_name.clone(),
            volume: 1.0,
        });
        if let Some(ref mut mixer) = self.audio_mixer {
            if let Err(e) = mixer.add_application_channel(sess.pid, sess.exe_name.clone()) {
                warn!("Failed to start app channel {}: {}", sess.display_name, e);
            }
        }
    }

    fn section_game(&mut self, ui: &mut egui::Ui) {
        section(ui, "Game", true, |ui| {
            // ---- target game (selects the capture pipeline) ----
            ui.label(
                egui::RichText::new("target")
                    .size(12.0)
                    .color(theme::white()),
            );
            let prev = self.settings.game_target;
            ui.horizontal(|ui| {
                for target in [GameTarget::RuneLite, GameTarget::Cs2] {
                    ui.selectable_value(
                        &mut self.settings.game_target,
                        target,
                        egui::RichText::new(target.label()).size(10.0),
                    );
                }
            });
            ui.end_row();

            if self.settings.game_target != prev {
                // Switching pipelines: stop whatever was running on the old one.
                self.stop_any_recording();
            }

            if self.settings.game_target == GameTarget::Cs2 {
                self.cs2_settings_rows(ui);
            }
        });
    }

    /// CS2-only controls (capture fps, blur, intermediate bitrate). Called
    /// inside the Game grid, so every row ends with `ui.end_row()`.
    fn cs2_settings_rows(&mut self, ui: &mut egui::Ui) {
        // ---- capture fps (capped at monitor refresh by WGC) ----
        ui.label(
            egui::RichText::new("capture fps")
                .size(12.0)
                .color(theme::white()),
        );
        ui.horizontal(|ui| {
            let mut fps = self.settings.cs2_capture_fps;
            if ui
                .add(egui::DragValue::new(&mut fps).range(60..=360).speed(1))
                .changed()
            {
                self.settings.cs2_capture_fps = fps;
            }
            ui.colored_label(theme::dim(), egui::RichText::new("≤ monitor Hz").size(9.0));
        });
        ui.end_row();

        // ---- output fps ----
        ui.label(
            egui::RichText::new("output fps")
                .size(12.0)
                .color(theme::white()),
        );
        let mut out_fps = self.settings.cs2_output_fps;
        if ui
            .add(egui::DragValue::new(&mut out_fps).range(24..=120).speed(1))
            .changed()
        {
            self.settings.cs2_output_fps = out_fps;
        }
        ui.end_row();

        // ---- motion blur toggle ----
        ui.label(
            egui::RichText::new("motion blur")
                .size(12.0)
                .color(theme::white()),
        );
        ui.checkbox(
            &mut self.settings.cs2_blur_enabled,
            egui::RichText::new("blend real frames").size(10.0),
        );
        ui.end_row();

        // ---- blur strength (shutter) ----
        if self.settings.cs2_blur_enabled {
            ui.label(
                egui::RichText::new("blur strength")
                    .size(12.0)
                    .color(theme::white()),
            );
            ui.horizontal(|ui| {
                let mut pct = self.settings.cs2_blur_pct;
                if ui
                    .add(
                        egui::Slider::new(&mut pct, 0..=100)
                            .text("")
                            .custom_formatter(|v, _| format!("{}%", v as u32))
                            .clamping(egui::SliderClamping::Always),
                    )
                    .changed()
                {
                    self.settings.cs2_blur_pct = pct;
                }
                ui.colored_label(
                    theme::dim(),
                    egui::RichText::new(if pct >= 100 {
                        "360° shutter"
                    } else if pct >= 50 {
                        "cinematic"
                    } else {
                        "subtle"
                    })
                    .size(9.0),
                );
            });
            ui.end_row();
        }

        // ---- capture (intermediate) bitrate ----
        ui.label(
            egui::RichText::new("capture bitrate")
                .size(12.0)
                .color(theme::white()),
        );
        let mut br = self.settings.cs2_bitrate_kbps;
        if ui
            .add(
                egui::DragValue::new(&mut br)
                    .range(5_000..=150_000)
                    .speed(500)
                    .suffix(" kbps"),
            )
            .changed()
        {
            self.settings.cs2_bitrate_kbps = br;
        }
        ui.end_row();
    }

    fn section_general(&mut self, ui: &mut egui::Ui) {
        section(ui, "General", true, |ui| {
            // ---- buffer ----
            ui.label(
                egui::RichText::new("length (seconds)")
                    .size(12.0)
                    .color(theme::white()),
            );
            let mut secs = self.settings.buffer_seconds;
            if ui
                .add(egui::DragValue::new(&mut secs).range(5..=300).speed(1))
                .changed()
            {
                self.settings.buffer_seconds = secs;
                if let Some(ref enc) = self.live_encoder {
                    enc.encoded_buffer().write().set_max_duration(secs);
                }
            }
            ui.end_row();

            // ---- fps ----
            ui.label(egui::RichText::new("fps").size(12.0).color(theme::white()));
            let mut fps = self.settings.fps;
            if ui
                .add(egui::DragValue::new(&mut fps).range(15..=360).speed(1))
                .changed()
            {
                self.settings.fps = fps;
            }
            ui.end_row();
        });
    }

    fn section_encoding(&mut self, ui: &mut egui::Ui) {
        section(ui, "Encoding", false, |ui| {
            // ---- rate control ----
            ui.label(
                egui::RichText::new("rate control")
                    .size(12.0)
                    .color(theme::white()),
            );
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.settings.rate_control,
                    RateControl::Quality,
                    egui::RichText::new("Quality (CRF)").size(10.0),
                );
                ui.selectable_value(
                    &mut self.settings.rate_control,
                    RateControl::CBR,
                    egui::RichText::new("CBR").size(10.0),
                );
            });
            ui.end_row();

            // ---- quality / bitrate ----
            match self.settings.rate_control {
                RateControl::Quality => {
                    ui.label(
                        egui::RichText::new("quality (CRF)")
                            .size(12.0)
                            .color(theme::white()),
                    );
                    let mut crf = self.settings.crf;
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Slider::new(&mut crf, 0..=51)
                                    .text("")
                                    .clamping(egui::SliderClamping::Always),
                            )
                            .changed()
                        {
                            self.settings.crf = crf;
                        }
                        ui.colored_label(
                            theme::dim(),
                            egui::RichText::new(if crf <= 18 {
                                "high"
                            } else if crf <= 23 {
                                "med"
                            } else {
                                "low"
                            })
                            .size(9.0),
                        );
                    });
                    ui.end_row();
                }
                RateControl::CBR => {
                    ui.label(
                        egui::RichText::new("bitrate (kbps)")
                            .size(12.0)
                            .color(theme::white()),
                    );
                    let mut br = self.settings.video_bitrate_kbps;
                    if ui
                        .add(
                            egui::DragValue::new(&mut br)
                                .range(500..=50000)
                                .speed(100)
                                .suffix(" kbps"),
                        )
                        .changed()
                    {
                        self.settings.video_bitrate_kbps = br;
                    }
                    ui.end_row();
                }
            }

            // ---- encoder preset ----
            ui.label(
                egui::RichText::new("encoder preset")
                    .size(12.0)
                    .color(theme::white()),
            );
            {
                let current = self.settings.x264_preset;
                egui::ComboBox::from_id_salt("x264_preset")
                    .selected_text(egui::RichText::new(current.as_str()).size(12.0))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.spacing_mut().interact_size.y = 14.0;
                        for &preset in X264Preset::ALL {
                            ui.selectable_value(
                                &mut self.settings.x264_preset,
                                preset,
                                egui::RichText::new(preset.as_str()).size(10.0),
                            );
                        }
                    });
            }
            ui.end_row();
        });
    }

    fn section_output(&mut self, ui: &mut egui::Ui) {
        section(ui, "Output", false, |ui| {
            // ---- resolution ----
            ui.label(
                egui::RichText::new("resolution")
                    .size(12.0)
                    .color(theme::white()),
            );
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut self.settings.native_resolution,
                    egui::RichText::new("native").size(10.0),
                )
                .on_hover_text("Record at the game's own size, no scaling");
                if !self.settings.native_resolution {
                    ui.checkbox(
                        &mut self.settings.custom_resolution,
                        egui::RichText::new("custom").size(10.0),
                    );
                }
            });
            ui.end_row();

            if self.settings.native_resolution {
                ui.label("");
                ui.colored_label(
                    theme::dim(),
                    egui::RichText::new("same as game canvas").size(10.0),
                );
                ui.end_row();
                self.output_folder_row(ui);
                return;
            }

            ui.label("");
            if self.settings.custom_resolution {
                ui.horizontal(|ui| {
                    let mut w = self.settings.output_width;
                    let mut h = self.settings.output_height;
                    let w_changed = ui
                        .add(egui::DragValue::new(&mut w).range(320..=7680).speed(2))
                        .changed();
                    ui.colored_label(theme::dim(), "x");
                    let h_changed = ui
                        .add(egui::DragValue::new(&mut h).range(240..=4320).speed(2))
                        .changed();
                    if w_changed {
                        self.settings.output_width = w;
                    }
                    if h_changed {
                        self.settings.output_height = h;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    let aspect_label = ASPECT_RATIOS[self.selected_aspect].0;
                    egui::ComboBox::from_id_salt("aspect_ratio")
                        .selected_text(egui::RichText::new(aspect_label).size(12.0))
                        .width(48.0)
                        .show_ui(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.spacing_mut().interact_size.y = 14.0;
                            for (i, &(label, _)) in ASPECT_RATIOS.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.selected_aspect,
                                    i,
                                    egui::RichText::new(label).size(10.0),
                                );
                            }
                        });

                    let (num, den) = ASPECT_RATIOS[self.selected_aspect].1;
                    let filtered: Vec<(u32, u32)> = RESOLUTION_PRESETS
                        .iter()
                        .copied()
                        .filter(|&(w, h)| matches_aspect(w, h, num, den))
                        .collect();

                    let current_label = format!(
                        "{}x{}",
                        self.settings.output_width, self.settings.output_height
                    );
                    egui::ComboBox::from_id_salt("resolution_preset")
                        .selected_text(egui::RichText::new(&current_label).size(12.0))
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.spacing_mut().interact_size.y = 14.0;
                            for &(w, h) in &filtered {
                                let label = format!("{}x{}", w, h);
                                let selected = w == self.settings.output_width
                                    && h == self.settings.output_height;
                                if ui
                                    .selectable_label(
                                        selected,
                                        egui::RichText::new(&label).size(10.0),
                                    )
                                    .clicked()
                                {
                                    self.settings.output_width = w;
                                    self.settings.output_height = h;
                                }
                            }
                        });
                });
            }
            ui.end_row();

            // ---- scale filter ----
            ui.label(
                egui::RichText::new("scaling")
                    .size(12.0)
                    .color(theme::white()),
            );
            {
                let current = self.settings.scale_filter;
                egui::ComboBox::from_id_salt("scale_filter")
                    .selected_text(egui::RichText::new(current.as_str()).size(12.0))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.spacing_mut().interact_size.y = 14.0;
                        for &filter in ScaleFilter::ALL {
                            let lbl = format!("{} — {}", filter.as_str(), filter.description());
                            ui.selectable_value(
                                &mut self.settings.scale_filter,
                                filter,
                                egui::RichText::new(&lbl).size(10.0),
                            );
                        }
                    });
            }
            ui.end_row();

            self.output_folder_row(ui);
        });
    }

    fn output_folder_row(&mut self, ui: &mut egui::Ui) {
        // ---- folder ----
        ui.label(
            egui::RichText::new("folder")
                .size(12.0)
                .color(theme::white()),
        );
        {
            let display_path = abbreviate_path(&self.settings.output_dir, 30);
            let btn = egui::Button::new(
                egui::RichText::new(display_path)
                    .size(12.0)
                    .color(theme::white()),
            )
            .fill(theme::bg_widget());
            if ui
                .add(btn)
                .on_hover_text(&self.settings.output_dir)
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_directory(&self.settings.output_dir)
                    .pick_folder()
                {
                    self.settings.output_dir = folder.to_string_lossy().into_owned();
                }
            }
        }
        ui.end_row();
    }

    fn section_hotkeys(&mut self, ui: &mut egui::Ui) {
        let mut rebuild_needed = false;

        section(ui, "Hotkeys", true, |ui| {
            // ---- save clip hotkey ----
            ui.label(
                egui::RichText::new("save clip")
                    .size(12.0)
                    .color(theme::white()),
            );
            if hotkey_bind_widget(
                ui,
                &mut self.listening_for_hotkey,
                &mut self.settings.hotkey_combo,
                &mut self.settings.hotkey_display,
            ) {
                rebuild_needed = true;
            }
            ui.end_row();

            // ---- start/stop recording hotkey ----
            ui.label(
                egui::RichText::new("start / stop")
                    .size(12.0)
                    .color(theme::white()),
            );
            if hotkey_bind_widget(
                ui,
                &mut self.listening_for_record_hotkey,
                &mut self.settings.record_hotkey_combo,
                &mut self.settings.record_hotkey_display,
            ) {
                rebuild_needed = true;
            }
            ui.end_row();
        });

        if rebuild_needed {
            self.rebuild_hotkeys();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_aspect_all() {
        // num=0 means "All" — everything matches
        assert!(matches_aspect(1920, 1080, 0, 0));
        assert!(matches_aspect(800, 600, 0, 0));
    }

    #[test]
    fn test_matches_aspect_16_9() {
        assert!(matches_aspect(1920, 1080, 16, 9));
        assert!(matches_aspect(1280, 720, 16, 9));
        assert!(!matches_aspect(1280, 1024, 16, 9));
    }

    #[test]
    fn test_matches_aspect_4_3() {
        assert!(matches_aspect(1024, 768, 4, 3));
        assert!(matches_aspect(640, 480, 4, 3));
        assert!(!matches_aspect(1920, 1080, 4, 3));
    }

    #[test]
    fn test_abbreviate_path_short() {
        assert_eq!(abbreviate_path("C:\\short", 30), "C:\\short");
    }

    #[test]
    fn test_abbreviate_path_long() {
        let long = "C:\\Users\\SomeUser\\Documents\\RSClips\\very\\long\\path";
        let result = abbreviate_path(long, 20);
        // The result is "…" (1 char) + last 20 chars of the path
        assert!(result.starts_with('\u{2026}')); // starts with "…"
                                                 // The path portion (after "…") should be exactly max_len chars
        assert_eq!(result.chars().count(), 21); // 1 ellipsis + 20 path chars
    }
}
