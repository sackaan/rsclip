//! Capture pipeline.
//!
//! - [`screen`] streams BGRA frames from a RuneLite window via Windows
//!   Graphics Capture.
//! - [`audio`] manages per-source ring buffers (game loopback + microphone)
//!   and mixes them on demand.
//! - `wasapi` (private) contains the low-level WASAPI activation and the
//!   stateful resampler used for microphone input.

pub mod audio;
pub mod cs2_capture;
pub mod screen;
pub mod sessions;
mod wasapi;
