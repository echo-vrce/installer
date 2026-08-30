// SPDX-License-Identifier: GPL-3.0-or-later
//! Icons drawn as vector strokes, not glyphs from a text font.
//!
//! This is a reliability decision, not a stylistic one. Inter has no U+26A0 (warning)
//! or U+24D8 (circled i), and Windows Arial has no U+2713 (check) - the original Java
//! installer hit exactly that and its comments record the icons rendering as empty
//! "tofu" boxes. Shapes have no font dependency, recolour per state for free, and
//! re-tessellate crisp at every UI scale instead of needing per-DPI bitmaps.
//!
//! Every icon takes the rect it should fill, so callers size them on the 12/14/16 pt
//! grid and the shared stroke weight keeps the set looking like a set.

use egui::{pos2, Color32, Painter, Pos2, Rect, Shape, Stroke};

/// One shared stroke weight, derived from the icon box. This is what makes eight
/// independently drawn shapes read as one family.
fn weight(r: Rect) -> f32 {
    (r.width() / 7.5).max(1.4)
}

/// Point at normalised (x, y) inside the icon box.
fn p(r: Rect, x: f32, y: f32) -> Pos2 {
    pos2(r.left() + r.width() * x, r.top() + r.height() * y)
}

fn polyline(painter: &Painter, pts: Vec<Pos2>, c: Color32, w: f32) {
    painter.add(Shape::line(pts, Stroke::new(w, c)));
}

/// Validation passed, dependency present, step complete.
pub fn check(painter: &Painter, r: Rect, c: Color32) {
    polyline(
        painter,
        vec![p(r, 0.14, 0.53), p(r, 0.40, 0.80), p(r, 0.87, 0.20)],
        c,
        weight(r),
    );
}

/// Validation failed, dependency missing.
pub fn cross(painter: &Painter, r: Rect, c: Color32) {
    let w = weight(r);
    painter.line_segment([p(r, 0.20, 0.20), p(r, 0.80, 0.80)], Stroke::new(w, c));
    painter.line_segment([p(r, 0.80, 0.20), p(r, 0.20, 0.80)], Stroke::new(w, c));
}

/// Neutral information - free space, detected versions, "this is what I see".
pub fn info(painter: &Painter, r: Rect, c: Color32) {
    let w = weight(r) * 0.8;
    painter.circle_stroke(r.center(), r.width() * 0.44, Stroke::new(w, c));
    // The "i": a dot over a short bar, drawn rather than typeset.
    painter.circle_filled(p(r, 0.5, 0.29), (r.width() * 0.055).max(0.9), c);
    painter.line_segment([p(r, 0.5, 0.44), p(r, 0.5, 0.72)], Stroke::new(w, c));
}

/// A caveat the user should read before continuing - needs admin, will overwrite.
pub fn warning(painter: &Painter, r: Rect, c: Color32) {
    let w = weight(r) * 0.8;
    polyline(
        painter,
        vec![
            p(r, 0.50, 0.10),
            p(r, 0.94, 0.86),
            p(r, 0.06, 0.86),
            p(r, 0.50, 0.10),
        ],
        c,
        w,
    );
    painter.line_segment([p(r, 0.5, 0.40), p(r, 0.5, 0.63)], Stroke::new(w, c));
    painter.circle_filled(p(r, 0.5, 0.755), (r.width() * 0.055).max(0.9), c);
}

/// Current step, or a checklist row that is running right now.
pub fn dot_filled(painter: &Painter, r: Rect, c: Color32) {
    painter.circle_filled(r.center(), r.width() * 0.26, c);
}

/// A step not reached yet.
pub fn dot_hollow(painter: &Painter, r: Rect, c: Color32) {
    painter.circle_stroke(r.center(), r.width() * 0.24, Stroke::new(weight(r) * 0.7, c));
}

/// Collapsed / expanded affordance for the log pane.
pub fn chevron(painter: &Painter, r: Rect, c: Color32, open: bool) {
    let w = weight(r) * 0.85;
    let pts = if open {
        vec![p(r, 0.22, 0.38), p(r, 0.50, 0.66), p(r, 0.78, 0.38)]
    } else {
        vec![p(r, 0.38, 0.22), p(r, 0.66, 0.50), p(r, 0.38, 0.78)]
    };
    polyline(painter, pts, c, w);
}

/// Link that leaves the app (Discord, the repo).
pub fn arrow_out(painter: &Painter, r: Rect, c: Color32) {
    let w = weight(r) * 0.8;
    painter.line_segment([p(r, 0.28, 0.72), p(r, 0.74, 0.26)], Stroke::new(w, c));
    polyline(
        painter,
        vec![p(r, 0.46, 0.24), p(r, 0.76, 0.24), p(r, 0.76, 0.54)],
        c,
        w,
    );
}
