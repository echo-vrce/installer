// SPDX-License-Identifier: GPL-3.0-or-later
//! Colour tokens, spacing, type scale, and the egui style they drive.
//!
//! Every colour in the app comes from this file. The aim is that a stock egui widget
//! already looks correct without per-call styling, so UI code stays about layout and
//! behaviour instead of paint.

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle, Visuals};
use std::sync::Arc;

// ---------------------------------------------------------------- surfaces
pub const BG: Color32 = Color32::from_rgb(0x13, 0x16, 0x19);
pub const SURFACE: Color32 = Color32::from_rgb(0x18, 0x1B, 0x1F);
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x1E, 0x22, 0x27);
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x25, 0x2A, 0x30);

// ---------------------------------------------------------------- borders
pub const DIVIDER: Color32 = Color32::from_rgb(0x2A, 0x2F, 0x36);
pub const BORDER: Color32 = Color32::from_rgb(0x36, 0x3C, 0x44);
pub const BORDER_HOVER: Color32 = Color32::from_rgb(0x45, 0x4C, 0x56);

// ---------------------------------------------------------------- text
pub const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE9, 0xED);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0xA0, 0xA8, 0xB4);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x6B, 0x73, 0x7F);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x4A, 0x51, 0x58);

// ---------------------------------------------------------------- accent
/// Primary button fill and focus ring. Chosen over ACCENT_HOVER because white text
/// on it clears 5:1 contrast, which the lighter blue does not.
pub const ACCENT: Color32 = Color32::from_rgb(0x25, 0x63, 0xEB);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x3B, 0x82, 0xF6);
pub const ACCENT_PRESS: Color32 = Color32::from_rgb(0x1D, 0x4F, 0xD7);
/// Accent used as *text* on a dark surface. A single blue cannot both fill a button
/// legibly and read as text on near-black, hence two tokens.
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0x5B, 0x9B, 0xFF);
pub const ACCENT_SUBTLE: Color32 = Color32::from_rgb(0x1B, 0x25, 0x40);

// ---------------------------------------------------------------- semantic
/// Reserved for validation results only - never for navigation state, or the step
/// column turns into a christmas tree.
pub const SUCCESS: Color32 = Color32::from_rgb(0x3F, 0xB9, 0x50);
pub const WARNING: Color32 = Color32::from_rgb(0xD2, 0x99, 0x22);
pub const ERROR: Color32 = Color32::from_rgb(0xF8, 0x51, 0x49);
pub const ON_ACCENT: Color32 = Color32::WHITE;

// ---------------------------------------------------------------- metrics
/// The one spacing unit. Every gap in the app is a multiple of it; that consistency
/// is most of what separates "minimal" from "empty".
pub const UNIT: f32 = 8.0;
/// Bottom bar heights.
///
/// A panel hands its contents `exact_size` minus its frame margins and nothing more, and
/// clips the frame if they overflow, so the bar then renders *shorter* than it asked for by
/// an amount that depends on what is inside it. These two numbers and `bar_frame`'s 10 px
/// vertical margin belong together: 32 px of room for a 30 px button, 26 for a status line.
/// Four bars asked for 52 and drew 40 for months because nobody had checked.
pub const BAR_H: f32 = 52.0;
/// For a bar that holds only a line of text.
pub const BAR_H_TEXT: f32 = 46.0;

pub const SIDEBAR_W: f32 = 196.0;
/// Content is left-aligned and capped, so a wide window grows the margin rather than
/// stretching lines to an unreadable length.
pub const CONTENT_MAX_W: f32 = 580.0;
pub const RADIUS: u8 = 4;

/// Font family for paths and hashes. Monospace is functional here (it aligns, and it
/// makes hex readable), not decorative.
pub const MONO: &str = "mono";
/// Medium-weight UI face, for headings and button labels.
pub const UI_MED: &str = "ui_medium";
pub const MONO_MED: &str = "mono_medium";

pub fn font_ui(size: f32) -> FontId { FontId::new(size, FontFamily::Proportional) }
pub fn font_med(size: f32) -> FontId { FontId::new(size, FontFamily::Name(UI_MED.into())) }
pub fn font_mono(size: f32) -> FontId { FontId::new(size, FontFamily::Name(MONO.into())) }
pub fn font_mono_med(size: f32) -> FontId { FontId::new(size, FontFamily::Name(MONO_MED.into())) }

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let mut add = |name: &str, bytes: &'static [u8]| {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_static(bytes)));
    };
    add("inter", include_bytes!("../assets/fonts/Inter-Regular.ttf"));
    add("inter_med", include_bytes!("../assets/fonts/Inter-Medium.ttf"));
    add("jb", include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"));
    add("jb_med", include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf"));

    // Insert ours at the front and leave egui's bundled faces behind them, so a stray
    // glyph Inter happens to lack still renders as itself rather than a tofu box.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "jb".to_owned());

    fonts.families.insert(
        FontFamily::Name(UI_MED.into()),
        vec!["inter_med".to_owned(), "inter".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name(MONO.into()),
        vec!["jb".to_owned()],
    );
    fonts.families.insert(
        FontFamily::Name(MONO_MED.into()),
        vec!["jb_med".to_owned(), "jb".to_owned()],
    );

    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(build_style);
}

fn build_style(style: &mut egui::Style) {
    let r = CornerRadius::same(RADIUS);

    // The scrollbar. Floating, so it sits over the content and steals no width, but always
    // drawn: a bar that appears only when needed is a bar you cannot use to judge how much
    // is below until you have already started scrolling. egui's floating default is 2 px at
    // rest, which is too thin to see and too thin to grab.
    let scroll = &mut style.spacing.scroll;
    scroll.floating = true;
    scroll.floating_width = 6.0;
    scroll.bar_width = 8.0;
    // No allocated width: this is what keeps the bar flush to the window edge instead of
    // pushing a margin in front of it.
    scroll.floating_allocated_width = 0.0;
    scroll.bar_inner_margin = 2.0;
    scroll.bar_outer_margin = 0.0;

    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    // extreme_bg_color is what TextEdit paints itself with.
    v.extreme_bg_color = SURFACE_RAISED;
    v.faint_bg_color = SURFACE;
    v.hyperlink_color = ACCENT_TEXT;
    v.selection.bg_fill = ACCENT_SUBTLE;
    v.selection.stroke = Stroke::new(1.0, ACCENT_TEXT);
    // Borders, not shadows.
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;

    // noninteractive: labels, separators, panel frames
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.weak_bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_MUTED);
    v.widgets.noninteractive.corner_radius = r;
    // inactive: a secondary button at rest
    v.widgets.inactive.bg_fill = SURFACE_RAISED;
    v.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.corner_radius = r;
    // hovered
    v.widgets.hovered.bg_fill = SURFACE_HOVER;
    v.widgets.hovered.weak_bg_fill = SURFACE_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.corner_radius = r;
    v.widgets.hovered.expansion = 0.0; // no nudge-on-hover; it reads as jitter
    // active: pressed
    v.widgets.active.bg_fill = ACCENT_PRESS;
    v.widgets.active.weak_bg_fill = ACCENT_PRESS;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.active.fg_stroke = Stroke::new(1.0, ON_ACCENT);
    v.widgets.active.corner_radius = r;
    v.widgets.active.expansion = 0.0;
    // open: combo popups
    v.widgets.open.bg_fill = SURFACE_RAISED;
    v.widgets.open.weak_bg_fill = SURFACE_RAISED;
    v.widgets.open.bg_stroke = Stroke::new(1.0, BORDER_HOVER);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.open.corner_radius = r;

    style.visuals = v;

    style.spacing.item_spacing = egui::vec2(UNIT, UNIT * 0.75);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.interact_size = egui::vec2(40.0, 30.0);
    style.spacing.window_margin = egui::Margin::same(0);
    style.spacing.text_edit_width = 340.0;

    style.text_styles = [
        (TextStyle::Heading, font_med(19.0)),
        (TextStyle::Body, font_ui(12.5)),
        (TextStyle::Button, font_med(12.5)),
        (TextStyle::Small, font_ui(10.5)),
        (TextStyle::Monospace, font_mono(12.0)),
    ]
    .into();

}
