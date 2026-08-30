// SPDX-License-Identifier: GPL-3.0-or-later
// The disc mark's geometry and rasteriser, with no dependencies at all.
//
// Separate from `logo.rs` because `build.rs` needs exactly this and nothing else: it
// generates the executable's icon resource at compile time, long before egui exists. It is
// `include!`d there rather than copied, so there stays one definition of the disc.
//
// Plain `//` and not `//!`: an inner doc comment has to open a file, and `include!` splices
// this into the middle of `build.rs`, where one is a syntax error.
//
// All lengths are fractions of the outer radius.

pub struct Geom {
    /// Inner edge of the ring.
    pub r_in: f32,
    /// The free-floating disc in the middle.
    pub r_disc: f32,
    /// Cut band, as perpendicular distance from the centre.
    pub cut_a: f32,
    pub cut_b: f32,
    /// Half-width of the central divider, the one that splits the disc itself in two. It
    /// crosses the middle, so it only ever cuts the ring: the inner disc returns before
    /// this is reached.
    pub cut_mid: f32,
}

/// Measured off the reference disc.
pub const DISPLAY: Geom = Geom { r_in: 0.62, r_disc: 0.42, cut_a: 0.78, cut_b: 0.88, cut_mid: 0.16 };

/// Optical size for 32 logical px and below. The display sliver is 0.12 R, which is
/// sub-pixel there and smears into the ring, so the cut band widens to keep the notches
/// readable. Same mark adjusted for size - not a second mark.
pub const SMALL: Geom = Geom { r_in: 0.62, r_disc: 0.42, cut_a: 0.72, cut_b: 0.86, cut_mid: 0.18 };

/// Leaves a little breathing room in the box, which an app icon wants.
pub const PAD: f32 = 0.93;

/// Subsamples per axis. 4x4 is plenty for edges this simple.
pub const SS: u32 = 4;

/// Is the normalised point inside the mark? `x`/`y` are in units of the outer radius.
pub fn inside(x: f32, y: f32, g: &Geom) -> bool {
    let r2 = x * x + y * y;

    // The inner disc is never cut - the strips sit well outside it, exactly as in the
    // original, so it is tested first and returns early.
    if r2 <= g.r_disc * g.r_disc {
        return true;
    }
    if r2 > 1.0 || r2 < g.r_in * g.r_in {
        return false;
    }

    // Perpendicular offset along the strip normal (1,1)/sqrt(2). Taking the absolute
    // value is what mirrors the cut through the centre and gives the 180-degree symmetry.
    let t = ((x + y) * std::f32::consts::FRAC_1_SQRT_2).abs();
    // Two cuts per half: the central divider that splits the disc, and the outer band that
    // splits each of those halves again. Four arcs in total.
    !(t <= g.cut_mid || (t >= g.cut_a && t <= g.cut_b))
}

/// Per-pixel coverage of the mark, 0..=255, row-major.
pub fn coverage(side: u32, g: &Geom) -> Vec<u8> {
    let n = side as f32;
    let centre = n * 0.5;
    let radius = centre * PAD;
    let step = 1.0 / SS as f32;
    let total = (SS * SS) as u32;

    let mut out = vec![0u8; (side * side) as usize];
    for y in 0..side {
        for x in 0..side {
            let mut hits = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) * step;
                    let py = y as f32 + (sy as f32 + 0.5) * step;
                    if inside((px - centre) / radius, (py - centre) / radius, g) {
                        hits += 1;
                    }
                }
            }
            out[(y * side + x) as usize] = (hits * 255 / total) as u8;
        }
    }
    out
}

