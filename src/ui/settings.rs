use serde::{Deserialize, Serialize};

use crate::encoding::config::{RateControl, ScaleFilter, X264Preset};
use crate::utils;

pub const SETTINGS_KEY: &str = "rsclip_settings";

/// Which game the app is currently set up to capture. RuneLite uses the
/// original CPU-readback + FFmpeg pipeline; CS2 uses the GPU zero-copy
/// (Media Foundation) pipeline with motion-blur on save.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GameTarget {
    #[default]
    RuneLite,
    Cs2,
}

impl GameTarget {
    pub fn label(&self) -> &'static str {
        match self {
            GameTarget::RuneLite => "RuneLite",
            GameTarget::Cs2 => "CS2",
        }
    }
}

/// One user-configured application audio source. Identified by exe basename
/// so the same configuration survives the process getting restarted with a
/// new PID.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppChannelConfig {
    /// Lowercased executable basename (e.g. `"discord.exe"`).
    pub exe_name: String,
    /// Friendly label for the UI.
    pub display_name: String,
    /// Mix volume (0.0–1.0).
    pub volume: f32,
}

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
    /// Encode at the captured size with no scaling. When false, output is
    /// scaled to `output_width`×`output_height` with the aspect ratio kept.
    pub native_resolution: bool,
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
    /// User-configured application audio sources (Discord, browser, etc.).
    /// Resolved to live PIDs at recording start via WASAPI session
    /// enumeration; entries whose exe is not currently running are skipped.
    pub app_channels: Vec<AppChannelConfig>,

    /// Which game the app is targeting (selects the capture pipeline).
    pub game_target: GameTarget,
    /// CS2 real-frame capture rate, in fps. Capped at the monitor refresh by
    /// Windows Graphics Capture (e.g. 240 on a 240 Hz panel).
    pub cs2_capture_fps: u32,
    /// CS2 final output frame rate after the motion-blur blend (e.g. 60).
    pub cs2_output_fps: u32,
    /// Whether to apply real-frame motion blur when saving CS2 clips.
    pub cs2_blur_enabled: bool,
    /// CS2 blur shutter strength, 0–100 % (100 = blend every frame in the
    /// interval = 360° shutter).
    pub cs2_blur_pct: u32,
    /// Bitrate (kbps) for the CS2 capture-time H.264 encode held in the ring
    /// buffer. High, since it is the intermediate that the blur re-encodes.
    pub cs2_bitrate_kbps: u32,
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
            native_resolution: true,
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
            app_channels: Vec::new(),
            game_target: GameTarget::default(),
            cs2_capture_fps: 240,
            cs2_output_fps: 60,
            cs2_blur_enabled: true,
            cs2_blur_pct: 50,
            cs2_bitrate_kbps: 50_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_output_is_native_resolution() {
        assert!(Settings::default().native_resolution);
    }

    #[test]
    fn settings_saved_before_native_flag_load_as_native() {
        // Older saves have no `native_resolution` key; they must not keep
        // the old forced 1920x1080 upscale.
        let old = "(output_width: 1920, output_height: 1080)";
        let s: Settings = ron::from_str(old).unwrap();
        assert!(s.native_resolution);
        assert_eq!(s.output_width, 1920);
    }
}
