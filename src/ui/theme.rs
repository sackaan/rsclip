use eframe::egui;

pub fn bg() -> egui::Color32 {
    egui::Color32::from_rgb(0, 0, 0)
}
pub fn bg_widget() -> egui::Color32 {
    egui::Color32::from_rgb(18, 18, 18)
}
pub fn bg_hover() -> egui::Color32 {
    egui::Color32::from_rgb(30, 30, 30)
}
pub fn bg_active() -> egui::Color32 {
    egui::Color32::from_rgb(40, 40, 40)
}
pub fn stroke() -> egui::Color32 {
    egui::Color32::from_rgb(50, 50, 50)
}
pub fn white() -> egui::Color32 {
    egui::Color32::from_rgb(255, 255, 255)
}
pub fn muted() -> egui::Color32 {
    egui::Color32::from_rgb(120, 120, 120)
}
pub fn dim() -> egui::Color32 {
    egui::Color32::from_rgb(70, 70, 70)
}

pub fn green() -> egui::Color32 {
    egui::Color32::from_rgb(80, 200, 80)
}
pub fn red() -> egui::Color32 {
    egui::Color32::from_rgb(200, 60, 60)
}
pub fn orange() -> egui::Color32 {
    egui::Color32::from_rgb(200, 150, 50)
}

pub fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;

    v.dark_mode = true;
    v.override_text_color = Some(white());
    v.panel_fill = bg();
    v.window_fill = bg_widget();
    v.extreme_bg_color = bg_widget();
    v.faint_bg_color = bg_widget();

    let cr = egui::CornerRadius::same(2);
    let no_stroke = egui::Stroke::new(1.0_f32, stroke());

    v.widgets.noninteractive.bg_fill = bg_widget();
    v.widgets.noninteractive.bg_stroke = no_stroke;
    v.widgets.noninteractive.corner_radius = cr;
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, muted());

    v.widgets.inactive.bg_fill = bg_widget();
    v.widgets.inactive.bg_stroke = no_stroke;
    v.widgets.inactive.corner_radius = cr;
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, white());

    v.widgets.hovered.bg_fill = bg_hover();
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0_f32, muted());
    v.widgets.hovered.corner_radius = cr;
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, white());

    v.widgets.active.bg_fill = bg_active();
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0_f32, white());
    v.widgets.active.corner_radius = cr;
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, white());

    v.selection.bg_fill = egui::Color32::from_rgb(50, 50, 50);
    v.selection.stroke = egui::Stroke::new(1.0_f32, muted());

    v.window_stroke = egui::Stroke::new(1.0_f32, stroke());

    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(16.0, 6.0);
    v.widgets.inactive.expansion = 0.0;

    ctx.set_style(style);
}
