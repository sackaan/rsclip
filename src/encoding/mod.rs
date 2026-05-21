//! Encoding pipeline.
//!
//! - [`encoder`] owns a persistent FFmpeg child process. Writer/reader
//!   threads feed it raw BGRA frames and capture encoded MPEG-TS chunks.
//! - [`buffer`] keeps a time-bounded ring of those chunks — the rolling
//!   replay window.
//! - [`remux`] (private) snapshots the ring + the mixed WAV and remuxes to
//!   MP4 (stream copy, no re-encode) when the user saves a clip.
//! - [`config`] holds [`config::EncoderConfig`] and the user-visible enums
//!   (rate control, x264 preset, scale filter).
//! - [`hw`] (private) probes hardware encoders and builds FFmpeg argv.
//! - [`types`] (private) defines [`encoder::RawFrame`] and the preview
//!   thumbnail type shared with the GUI.

pub mod buffer;
pub mod config;
pub mod encoder;
pub(crate) mod hw;
pub(crate) mod remux;
pub(crate) mod types;
