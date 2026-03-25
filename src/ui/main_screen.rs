use std::sync::atomic::Ordering;

use eframe::egui;

use super::app::ClipperApp;
use super::theme;
use crate::utils;
use crate::utils::{RecordingStatus, Severity};

enum ButtonAction {
    None,
    Start,
    Stop,
    Save,
    OpenFolder,
}

impl ClipperApp {
    pub(super) fn draw_main_screen(&mut self, ui: &mut egui::Ui, content_rect: egui::Rect) {
        let mut content_ui = ui.new_child(
            egui::UiBuilder::new()
                .layout(egui::Layout::top_down(egui::Align::Center))
                .max_rect(content_rect),
        );

        if self.show_preview {
            self.draw_main_with_preview(&mut content_ui);
        } else {
            self.draw_main_centered(&mut content_ui, content_rect.height());
        }
    }

    fn draw_main_centered(&mut self, ui: &mut egui::Ui, avail_height: f32) {
        let content_h = 200.0_f32;
        let top_pad = ((avail_height - content_h) / 2.0).max(12.0);
        ui.add_space(top_pad);

        self.draw_title(ui);
        ui.add_space(8.0);
        self.draw_status(ui);
        ui.add_space(14.0);
        self.draw_info(ui);
        ui.add_space(20.0);
        self.draw_buttons(ui);
        ui.add_space(16.0);
        self.draw_message(ui);
    }

    fn draw_main_with_preview(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let title_job = egui::text::LayoutJob::simple(
                "RSClip".to_owned(),
                egui::FontId::new(18.0, egui::FontFamily::Name("RuneScape".into())),
                theme::white(),
                f32::INFINITY,
            );
            ui.label(title_job);
            ui.add_space(8.0);

            let (status_text, status_color) = self.status_label();
            ui.colored_label(status_color, egui::RichText::new(status_text).size(11.0));
        });

        ui.add_space(4.0);

        let preview_avail = ui.available_size() - egui::vec2(0.0, 80.0); // reserve space for controls below
        let preview_avail = egui::vec2(preview_avail.x.max(100.0), preview_avail.y.max(60.0));

        if let Some(ref tex) = self.preview_texture {
            let size = tex.size_vec2();
            let scale = (preview_avail.x / size.x)
                .min(preview_avail.y / size.y)
                .min(1.0);
            let display_size = egui::vec2(size.x * scale, size.y * scale);

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(preview_avail.x, display_size.y),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(rect, 0.0, theme::bg_widget());

            let img_rect = egui::Rect::from_center_size(rect.center(), display_size);
            ui.painter().image(
                tex.id(),
                img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(preview_avail.x, preview_avail.y),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(rect, 0.0, theme::bg_widget());
            let msg = if self.active_window.is_none() {
                "no RuneLite window"
            } else {
                "waiting for frame..."
            };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(11.0),
                theme::dim(),
            );
        }

        ui.add_space(6.0);

        ui.horizontal(|ui| {
            self.draw_info_inline(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.draw_buttons_inline(ui);
            });
        });

        ui.add_space(4.0);
        self.draw_message(ui);
    }

    fn status_label(&self) -> (&str, egui::Color32) {
        match self.status {
            RecordingStatus::Idle => ("not recording", theme::dim()),
            RecordingStatus::Recording => ("recording", theme::green()),
            RecordingStatus::Saving => ("saving...", theme::orange()),
            RecordingStatus::Error => ("error", theme::red()),
        }
    }

    fn draw_title(&self, ui: &mut egui::Ui) {
        let title_job = egui::text::LayoutJob::simple(
            "RSClip".to_owned(),
            self.rs_font.clone(),
            theme::white(),
            f32::INFINITY,
        );
        ui.label(title_job);
    }

    fn draw_status(&self, ui: &mut egui::Ui) {
        let (text, color) = self.status_label();
        ui.colored_label(color, text);
    }

    fn draw_info(&self, ui: &mut egui::Ui) {
        let n_clients = self.runelite_windows.len();
        if let Some(ref win) = self.active_window {
            let canvas_tag = if win.canvas_found { " (canvas)" } else { "" };
            let client_info = if n_clients > 1 {
                format!(
                    "{}x{}{}  ·  {} clients",
                    win.width, win.height, canvas_tag, n_clients
                )
            } else {
                format!("{}x{}{}", win.width, win.height, canvas_tag)
            };
            ui.colored_label(theme::muted(), client_info);
        } else {
            ui.colored_label(theme::dim(), "no client detected");
        }

        if let Some(ref enc) = self.live_encoder {
            let buf = enc.encoded_buffer().read();
            let dur = buf.duration_secs();
            let mins = dur as u32 / 60;
            let secs = dur as u32 % 60;
            let mb = buf.memory_usage_bytes() as f64 / (1024.0 * 1024.0);
            ui.colored_label(
                theme::muted(),
                format!("{}:{:02} · {:.1} MB", mins, secs, mb),
            );
        } else {
            ui.colored_label(theme::dim(), "0:00 · 0.0 MB");
        }
    }

    fn draw_info_inline(&self, ui: &mut egui::Ui) {
        let mut parts: Vec<String> = Vec::new();

        if let Some(ref win) = self.active_window {
            let tag = if win.canvas_found { " (canvas)" } else { "" };
            parts.push(format!("{}x{}{}", win.width, win.height, tag));
        }
        if let Some(ref enc) = self.live_encoder {
            let buf = enc.encoded_buffer().read();
            let dur = buf.duration_secs();
            let mins = dur as u32 / 60;
            let secs = dur as u32 % 60;
            let mb = buf.memory_usage_bytes() as f64 / (1024.0 * 1024.0);
            parts.push(format!("{}:{:02} · {:.1} MB", mins, secs, mb));
        }
        if parts.is_empty() {
            ui.colored_label(
                theme::dim(),
                egui::RichText::new("no client detected").size(10.0),
            );
        } else {
            ui.colored_label(
                theme::muted(),
                egui::RichText::new(parts.join("  ·  ")).size(10.0),
            );
        }
    }

    fn render_action_buttons(&self, ui: &mut egui::Ui, btn_size: f32) -> ButtonAction {
        let is_recording = self.capture_handle.is_some();
        let rl_found = self.active_window.is_some();
        let can_save = self
            .live_encoder
            .as_ref()
            .is_some_and(|e| !e.encoded_buffer().read().is_empty())
            && !self.is_saving.load(Ordering::SeqCst);

        let mut action = ButtonAction::None;

        if !is_recording {
            let enabled = rl_found && self.ffmpeg_available;
            let btn = egui::Button::new(egui::RichText::new("\u{25b6}").size(btn_size).color(
                if enabled {
                    theme::white()
                } else {
                    theme::dim()
                },
            ))
            .fill(if enabled {
                theme::bg_widget()
            } else {
                theme::bg()
            });
            if ui
                .add_enabled(enabled, btn)
                .on_hover_text("Start Buffer")
                .clicked()
            {
                action = ButtonAction::Start;
            }
        } else {
            let btn = egui::Button::new(
                egui::RichText::new("\u{23f9}")
                    .size(btn_size)
                    .color(theme::red()),
            )
            .fill(theme::bg_widget());
            if ui.add(btn).on_hover_text("Stop").clicked() {
                action = ButtonAction::Stop;
            }
        }

        let btn = egui::Button::new(egui::RichText::new("\u{1f4be}").size(btn_size).color(
            if can_save {
                theme::white()
            } else {
                theme::dim()
            },
        ))
        .fill(theme::bg_widget());
        if ui
            .add_enabled(can_save, btn)
            .on_hover_text("Save Clip")
            .clicked()
        {
            action = ButtonAction::Save;
        }

        let btn = egui::Button::new(
            egui::RichText::new("\u{1f5c0}")
                .size(btn_size)
                .color(theme::muted()),
        )
        .fill(theme::bg_widget());
        if ui.add(btn).on_hover_text("Clips Folder").clicked() {
            action = ButtonAction::OpenFolder;
        }

        action
    }

    fn handle_button_action(&mut self, action: ButtonAction) {
        match action {
            ButtonAction::Start => self.start_recording(),
            ButtonAction::Stop => self.stop_recording(),
            ButtonAction::Save => self.save_clip(),
            ButtonAction::OpenFolder => {
                let _ = utils::open_clips_folder(&self.settings.output_dir);
            }
            ButtonAction::None => {}
        }
    }

    fn draw_buttons(&mut self, ui: &mut egui::Ui) {
        let btn_size = 18.0;
        let btn_spacing = ui.spacing().item_spacing.x;
        let btn_pad = ui.spacing().button_padding;
        let single_w = btn_size + btn_pad.x * 2.0;
        let total_w = single_w * 3.0 + btn_spacing * 2.0;
        let avail = ui.available_width();
        let offset = ((avail - total_w) / 2.0).max(0.0);

        let action = ui
            .horizontal(|ui| {
                ui.add_space(offset);
                self.render_action_buttons(ui, btn_size)
            })
            .inner;
        self.handle_button_action(action);
    }

    fn draw_buttons_inline(&mut self, ui: &mut egui::Ui) {
        let action = self.render_action_buttons(ui, 14.0);
        self.handle_button_action(action);
    }

    fn draw_message(&self, ui: &mut egui::Ui) {
        let color = match self.status_msg.severity {
            Severity::Info => theme::dim(),
            Severity::Success => theme::green(),
            Severity::Warning => theme::orange(),
            Severity::Error => theme::red(),
        };
        ui.colored_label(color, egui::RichText::new(&self.status_msg.text).size(11.0));
    }
}
