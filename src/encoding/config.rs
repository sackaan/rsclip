use serde::{Deserialize, Serialize};

/// Video rate control mode.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum RateControl {
    /// Constant quality (CRF for x264, QP for hardware encoders).
    Quality,
    /// Constant bitrate.
    CBR,
}

impl Default for RateControl {
    fn default() -> Self {
        Self::Quality
    }
}

/// x264 encoding preset (software encoder only).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum X264Preset {
    Ultrafast,
    Superfast,
    Veryfast,
    Faster,
    Fast,
    Medium,
    Slow,
    Slower,
}

impl X264Preset {
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

impl Default for X264Preset {
    fn default() -> Self {
        Self::Veryfast
    }
}

impl std::fmt::Display for X264Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FFmpeg scaling algorithm.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum ScaleFilter {
    Bilinear,
    Bicubic,
    Lanczos,
    Spline,
    Area,
}

impl ScaleFilter {
    pub const ALL: &[Self] = &[
        Self::Bilinear,
        Self::Bicubic,
        Self::Lanczos,
        Self::Spline,
        Self::Area,
    ];

    /// FFmpeg sws_flags name for this algorithm.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bilinear => "bilinear",
            Self::Bicubic => "bicubic",
            Self::Lanczos => "lanczos",
            Self::Spline => "spline",
            Self::Area => "area",
        }
    }

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

impl Default for ScaleFilter {
    fn default() -> Self {
        Self::Lanczos
    }
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub buffer_seconds: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub rate_control: RateControl,
    pub crf: u32,
    pub video_bitrate_kbps: u32,
    pub x264_preset: X264Preset,
    pub audio_bitrate_kbps: u32,
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
