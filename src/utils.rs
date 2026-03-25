use anyhow::Result;
use chrono::Local;
use std::path::{Path, PathBuf};

pub fn default_clips_dir() -> String {
    dirs::video_dir()
        .map(|p| p.join("RSClips"))
        .unwrap_or_else(|| PathBuf::from("clips"))
        .to_string_lossy()
        .into_owned()
}

pub const DEFAULT_BUFFER_SECONDS: u32 = 30;
pub const DEFAULT_FPS: u32 = 60;
pub const DEFAULT_HOTKEY_NAME: &str = "F8";

pub fn clip_filename() -> String {
    let now = Local::now();
    now.format("RSClip_%Y%m%d_%H%M%S.mp4").to_string()
}

pub fn ensure_clips_dir(base: &str) -> Result<PathBuf> {
    let path = Path::new(base);
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    Ok(path.to_path_buf())
}

pub fn open_clips_folder(dir: &str) -> Result<()> {
    let path = Path::new(dir);
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    open::that(path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingStatus {
    Idle,
    Recording,
    Saving,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub severity: Severity,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Info,
        }
    }

    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Success,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            severity: Severity::Warning,
        }
    }

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
    fn test_ensure_clips_dir_creates_and_returns() {
        let tmp = std::env::temp_dir().join("rsclip_test_dir");
        let _ = std::fs::remove_dir_all(&tmp);
        let result = ensure_clips_dir(tmp.to_str().unwrap());
        assert!(result.is_ok());
        assert!(tmp.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
