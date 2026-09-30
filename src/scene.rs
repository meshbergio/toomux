//! A voyage's scene: a little pirate ship sails a dashed sea path to an
//! island where the treasure is. It's pixel art in text: every cell holds a
//! 2 by 3 grid of pixels (Unicode's sextant blocks), drawn in the two
//! colours that fit its six pixels best. The ship's place on the path is the
//! judge's own estimate of how much of the outcome is done; she only reaches
//! the island when its outcome is met.

/// Lines of text.
pub const ROWS: usize = 5;
/// Pixels per cell, across and down.
const CW: usize = 2;
const CH: usize = 3;
/// Pixel rows.
const PH: usize = ROWS * CH;
/// Where the sky meets the sea, in pixel rows.
const HZ: i64 = 9;
/// Narrower than this (in cells) and there's no room for the scene.
pub const MIN_WIDTH: usize = 40;
pub const MAX_WIDTH: usize = 200;
/// The small scene: lines of text, and the narrowest it goes (in cells).

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sea {
    /// Under sail.
    Sailing,
    /// Waiting on you, or on a usage limit: sails furled, at anchor.
    Anchored,
    /// Arrived: the chest is open.
    Landed,
}

pub struct Scene {
    /// 0 to 1, how far along the path.
    pub progress: f64,
    pub sea: Sea,
    /// Moves the waves, the stars and the glint on the gold.
    pub frame: u64,
}

pub type Rgb = [u8; 3];

/// One cell of the scene: a block glyph in front of a background.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
}

const fn rgb(h: u32) -> Rgb {
    [(h >> 16) as u8, (h >> 8) as u8, h as u8]
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

/// A colour along evenly spaced stops, `t` from 0 to 1.
fn ramp(stops: &[Rgb], t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
    let i = (t.floor() as usize).min(stops.len() - 2);
    mix(stops[i], stops[i + 1], t - i as f64)
}

/// A small, stable hash for scattering stars and glints.
fn hash(x: u64, y: u64) -> u64 {
    let mut h = x.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ y.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^ (h >> 32)
}

// Dusk over a warm sea: deep indigo above, coral at the horizon, the sun
// setting just off the island.
const SKY: [Rgb; 4] = [rgb(0x0a0f2e), rgb(0x191b4f), rgb(0x3a2b66), rgb(0x734377)];
const SKY_WARM: [Rgb; 4] = [rgb(0x141848), rgb(0x3a2862), rgb(0x92487a), rgb(0xf28c6c)];
const SEA: [Rgb; 3] = [rgb(0x2f5d90), rgb(0x1a3d6a), rgb(0x0b2140)];
const MOON: Rgb = rgb(0xeef0ff);
const CLOUD: Rgb = rgb(0xa8567c);
const CREST: Rgb = rgb(0x7fb0e0);
const GLOW: Rgb = rgb(0xf6a56a);
const SUN: Rgb = rgb(0xffe08f);
const SUN_EDGE: Rgb = rgb(0xffb36a);
const STAR: Rgb = rgb(0xd4dcff);
const PATH: Rgb = rgb(0xa9cdf0);
const TRAIL: Rgb = rgb(0xf0bf55);
const FOAM: Rgb = rgb(0xeef8ff);
const ROPE: Rgb = rgb(0x5a3a22);

/// Sprite colours; '.' is see-through.
fn palette(c: char) -> Option<Rgb> {
    Some(match c {
        // the ship
        'H' => rgb(0x8a4f2a), // hull
        'h' => rgb(0x57301a), // hull shade
        'g' => rgb(0xe6b04a), // gilt trim
        'o' => rgb(0x24130a), // gun port
        'w' => rgb(0xffc86b), // stern window, lit
        'm' => rgb(0x3e2616), // mast and yard
        'S' => rgb(0xf6ecd4), // sail
        's' => rgb(0xcdbd98), // sail shade
        'K' => rgb(0x141219), // flag
        'W' => rgb(0xf4f1ea), // skull
        'f' => rgb(0xc2b28e), // furled sail
        // the island
        'Y' => rgb(0xf2d69a), // sand
        'y' => rgb(0xcaa66a), // wet sand
        'T' => rgb(0x946236), // trunk
        't' => rgb(0x5f3b1c), // trunk shade
        'P' => rgb(0x4cbc72), // frond
        'p' => rgb(0x2b7a48), // frond shade
        'C' => rgb(0xa2602b), // chest
        'c' => rgb(0x5e3414), // chest shade
        'L' => rgb(0xf5c34e), // gold, lock and hoard
        'l' => rgb(0xfff4b8), // gold glint
        'r' => rgb(0x6f6b78), // rock
        'u' => rgb(0x5a3c1e), // coconut
        _ => return None,
    })
}

// Sails set, bow to the right, the Jolly Roger at the main.
const SHIP: [&str; 12] = [
    "..............m...............",
    "..............mKKKKK..........",
    ".......m......mKWWWK......m...",
    ".......m......mKKWKK......m...",
    ".....sSSSs...sSSSSSSSs...sSSs.",
    "....sSSSSSs..sSSSSSSSSs..sSSSs",
    "....sSSSSSs.sSSSSSSSSSs.sSSSSs",
    ".hhh..sSSs...sSSSSSSSs...sSSsm",
    "hHwHh..m......m...........m.m.",
    "hHHHgggggggggggggggggggggggHH.",
    ".hHHHoHHHoHHHoHHHoHHHoHHHHHh..",
    "..hhHHHHHHHHHHHHHHHHHHHHHHh...",
];

// Sails furled, at anchor.
const SHIP_ANCHORED: [&str; 12] = [
    "..............m...............",
    "..............mKKKKK..........",
    ".......m......mKWWWK......m...",
    ".......m......mKKWKK......m...",
    ".....fffff...fffffffff..ffff..",
    ".......m......m...........m...",
    ".......m......m...........m...",
    ".hhh...m......m...........m..m",
    "hHwHh..m......m...........m.m.",
    "hHHHgggggggggggggggggggggggHH.",
    ".hHHHoHHHoHHHoHHHoHHHoHHHHHh..",
    "..hhHHHHHHHHHHHHHHHHHHHHHHh...",
];

const PALM: [&str; 9] = [
    "......pPPPp....pPPPp......",
    "...pPPPPPPPPppPPPPPPPPp...",
    ".pPPp....pPPPTTPPp...pPPp.",
    "pp......pP..uTTu.Pp.....pp",
    "p............Tt.........p.",
    ".............Tt...........",
    "..............Tt..........",
    "..............tT..........",
    "..............tTt.........",
];

const CHEST: [&str; 5] = [
    ".cCCCCCCCc.",
    "cCCCCCCCCCc",
    "gggggLggggg",
    "CCCCCLCCCCC",
    "cCCCCCCCCCc",
];

const CHEST_OPEN: [&str; 8] = [
    "..cCCCCCc..",
    ".cCCCCCCCc.",
    ".c.......c.",
    "..lLLlLLl..",
    ".LLLLlLLLL.",
    "gggggLggggg",
    "CCCCCCCCCCC",
    "cCCCCCCCCCc",
];

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<Rgb>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![[0, 0, 0]; w * h],
        }
    }
    fn get(&self, x: i64, y: i64) -> Rgb {
        let (x, y) = (
            x.clamp(0, self.w as i64 - 1) as usize,
            y.clamp(0, self.h as i64 - 1) as usize,
        );
        self.px[y * self.w + x]
    }
    fn set(&mut self, x: i64, y: i64, c: Rgb) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.px[y as usize * self.w + x as usize] = c;
        }
    }
    fn tint(&mut self, x: i64, y: i64, c: Rgb, t: f64) {
        let base = self.get(x, y);
        self.set(x, y, mix(base, c, t));
    }
    fn sprite(&mut self, rows: &[&str], x: i64, y: i64) {
        for (dy, row) in rows.iter().enumerate() {
            for (dx, ch) in row.chars().enumerate() {
                if let Some(c) = palette(ch) {
                    self.set(x + dx as i64, y + dy as i64, c);
                }
            }
        }
    }
    /// A one-pixel line.
    fn line(&mut self, (x0, y0): (i64, i64), (x1, y1): (i64, i64), c: Rgb) {
        let n = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        for i in 0..=n {
            let x = x0 + ((x1 - x0) as f64 * i as f64 / n as f64).round() as i64;
            let y = y0 + ((y1 - y0) as f64 * i as f64 / n as f64).round() as i64;
            self.set(x, y, c);
        }
    }
}

/// Pixels are about 1.4 times taller than wide: stretch circles to match.
const ASPECT: f64 = 1.4;

/// The scene as lines of ANSI text, `width` cells wide.
pub fn render(width: usize, scene: &Scene) -> Vec<String> {
    ansi(&cells(width, scene))
}

/// The scene, `ROWS` rows of `width` cells.
pub fn cells(width: usize, scene: &Scene) -> Vec<Vec<Cell>> {
    encode(&draw(width.clamp(MIN_WIDTH, MAX_WIDTH), scene))
}

fn draw(cells: usize, scene: &Scene) -> Canvas {
    let w = cells * CW;
    let wi = w as i64;
    let mut cv = Canvas::new(w, PH);
    let f = scene.frame;
    let phase = f as i64;
    let island_w = 44i64;
    let island_x = wi - island_w - 3;
    let sun_x = island_x as f64 - 7.0;

    // Sky and sea, warmer toward the sun.
    for x in 0..wi {
        let d = (x as f64 - sun_x).abs() / (w as f64 * 0.45);
        let warm = (-d * d).exp();
        let near = (-(d * 2.2) * (d * 2.2)).exp();
        for y in 0..HZ {
            let t = y as f64 / (HZ - 1) as f64;
            cv.set(x, y, mix(ramp(&SKY, t), ramp(&SKY_WARM, t), warm));
        }
        for y in HZ..PH as i64 {
            let depth = (y - HZ) as f64 / (PH as i64 - HZ - 1) as f64;
            cv.set(
                x,
                y,
                mix(ramp(&SEA, depth), GLOW, near * 0.3 * (1.0 - depth).powi(2)),
            );
        }
    }
    // Stars where the sky is dark enough, a few twinkling.
    for x in 0..wi {
        for y in 0..5 {
            let h = hash(x as u64, y as u64 + 11);
            if h.is_multiple_of(29) && (x as f64) < sun_x - 26.0 {
                let bright = if (h / 29 + f).is_multiple_of(5) {
                    1.0
                } else {
                    0.55 - 0.08 * y as f64
                };
                cv.tint(x, y, STAR, bright);
            }
        }
    }
    // A crescent moon.
    let (mx, my) = ((w as f64 * 0.3).round(), 2.5);
    for y in 0..6i64 {
        for x in -5..=5i64 {
            let px = mx + x as f64;
            let inside = |cx: f64, cy: f64, r: f64| {
                (((px - cx) / ASPECT).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt() <= r
            };
            let bite = inside(mx + 1.6, my - 0.6, 2.0);
            if inside(mx, my, 2.3) && !bite {
                cv.set(px as i64, y, MOON);
            } else if inside(mx, my, 3.3) && !bite {
                cv.tint(px as i64, y, MOON, 0.12);
            }
        }
    }
    // The sun, half set, with a haze around it.
    for y in 0..HZ {
        for dx in -12..=12i64 {
            let dy = (y - HZ) as f64 + 0.5;
            let r = ((dx as f64 / ASPECT).powi(2) + dy * dy).sqrt();
            let sx = (sun_x + dx as f64).round() as i64;
            if r <= 4.2 {
                cv.set(sx, y, mix(SUN, SUN_EDGE, (r / 4.2).powi(3)));
            } else if r <= 5.2 {
                cv.set(sx, y, SUN_EDGE);
            } else if r <= 8.0 {
                cv.tint(sx, y, SUN_EDGE, 0.28 * (8.0 - r) / 2.8);
            }
        }
    }
    // Thin clouds, one across the sun.
    for (y, from, len) in [
        (6i64, sun_x as i64 - 14, 24i64),
        (4, sun_x as i64 - 40, 16),
        (7, sun_x as i64 - 64, 12),
        (5, sun_x as i64 + 8, 7),
    ] {
        for x in from..from + len {
            let edge = (x - from).min(from + len - 1 - x);
            cv.tint(x, y, CLOUD, [0.3, 0.55, 0.75][edge.min(2) as usize]);
        }
    }
    // The sun's glitter path: broken dashes, wider nearer.
    for y in HZ..PH as i64 {
        let depth = y - HZ;
        let reach = 5 + depth * 3;
        let mut x = -reach;
        while x <= reach {
            let h = hash((x + 64) as u64 + (phase as u64) * 13, y as u64);
            let len = 1 + (h % 4) as i64;
            if h % 7 < 4 {
                for i in 0..len.min(reach - x + 1) {
                    let sx = (sun_x + (x + i) as f64).round() as i64;
                    let hot = if depth < 2 { SUN } else { GLOW };
                    cv.tint(sx, y, hot, 0.9 - 0.13 * depth as f64);
                }
            }
            x += len + 1 + (h % 3) as i64;
        }
    }
    // Waves roll past, leftward, so the ship seems to make way: short
    // glints of light, scattered, longer nearer.
    let path_y = HZ + 3;
    for y in HZ..PH as i64 {
        let depth = y - HZ;
        if y == path_y {
            continue;
        }
        let drift = phase * (1 + depth / 2);
        for x in 0..wi {
            let u = x + drift;
            let h = hash(u.div_euclid(3) as u64, y as u64);
            let len = 1 + depth / 3;
            if h.is_multiple_of(7 + depth as u64) && u.rem_euclid(3) < len {
                cv.tint(x, y, CREST, 0.5 - 0.05 * depth as f64);
            }
        }
    }

    // Where the ship is: from the harbour to the island's shore.
    let ship_w = SHIP[0].len() as i64;
    let start = 2i64;
    let dock = island_x - ship_w + 6;
    let p = match scene.sea {
        Sea::Landed => 1.0,
        _ => scene.progress.clamp(0.0, 0.95),
    };
    let ship_x = start + ((dock - start) as f64 * p).round() as i64;
    let sy = i64::from(scene.sea == Sea::Sailing && f % 3 == 1);

    // The path: gold where she has sailed, pale dashes still to go.
    for x in 0..island_x + 6 {
        if x < ship_x {
            if x.rem_euclid(4) < 3 {
                cv.tint(x, path_y, TRAIL, 0.8);
            }
        } else if x >= ship_x + ship_w && (x - ship_x - ship_w).rem_euclid(6) < 2 {
            cv.tint(x, path_y, PATH, 0.5);
        }
    }
    // The island: a dome of sand, a palm, and the chest on the beach.
    let (cx, half) = (
        island_x as f64 + island_w as f64 / 2.0,
        island_w as f64 / 2.0,
    );
    let top_at = |x: i64| -> Option<i64> {
        let u = (x as f64 + 0.5 - cx) / half;
        (u.abs() < 1.0).then(|| (PH as f64 - 1.0 - 6.2 * (1.0 - u * u).powf(0.7)).round() as i64)
    };
    for x in island_x..island_x + island_w {
        let Some(t) = top_at(x) else { continue };
        for y in t..PH as i64 {
            let c = if y == PH as i64 - 1 {
                palette('y').unwrap_or(SUN)
            } else if y == t {
                rgb(0xfbe6b4)
            } else if hash(x as u64, y as u64 + 99).is_multiple_of(19) {
                rgb(0xe6c88e)
            } else {
                palette('Y').unwrap_or(SUN)
            };
            cv.set(x, y, c);
        }
    }
    // Surf along the shore.
    for x in island_x - 2..island_x + island_w + 2 {
        if (x + phase).rem_euclid(3) != 0 {
            cv.tint(x, PH as i64 - 1, FOAM, 0.55);
        }
    }
    let palm_x = island_x + 4;
    let palm_base = top_at(palm_x + 15).unwrap_or(HZ);
    cv.sprite(&PALM, palm_x, palm_base - PALM.len() as i64 + 1);
    let chest_x = island_x + 27;
    let chest_base = top_at(chest_x + 5).unwrap_or(HZ) + 2;
    if scene.sea == Sea::Landed {
        cv.sprite(
            &CHEST_OPEN,
            chest_x,
            chest_base - CHEST_OPEN.len() as i64 + 1,
        );
        // Gold catches the light: sparkles that come and go.
        for (i, (dx, dy)) in [(5i64, -10i64), (1, -8), (10, -9), (-2, -5), (13, -6)]
            .into_iter()
            .enumerate()
        {
            if hash(i as u64, f).is_multiple_of(2) {
                let (x, y) = (chest_x + dx, chest_base + dy);
                cv.set(x, y, palette('l').unwrap_or(SUN));
                for (ox, oy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                    cv.tint(x + ox, y + oy, SUN, 0.6);
                }
            }
        }
    } else {
        cv.sprite(&CHEST, chest_x, chest_base - CHEST.len() as i64 + 1);
    }

    let ship = if scene.sea == Sea::Anchored {
        &SHIP_ANCHORED
    } else {
        &SHIP
    };
    // Rigging, behind the sails: stays from the mastheads to the bowsprit.
    cv.line((ship_x + 14, sy), (ship_x + ship_w - 1, sy + 7), ROPE);
    cv.line((ship_x + 7, sy + 2), (ship_x + 14, sy), ROPE);
    cv.line((ship_x + 1, sy + 7), (ship_x + 7, sy + 2), ROPE);
    cv.sprite(ship, ship_x, sy);
    // Her shadow on the water, and a wake while she sails.
    for x in ship_x + 1..ship_x + ship_w - 3 {
        cv.tint(x, HZ + 3 + sy, rgb(0x0a1a33), 0.45);
    }
    if scene.sea == Sea::Sailing {
        for i in 1..14i64 {
            let x = ship_x + 1 - i;
            let fade = 0.95 - 0.065 * i as f64;
            if (i + phase).rem_euclid(3) != 0 || i < 4 {
                cv.tint(x, HZ + 2 + sy, FOAM, fade);
            }
            if i > 3 && (i + phase).rem_euclid(2) == 0 {
                cv.tint(x, HZ + 1 + sy, FOAM, fade * 0.6);
            }
        }
        // A bow wave.
        cv.tint(ship_x + ship_w - 3, HZ + 2 + sy, FOAM, 0.8);
        cv.tint(ship_x + ship_w - 2, HZ + 2 + sy, FOAM, 0.5);
    }
    cv
}

/// Each cell's six pixels, in the two colours that fit them best: the
/// sextant block whose pattern splits them, one colour in front and the
/// other behind.
fn encode(cv: &Canvas) -> Vec<Vec<Cell>> {
    let cells = cv.w / CW;
    (0..cv.h / CH)
        .map(|r| {
            (0..cells)
                .map(|c| {
                    let mut p = [[0u8; 3]; 6];
                    for (i, px) in p.iter_mut().enumerate() {
                        *px = cv.get((c * CW + i % 2) as i64, (r * CH + i / 2) as i64);
                    }
                    let (mask, front, back) = split(&p);
                    match mask {
                        0 => Cell {
                            ch: ' ',
                            fg: back,
                            bg: back,
                        },
                        63 => Cell {
                            ch: ' ',
                            fg: front,
                            bg: front,
                        },
                        m => Cell {
                            ch: glyph(m),
                            fg: front,
                            bg: back,
                        },
                    }
                })
                .collect()
        })
        .collect()
}

/// Cells as ANSI text, with as few colour codes as it can.
fn ansi(rows: &[Vec<Cell>]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            let (mut fg, mut bg): (Option<Rgb>, Option<Rgb>) = (None, None);
            for cell in row {
                let (mut ch, mut front, mut back) = (cell.ch, cell.fg, cell.bg);
                if ch == ' ' {
                    if bg != Some(back) {
                        line.push_str(&sgr(48, back));
                        bg = Some(back);
                    }
                    line.push(' ');
                    continue;
                }
                // Keep the colour already behind when the other way round
                // draws the same: fewer codes.
                if bg == Some(front) {
                    ch = glyph(63 - unglyph(ch));
                    (front, back) = (back, front);
                }
                if bg != Some(back) {
                    line.push_str(&sgr(48, back));
                    bg = Some(back);
                }
                if fg != Some(front) {
                    line.push_str(&sgr(38, front));
                    fg = Some(front);
                }
                line.push(ch);
            }
            line.push_str("\x1b[0m");
            line
        })
        .collect()
}

fn sgr(kind: u8, c: Rgb) -> String {
    format!("\x1b[{kind};2;{};{};{}m", c[0], c[1], c[2])
}

fn dist(a: Rgb, b: Rgb) -> u32 {
    let d = |x: u8, y: u8| (x as i32 - y as i32).pow(2) as u32;
    // Green counts most, blue least, roughly as eyes do.
    3 * d(a[0], b[0]) + 4 * d(a[1], b[1]) + 2 * d(a[2], b[2])
}

fn mean(px: &[Rgb; 6], mask: u8, set: bool) -> Rgb {
    let mut sum = [0u32; 3];
    let mut n = 0;
    for (i, p) in px.iter().enumerate() {
        if ((mask >> i) & 1 == 1) == set {
            for k in 0..3 {
                sum[k] += p[k] as u32;
            }
            n += 1;
        }
    }
    if n == 0 {
        return [0, 0, 0];
    }
    [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
}

/// The split of six pixels into two colours with the least error: (which
/// pixels are in front, the front colour, the back colour).
fn split(px: &[Rgb; 6]) -> (u8, Rgb, Rgb) {
    if px.iter().all(|p| *p == px[0]) {
        return (0, px[0], px[0]);
    }
    let mut best = (u32::MAX, 0u8, px[0], px[0]);
    // Pixel 0 is always behind, so each split is tried once.
    for mask in (2u8..64).step_by(2) {
        let (a, b) = (mean(px, mask, true), mean(px, mask, false));
        let err: u32 = px
            .iter()
            .enumerate()
            .map(|(i, p)| dist(*p, if (mask >> i) & 1 == 1 { a } else { b }))
            .sum();
        if err < best.0 {
            best = (err, mask, a, b);
        }
    }
    (best.1, best.2, best.3)
}

/// The block for a pattern of lit pixels, bit 0 top left, row by row.
fn glyph(mask: u8) -> char {
    match mask {
        0 => ' ',
        21 => '▌',
        42 => '▐',
        63 => '█',
        m => {
            let skip = u32::from(m > 21) + u32::from(m > 42);
            char::from_u32(0x1FB00 + m as u32 - 1 - skip).unwrap_or('█')
        }
    }
}

/// The pattern a block glyph lights.
fn unglyph(ch: char) -> u8 {
    (1..63).find(|&m| glyph(m) == ch).unwrap_or(63)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(line: &str) -> usize {
        let mut n = 0;
        let mut esc = false;
        for ch in line.chars() {
            match (esc, ch) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, _) => n += 1,
            }
        }
        n
    }

    #[test]
    fn sextants_follow_unicode_numbering() {
        assert_eq!(glyph(1), '\u{1FB00}');
        assert_eq!(glyph(20), '\u{1FB13}');
        assert_eq!(glyph(22), '\u{1FB14}');
        assert_eq!(glyph(41), '\u{1FB27}');
        assert_eq!(glyph(43), '\u{1FB28}');
        assert_eq!(glyph(62), '\u{1FB3B}');
    }

    #[test]
    fn two_colours_split_exactly() {
        let (a, b) = ([200, 10, 10], [10, 10, 200]);
        let (mask, front, back) = split(&[a, b, a, b, b, b]);
        assert_eq!((mask, front, back), (0b111010, b, a));
    }

    #[test]
    fn the_scene_fills_its_width_and_the_ship_only_lands_when_met() {
        for w in [MIN_WIDTH, 60, 80, MAX_WIDTH] {
            for sea in [Sea::Sailing, Sea::Anchored, Sea::Landed] {
                let lines = render(
                    w,
                    &Scene {
                        progress: 0.5,
                        sea,
                        frame: 3,
                    },
                );
                assert_eq!(lines.len(), ROWS);
                assert!(lines.iter().all(|l| cells(l) == w), "{w} {sea:?}");
            }
        }
        // All the way there by the estimate, but not met: still at sea.
        let near = render(
            80,
            &Scene {
                progress: 1.0,
                sea: Sea::Sailing,
                frame: 0,
            },
        );
        let landed = render(
            80,
            &Scene {
                progress: 1.0,
                sea: Sea::Landed,
                frame: 0,
            },
        );
        assert_ne!(near, landed);
    }
}
