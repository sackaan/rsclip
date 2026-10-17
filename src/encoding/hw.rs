//! Hardware encoder selection and FFmpeg invocation helpers.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use log::{info, warn};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::encoding::config::{RateControl, X264Preset};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Create an `ffmpeg` `Command` with no visible console window.
pub(crate) fn ffmpeg_cmd() -> Command {
    // Look for ffmpeg.exe next to our own executable first.
    let ffmpeg_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("ffmpeg.exe")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "ffmpeg".to_string());

    let mut cmd = Command::new(ffmpeg_path);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Which H.264 encoder to drive FFmpeg with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwEncoder {
    /// NVIDIA NVENC (`h264_nvenc`).
    Nvenc,
    /// Intel QuickSync (`h264_qsv`).
    Qsv,
    /// AMD AMF (`h264_amf`).
    Amf,
    /// libx264 software fallback.
    Software,
}

impl HwEncoder {
    /// Human-friendly short name (shown in the GUI status bar).
    pub fn display_name(&self) -> &'static str {
        match self {
            HwEncoder::Nvenc => "NVENC",
            HwEncoder::Qsv => "QSV",
            HwEncoder::Amf => "AMF",
            HwEncoder::Software => "x264",
        }
    }

    /// FFmpeg codec name (`-c:v` value).
    pub fn codec_name(&self) -> &'static str {
        match self {
            HwEncoder::Nvenc => "h264_nvenc",
            HwEncoder::Qsv => "h264_qsv",
            HwEncoder::Amf => "h264_amf",
            HwEncoder::Software => "libx264",
        }
    }

    /// FFmpeg pixel format expected on output (QSV wants NV12; the rest are
    /// happy with YUV420P).
    pub fn pix_fmt(&self) -> &'static str {
        match self {
            HwEncoder::Qsv => "nv12",
            _ => "yuv420p",
        }
    }

    /// Build the rate-control + quality argv fragment for this encoder.
    pub fn quality_args(
        &self,
        rate_control: RateControl,
        crf: u32,
        bitrate_kbps: u32,
        preset: X264Preset,
    ) -> Vec<String> {
        match rate_control {
            RateControl::Quality => match self {
                HwEncoder::Nvenc => vec!["-qp".into(), crf.to_string()],
                HwEncoder::Qsv => vec!["-global_quality".into(), crf.to_string()],
                HwEncoder::Amf => vec![
                    "-quality".into(),
                    "balanced".into(),
                    "-qp_i".into(),
                    crf.to_string(),
                    "-qp_p".into(),
                    crf.to_string(),
                ],
                HwEncoder::Software => vec![
                    "-preset".into(),
                    preset.as_str().into(),
                    "-crf".into(),
                    crf.to_string(),
                ],
            },
            RateControl::CBR => {
                let br = format!("{}k", bitrate_kbps);
                match self {
                    HwEncoder::Nvenc => vec!["-rc".into(), "cbr".into(), "-b:v".into(), br],
                    HwEncoder::Qsv => vec!["-b:v".into(), br],
                    HwEncoder::Amf => vec!["-rc".into(), "cbr".into(), "-b:v".into(), br],
                    HwEncoder::Software => {
                        let bufsize = format!("{}k", bitrate_kbps * 2);
                        vec![
                            "-preset".into(),
                            preset.as_str().into(),
                            "-b:v".into(),
                            br.clone(),
                            "-maxrate".into(),
                            br,
                            "-bufsize".into(),
                            bufsize,
                        ]
                    }
                }
            }
        }
    }
}

impl std::fmt::Display for HwEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Probe NVENC / QSV / AMF in that order; return the first that succeeds,
/// otherwise [`HwEncoder::Software`].
///
/// Probing spawns up to three FFmpeg processes (~2 s), so the result is
/// computed once per process and cached. Concurrent callers block until the
/// first probe finishes; call it early from a background thread to warm it.
pub fn detect_best_encoder() -> HwEncoder {
    static DETECTED: std::sync::OnceLock<HwEncoder> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(probe_best_encoder)
}

fn probe_best_encoder() -> HwEncoder {
    for enc in &[HwEncoder::Nvenc, HwEncoder::Qsv, HwEncoder::Amf] {
        if probe_encoder(enc.codec_name()) {
            info!("Hardware encoder available: {:?}", enc);
            return *enc;
        }
    }
    info!("No hardware encoder found, falling back to libx264");
    HwEncoder::Software
}

/// Whether `ffmpeg -version` succeeds (FFmpeg is on PATH or beside the .exe).
pub fn is_ffmpeg_available() -> bool {
    ffmpeg_cmd()
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Probe whether an encoder works by encoding a single test frame.
fn probe_encoder(codec: &str) -> bool {
    let mut cmd = ffmpeg_cmd();
    cmd.arg("-y")
        .arg("-hide_banner")
        .args(["-loglevel", "error"])
        .args(["-f", "rawvideo"])
        .args(["-pixel_format", "bgra"])
        .args(["-video_size", "256x256"])
        .args(["-framerate", "1"])
        .args(["-i", "pipe:0"])
        .args(["-c:v", codec])
        .args(["-frames:v", "1"])
        .args(["-f", "null"])
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                let frame = vec![0u8; 256 * 256 * 4];
                let _ = stdin.write_all(&frame);
            }
            drop(child.stdin.take());

            let stderr_output = child
                .stderr
                .take()
                .map(|mut se| {
                    let mut s = String::new();
                    let _ = se.read_to_string(&mut s);
                    s
                })
                .unwrap_or_default();

            let ok = child.wait().map(|s| s.success()).unwrap_or(false);
            if !ok && !stderr_output.is_empty() {
                warn!("Encoder probe '{}' failed: {}", codec, stderr_output.trim());
            }
            ok
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::config::{RateControl, X264Preset};

    #[test]
    fn display_names_are_non_empty() {
        for enc in &[
            HwEncoder::Nvenc,
            HwEncoder::Qsv,
            HwEncoder::Amf,
            HwEncoder::Software,
        ] {
            assert!(!enc.display_name().is_empty());
            assert!(!enc.codec_name().is_empty());
            assert!(!enc.pix_fmt().is_empty());
        }
    }

    #[test]
    fn software_codec_is_libx264() {
        assert_eq!(HwEncoder::Software.codec_name(), "libx264");
    }

    #[test]
    fn qsv_uses_nv12() {
        assert_eq!(HwEncoder::Qsv.pix_fmt(), "nv12");
    }

    #[test]
    fn non_qsv_uses_yuv420p() {
        assert_eq!(HwEncoder::Nvenc.pix_fmt(), "yuv420p");
        assert_eq!(HwEncoder::Amf.pix_fmt(), "yuv420p");
        assert_eq!(HwEncoder::Software.pix_fmt(), "yuv420p");
    }

    #[test]
    fn quality_args_software_crf() {
        let args =
            HwEncoder::Software.quality_args(RateControl::Quality, 23, 6000, X264Preset::Veryfast);
        assert!(args.contains(&"-crf".to_string()));
        assert!(args.contains(&"23".to_string()));
        assert!(args.contains(&"-preset".to_string()));
        assert!(args.contains(&"veryfast".to_string()));
    }

    #[test]
    fn quality_args_software_cbr() {
        let args =
            HwEncoder::Software.quality_args(RateControl::CBR, 23, 6000, X264Preset::Veryfast);
        assert!(args.contains(&"-b:v".to_string()));
        assert!(args.contains(&"6000k".to_string()));
        assert!(args.contains(&"-bufsize".to_string()));
        assert!(args.contains(&"12000k".to_string()));
    }

    #[test]
    fn quality_args_nvenc_quality() {
        let args =
            HwEncoder::Nvenc.quality_args(RateControl::Quality, 20, 6000, X264Preset::Medium);
        assert!(args.contains(&"-qp".to_string()));
        assert!(args.contains(&"20".to_string()));
        // Should not contain software-specific args
        assert!(!args.contains(&"-preset".to_string()));
        assert!(!args.contains(&"-crf".to_string()));
    }

    #[test]
    fn quality_args_nvenc_cbr() {
        let args = HwEncoder::Nvenc.quality_args(RateControl::CBR, 20, 8000, X264Preset::Medium);
        assert!(args.contains(&"-rc".to_string()));
        assert!(args.contains(&"cbr".to_string()));
        assert!(args.contains(&"8000k".to_string()));
    }

    #[test]
    fn quality_args_qsv_quality() {
        let args = HwEncoder::Qsv.quality_args(RateControl::Quality, 25, 6000, X264Preset::Medium);
        assert!(args.contains(&"-global_quality".to_string()));
        assert!(args.contains(&"25".to_string()));
    }

    #[test]
    fn quality_args_amf_quality() {
        let args = HwEncoder::Amf.quality_args(RateControl::Quality, 18, 6000, X264Preset::Medium);
        assert!(args.contains(&"-quality".to_string()));
        assert!(args.contains(&"balanced".to_string()));
        assert!(args.contains(&"-qp_i".to_string()));
        assert!(args.contains(&"18".to_string()));
    }
}
