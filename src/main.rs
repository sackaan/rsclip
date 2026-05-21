//! RSClip — instant-replay clipper for RuneLite (Old School RuneScape).
//!
//! Continuously buffers the last N seconds of the RuneLite game canvas via
//! Windows Graphics Capture, encodes them live through an FFmpeg subprocess,
//! and dumps the buffered window to MP4 on hotkey.
//!
//! The crate is organised into four top-level modules:
//!
//! - [`capture`] — Windows Graphics Capture + WASAPI audio capture.
//! - [`encoding`] — FFmpeg pipeline, encoded ring buffer, and MP4 remux.
//! - [`platform`] — Win32 glue: RuneLite window discovery, global hotkeys.
//! - [`ui`] — the egui-based GUI.
//!
//! See the project README for build instructions and an architecture overview.

#![windows_subsystem = "windows"]
#![warn(missing_docs, rust_2018_idioms)]

mod capture;
mod encoding;
mod logging;
mod platform;
mod ui;
mod utils;

use eframe::egui;

/// Decode the embedded PNG icon into an `egui` icon data buffer for the
/// title bar.
fn load_icon() -> Option<egui::IconData> {
    let png_bytes = include_bytes!("assets/icon2.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    })
}

fn main() -> eframe::Result<()> {
    logging::init_gui_logger(log::LevelFilter::Info);
    log::info!("RSClip starting");

    if let Err(e) = utils::ensure_clips_dir(&utils::default_clips_dir()) {
        log::warn!("Could not create clips directory: {}", e);
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("RSClip")
        .with_inner_size([380.0_f32, 340.0_f32])
        .with_min_inner_size([380.0_f32, 340.0_f32]);

    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        persist_window: false,
        ..Default::default()
    };

    eframe::run_native(
        "RSClip",
        options,
        Box::new(|cc| Ok(Box::new(ui::app::ClipperApp::new(cc)))),
    )
}
