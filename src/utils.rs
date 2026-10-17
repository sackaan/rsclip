//! Defaults, filesystem helpers, and the small status/severity types shared
//! between the GUI and the backend.

use anyhow::Result;
use chrono::Local;
use std::path::{Path, PathBuf};

/// Default rolling-buffer length, in seconds.
pub const DEFAULT_BUFFER_SECONDS: u32 = 30;
/// Default capture/encode frame rate.
pub const DEFAULT_FPS: u32 = 60;
/// Default save-clip hotkey combo string (parsed by `global-hotkey`).
pub const DEFAULT_HOTKEY_NAME: &str = "F8";

/// Default clips output directory: `<user videos>/RSClips`, or `./clips` if
/// the user-videos folder cannot be resolved.
pub fn default_clips_dir() -> String {
    dirs::video_dir()
        .map(|p| p.join("RSClips"))
        .unwrap_or_else(|| PathBuf::from("clips"))
        .to_string_lossy()
        .into_owned()
}

/// Build a timestamped clip filename (`RSClip_YYYYMMDD_HHMMSS.mp4`) using
/// the local clock.
pub fn clip_filename() -> String {
    let now = Local::now();
    now.format("RSClip_%Y%m%d_%H%M%S.mp4").to_string()
}

/// Path for a new clip in `dir` that does not overwrite an existing file.
/// Uses [`clip_filename`], adding `_2`, `_3`, … when two clips are saved
/// within the same second.
pub fn unique_clip_path(dir: &Path) -> PathBuf {
    let name = clip_filename();
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let stem = name.trim_end_matches(".mp4");
    (2..)
        .map(|n| dir.join(format!("{}_{}.mp4", stem, n)))
        .find(|p| !p.exists())
        .expect("unbounded suffix search always finds a free name")
}

/// Ensure `base` exists as a directory, creating it (and parents) if not.
pub fn ensure_clips_dir(base: &str) -> Result<PathBuf> {
    let path = Path::new(base);
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(path.to_path_buf())
}

/// Open `dir` in the system file explorer, creating it first if needed.
pub fn open_clips_folder(dir: &str) -> Result<()> {
    let path = Path::new(dir);
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    open::that(path)?;
    Ok(())
}

/// High-level recorder state surfaced to the GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
    /// No capture running.
    Idle,
    /// Frames are being buffered.
    Recording,
    /// A save is in flight (remux to MP4).
    Saving,
    /// The last operation failed; see [`StatusMessage`] for details.
    Error,
}

/// Severity used to colour [`StatusMessage`] in the GUI footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational (default colour).
    Info,
    /// Successful operation (green).
    Success,
    /// Non-fatal problem (orange).
    Warning,
    /// Fatal/blocking problem (red).
    Error,
}

/// A single status line shown to the user.
#[derive(Debug, Clone)]
pub struct StatusMessage {
    /// Free-form message text.
    pub text: String,
    /// Severity controlling the rendered colour.
    pub severity: Severity,
}

impl StatusMessage {
    /// Build an informational message.
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Info,
        }
    }

    /// Build a success message.
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Success,
        }
    }

    /// Build a warning message.
    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Warning,
        }
    }

    /// Build an error message.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Error,
        }
    }
}

impl Default for StatusMessage {
    fn default() -> Self {
        Self::info("Ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clip_filename_format() {
        let name = clip_filename();
        assert!(name.starts_with("RSClip_"));
        assert!(name.ends_with(".mp4"));
        // RSClip_YYYYMMDD_HHMMSS.mp4 = 26 chars
        assert_eq!(name.len(), 26);
    }

    #[test]
    fn test_default_clips_dir_not_empty() {
        let dir = default_clips_dir();
        assert!(!dir.is_empty());
        assert!(dir.contains("RSClips"));
    }

    #[test]
    fn test_status_message_default_is_ready() {
        let msg = StatusMessage::default();
        assert_eq!(msg.text, "Ready");
        assert_eq!(msg.severity, Severity::Info);
    }

    #[test]
    fn test_status_message_constructors() {
        let info = StatusMessage::info("hello");
        assert_eq!(info.severity, Severity::Info);
        assert_eq!(info.text, "hello");

        let success = StatusMessage::success("saved");
        assert_eq!(success.severity, Severity::Success);

        let warn = StatusMessage::warning("no data");
        assert_eq!(warn.severity, Severity::Warning);

        let err = StatusMessage::error("failed");
        assert_eq!(err.severity, Severity::Error);
        assert_eq!(err.text, "failed");
    }

    #[test]
    fn test_status_message_accepts_string() {
        let msg = StatusMessage::info(format!("Detected: {}x{}", 1920, 1080));
        assert_eq!(msg.text, "Detected: 1920x1080");
    }

    #[test]
    fn test_unique_clip_path_never_overwrites() {
        let tmp = std::env::temp_dir().join(format!("rsclip_unique_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Pre-create the plain name and the first suffix for every second
        // the test might straddle, so the helper must skip past both.
        let mut taken = Vec::new();
        for _ in 0..2 {
            let p = unique_clip_path(&tmp);
            std::fs::write(&p, b"x").unwrap();
            taken.push(p);
        }
        let next = unique_clip_path(&tmp);
        assert!(!next.exists());
        assert!(!taken.contains(&next));
        assert_eq!(next.extension().unwrap(), "mp4");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_ensure_clips_dir_creates_and_returns() {
        let tmp = std::env::temp_dir().join("rsclip_test_dir");
        let _ = std::fs::remove_dir_all(&tmp);
        let result = ensure_clips_dir(tmp.to_str().unwrap());
        assert!(result.is_ok());
        assert!(tmp.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
