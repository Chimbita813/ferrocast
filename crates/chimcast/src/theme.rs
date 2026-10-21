//! Chimcast's look: a deep navy base with the purple-to-cyan of the logo as the accent.

use egui::{Color32, CornerRadius, Margin, Stroke};

pub const BG: Color32 = Color32::from_rgb(0x0B, 0x0F, 0x17);
pub const PANEL: Color32 = Color32::from_rgb(0x11, 0x18, 0x27);
pub const CARD: Color32 = Color32::from_rgb(0x16, 0x1E, 0x2E);
pub const RAISED: Color32 = Color32::from_rgb(0x1E, 0x28, 0x3B);
pub const BORDER: Color32 = Color32::from_rgb(0x24, 0x30, 0x47);
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xEA, 0xF2);
pub const WEAK: Color32 = Color32::from_rgb(0x8A, 0x94, 0xA7);
pub const ACCENT: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x9D, 0x74, 0xF8);
pub const CYAN: Color32 = Color32::from_rgb(0x22, 0xD3, 0xEE);
pub const LIVE: Color32 = Color32::from_rgb(0xEF, 0x44, 0x44);
pub const GOOD: Color32 = Color32::from_rgb(0x22, 0xC5, 0x5E);
pub const WARN: Color32 = Color32::from_rgb(0xEA, 0xB3, 0x08);

pub const RADIUS: u8 = 8;

pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(egui::Theme::Dark, style);
    ctx.style_mut_of(egui::Theme::Light, style);
    ctx.set_theme(egui::Theme::Dark);
}

fn style(s: &mut egui::Style) {
    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.button_padding = egui::vec2(10.0, 5.0);
    sp.interact_size.y = 24.0;
    sp.window_margin = Margin::same(14);
    sp.menu_margin = Margin::same(8);

    let v = &mut s.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.extreme_bg_color = BG;
    v.faint_bg_color = CARD;
    v.code_bg_color = BG;
    v.hyperlink_color = CYAN;
    v.warn_fg_color = WARN;
    v.error_fg_color = Color32::from_rgb(0xF8, 0x71, 0x71);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.slider_trailing_fill = true;

    let r = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = RAISED;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = r;

    w.hovered.bg_fill = Color32::from_rgb(0x2A, 0x36, 0x4F);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x2A, 0x36, 0x4F);
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = ACCENT;
    w.active.weak_bg_fill = ACCENT;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT_HOVER);
    w.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = RAISED;
    w.open.weak_bg_fill = RAISED;
    w.open.bg_stroke = Stroke::new(1.0, BORDER);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.corner_radius = r;
}

/// A rounded card for one dock column.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(10))
}

/// A small rounded label, e.g. a status-bar stat or a source type.
pub fn chip(
    ui: &mut egui::Ui,
    text: impl Into<String>,
    fg: Color32,
    bg: Color32,
) -> egui::Response {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(fg).size(11.5));
        })
        .response
}

/// The main call-to-action button: filled, bold, tall.
pub fn primary(text: &str, fill: Color32) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text)
            .strong()
            .size(15.0)
            .color(Color32::WHITE),
    )
    .fill(fill)
    .corner_radius(CornerRadius::same(RADIUS))
}
