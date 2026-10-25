//! The egui-based GUI.
//!
//! [`app::ClipperApp`] is the top-level `eframe::App`. Its behaviour is
//! split across sibling modules by responsibility:
//!
//! - `app` — struct definition, `eframe::App` impl, screen routing.
//! - `backend` — non-UI lifecycle: recording, audio, capture switching.
//! - `main_screen`, `settings_screen`, `footer` — view code.
//! - `settings` — the persisted [`settings::Settings`] struct.
//! - `theme` — colour palette + global egui style.

pub mod app;
mod backend;
mod footer;
mod main_screen;
pub(crate) mod settings;
mod settings_screen;
mod theme;
