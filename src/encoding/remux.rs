use std::path::Path;
use std::process::Stdio;

use anyhow::anyhow;
use log::{error, info};

use super::hw::ffmpeg_cmd;
use super::types::SaveRequest;
use crate::utils;

/// Estimate the duration of a WAV file from its header.
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

pub(crate) fn remux_to_mp4(request: &SaveRequest) -> anyhow::Result<String> {
    utils::ensure_clips_dir(&request.output_dir)?;

    let filename = utils::clip_filename();
    let output_path = Path::new(&request.output_dir).join(&filename);
    let stem = filename.trim_end_matches(".mp4");
    let temp_path = Path::new(&request.output_dir).join(format!("_temp_{}.ts", stem));

    std::fs::write(&temp_path, &request.encoded_data)?;

    let temp_audio_path = Path::new(&request.output_dir).join(format!("_temp_{}.wav", stem));
    let has_audio = match &request.audio_wav {
        Some(wav) if !wav.is_empty() => std::fs::write(&temp_audio_path, wav).is_ok(),
        _ => false,
    };

    let mut cmd = ffmpeg_cmd();
    cmd.arg("-y")
        .args(["-fflags", "+genpts"])
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

    cmd.args(["-c:v", "copy"]);

    if has_audio {
        cmd.args([
            "-af",
            "aresample=async=1000:first_pts=0",
            "-c:a",
            "aac",
            "-b:a",
            &format!("{}k", request.audio_bitrate_kbps),
        ]);
        cmd.arg("-shortest");
    }

    cmd.args(["-movflags", "+faststart"])
        .arg(output_path.to_str().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let result = match cmd.spawn().and_then(|c| c.wait_with_output()) {
        Ok(out) if out.status.success() => {
            info!("Clip saved: {}", output_path.display());
            Ok(format!("Clip saved: {}", filename))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            error!("Remux failed: {}", stderr);
            Err(anyhow!(
                "Remux error: {}",
                stderr.chars().take(200).collect::<String>()
            ))
        }
        Err(e) => Err(anyhow!("Failed to start FFmpeg for remux: {}", e)),
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

    fn make_wav_header(byte_rate: u32, data_size: u32) -> Vec<u8> {
        let mut wav = vec![0u8; 44 + data_size as usize];
        wav[0..4].copy_from_slice(b"RIFF");
        wav[8..12].copy_from_slice(b"WAVE");
        wav[12..16].copy_from_slice(b"fmt ");
        wav[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        wav[36..40].copy_from_slice(b"data");
        wav[40..44].copy_from_slice(&data_size.to_le_bytes());
        wav
    }

    #[test]
    fn wav_duration_48khz_stereo_float_1sec() {
        // 48000 Hz * 2 channels * 4 bytes/sample = 384000 bytes/sec
        let byte_rate = 48000 * 2 * 4;
        let data_size = byte_rate; // 1 second
        let wav = make_wav_header(byte_rate, data_size);
        let dur = wav_duration_secs(&wav);
        assert!((dur - 1.0).abs() < 0.001);
    }

    #[test]
    fn wav_duration_half_second() {
        let byte_rate: u32 = 48000 * 2 * 4;
        let data_size = byte_rate / 2;
        let wav = make_wav_header(byte_rate, data_size);
        let dur = wav_duration_secs(&wav);
        assert!((dur - 0.5).abs() < 0.001);
    }

    #[test]
    fn wav_duration_too_short() {
        let wav = vec![0u8; 10];
        assert_eq!(wav_duration_secs(&wav), 0.0);
    }

    #[test]
    fn wav_duration_zero_byte_rate() {
        let wav = make_wav_header(0, 1000);
        assert_eq!(wav_duration_secs(&wav), 0.0);
    }

    #[test]
    fn wav_duration_empty() {
        assert_eq!(wav_duration_secs(&[]), 0.0);
    }
}
