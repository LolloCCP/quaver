//! Turns cover art into the window's backdrop, accent color and thumbnails.

use slint::{Rgb8Pixel, Rgba8Pixel, SharedPixelBuffer};

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;
type Rgb = [f32; 3];

/// A theme: what the cover's colors are turned into, and how panels look.
#[derive(Clone, Copy)]
pub struct Style {
    pub id: &'static str,
    pub name: &'static str,
    /// Dark text on light surfaces.
    pub bright: bool,
    base: Base,
    surface: Surface,
    radius: f32,
    /// How wet buttons and covers look, 0 to 1.
    pub gloss: f32,
    /// Whether a light follows the pointer.
    pub glow: bool,
    /// Colors used instead of the cover's.
    colors: Option<[Rgb; 2]>,
}

#[derive(Clone, Copy)]
enum Base {
    Cover,
    /// The cover's own gradient, as vivid and bright as text on it allows.
    Vivid,
    /// The cover's colors, washed out almost to white.
    Pastel,
    Solid(Rgb),
}

#[derive(Clone, Copy)]
enum Surface {
    Glass,
    /// A plain color, how much of it covers what is behind, and how visible its outline is.
    Flat(Rgb, f32, f32),
    /// Black, with a glowing outline.
    Neon,
}

pub const STYLES: [Style; 8] = [
    Style { id: "glass", name: "Liquid glass", bright: false, base: Base::Cover, surface: Surface::Glass, radius: 24., gloss: 1., glow: true, colors: None },
    Style { id: "midnight", name: "Midnight", bright: false, base: Base::Solid([9., 9., 13.]), surface: Surface::Flat([21., 21., 27.], 1., 0.06), radius: 14., gloss: 0., glow: false, colors: None },
    Style { id: "black", name: "Pure black", bright: false, base: Base::Solid([0.; 3]), surface: Surface::Flat([0.; 3], 1., 0.16), radius: 10., gloss: 0., glow: false, colors: None },
    Style { id: "neon", name: "Neon", bright: false, base: Base::Solid([5., 4., 10.]), surface: Surface::Neon, radius: 18., gloss: 0.5, glow: true, colors: None },
    Style { id: "vinyl", name: "Warm vinyl", bright: false, base: Base::Solid([27., 20., 15.]), surface: Surface::Flat([42., 31., 24.], 1., 0.09), radius: 14., gloss: 0.35, glow: false, colors: Some([[232., 150., 70.], [196., 84., 62.]]) },
    Style { id: "paper", name: "Paper", bright: true, base: Base::Solid([233., 228., 218.]), surface: Surface::Flat([250., 247., 241.], 1., 0.1), radius: 16., gloss: 0., glow: false, colors: None },
    Style { id: "mist", name: "Mist", bright: true, base: Base::Pastel, surface: Surface::Flat([255.; 3], 0.6, 0.12), radius: 22., gloss: 0.4, glow: false, colors: None },
    Style { id: "cover", name: "Cover", bright: false, base: Base::Vivid, surface: Surface::Flat([0.; 3], 0.4, 0.14), radius: 20., gloss: 0.6, glow: true, colors: None },
];

pub const CUSTOM: &str = "custom";

impl Style {
    /// The themes, with the one made of the person's own background and highlight last.
    pub fn all(custom: [[u8; 3]; 2]) -> impl Iterator<Item = Style> {
        STYLES.into_iter().chain([Style::custom(custom)])
    }

    pub fn by_id(id: &str, custom: [[u8; 3]; 2]) -> Style {
        Style::all(custom).find(|s| s.id == id).unwrap_or(STYLES[0])
    }

    fn custom(colors: [[u8; 3]; 2]) -> Style {
        let [base, highlight] = colors.map(|c| c.map(f32::from));
        let bright = takes_black(colors[0]);
        // Panels move away from the text's color, so it reads at least as well on them.
        let panel = mix(base, [if bright { 255. } else { 0. }; 3], 0.25).map(f32::floor);
        Style { id: CUSTOM, name: "Custom", bright, base: Base::Solid(base), surface: Surface::Flat(panel, 1., 0.14), radius: 16., gloss: 0., glow: false, colors: Some([highlight; 2]) }
    }

    /// What lies over a surface to make something stand out: white on dark, black on light.
    pub fn over(&self) -> [u8; 3] {
        if self.bright { [0; 3] } else { [255; 3] }
    }
}

impl std::fmt::Debug for Style {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.id)
    }
}

/// Colors taken from a cover. The UI fades from one palette to the next.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Palette {
    /// For text and thin shapes on the glass.
    pub accent: [u8; 3],
    /// Vivid pair for filled shapes, readable under `ink`.
    pub fill: [u8; 3],
    pub fill2: [u8; 3],
    pub ink: [u8; 3],
    /// What the glass is tinted with.
    pub tint: [u8; 3],
    pub text: [u8; 3],
    pub dim: [u8; 3],
}

impl Palette {
    pub fn fade(&self, to: &Palette, t: f32) -> Palette {
        let mix = |a: [u8; 3], b: [u8; 3]| std::array::from_fn(|c| (a[c] as f32 + (b[c] as f32 - a[c] as f32) * t).round() as u8);
        Palette {
            accent: mix(self.accent, to.accent),
            fill: mix(self.fill, to.fill),
            fill2: mix(self.fill2, to.fill2),
            ink: mix(self.ink, to.ink),
            tint: mix(self.tint, to.tint),
            text: mix(self.text, to.text),
            dim: mix(self.dim, to.dim),
        }
    }
}

pub struct Look {
    pub backdrop: Pixels,
    pub palette: Palette,
    /// The backdrop's top edge, for the title bar.
    pub edge: [u8; 3],
}

/// Enough detail for the glass to visibly bend, few enough cells to stay blurred.
const GRID: usize = 8;
const BACKDROP: usize = 256;
/// Keeps every backdrop dark enough for the text on it.
const MAX_LUMINANCE: f32 = 0.05;
/// The Cover theme's: as bright as the accent on it stays readable.
const VIVID_LUMINANCE: f32 = 0.09;
/// The same for themes with dark text.
const MIN_BRIGHT_LUMINANCE: f32 = 0.72;

/// `cover` is `None` until the first one arrives.
pub fn look(cover: Option<&Pixels>, style: &Style) -> Look {
    let Some(cover) = cover else { return look_from(&default_cells(), style) };
    let small = shrink(cover, GRID as u32);
    let cells: Vec<Rgb> = small.as_slice().iter().map(|p| [p.r as f32, p.g as f32, p.b as f32]).collect();
    look_from(&cells, style)
}

fn default_cells() -> Vec<Rgb> {
    const LIGHTS: [(f32, f32, Rgb); 4] = [
        (0.5, 0.5, [255., 138., 60.]),
        (6.5, 1., [140., 70., 190.]),
        (1.5, 6.5, [40., 110., 190.]),
        (6., 6., [210., 60., 110.]),
    ];
    (0..GRID * GRID)
        .map(|i| {
            let (x, y) = ((i % GRID) as f32, (i / GRID) as f32);
            LIGHTS.iter().fold([26., 22., 34.], |base: Rgb, (lx, ly, color)| {
                let glow = (-((x - lx).powi(2) + (y - ly).powi(2)) / 6.).exp();
                std::array::from_fn(|c| base[c] + (color[c] - base[c]) * glow)
            })
        })
        .collect()
}

fn look_from(cells: &[Rgb], style: &Style) -> Look {
    let palette = palette(cells, style);
    let cells = &match style.base {
        Base::Cover => backdrop_cells(cells, 0.75, 1.3, MAX_LUMINANCE),
        Base::Vivid => backdrop_cells(cells, 1., 1.5, VIVID_LUMINANCE),
        Base::Pastel => cells.iter().map(|c| lighten(*c, MIN_BRIGHT_LUMINANCE)).collect(),
        Base::Solid(color) => vec![color; GRID * GRID],
    };
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
    Look { backdrop, palette, edge }
}

/// Richer colors, dimmed only as far as reading text needs: a deep blue stays vivid
/// where a flat factor would turn it black.
/// `own` is how much of each cell's color is kept against the cover's mean, `saturation` how much it is boosted.
fn backdrop_cells(cells: &[Rgb], own: f32, saturation: f32, max_luminance: f32) -> Vec<Rgb> {
    let mean: Rgb = std::array::from_fn(|c| cells.iter().map(|v| v[c]).sum::<f32>() / cells.len() as f32);
    cells
        .iter()
        .map(|cell| {
            let mut color: Rgb = std::array::from_fn(|c| cell[c] * own + mean[c] * (1. - own));
            let gray = color.iter().sum::<f32>() / 3.;
            color = color.map(|v| (gray + (v - gray) * saturation).clamp(0., 254.));
            while luminance(color.map(|v| v as u8)) > max_luminance {
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

/// Uses the colors as they were picked; only text in them is moved as far as reading needs.
fn custom_palette(base: Rgb, panel: Rgb, highlight: Rgb, style: &Style) -> Palette {
    let text = if style.bright { [0.; 3] } else { [255.; 3] };
    let backs = [base, panel, mix(panel, text, 0.08)].map(|c| luminance(bytes(c)));
    let readable = |color: Rgb| {
        let steps = (0..=20).map(|i| mix(color, text, i as f32 / 20.));
        bytes(steps.into_iter().find(|c| backs.iter().all(|b| contrast(luminance(bytes(*c)), *b) >= 4.5)).unwrap_or(text))
    };
    Palette {
        accent: readable(highlight),
        fill: bytes(highlight),
        fill2: bytes(highlight),
        ink: if takes_black(bytes(highlight)) { [0; 3] } else { [255; 3] },
        tint: bytes(panel),
        text: bytes(text),
        dim: readable(mix(text, base, 0.35)),
    }
}

fn palette(cells: &[Rgb], style: &Style) -> Palette {
    if let (CUSTOM, Base::Solid(base), Surface::Flat(panel, ..), Some([highlight, _])) = (style.id, style.base, style.surface, style.colors) {
        return custom_palette(base, panel, highlight, style);
    }
    let spread = |c: &Rgb| c.iter().fold(0f32, |m, v| m.max(*v)) - c.iter().fold(255f32, |m, v| m.min(*v));
    let distance = |a: &Rgb, b: &Rgb| (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>();
    let full = |c: Rgb| {
        let peak = c.iter().fold(1f32, |m, v| m.max(*v));
        c.map(|v| v * 240. / peak)
    };
    // A gray cover keeps the app's own colors.
    let colorful: Vec<Rgb> = cells.iter().filter(|c| spread(c) >= 24.).copied().collect();
    let main = colorful.iter().copied().max_by(|a, b| spread(a).total_cmp(&spread(b)));
    let main = full(main.unwrap_or([242., 140., 80.]));
    // The second color is the one least like the first, if the cover has one at all.
    let second = colorful.iter().map(|c| full(*c)).max_by(|a, b| distance(a, &main).total_cmp(&distance(b, &main)));
    let second = second.unwrap_or([220., 90., 130.]);

    let [main, second] = style.colors.unwrap_or([main, second]);
    let accent = if style.bright { darken(main, 0.075) } else { lighten(main, 0.6) };
    let fill = lighten(main, 0.3);
    let mean: Rgb = std::array::from_fn(|c| cells.iter().map(|v| v[c]).sum::<f32>() / cells.len() as f32);
    let mut tint = mix(mean, main, 0.4);
    while luminance(bytes(tint)) > 0.008 {
        tint = tint.map(|v| v * 0.92);
    }
    Palette {
        accent: bytes(accent),
        fill: bytes(fill),
        fill2: bytes(lighten(second, 0.3)),
        ink: bytes(fill.map(|v| v * 0.07)),
        tint: match style.surface {
            Surface::Glass => bytes(tint),
            Surface::Flat(color, ..) => bytes(color),
            Surface::Neon => [4, 3, 8],
        },
        text: bytes(if style.bright { mix([18.; 3], accent, 0.15) } else { mix([255.; 3], accent, 0.1) }),
        dim: bytes(if style.bright { darken(mix([80.; 3], accent, 0.3), 0.075) } else { lighten(mix([205.; 3], accent, 0.3), 0.6) }),
    }
}

fn bytes(color: Rgb) -> [u8; 3] {
    color.map(|v| v as u8)
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    std::array::from_fn(|c| a[c] + (b[c] - a[c]) * t)
}

/// WCAG contrast of two luminances.
fn contrast(a: f32, b: f32) -> f32 {
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Whether black text reads better on a color than white text.
fn takes_black(color: [u8; 3]) -> bool {
    contrast(luminance(color), 0.) >= contrast(luminance(color), 1.)
}

/// Mixes in white until the color is bright enough.
fn lighten(mut color: Rgb, target: f32) -> Rgb {
    while luminance(bytes(color)) < target {
        color = color.map(|v| v + (256. - v) * 0.08);
    }
    color
}

fn darken(mut color: Rgb, target: f32) -> Rgb {
    while luminance(bytes(color)) > target {
        color = color.map(|v| v * 0.94);
    }
    color
}

/// WCAG relative luminance.
fn luminance(color: [u8; 3]) -> f32 {
    let [r, g, b] = color.map(|v| {
        let v = v as f32 / 255.;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Everything behind the content: the backdrop, and on it the glass panels. Painting it once
/// and copying it each frame is what keeps the app smooth.
#[derive(Clone)]
pub struct Scene {
    pub backdrop: Pixels,
    pub palette: Palette,
    pub style: Style,
    /// How much of the palette's tint is in the glass.
    pub tint: f32,
    /// Physical size of the window, and physical pixels per logical one.
    pub size: (u32, u32),
    pub scale: f32,
    pub panels: Vec<Panel>,
}

/// A glass panel, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Panel {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// How far the shadow of a panel reaches, and how far down it is moved.
const SHADOW: f32 = 28.;
const SHADOW_DROP: f32 = 6.;
/// How much bigger the backdrop looks through the glass.
const ZOOM: f32 = 1.5;
/// Light on the top of the glass, and how far down it reaches (see Theme in glass.slint).
const SHEEN: f32 = 13. / 255.;
const SHEEN_HEIGHT: f32 = 120.;

impl Scene {
    pub fn paint(&self) -> SharedPixelBuffer<Rgb8Pixel> {
        let (width, height) = self.size;
        let mut out = SharedPixelBuffer::new(width, height);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
        let rows = (height as usize).div_ceil(threads).max(1);
        if width > 0 {
            std::thread::scope(|scope| {
                for (part, lines) in out.make_mut_slice().chunks_mut(rows * width as usize).enumerate() {
                    scope.spawn(move || {
                        for (i, line) in lines.chunks_mut(width as usize).enumerate() {
                            self.paint_line((part * rows + i) as u32, line);
                        }
                    });
                }
            });
        }
        out
    }

    fn paint_line(&self, row: u32, line: &mut [Rgb8Pixel]) {
        let (w, h) = (self.size.0 as f32 / self.scale, self.size.1 as f32 / self.scale);
        let y = (row as f32 + 0.5) / self.scale;
        for (column, pixel) in line.iter_mut().enumerate() {
            let x = (column as f32 + 0.5) / self.scale;
            // Two dithered steps per channel hide the bands of a very dark gradient.
            let noise = [0.1, 0.6, 0.85, 0.35][(column % 2) + 2 * (row as usize % 2)];
            *pixel = self.color(x, y, w, h, noise);
        }
        // Everything that isn't glass or its shadow is done by now.
        let near = |p: &&Panel| y > p.y - SHADOW && y < p.y + p.height + SHADOW + SHADOW_DROP;
        for panel in self.panels.iter().filter(near) {
            let from = (((panel.x - SHADOW) * self.scale).max(0.) as usize).min(line.len());
            let to = (((panel.x + panel.width + SHADOW) * self.scale).max(0.) as usize).min(line.len());
            for (i, pixel) in line[from..to].iter_mut().enumerate() {
                let x = ((from + i) as f32 + 0.5) / self.scale;
                let noise = [0.1, 0.6, 0.85, 0.35][((from + i) % 2) + 2 * (row as usize % 2)];
                self.glass(panel, x, y, w, h, noise, pixel);
            }
        }
    }

    fn glass(&self, panel: &Panel, x: f32, y: f32, w: f32, h: f32, noise: f32, pixel: &mut Rgb8Pixel) {
        let (cx, cy) = (panel.x + panel.width / 2., panel.y + panel.height / 2.);
        let radius = self.style.radius.min(panel.width / 2.).min(panel.height / 2.);
        // How deep inside the rounded outline a point is, and which way the nearest edge faces.
        let depth = |x: f32, y: f32| {
            let (dx, dy) = (x - cx, y - cy);
            let (ex, ey) = (dx.abs() - (panel.width / 2. - radius), dy.abs() - (panel.height / 2. - radius));
            let inner = if ex > ey { (ex, 0.) } else { (0., ey) };
            let (nx, ny) = if ex > 0. && ey > 0. { (ex, ey) } else { inner };
            let along = if ex > 0. && ey > 0. { ex.hypot(ey) } else { ex.max(ey) };
            let length = nx.hypot(ny).max(0.001);
            (radius - along, nx.abs() / length * dx.signum(), ny.abs() / length * dy.signum())
        };
        let mix = |v: u8, to: f32, t: f32| v as f32 + (to - v as f32) * t;
        let (inside, nx, ny) = depth(x, y);
        let cover = (inside * self.scale + 0.5).clamp(0., 1.);
        if cover < 1. {
            let outside = -depth(x, y - SHADOW_DROP).0;
            let fade = (1. - outside.max(0.) / SHADOW).max(0.).powi(2);
            let dark = if self.style.bright { 0.1 } else { 0.42 } * fade;
            let p = &self.palette;
            let shade = |c: usize, v: u8| match self.style.surface {
                // Neon shines on what is around it instead of shading it.
                Surface::Neon => (v as f32 + (p.fill[c] as f32 - v as f32) * 0.34 * fade) as u8,
                _ => (v as f32 * (1. - dark)) as u8,
            };
            *pixel = Rgb8Pixel { r: shade(0, pixel.r), g: shade(1, pixel.g), b: shade(2, pixel.b) };
        }
        if cover <= 0. {
            return;
        }
        let across = ((x - panel.x) / panel.width * 0.5 + (y - panel.y) / panel.height * 0.5).clamp(0., 1.);
        let hue = |c: usize| {
            let p = &self.palette;
            p.fill[c] as f32 + (p.fill2[c] as f32 - p.fill[c] as f32) * across
        };
        let outline = (1.2 - inside).clamp(0., 1.);
        let plain = |c: usize, below: u8| {
            let v = match self.style.surface {
                Surface::Flat(color, amount, line) => {
                    let v = mix(below, color[c], amount);
                    v + (self.style.over()[c] as f32 - v) * line * outline
                }
                _ => {
                    let v = self.palette.tint[c] as f32;
                    let glow = 0.22 * (-inside.max(0.) / 5.).exp();
                    v + (hue(c) - v) * ((2. - inside).clamp(0., 1.) + glow).min(1.)
                }
            };
            (below as f32 + (v - below as f32) * cover) as u8
        };
        if !matches!(self.style.surface, Surface::Glass) {
            *pixel = Rgb8Pixel { r: plain(0, pixel.r), g: plain(1, pixel.g), b: plain(2, pixel.b) };
            return;
        }
        let lens = self.color(cx + (x - cx) / ZOOM, cy + (y - cy) / ZOOM, w, h, noise);
        let sheen = SHEEN * (1. - (y - panel.y) / SHEEN_HEIGHT).clamp(0., 1.);
        // Bright where the edge faces the top left, weaker where light leaves at the bottom right.
        let facing = -(nx + ny) / std::f32::consts::SQRT_2;
        let light = 0.22 + 0.78 * facing.max(0.).powi(2) + 0.5 * (-facing).max(0.).powi(2);
        let line = (1.6 - inside).clamp(0., 1.) + 0.28 * (-inside.max(0.) / 5.).exp();
        let rim = (light * line).min(1.);
        // Thick glass splits light just inside its edge, from one color to the other across the panel.
        let band = 0.34 * (0.35 + 0.65 * facing.abs()) * (-((inside - 4.) / 3.).powi(2)).exp();
        let channel = |c: usize, below: u8, lens: u8| {
            let p = &self.palette;
            let mut v = mix(lens, p.tint[c] as f32, self.tint);
            v += (255. - v) * sheen;
            v += (hue(c) - v) * band;
            v += (255. - v) * rim;
            (below as f32 + (v - below as f32) * cover) as u8
        };
        *pixel = Rgb8Pixel { r: channel(0, pixel.r, lens.r), g: channel(1, pixel.g, lens.g), b: channel(2, pixel.b, lens.b) };
    }

    /// The smoothly stretched backdrop at a point of the window.
    fn color(&self, x: f32, y: f32, w: f32, h: f32, noise: f32) -> Rgb8Pixel {
        let (bw, bh) = (self.backdrop.width() as usize, self.backdrop.height() as usize);
        let at = |v: f32, length: f32, count: usize| {
            let v = (v / length).clamp(0., 1.) * (count - 1) as f32;
            let i = (v as usize).min(count.saturating_sub(2));
            (i, v - i as f32)
        };
        let ((ix, tx), (iy, ty)) = (at(x, w, bw), at(y, h, bh));
        let pixels = self.backdrop.as_slice();
        let (p, q) = (&pixels[iy * bw + ix..], &pixels[(iy + 1) * bw + ix..]);
        let blend = |f: fn(&Rgba8Pixel) -> u8| {
            let (a, b, c, d) = (f(&p[0]) as f32, f(&p[1]) as f32, f(&q[0]) as f32, f(&q[1]) as f32);
            let (top, bottom) = (a + (b - a) * tx, c + (d - c) * tx);
            (top + (bottom - top) * ty + noise) as u8
        };
        Rgb8Pixel { r: blend(|p| p.r), g: blend(|p| p.g), b: blend(|p| p.b) }
    }
}

/// A small picture of the app in a style, for the theme picker.
pub fn preview(cover: Option<&Pixels>, style: &Style, tint: f32, size: (u32, u32)) -> SharedPixelBuffer<Rgb8Pixel> {
    let Look { backdrop, palette, .. } = look(cover, style);
    let scale = size.0 as f32 / 320.;
    let panel = |x, y, width, height| Panel { x, y, width, height };
    let panels = vec![panel(12., 12., 74., 118.), panel(98., 12., 210., 118.), panel(12., 142., 296., 34.)];
    let scene = Scene { backdrop, palette, style: *style, tint, size, scale, panels };
    let mut out = scene.paint();
    // A few strokes standing for text and buttons.
    let marks = [(24., 30., 48., palette.text), (24., 48., 40., palette.dim), (24., 64., 44., palette.dim),
        (112., 32., 90., palette.text), (112., 54., 130., palette.dim), (112., 70., 110., palette.dim), (112., 96., 60., palette.accent)];
    let width = size.0 as usize;
    let mut fill = |x: f32, y: f32, w: f32, h: f32, color: &dyn Fn(f32) -> [u8; 3]| {
        for row in (y * scale) as usize..((y + h) * scale) as usize {
            for column in (x * scale) as usize..((x + w) * scale) as usize {
                if let Some(pixel) = out.make_mut_slice().get_mut(row * width + column) {
                    let [r, g, b] = color((column as f32 / scale - x) / w);
                    *pixel = Rgb8Pixel { r, g, b };
                }
            }
        }
    };
    for (x, y, w, color) in marks {
        fill(x, y, w, 7., &|_| color);
    }
    fill(112., 156., 150., 6., &|t| std::array::from_fn(|c| (palette.fill2[c] as f32 + (palette.fill[c] as f32 - palette.fill2[c] as f32) * t) as u8));
    out
}

/// One picture, or four laid out as a square.
pub fn mosaic(pictures: &[Pixels], size: u32) -> Option<Pixels> {
    let [a, b, c, d] = pictures else { return pictures.first().cloned() };
    let half = size.div_ceil(2);
    let parts = [a, b, c, d].map(|p| shrink(p, half));
    let mut out = Pixels::new(half * 2, half * 2);
    for (i, pixel) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % (half * 2), i as u32 / (half * 2));
        let part = &parts[(x / half + y / half * 2) as usize];
        *pixel = part.as_slice()[(y % half * half + x % half) as usize];
    }
    Some(out)
}

/// Small cover with rounded corners and gloss already in it.
pub fn jewel(cover: &Pixels, size: u32, rounding: f32, gloss: f32) -> Pixels {
    let mut out = shrink(cover, size);
    let (n, radius) = (size as f32, size as f32 * rounding);
    for (i, p) in out.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i as u32 % size) as f32 + 0.5, (i as u32 / size) as f32 + 0.5);
        let corner = |v: f32| ((v - n / 2.).abs() - (n / 2. - radius)).max(0.);
        let inside = (radius - corner(x).hypot(corner(y)) + 0.5).clamp(0., 1.);
        let slope = (x * 0.45 + y) / n;
        let gloss = if slope < 0.52 { gloss * (0.26 * (1. - slope / 0.52) + 0.07) } else { 0. };
        let shine = |v: u8| (v as f32 + (255. - v as f32) * gloss) as u8;
        *p = Rgba8Pixel { r: shine(p.r), g: shine(p.g), b: shine(p.b), a: (inside * 255.) as u8 };
    }
    out
}

/// Soft round light, colored and placed by the UI.
pub fn glow() -> slint::Image {
    const SIZE: u32 = 200;
    let mut glow = Pixels::new(SIZE, SIZE);
    for (i, pixel) in glow.make_mut_slice().iter_mut().enumerate() {
        let at = |n: u32| (n as f32 + 0.5) / SIZE as f32 * 2. - 1.;
        let edge = (1. - at(i as u32 % SIZE).hypot(at(i as u32 / SIZE))).max(0.);
        *pixel = Rgba8Pixel { r: 255, g: 255, b: 255, a: (edge * edge * (3. - 2. * edge) * 255.) as u8 };
    }
    slint::Image::from_rgba8(glow)
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
        let steps = [0u8, 85, 170, 255];
        for style in &STYLES {
        for r in steps {
            for g in steps {
                for b in steps {
                    let Look { backdrop, palette: p, .. } = look(Some(&solid(r, g, b, 8, 8)), style);
                    let over = |base: f32, top: f32, alpha: f32| base + (top - base) * alpha;
                    let worst: fn(f32, f32) -> f32 = if style.bright { f32::min } else { f32::max };
                    // The worst place text can be: a selected item under the sheen of the
                    // clearest glass with the pointer's light on it (see Theme in glass.slint).
                    let scene = Scene {
                        backdrop: backdrop.clone(),
                        palette: p,
                        style: *style,
                        tint: crate::settings::MIN_TINT,
                        size: (90, 90),
                        scale: 1.,
                        panels: vec![Panel { x: 5., y: 5., width: 80., height: 80. }],
                    };
                    let painted = scene.paint();
                    let text_area = (0..90 * 90).filter(|i| (27..63).contains(&(i % 90)) && (27..63).contains(&(i / 90)));
                    let glass = text_area.map(|i| {
                        let pixel = painted.as_slice()[i];
                        let base = [pixel.r, pixel.g, pixel.b];
                        luminance(std::array::from_fn(|c| {
                            let selected = over(base[c] as f32, style.over()[c] as f32, 0.08);
                            over(selected, p.accent[c] as f32, if style.glow { 0.1 } else { 0. }) as u8
                        }))
                    }).fold(if style.bright { 1. } else { 0. }, worst);
                    let bare = backdrop.as_slice().iter().map(|p| luminance([p.r, p.g, p.b])).fold(if style.bright { 1. } else { 0. }, worst);
                    for (name, color) in [("accent", p.accent), ("text", p.text), ("dim", p.dim)] {
                        for (place, back) in [("backdrop", bare), ("glass", glass)] {
                            let c = contrast(luminance(color), back);
                            assert!(c >= 4.5, "{style:?}: {name} on {place} over {r},{g},{b}: {c}");
                        }
                    }
                    for fill in [p.fill, p.fill2] {
                        let c = contrast(luminance(p.ink), luminance(fill));
                        assert!(c >= 4.5, "{style:?}: ink over {r},{g},{b}: {c}");
                    }
                }
            }
        }
        }
    }

    #[test]
    fn any_custom_colors_keep_text_readable() {
        let steps = [0u8, 51, 102, 117, 120, 153, 204, 255];
        let colors: Vec<[u8; 3]> = steps.iter().flat_map(|r| steps.iter().flat_map(move |g| steps.map(|b| [*r, *g, b]))).collect();
        for base in &colors {
            for highlight in colors.iter().step_by(13) {
                let style = Style::custom([*base, *highlight]);
                let p = palette(&default_cells(), &style);
                assert_eq!((p.fill, p.text), (*highlight, if style.bright { [0; 3] } else { [255; 3] }));
                let selected = std::array::from_fn(|c| (p.tint[c] as f32 + (style.over()[c] as f32 - p.tint[c] as f32) * 0.08) as u8);
                for back in [*base, p.tint, selected] {
                    for (name, color) in [("accent", p.accent), ("text", p.text), ("dim", p.dim)] {
                        let c = contrast(luminance(color), luminance(back));
                        assert!(c >= 4.5, "{name} on {back:?} with {base:?} {highlight:?}: {c}");
                    }
                }
                assert!(contrast(luminance(p.ink), luminance(p.fill)) >= 4.5, "ink on {highlight:?}");
            }
        }
        assert_eq!(look(None, &Style::custom([[9, 99, 199], [255; 3]])).edge, [9, 99, 199]);
        let bright = |color| Style::custom([color, [255, 0, 0]]).bright;
        assert!(bright([255, 255, 255]) && bright([250, 220, 120]) && !bright([20, 30, 60]) && !bright([0, 0, 0]));
    }

    #[test]
    fn two_colored_covers_give_two_colors() {
        let mut cover = solid(220, 30, 40, 16, 16);
        cover.make_mut_slice()[128..].fill(Rgba8Pixel { r: 20, g: 60, b: 230, a: 255 });
        let p = look(Some(&cover), &STYLES[0]).palette;
        let red = |c: [u8; 3]| c[0] > c[2];
        assert_ne!(red(p.fill), red(p.fill2), "{p:?}");
    }

    fn scene(size: (u32, u32), scale: f32, panels: Vec<Panel>) -> Scene {
        let Look { backdrop, mut palette, .. } = look(Some(&solid(120, 120, 120, 8, 8)), &STYLES[0]);
        (palette.fill, palette.fill2) = ([255, 0, 0], [0, 0, 255]);
        Scene { backdrop, palette, style: STYLES[0], tint: 0.5, size, scale, panels }
    }

    #[test]
    fn the_edge_shows_both_colors() {
        let painted = scene((200, 200), 1., vec![Panel { x: 20., y: 20., width: 160., height: 160. }]).paint();
        let at = |x: usize, y: usize| painted.as_slice()[y * 200 + x];
        let (near, far) = (at(24, 60), at(175, 140));
        assert!(near.r > near.b + 10 && far.b > far.r + 10, "{near:?} {far:?}");
    }

    #[test]
    fn every_theme_has_its_own_look_and_preview() {
        let mut seen = Vec::new();
        let custom = [[30, 34, 54], [255, 150, 80]];
        for style in &Style::all(custom).collect::<Vec<_>>() {
            assert_eq!(Style::by_id(style.id, custom).name, style.name);
            let preview = preview(None, style, 0.5, (160, 94));
            assert!(!seen.contains(&preview.as_slice().to_vec()), "{style:?} looks like another");
            seen.push(preview.as_slice().to_vec());
        }
        assert_eq!(Style::by_id("removed theme", custom).id, "glass");
        preview(None, &STYLES[0], 0.5, (1, 1));
    }

    #[test]
    fn odd_windows_and_panels_do_not_panic() {
        let panels = vec![
            Panel { x: -50., y: -50., width: 80., height: 70. },
            Panel { x: 900., y: 900., width: 10., height: 10. },
            Panel { x: 10., y: 10., width: 0., height: 0. },
            Panel { x: 0., y: 0., width: 4000., height: 4000. },
        ];
        for size in [(0, 0), (0, 5), (1, 1), (7, 3), (333, 111)] {
            for scale in [1., 1.5, 2.] {
                let painted = scene(size, scale, panels.clone()).paint();
                assert_eq!((painted.width(), painted.height()), size);
            }
        }
    }

    #[test]
    fn painting_a_full_screen_is_quick() {
        let panels = vec![Panel { x: 12., y: 70., width: 300., height: 850. }, Panel { x: 324., y: 70., width: 1280., height: 850. }];
        let start = std::time::Instant::now();
        scene((1920, 1080), 1., panels).paint();
        // Generous: this also runs unoptimized. It is done on another thread anyway.
        assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());
        println!("full screen: {:?}", start.elapsed());
    }

    #[test]
    fn fades_start_and_end_exactly() {
        let (a, b) = (look(Some(&solid(200, 20, 20, 8, 8)), &STYLES[0]).palette, look(Some(&solid(20, 20, 200, 8, 8)), &STYLES[0]).palette);
        assert_eq!((a.fade(&b, 0.), a.fade(&b, 1.)), (a, b));
    }

    #[test]
    fn odd_sizes_do_not_panic_and_keep_the_color() {
        for (w, h) in [(1, 1), (3, 7), (640, 480)] {
            let p = shrink(&solid(10, 20, 30, w, h), 112).as_slice()[5000];
            assert_eq!((p.r, p.g, p.b), (10, 20, 30));
        }
        shrink(&Pixels::new(0, 0), 4);
        let j = jewel(&solid(10, 20, 30, 64, 64), 48, 0.22, 1.);
        assert_eq!(j.as_slice()[0].a, 0, "corners are cut");
        let four = [10, 20, 30, 40].map(|v| solid(v, v, v, 5, 9));
        let m = mosaic(&four, 15).unwrap();
        let at = |x: u32, y: u32| m.as_slice()[(y * m.width() + x) as usize].r;
        assert_eq!([at(0, 0), at(15, 0), at(0, 15), at(15, 15)], [10, 20, 30, 40]);
        assert_eq!(mosaic(&four[..1], 15).unwrap().as_slice()[0].r, 10);
        assert!(mosaic(&[], 15).is_none());
        assert_eq!(j.as_slice()[48 * 24 + 24].a, 255);
    }
}
