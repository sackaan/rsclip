use serde::{Deserialize, Serialize};

use crate::encoding::config::{RateControl, ScaleFilter, X264Preset};
use crate::utils;

pub const SETTINGS_KEY: &str = "rsclip_settings";

/// Struct-level `serde(default)` means any missing field during deserialization
/// falls back to the value from `Settings::default()`, so we don't need
/// per-field default functions.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub buffer_seconds: u32,
    pub fps: u32,
    pub output_dir: String,
    pub output_width: u32,
    pub output_height: u32,
    pub custom_resolution: bool,
    /// Save clip hotkey combo string, e.g. "F8" or "Ctrl+Shift+F8".
    pub hotkey_combo: String,
    pub hotkey_display: String,
    /// Start/stop recording hotkey combo string (empty = disabled).
    pub record_hotkey_combo: String,
    pub record_hotkey_display: String,
    pub rate_control: RateControl,
    /// CRF (x264) / QP (hardware) value. Lower = better quality, bigger files.
    pub crf: u32,
    /// Video bitrate in kbps (used when rate_control == CBR).
    pub video_bitrate_kbps: u32,
    pub x264_preset: X264Preset,
    /// Audio bitrate in kbps for AAC output.
    pub audio_bitrate_kbps: u32,
    pub scale_filter: ScaleFilter,
    pub mic_enabled: bool,
    /// WASAPI endpoint ID of the selected microphone (empty = none selected).
    pub mic_device_id: String,
    /// Friendly name of the selected mic (for display only).
    pub mic_device_name: String,
    /// Game audio volume (0.0–1.0).
    pub game_volume: f32,
    /// Microphone volume (0.0–1.0).
    pub mic_volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buffer_seconds: utils::DEFAULT_BUFFER_SECONDS,
            fps: utils::DEFAULT_FPS,
            output_dir: utils::default_clips_dir(),
            output_width: 1920,
            output_height: 1080,
            custom_resolution: false,
            hotkey_combo: utils::DEFAULT_HOTKEY_NAME.to_string(),
            hotkey_display: utils::DEFAULT_HOTKEY_NAME.to_string(),
            record_hotkey_combo: String::new(),
            record_hotkey_display: String::new(),
            rate_control: RateControl::default(),
            crf: 23,
            video_bitrate_kbps: 6000,
            x264_preset: X264Preset::default(),
            audio_bitrate_kbps: 128,
            scale_filter: ScaleFilter::default(),
            mic_enabled: false,
            mic_device_id: String::new(),
            mic_device_name: String::new(),
            game_volume: 1.0,
            mic_volume: 1.0,
        }
    }
}
