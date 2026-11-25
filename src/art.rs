//! Turns cover art into the window's backdrop, accent color and thumbnails.

use slint::{Rgba8Pixel, SharedPixelBuffer};

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;
type Rgb = [f32; 3];

pub struct Look {
    pub backdrop: Pixels,
    pub accent: [u8; 3],
    /// The backdrop's top edge, for the title bar.
    pub edge: [u8; 3],
}

/// Enough detail for the glass to visibly bend, few enough cells to stay blurred.
const GRID: usize = 8;
const BACKDROP: usize = 256;
/// Keeps every backdrop dark enough for the text on it.
const MAX_LUMINANCE: f32 = 0.05;

/// Shown until the first cover arrives.
pub fn default_look() -> Look {
    const LIGHTS: [(f32, f32, Rgb); 4] = [
        (0.5, 0.5, [255., 138., 60.]),
        (6.5, 1., [140., 70., 190.]),
        (1.5, 6.5, [40., 110., 190.]),
        (6., 6., [210., 60., 110.]),
    ];
    let cells: Vec<Rgb> = (0..GRID * GRID)
        .map(|i| {
            let (x, y) = ((i % GRID) as f32, (i / GRID) as f32);
            LIGHTS.iter().fold([26., 22., 34.], |base: Rgb, (lx, ly, color)| {
                let glow = (-((x - lx).powi(2) + (y - ly).powi(2)) / 6.).exp();
                std::array::from_fn(|c| base[c] + (color[c] - base[c]) * glow)
            })
        })
        .collect();
    look_from(&cells)
}

pub fn look(cover: &Pixels) -> Look {
    let small = shrink(cover, GRID as u32);
    let cells: Vec<Rgb> = small.as_slice().iter().map(|p| [p.r as f32, p.g as f32, p.b as f32]).collect();
    look_from(&cells)
}

fn look_from(cells: &[Rgb]) -> Look {
    let accent = accent(cells);
    let cells = &backdrop_cells(cells);
    let mut backdrop = Pixels::new(BACKDROP as u32, BACKDROP as u32);
    for (i, pixel) in backdrop.make_mut_slice().iter_mut().enumerate() {
        let at = |n: usize| n as f32 / (BACKDROP - 1) as f32 * (GRID - 1) as f32;
        let c = blend(cells, at(i % BACKDROP), at(i / BACKDROP));
        // A pixel of ordered noise hides the bands of a very dark gradient.
        let noise = [0., 0.5, 0.75, 0.25][(i % 2) + 2 * (i / BACKDROP % 2)];
        let [r, g, b] = c.map(|v| (v + noise) as u8);
        *pixel = Rgba8Pixel { r, g, b, a: 255 };
    }
    let edge = blend(cells, (GRID - 1) as f32 / 2., 0.).map(|v| v as u8);
    Look { backdrop, accent, edge }
}

/// Richer colors, dimmed only as far as reading text needs: a deep blue stays vivid
/// where a flat factor would turn it black.
fn backdrop_cells(cells: &[Rgb]) -> Vec<Rgb> {
    let mean: Rgb = std::array::from_fn(|c| cells.iter().map(|v| v[c]).sum::<f32>() / cells.len() as f32);
    cells
        .iter()
        .map(|cell| {
            let mut color: Rgb = std::array::from_fn(|c| cell[c] * 0.75 + mean[c] * 0.25);
            let gray = color.iter().sum::<f32>() / 3.;
            color = color.map(|v| (gray + (v - gray) * 1.3).clamp(0., 254.));
            while luminance(color.map(|v| v as u8)) > MAX_LUMINANCE {
                color = color.map(|v| v * 0.96);
            }
            color
        })
        .collect()
}

fn blend(cells: &[Rgb], x: f32, y: f32) -> Rgb {
    let cell = |v: f32| {
        let i = (v as usize).min(GRID - 2);
        let t = v - i as f32;
        (i, t * t * (3. - 2. * t))
    };
    let ((x, tx), (y, ty)) = (cell(x), cell(y));
    let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
    std::array::from_fn(|c| {
        let at = |x: usize, y: usize| cells[y * GRID + x][c];
        mix(mix(at(x, y), at(x + 1, y), tx), mix(at(x, y + 1), at(x + 1, y + 1), tx), ty)
    })
}

/// The most colorful part of the cover, lightened until it reads on a dark backdrop.
fn accent(cells: &[Rgb]) -> [u8; 3] {
    let spread = |c: &Rgb| c.iter().fold(0f32, |m, v| m.max(*v)) - c.iter().fold(255f32, |m, v| m.min(*v));
    let best = cells.iter().max_by(|a, b| spread(a).total_cmp(&spread(b))).copied().unwrap_or_default();
    let mut color = if spread(&best) < 24. { [242., 166., 90.] } else { best };
    let peak = color.iter().fold(1f32, |m, v| m.max(*v));
    color = color.map(|v| v * 240. / peak);
    while luminance(color.map(|v| v as u8)) < 0.55 {
        color = color.map(|v| v + (255. - v) * 0.1);
    }
    color.map(|v| v as u8)
}

/// WCAG relative luminance.
fn luminance(color: [u8; 3]) -> f32 {
    let [r, g, b] = color.map(|v| {
        let v = v as f32 / 255.;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Corner radius of the glass panels, which the rim is drawn for.
pub const RIM_RADIUS: u32 = 24;

/// Light caught by the edge of a glass panel: bright where it faces the top left,
/// weaker where light leaves at the bottom right. Stretched as a nine-slice image.
pub fn glass_rim() -> slint::Image {
    let size = RIM_RADIUS * 2 + 2;
    let mut rim = Pixels::new(size, size);
    let (half, radius) = (size as f32 / 2., RIM_RADIUS as f32);
    for (i, pixel) in rim.make_mut_slice().iter_mut().enumerate() {
        let at = |n: u32| n as f32 + 0.5 - half;
        let (x, y) = (at(i as u32 % size), at(i as u32 / size));
        let corner = |v: f32| (v.abs() - (half - radius)).max(0.) * v.signum();
        let (cx, cy) = (corner(x), corner(y));
        let distance = cx.hypot(cy);
        let depth = radius - distance;
        let facing = if distance > 0. { -(cx + cy) / distance / std::f32::consts::SQRT_2 } else { 0. };
        let light = 0.14 + 0.7 * facing.max(0.).powi(2) + 0.36 * (-facing).max(0.).powi(2);
        let line = (1.5 - depth).clamp(0., 1.) * (depth + 0.5).clamp(0., 1.);
        let glow = if depth > 0. { 0.2 * (-depth / 6.).exp() } else { 0. };
        *pixel = Rgba8Pixel { r: 255, g: 255, b: 255, a: (light * (line + glow).min(1.) * 255.) as u8 };
    }
    let mut image = slint::Image::from_rgba8(rim);
    let edge = RIM_RADIUS as u16;
    image.set_nine_slice_edges(edge, edge, edge, edge);
    image
}

/// Box-filtered square thumbnail; the renderer itself only samples the nearest pixel.
pub fn shrink(source: &Pixels, size: u32) -> Pixels {
    let (w, h) = (source.width(), source.height());
    let mut out = Pixels::new(size, size);
    if w == 0 || h == 0 {
        return out;
    }
    let pixels = source.as_slice();
    for (i, target) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % size, i as u32 / size);
        let range = |n: u32, len: u32| {
            let from = n * len / size;
            from..((n + 1) * len / size).max(from + 1)
        };
        let (mut sum, mut count) = ([0u32; 3], 0);
        for sy in range(y, h) {
            for sx in range(x, w) {
                let p = pixels[(sy * w + sx) as usize];
                sum = [sum[0] + p.r as u32, sum[1] + p.g as u32, sum[2] + p.b as u32];
                count += 1;
            }
        }
        let [r, g, b] = sum.map(|s| (s / count) as u8);
        *target = Rgba8Pixel { r, g, b, a: 255 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(r: u8, g: u8, b: u8, w: u32, h: u32) -> Pixels {
        let mut p = Pixels::new(w, h);
        p.make_mut_slice().fill(Rgba8Pixel { r, g, b, a: 255 });
        p
    }

    /// Text must stay readable whatever the cover looks like (WCAG AA is 4.5:1).
    #[test]
    fn every_cover_color_keeps_text_readable() {
        let steps = [0u8, 51, 102, 153, 204, 255];
        for r in steps {
            for g in steps {
                for b in steps {
                    let look = look(&solid(r, g, b, 8, 8));
                    let back = look.backdrop.as_slice().iter().map(|p| luminance([p.r, p.g, p.b])).fold(0., f32::max);
                    let contrast = |c| (luminance(c) + 0.05) / (back + 0.05);
                    assert!(contrast(look.accent) >= 4.5, "accent on {r},{g},{b}: {}", contrast(look.accent));
                    assert!(contrast([0xc9, 0xc1, 0xb9]) >= 4.5, "dim text on {r},{g},{b}");

                    // A selected item under the sheen of the clearest glass (see Theme in glass.slint).
                    let over = |base: f32, top: f32, alpha: f32| base + (top - base) * alpha;
                    let glass = |v: u8| {
                        let tinted = over(v as f32, 8., crate::settings::MIN_TINT);
                        over(over(tinted, 255., 13. / 255.), 255., 20. / 255.) as u8
                    };
                    let back = look.backdrop.as_slice().iter().map(|p| luminance([p.r, p.g, p.b].map(glass))).fold(0., f32::max);
                    for (name, c) in [("accent", look.accent), ("dim", [0xd2, 0xcb, 0xc4])] {
                        let contrast = (luminance(c) + 0.05) / (back + 0.05);
                        assert!(contrast >= 4.5, "{name} on glass over {r},{g},{b}: {contrast}");
                    }
                }
            }
        }
    }

    #[test]
    fn odd_sizes_do_not_panic_and_keep_the_color() {
        for (w, h) in [(1, 1), (3, 7), (640, 480)] {
            let p = shrink(&solid(10, 20, 30, w, h), 112).as_slice()[5000];
            assert_eq!((p.r, p.g, p.b), (10, 20, 30));
        }
        shrink(&Pixels::new(0, 0), 4);
    }
}
