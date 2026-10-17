//! Platform glue (Win32 + global hotkeys).
//!
//! - [`runelite`] discovers RuneLite top-level windows and computes the
//!   crop rectangle for the game canvas child window.
//! - [`cs2`] discovers the Counter-Strike 2 game window (full window, no
//!   crop) for the GPU zero-copy capture path.
//! - [`hotkeys`] wraps `global-hotkey` and translates `egui` key events
//!   into hotkey combo strings.

pub mod cs2;
pub mod hotkeys;
pub mod runelite;
