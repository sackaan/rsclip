//! Platform glue (Win32 + global hotkeys).
//!
//! - [`runelite`] discovers RuneLite top-level windows and computes the
//!   crop rectangle for the game canvas child window.
//! - [`hotkeys`] wraps `global-hotkey` and translates `egui` key events
//!   into hotkey combo strings.

pub mod hotkeys;
pub mod runelite;
