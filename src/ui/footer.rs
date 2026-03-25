use std::sync::atomic::Ordering;

use eframe::egui;

use crate::utils::RecordingStatus;

use super::app::{ClipperApp, Screen};
use super::theme;

impl ClipperApp {
    pub(super) fn draw_footer(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("footer")
            .exact_height(24.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::bg_widget())
                    .stroke(egui::Stroke::new(1.0, theme::stroke()))
                    .inner_margin(egui::Margin {
                        left: 10,
                        right: 10,
                        top: 3,
                        bottom: 3,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let s = 10.0;

                    match self.screen {
                        Screen::Main => {
                            self.footer_nav_buttons(ui, s);
                        }
                        Screen::Settings => {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("back").size(s).color(theme::white()),
                                    )
                                    .frame(false),
                                )
                                .clicked()
                            {
                                self.screen = Screen::Main;
                                self.listening_for_hotkey = false;
                                self.listening_for_record_hotkey = false;
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        self.draw_status_bar(ui, s);
                    });
                });
            });
    }

    fn footer_nav_buttons(&mut self, ui: &mut egui::Ui, s: f32) {
        if ui
            .add(
                egui::Button::new(egui::RichText::new("config").size(s).color(theme::white()))
                    .frame(false),
            )
            .clicked()
        {
            self.screen = Screen::Settings;
        }
        self.separator(ui, s);
        let console_color = if self.show_console {
            theme::green()
        } else {
            theme::white()
        };
        if ui
            .add(
                egui::Button::new(egui::RichText::new("console").size(s).color(console_color))
                    .frame(false),
            )
            .clicked()
        {
            self.show_console = !self.show_console;
        }
        self.separator(ui, s);
        let preview_color = if self.show_preview {
            theme::green()
        } else {
            theme::white()
        };
        if ui
            .add(
                egui::Button::new(egui::RichText::new("preview").size(s).color(preview_color))
                    .frame(false),
            )
            .clicked()
        {
            self.show_preview = !self.show_preview;
            if !self.show_preview {
                self.stop_preview_capture();
                self.preview_texture = None;
            }
        }
    }

    fn draw_status_bar(&self, ui: &mut egui::Ui, s: f32) {
        let is_recording = self.status == RecordingStatus::Recording;

        if is_recording {
            let dropped = self.dropped_frames.load(Ordering::Relaxed);
            let color = if dropped > 0 {
                theme::red()
            } else {
                theme::dim()
            };
            ui.colored_label(
                color,
                egui::RichText::new(format!("Dropped: {}", dropped)).size(s),
            );
            self.separator(ui, s);
        }

        ui.colored_label(
            theme::dim(),
            egui::RichText::new(format!("FPS: {}", self.settings.fps)).size(s),
        );
        self.separator(ui, s);

        if let Some(ref enc) = self.live_encoder {
            let name = enc.hw_encoder().display_name();
            ui.colored_label(
                theme::dim(),
                egui::RichText::new(format!("Encoder: {}", name)).size(s),
            );
        } else {
            ui.colored_label(theme::dim(), egui::RichText::new("Encoder: —").size(s));
        }
        self.separator(ui, s);

        if let Some(ref enc) = self.live_encoder {
            let buf = enc.encoded_buffer().read();
            let dur = buf.duration_secs() as u32;
            let max = self.settings.buffer_seconds;
            ui.colored_label(
                theme::dim(),
                egui::RichText::new(format!("Buffer: {}s / {}s", dur, max)).size(s),
            );
        } else {
            let max = self.settings.buffer_seconds;
            ui.colored_label(
                theme::dim(),
                egui::RichText::new(format!("Buffer: 0s / {}s", max)).size(s),
            );
        }
        self.separator(ui, s);

        let (dot_color, label, label_color) = match self.status {
            RecordingStatus::Recording => (theme::green(), "Recording", theme::green()),
            RecordingStatus::Saving => (theme::orange(), "Saving", theme::orange()),
            RecordingStatus::Error => (theme::red(), "Error", theme::red()),
            RecordingStatus::Idle => (theme::dim(), "Idle", theme::dim()),
        };
        ui.colored_label(label_color, egui::RichText::new(label).size(s));
        ui.add_space(4.0);
        ui.colored_label(dot_color, egui::RichText::new("●").size(s));
    }

    fn separator(&self, ui: &mut egui::Ui, s: f32) {
        ui.add_space(6.0);
        ui.colored_label(theme::dim(), egui::RichText::new("|").size(s));
        ui.add_space(6.0);
    }

    pub(super) fn draw_console(&mut self, ctx: &egui::Context) {
        if !self.show_console {
            return;
        }
        egui::TopBottomPanel::bottom("console")
            .resizable(true)
            .default_height(150.0)
            .height_range(60.0..=400.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::bg_widget())
                    .stroke(egui::Stroke::new(1.0, theme::stroke()))
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ctx, |ui| {
                if let Some(logger) = crate::logging::gui_logger() {
                    if let Some(entries) = logger.entries_if_changed(&mut self.log_generation) {
                        self.cached_log_entries = entries;
                    }
                }

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for entry in &self.cached_log_entries {
                            let color = match entry.level {
                                log::Level::Error => theme::red(),
                                log::Level::Warn => theme::orange(),
                                log::Level::Info => theme::muted(),
                                _ => theme::dim(),
                            };
                            ui.colored_label(
                                color,
                                egui::RichText::new(format!(
                                    "{} {}",
                                    entry.timestamp, entry.message
                                ))
                                .size(10.0),
                            );
                        }
                    });
            });
    }
}
