#![windows_subsystem = "windows"]

mod capture;
mod encoding;
mod logging;
mod platform;
mod ui;
mod utils;

use eframe::egui;

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
