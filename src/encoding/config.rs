//! Encoder configuration: rate control, x264 preset, scale filter, and the
//! [`EncoderConfig`] struct passed to [`crate::encoding::encoder::LiveEncoder::start`].

use serde::{Deserialize, Serialize};

/// Video rate control mode.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::upper_case_acronyms)]
pub enum RateControl {
    /// Constant quality (CRF for x264, QP for hardware encoders).
    #[default]
    Quality,
    /// Constant bitrate.
    CBR,
}

/// x264 encoding preset (software encoder only). Faster presets compress
/// less efficiently but use less CPU.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum X264Preset {
    /// `ultrafast` — minimum CPU, largest files.
    Ultrafast,
    /// `superfast`.
    Superfast,
    /// `veryfast` — RSClip default.
    #[default]
    Veryfast,
    /// `faster`.
    Faster,
    /// `fast`.
    Fast,
    /// `medium` — x264 default.
    Medium,
    /// `slow`.
    Slow,
    /// `slower` — high quality, heavy CPU.
    Slower,
}

impl X264Preset {
    /// All variants, in CPU-cost order.
    pub const ALL: &[Self] = &[
        Self::Ultrafast,
        Self::Superfast,
        Self::Veryfast,
        Self::Faster,
        Self::Fast,
        Self::Medium,
        Self::Slow,
        Self::Slower,
    ];

    /// FFmpeg `-preset` value for this variant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ultrafast => "ultrafast",
            Self::Superfast => "superfast",
            Self::Veryfast => "veryfast",
            Self::Faster => "faster",
            Self::Fast => "fast",
            Self::Medium => "medium",
            Self::Slow => "slow",
            Self::Slower => "slower",
        }
    }
}

impl std::fmt::Display for X264Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FFmpeg scaling algorithm used when output resolution differs from input.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScaleFilter {
    /// Bilinear — fast, slightly soft.
    Bilinear,
    /// Bicubic — balanced.
    Bicubic,
    /// Lanczos — sharp, RSClip default.
    #[default]
    Lanczos,
    /// Spline — smooth, high quality.
    Spline,
    /// Area — best for downscaling.
    Area,
}

impl ScaleFilter {
    /// All variants in display order.
    pub const ALL: &[Self] = &[
        Self::Bilinear,
        Self::Bicubic,
        Self::Lanczos,
        Self::Spline,
        Self::Area,
    ];

    /// FFmpeg `sws_flags` value for this algorithm.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bilinear => "bilinear",
            Self::Bicubic => "bicubic",
            Self::Lanczos => "lanczos",
            Self::Spline => "spline",
            Self::Area => "area",
        }
    }

    /// Short human-readable description shown in the GUI.
    pub fn description(self) -> &'static str {
        match self {
            Self::Bilinear => "fast, soft",
            Self::Bicubic => "balanced",
            Self::Lanczos => "sharp, smooth edges",
            Self::Spline => "smooth, high quality",
            Self::Area => "best for downscaling",
        }
    }
}

impl std::fmt::Display for ScaleFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Snapshot of every encoder knob, taken once when recording starts.
pub struct EncoderConfig {
    /// Source frame width in pixels (one captured BGRA frame).
    pub width: u32,
    /// Source frame height in pixels.
    pub height: u32,
    /// Output frame rate.
    pub fps: u32,
    /// Rolling buffer retention window, in seconds.
    pub buffer_seconds: u32,
    /// Output (post-scale) frame width.
    pub output_width: u32,
    /// Output (post-scale) frame height.
    pub output_height: u32,
    /// Rate control mode.
    pub rate_control: RateControl,
    /// CRF (x264) or QP (hardware) quality value when [`RateControl::Quality`].
    pub crf: u32,
    /// Target video bitrate in kbps when [`RateControl::CBR`].
    pub video_bitrate_kbps: u32,
    /// libx264 preset (ignored by hardware encoders).
    pub x264_preset: X264Preset,
    /// AAC bitrate (kbps) used when remuxing audio into the final MP4.
    pub audio_bitrate_kbps: u32,
    /// Scaling algorithm applied when input/output dimensions differ.
    pub scale_filter: ScaleFilter,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_control_default_is_quality() {
        assert_eq!(RateControl::default(), RateControl::Quality);
    }

    #[test]
    fn x264_preset_default_is_veryfast() {
        assert_eq!(X264Preset::default(), X264Preset::Veryfast);
    }

    #[test]
    fn x264_preset_all_contains_all_variants() {
        assert_eq!(X264Preset::ALL.len(), 8);
        assert_eq!(X264Preset::ALL[0], X264Preset::Ultrafast);
        assert_eq!(X264Preset::ALL[7], X264Preset::Slower);
    }

    #[test]
    fn x264_preset_as_str_roundtrip() {
        for preset in X264Preset::ALL {
            let s = preset.as_str();
            assert!(!s.is_empty());
            assert_eq!(s, format!("{}", preset));
        }
    }

    #[test]
    fn scale_filter_default_is_lanczos() {
        assert_eq!(ScaleFilter::default(), ScaleFilter::Lanczos);
    }

    #[test]
    fn scale_filter_display_matches_as_str() {
        assert_eq!(format!("{}", ScaleFilter::Lanczos), "lanczos");
    }

    #[test]
    fn scale_filter_all_have_descriptions() {
        for filter in ScaleFilter::ALL {
            assert!(!filter.as_str().is_empty());
            assert!(!filter.description().is_empty());
        }
    }

    #[test]
    fn scale_filter_all_contains_all_variants() {
        assert_eq!(ScaleFilter::ALL.len(), 5);
    }
}
