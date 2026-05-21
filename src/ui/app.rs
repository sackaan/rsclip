use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use log::{error, info, warn};

use crate::capture::audio::{AudioMixer, MicDevice};
use crate::capture::screen::CaptureHandle;
use crate::encoding::config::EncoderConfig;
use crate::encoding::encoder::{self, LiveEncoder, SharedPreviewFrame};
use crate::platform::hotkeys::HotkeyManager;
use crate::platform::runelite::RuneLiteWindow;
use crate::utils::{RecordingStatus, StatusMessage};

use super::settings::{Settings, SETTINGS_KEY};
use super::theme;

const RS_FONT_FAMILY: &str = "RuneScape";

#[derive(Default, PartialEq, Eq)]
pub(super) enum Screen {
    #[default]
    Main,
    Settings,
}

/// Top-level GUI state. Implements [`eframe::App`].
///
/// Fields are visible only inside the [`crate::ui`] module tree; the rest of
/// the crate interacts with the app through the [`eframe::App`] trait.
pub struct ClipperApp {
    pub(super) settings: Settings,

    pub(super) status: RecordingStatus,
    pub(super) runelite_windows: Vec<RuneLiteWindow>,
    pub(super) active_window: Option<RuneLiteWindow>,
    pub(super) last_window_check: Instant,

    pub(super) capture_handle: Option<CaptureHandle>,
    pub(super) live_encoder: Option<LiveEncoder>,
    pub(super) audio_mixer: Option<AudioMixer>,

    pub(super) mic_devices: Vec<MicDevice>,

    pub(super) hotkey_manager: Option<HotkeyManager>,

    pub(super) status_msg: StatusMessage,
    pub(super) ffmpeg_available: bool,
    pub(super) is_saving: Arc<AtomicBool>,

    pub(super) show_console: bool,
    pub(super) screen: Screen,
    pub(super) show_preview: bool,

    pub(super) listening_for_hotkey: bool,
    pub(super) listening_for_record_hotkey: bool,
    pub(super) selected_aspect: usize,
    pub(super) rs_font: egui::FontId,
    pub(super) preview_texture: Option<egui::TextureHandle>,
    pub(super) dropped_frames: Arc<AtomicU64>,
    pub(super) preview_capture: Option<CaptureHandle>,
    pub(super) preview_frame: SharedPreviewFrame,
    pub(super) cached_log_entries: Vec<crate::logging::LogEntry>,
    pub(super) log_generation: u64,
}

impl ClipperApp {
    /// Construct the app: load persisted settings, install fonts and theme,
    /// register hotkeys, probe FFmpeg. Called once at startup by `eframe`.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            RS_FONT_FAMILY.to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                "../assets/runescape_large.ttf"
            ))),
        );
        fonts
            .families
            .entry(egui::FontFamily::Name(RS_FONT_FAMILY.into()))
            .or_default()
            .push(RS_FONT_FAMILY.to_owned());
        cc.egui_ctx.set_fonts(fonts);

        let rs_font = egui::FontId::new(40.0, egui::FontFamily::Name(RS_FONT_FAMILY.into()));

        theme::apply_theme(&cc.egui_ctx);

        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, SETTINGS_KEY))
            .unwrap_or_default();

        let hotkey_manager = match HotkeyManager::new(
            &settings.hotkey_combo,
            (!settings.record_hotkey_combo.is_empty())
                .then_some(settings.record_hotkey_combo.as_str()),
        ) {
            Ok(hm) => Some(hm),
            Err(e) => {
                error!("Failed to init hotkeys: {}", e);
                None
            }
        };

        let ffmpeg_available = encoder::is_ffmpeg_available();
        if !ffmpeg_available {
            warn!("FFmpeg not found in PATH — encoding will fail");
        }

        Self {
            settings,

            status: RecordingStatus::Idle,
            runelite_windows: Vec::new(),
            active_window: None,
            last_window_check: Instant::now(),

            capture_handle: None,
            live_encoder: None,
            audio_mixer: None,
            mic_devices: Vec::new(),

            hotkey_manager,

            status_msg: if ffmpeg_available {
                StatusMessage::default()
            } else {
                StatusMessage::warning("FFmpeg not found")
            },
            ffmpeg_available,
            is_saving: Arc::new(AtomicBool::new(false)),

            show_console: false,
            screen: Screen::Main,
            show_preview: false,
            listening_for_hotkey: false,
            listening_for_record_hotkey: false,
            selected_aspect: 0,
            rs_font,
            preview_texture: None,
            dropped_frames: Arc::new(AtomicU64::new(0)),
            preview_capture: None,
            preview_frame: Arc::new(parking_lot::Mutex::new(None)),
            cached_log_entries: Vec::new(),
            log_generation: 0,
        }
    }

    pub(super) fn rebuild_hotkeys(&mut self) {
        if let Some(ref hm) = self.hotkey_manager {
            hm.unregister_all();
        }
        match HotkeyManager::new(
            &self.settings.hotkey_combo,
            (!self.settings.record_hotkey_combo.is_empty())
                .then_some(self.settings.record_hotkey_combo.as_str()),
        ) {
            Ok(hm) => {
                self.hotkey_manager = Some(hm);
                info!("Hotkeys updated");
            }
            Err(e) => {
                error!("Failed to register hotkeys: {}", e);
                self.hotkey_manager = None;
                self.status_msg = StatusMessage::error(format!("Hotkey error: {}", e));
            }
        }
    }

    pub(super) fn encoder_config(&self, width: u32, height: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            fps: self.settings.fps,
            buffer_seconds: self.settings.buffer_seconds,
            output_width: self.settings.output_width,
            output_height: self.settings.output_height,
            rate_control: self.settings.rate_control,
            crf: self.settings.crf,
            video_bitrate_kbps: self.settings.video_bitrate_kbps,
            x264_preset: self.settings.x264_preset,
            audio_bitrate_kbps: self.settings.audio_bitrate_kbps,
            scale_filter: self.settings.scale_filter,
        }
    }
}

impl eframe::App for ClipperApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_runelite_window();
        self.poll_hotkeys();
        self.poll_encode_results();
        ctx.request_repaint_after(std::time::Duration::from_millis(100));

        if self.show_preview {
            if self.capture_handle.is_none() && self.preview_capture.is_none() {
                self.start_preview_capture();
            }
        } else if self.preview_capture.is_some() {
            self.stop_preview_capture();
        }

        self.update_preview_texture(ctx);

        self.draw_footer(ctx);
        self.draw_console(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::bg())
                    .inner_margin(egui::Margin::same(0)),
            )
            .show(ctx, |ui| {
                let content_rect = ui.available_rect_before_wrap();
                match self.screen {
                    Screen::Main => self.draw_main_screen(ui, content_rect),
                    Screen::Settings => self.draw_settings_screen(ui, content_rect),
                }
            });
    }
}
