//! Save-time remux from the encoded MPEG-TS ring + mixed WAV into a final
//! MP4. Stream-copy on video (no re-encode); the audio track is encoded to
//! AAC.

use std::path::PathBuf;
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
    // The remux-vs-re-encode comparison lives or dies on this number.
    let _export = crate::metrics::Timer::start(crate::metrics::Stage::Export);
    let output_dir = utils::ensure_clips_dir(&request.output_dir)?;

    let output_path = utils::unique_clip_path(&output_dir);
    let filename = output_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Intermediates go to the system temp dir, not the user's clips folder,
    // and are removed on every exit path by the guard.
    let temp_dir = std::env::temp_dir().join("rsclip");
    std::fs::create_dir_all(&temp_dir)?;
    let stem = format!(
        "{}_{}",
        output_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy(),
        std::process::id()
    );
    let temp_path = temp_dir.join(format!("{}.ts", stem));
    let temp_audio_path = temp_dir.join(format!("{}.wav", stem));
    let mut cleanup = TempFiles(vec![temp_path.clone()]);

    std::fs::write(&temp_path, &request.encoded_data)?;

    let has_audio = match &request.audio_wav {
        Some(wav) if !wav.is_empty() => {
            cleanup.0.push(temp_audio_path.clone());
            std::fs::write(&temp_audio_path, wav).is_ok()
        }
        _ => false,
    };

    let mut cmd = ffmpeg_cmd();
    cmd.arg("-y")
        .args(["-fflags", "+genpts"])
        .arg("-i")
        .arg(&temp_path);

    if has_audio {
        let audio_secs = wav_duration_secs(request.audio_wav.as_deref().unwrap_or(&[]));
        let offset = (request.video_duration_secs - audio_secs).max(0.0);

        if offset > 0.020 {
            cmd.args(["-itsoffset", &format!("{:.3}", offset)]);
        }
        cmd.arg("-i").arg(&temp_audio_path);
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
        .arg(&output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    match cmd.spawn().and_then(|c| c.wait_with_output()) {
        Ok(out) if out.status.success() => {
            info!("Clip saved: {}", output_path.display());
            Ok(format!("Clip saved: {}", filename))
        }
        Ok(out) => {
            // Don't leave a truncated MP4 behind looking like a real clip.
            let _ = std::fs::remove_file(&output_path);
            let stderr = String::from_utf8_lossy(&out.stderr);
            error!("Remux failed: {}", stderr);
            Err(anyhow!(
                "Remux error: {}",
                stderr.chars().take(200).collect::<String>()
            ))
        }
        Err(e) => Err(anyhow!("Failed to start FFmpeg for remux: {}", e)),
    }
}

/// Deletes the listed files when dropped.
struct TempFiles(Vec<PathBuf>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
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

    /// Two back-to-back saves must yield two clips and leave no scratch
    /// files in the clips folder. Needs FFmpeg, so opt-in:
    /// `cargo test -- --ignored remux`.
    #[test]
    #[ignore]
    fn remux_twice_in_same_second_keeps_both_clips() {
        let dir = std::env::temp_dir().join(format!("rsclip_remux_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ts = dir.join("src.ts");
        let ok = ffmpeg_cmd()
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i"])
            .arg("testsrc2=s=320x240:r=30")
            .args(["-t", "2", "-c:v", "libx264", "-f", "mpegts"])
            .arg(&ts)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg available");
        let data = std::fs::read(&ts).unwrap();
        std::fs::remove_file(&ts).unwrap();

        let out_dir = dir.join("clips with space");
        let request = SaveRequest {
            encoded_data: data,
            output_dir: out_dir.to_string_lossy().into_owned(),
            audio_wav: None,
            video_duration_secs: 2.0,
            audio_bitrate_kbps: 128,
        };
        remux_to_mp4(&request).expect("first save");
        remux_to_mp4(&request).expect("second save");

        let names: Vec<_> = std::fs::read_dir(&out_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.iter().all(|n| n.ends_with(".mp4")), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
