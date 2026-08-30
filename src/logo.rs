// SPDX-License-Identifier: GPL-3.0-or-later
//! The Echo VRCE disc mark.
//!
//! Geometry taken from the Echo Arena disc: an annulus with a detached inner disc, cut
//! by two parallel 45-degree strips mirrored through the centre - 180-degree rotational
//! symmetry. All lengths below are fractions of the outer radius.
//!
//! The mark is rasterised into an alpha mask and cached as a texture, then drawn tinted.
//! An earlier version painted the cuts in the background colour on top of a ring stroke,
//! which was three draw calls and no rasteriser - but it only looked right when the
//! caller knew the exact colour behind the mark, and the first call site that got that
//! wrong showed faint grey slashes. A real alpha channel has no such coupling: the mark
//! is correct on any surface, and the geometry stays in one place.
//!
//! Cost is paid once per (pixel size, optical size) pair and cached on the egui context,
//! so resizing or moving to a HiDPI screen re-rasterises rather than scaling a bitmap.

use egui::{pos2, Color32, ColorImage, Painter, Pos2, Rect, TextureHandle, TextureOptions};

use crate::mark::{coverage, DISPLAY, SMALL};
use crate::theme;

fn texture(ctx: &egui::Context, physical: u32, small: bool) -> TextureHandle {
    let id = egui::Id::new(("evrce.logo", physical, small));
    if let Some(handle) = ctx.data(|d| d.get_temp::<TextureHandle>(id)) {
        return handle;
    }

    let alpha = coverage(physical, if small { &SMALL } else { &DISPLAY });
    // Premultiplied white: tinting then multiplies in the wanted colour and leaves the
    // coverage as the alpha channel.
    let pixels = alpha
        .iter()
        .map(|&a| Color32::from_rgba_premultiplied(a, a, a, a))
        .collect();
    let image = ColorImage::new([physical as usize, physical as usize], pixels);

    let handle = ctx.load_texture(
        format!("evrce.logo.{physical}.{small}"),
        image,
        TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    handle
}

/// Draws the mark inside `rect`, tinted `colour`. Works on any background.
pub fn mark(ctx: &egui::Context, painter: &Painter, rect: Rect, colour: Color32) {
    let side = rect.width().min(rect.height());
    if side < 4.0 {
        return;
    }
    // Rasterise at device pixels so the mark is crisp at 125% / 150% / HiDPI, but pick
    // the optical size from the *logical* size - how big it looks, not how many pixels
    // it happens to get.
    let physical = ((side * ctx.pixels_per_point()).round() as u32).clamp(8, 1024);
    let handle = texture(ctx, physical, side <= 32.0);

    let square = Rect::from_center_size(rect.center(), egui::vec2(side, side));
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    painter.image(handle.id(), square, uv, colour);
}

/// Straight RGBA for the window/executable icon, from the same geometry as everything
/// else. Runs before any egui context exists, so it takes no context and ships no asset.
pub fn icon_rgba(side: u32, colour: Color32) -> Vec<u8> {
    let alpha = coverage(side, if side <= 32 { &SMALL } else { &DISPLAY });
    let (r, g, b) = (colour.r(), colour.g(), colour.b());
    let mut out = Vec::with_capacity(alpha.len() * 4);
    for &a in &alpha {
        // Straight (non-premultiplied) RGBA is what window managers expect here.
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

/// Draws text with manual letter tracking and returns the width consumed. egui has no
/// tracking control, and 25 px caps set solid look cramped. `Painter::text` hands back
/// the rect it drew, which is all the advance information this needs.
fn tracked(
    painter: &Painter,
    at: Pos2,
    text: &str,
    font: egui::FontId,
    colour: Color32,
    track: f32,
) -> f32 {
    let mut x = at.x;
    for ch in text.chars() {
        let r = painter.text(pos2(x, at.y), egui::Align2::LEFT_TOP, ch, font.clone(), colour);
        x += r.width() + track;
    }
    (x - at.x - track).max(0.0)
}

/// Where a lockup is being drawn. Each variant is a size, not a different design.
#[derive(Clone, Copy, PartialEq)]
pub enum Lockup {
    /// Top of the step column, 196 px wide. One line.
    Sidebar,
    /// Home screen header. One line.
    Header,
    /// About screen. Stacked, with the subtitle in tracked caps.
    Hero,
}

struct Metrics {
    mark: f32,
    cap: f32,
    sub: f32,
    /// Gap between wordmark and subtitle on the one-line variants.
    gap: f32,
    track_cap: f32,
    track_sub: f32,
    /// Left inset. The sidebar needs one; the others are already inside a margin.
    inset: f32,
    stacked: bool,
}

impl Lockup {
    fn metrics(self) -> Metrics {
        match self {
            // Measured against the 196 px column: this ends at x=160 of 184 usable, so
            // there is real slack rather than a hairline. Stacking was the alternative
            // and it read bottom-heavy against the mark.
            Lockup::Sidebar => Metrics {
                mark: 20.0, cap: 11.0, sub: 11.0, gap: 7.0,
                track_cap: 0.8, track_sub: 0.0, inset: 14.0, stacked: false,
            },
            Lockup::Header => Metrics {
                mark: 28.0, cap: 13.0, sub: 13.0, gap: 10.0,
                track_cap: 1.0, track_sub: 0.0, inset: 0.0, stacked: false,
            },
            Lockup::Hero => Metrics {
                mark: 64.0, cap: 25.0, sub: 12.0, gap: 0.0,
                track_cap: 2.0, track_sub: 4.2, inset: 0.0, stacked: true,
            },
        }
    }
}

/// Mark plus wordmark.
///
/// Identity is drawn in the step column as well as on Home, because the native title bar
/// is not guaranteed: it is absent or unstyled under Wine and on tiling window managers,
/// and then this is the only place the app says what it is. The top of that column is
/// empty space anyway, so it costs no content height.
pub fn lockup(ui: &mut egui::Ui, size: Lockup) {
    let m = size.metrics();
    let height = if m.stacked { m.mark.max(m.cap + m.sub + 6.0) } else { m.mark.max(m.cap + 6.0) };
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter().clone();
    let ctx = ui.ctx().clone();

    let mark_rect = Rect::from_min_size(
        pos2(rect.left() + m.inset, rect.center().y - m.mark * 0.5),
        egui::vec2(m.mark, m.mark),
    );
    mark(&ctx, &painter, mark_rect, theme::ACCENT_HOVER);

    let tx = mark_rect.right() + m.mark * 0.34;
    if m.stacked {
        tracked(&painter, pos2(tx, rect.top() + 6.0), "ECHO VRCE",
                theme::font_med(m.cap), theme::TEXT, m.track_cap);
        tracked(&painter, pos2(tx + 2.0, rect.top() + 6.0 + m.cap + 9.0), "INSTALLER",
                theme::font_ui(m.sub), theme::TEXT_DIM, m.track_sub);
    } else {
        let y = rect.center().y - m.cap * 0.62;
        let w = tracked(&painter, pos2(tx, y), "ECHO VRCE",
                        theme::font_med(m.cap), theme::TEXT, m.track_cap);
        // TEXT_MUTED, not TEXT_DIM: at these sizes the dim grey only reaches about 4:1
        // against the panel, which is where "technically drawn" becomes "unreadable".
        // Size and weight carry the hierarchy instead of contrast doing it.
        painter.text(
            pos2(tx + w + m.gap, y),
            egui::Align2::LEFT_TOP,
            "Installer",
            theme::font_ui(m.sub),
            theme::TEXT_MUTED,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build.rs` cannot use `theme`, because it runs before egui is built, so it carries
    /// its own copy of the accent. This is the thing that stops the two drifting: the
    /// executable icon and the in-app mark are supposed to be the same blue.
    #[test]
    fn icon_colour_matches_the_build_script() {
        let build_rs = include_str!("../build.rs");
        let line = build_rs
            .lines()
            .find(|l| l.contains("const ICON_RGB"))
            .expect("build.rs no longer defines ICON_RGB");
        let c = theme::ACCENT_TEXT;
        let expected = format!("(0x{:02X}, 0x{:02X}, 0x{:02X})", c.r(), c.g(), c.b());
        assert!(line.contains(&expected), "build.rs has {line}, theme has {expected}");
    }
}
