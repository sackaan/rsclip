//! Save-time motion-blur render for the CS2 (GPU) capture path.
//!
//! The ring buffer hands us a high-frame-rate H.264 elementary stream (real
//! frames captured at the panel refresh, e.g. 240 fps). To get the smooth,
//! cinematic montage look we blend consecutive *real* frames together with
//! FFmpeg's `tmix` filter and decimate to the output frame rate — no
//! interpolation, so no warping/ghosting on fast CS2 motion.
//!
//! The number of frames blended per output frame is the shutter: at 240→60
//! there are 4 source frames per output frame, so blending all 4 is a 360°
//! shutter (maximum blur) and blending 2 is 180°.

use std::path::Path;
use std::process::Stdio;

use anyhow::anyhow;
use log::{error, info};

use super::config::{RateControl, X264Preset};
use super::gpu_encoder::VideoCodec;
use super::hw::{detect_best_encoder, ffmpeg_cmd};
use crate::utils;

/// A request to render one CS2 clip with motion blur.
pub struct Cs2SaveRequest {
    /// Sequence header + access units (Annex-B), starting at a keyframe.
    pub annexb: Vec<u8>,
    /// Codec of the intermediate elementary stream (H.264 or HEVC).
    pub intermediate_codec: VideoCodec,
    /// Destination directory for the finished MP4.
    pub output_dir: String,
    /// Mixed audio WAV, if any.
    pub audio_wav: Option<Vec<u8>>,
    /// Buffered video duration (for audio alignment).
    pub video_duration_secs: f64,
    /// AAC bitrate for the muxed audio.
    pub audio_bitrate_kbps: u32,
    /// Frame rate the elementary stream was captured at (e.g. 240).
    pub capture_fps: u32,
    /// Target output frame rate (e.g. 60).
    pub output_fps: u32,
    /// Shutter / blur strength, 0–100 %. 0 disables blur (plain decimation).
    pub blur_pct: u32,
    /// Re-encode rate control.
    pub rate_control: RateControl,
    /// CRF / QP when rate control is quality-based.
    pub crf: u32,
    /// Target bitrate when rate control is CBR.
    pub video_bitrate_kbps: u32,
    /// libx264 preset (software fallback only).
    pub x264_preset: X264Preset,
}

/// How many consecutive source frames to average per output frame, given the
/// capture/output ratio and the shutter percentage. Always at least 1.
fn blend_count(capture_fps: u32, output_fps: u32, blur_pct: u32) -> u32 {
    if blur_pct == 0 || output_fps == 0 {
        return 1;
    }
    let frames_per_output = (capture_fps as f64 / output_fps as f64).max(1.0);
    let count = (frames_per_output * blur_pct as f64 / 100.0).round() as u32;
    count.clamp(1, frames_per_output.round() as u32)
}

/// Build the `-vf` filter chain.
///
/// 1. `setpts=N/fps/TB` forcibly re-stamps every frame at the *measured*
///    capture rate. This is essential: the hardware encoder bakes the
///    configured (nominal) frame rate into the bitstream's VUI timing, and
///    FFmpeg honours that over the `-framerate` input flag — so without this
///    the clip plays at the wrong speed.
/// 2. Optional `tmix` blends N real frames for motion blur.
/// 3. `fps` decimates to the output rate.
fn build_vf(capture_fps: u32, output_fps: u32, blur_pct: u32) -> String {
    let retime = format!("setpts=N/{}/TB", capture_fps.max(1));
    let n = blend_count(capture_fps, output_fps, blur_pct);
    if n <= 1 {
        format!("{},fps={}", retime, output_fps)
    } else {
        // Equal weights (omitted) = simple average of N real frames.
        format!("{},tmix=frames={},fps={}", retime, n, output_fps)
    }
}

fn wav_duration_secs(wav: &[u8]) -> f64 {
    if wav.len() < 44 {
        return 0.0;
    }
    let byte_rate = u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]);
    if byte_rate == 0 {
        return 0.0;
    }
    let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
    data_size as f64 / byte_rate as f64
}

/// Render the buffered CS2 clip to an MP4 with motion blur and muxed audio.
pub fn save_cs2_clip(request: &Cs2SaveRequest) -> anyhow::Result<String> {
    // Covers scratch-file writes plus the whole FFmpeg pass — the number a
    // user feels between hotkey and clip.
    let _export = crate::metrics::Timer::start(crate::metrics::Stage::Export);
    utils::ensure_clips_dir(&request.output_dir)?;

    let output_path = utils::unique_clip_path(Path::new(&request.output_dir));
    let filename = output_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = filename.trim_end_matches(".mp4");
    let demuxer = request.intermediate_codec.ffmpeg_demuxer();
    let temp_path = Path::new(&request.output_dir).join(format!("_temp_{}.{}", stem, demuxer));
    std::fs::write(&temp_path, &request.annexb)?;

    let temp_audio_path = Path::new(&request.output_dir).join(format!("_temp_{}.wav", stem));
    let has_audio = match &request.audio_wav {
        Some(wav) if !wav.is_empty() => std::fs::write(&temp_audio_path, wav).is_ok(),
        _ => false,
    };

    let encoder = detect_best_encoder();
    let vf = build_vf(request.capture_fps, request.output_fps, request.blur_pct);

    let mut cmd = ffmpeg_cmd();
    cmd.arg("-y")
        .arg("-hide_banner")
        .args(["-loglevel", "warning"])
        // Raw elementary stream (H.264 or HEVC) at the capture frame rate.
        .args(["-framerate", &request.capture_fps.to_string()])
        .args(["-f", demuxer])
        .args(["-i", temp_path.to_str().unwrap()]);

    if has_audio {
        let audio_secs = wav_duration_secs(request.audio_wav.as_deref().unwrap_or(&[]));
        let offset = (request.video_duration_secs - audio_secs).max(0.0);
        if offset > 0.020 {
            cmd.args(["-itsoffset", &format!("{:.3}", offset)]);
        }
        cmd.args(["-i", temp_audio_path.to_str().unwrap()]);
    }

    cmd.args(["-map", "0:v:0"]);
    if has_audio {
        cmd.args(["-map", "1:a:0"]);
    }

    cmd.args(["-vf", &vf]).args(["-c:v", encoder.codec_name()]);
    for arg in encoder.quality_args(
        request.rate_control,
        request.crf,
        request.video_bitrate_kbps,
        request.x264_preset,
    ) {
        cmd.arg(arg);
    }
    cmd.args(["-pix_fmt", encoder.pix_fmt()]);

    if has_audio {
        cmd.args([
            "-af",
            "aresample=async=1000:first_pts=0",
            "-c:a",
            "aac",
            "-b:a",
            &format!("{}k", request.audio_bitrate_kbps),
        ])
        .arg("-shortest");
    }

    cmd.args(["-movflags", "+faststart"])
        .arg(output_path.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let result = match cmd.spawn().and_then(|c| c.wait_with_output()) {
        Ok(out) if out.status.success() => {
            info!("CS2 clip saved: {}", output_path.display());
            Ok(format!("Clip saved: {}", filename))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            error!("CS2 blur render failed: {}", stderr);
            Err(anyhow!(
                "Render error: {}",
                stderr.chars().take(200).collect::<String>()
            ))
        }
        Err(e) => Err(anyhow!("Failed to start FFmpeg for CS2 render: {}", e)),
    };

    let _ = std::fs::remove_file(&temp_path);
    if has_audio {
        let _ = std::fs::remove_file(&temp_audio_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_full_shutter_240_to_60() {
        // 4 source frames per output frame, 100% shutter -> blend all 4.
        assert_eq!(blend_count(240, 60, 100), 4);
    }

    #[test]
    fn blend_half_shutter_240_to_60() {
        // 180-degree shutter -> blend 2 of the 4.
        assert_eq!(blend_count(240, 60, 50), 2);
    }

    #[test]
    fn blend_zero_disables() {
        assert_eq!(blend_count(240, 60, 0), 1);
    }

    #[test]
    fn blend_never_exceeds_ratio() {
        assert_eq!(blend_count(240, 60, 100), 4);
        assert_eq!(blend_count(165, 60, 100), 3); // round(2.75)=3
    }

    #[test]
    fn vf_no_blur_retimes_then_fps() {
        assert_eq!(build_vf(240, 60, 0), "setpts=N/240/TB,fps=60");
    }

    #[test]
    fn vf_with_blur_retimes_then_tmix() {
        assert_eq!(build_vf(240, 60, 100), "setpts=N/240/TB,tmix=frames=4,fps=60");
    }
}
