//! Terminal color-depth detection and RGB down-conversion (spec 0111).
//!
//! The TUI palette is authored in 24-bit RGB, which ratatui emits as
//! `ESC[38;2;R;G;Bm`. Terminals that only speak 256 colors do not merely
//! approximate that sequence — Apple Terminal, the terminal that ships on
//! every Mac, drops the `38;2` introducer and re-reads the channel values as
//! independent SGR parameters. A channel that happens to land on a color or
//! attribute code then hijacks the cell: a slot like `(64, 82, 104)` paints a
//! bright-blue background (SGR 104), `(92, 103, 118)` paints green-on-yellow
//! (SGR 92 + 103), and the `2` in every introducer turns on faint. The result
//! is an unreadable frame, not a slightly-off one.
//!
//! So instead of trusting every terminal with truecolor, we detect the depth
//! once at startup and quantize on the way out (see [`QuantizingBackend`]),
//! which covers the theme palette, ad-hoc colors in the render code, and the
//! colors child harnesses emit into their PTY panes alike.

use ratatui::style::Color;

/// How many colors the attached terminal can actually be trusted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorDepth {
    /// 24-bit `38;2;R;G;B` — the palette goes out as authored.
    #[default]
    TrueColor,
    /// 256-color `38;5;N`, quantized to the 6×6×6 cube + grayscale ramp.
    Ansi256,
    /// The 16 basic SGR colors, whose exact hues the user's profile owns.
    Ansi16,
}

/// Environment variable that overrides detection outright, for terminals that
/// misreport (a truecolor terminal behind an old `screen`, say) and for tests.
pub const COLOR_ENV: &str = "CONSTRUCT_COLOR";

/// Detect the depth from the process environment.
pub fn detect() -> ColorDepth {
    detect_from(|key| std::env::var(key).ok())
}

/// Detection against an arbitrary environment, so the rules stay testable.
///
/// Truecolor is the default: nearly every modern terminal supports it, and
/// plenty of them advertise nothing at all (notably over SSH, where
/// `COLORTERM` is routinely dropped). We only step down when the environment
/// positively identifies a terminal known to mishandle `38;2`.
///
/// Those known-bad identifications outrank a `COLORTERM=truecolor` claim.
/// `COLORTERM` is a hint anyone's shell profile can export — and frequently
/// does, cargo-culted from a config written for a different terminal — whereas
/// `TERM_PROGRAM=Apple_Terminal` is set by the terminal itself and is a fact
/// about a terminal that has no 24-bit support to claim. `CONSTRUCT_COLOR`
/// remains the escape hatch if we ever call it wrong.
pub fn detect_from(env: impl Fn(&str) -> Option<String>) -> ColorDepth {
    if let Some(forced) = env(COLOR_ENV).and_then(|v| parse_depth(&v)) {
        return forced;
    }
    let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
    if term == "dumb" || term == "linux" || term == "vt100" || term == "ansi" {
        return ColorDepth::Ansi16;
    }
    // Apple Terminal is 256-color only, full stop. tmux identifies itself as
    // TERM_PROGRAM=tmux rather than passing its host terminal through, so a
    // truecolor terminal behind tmux isn't caught here.
    if env("TERM_PROGRAM").as_deref() == Some("Apple_Terminal") {
        return ColorDepth::Ansi256;
    }
    let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorDepth::TrueColor;
    }
    // `*-direct` terminfo entries are the standard way to say "direct color".
    if term.contains("direct") {
        return ColorDepth::TrueColor;
    }
    // GNU screen mangles `38;2` much the way Apple Terminal does unless it was
    // built/configured for truecolor — in which case it says so via COLORTERM
    // above, so reaching here means assuming it can't.
    if term.starts_with("screen") {
        return ColorDepth::Ansi256;
    }
    ColorDepth::TrueColor
}

/// Parse a `CONSTRUCT_COLOR` value. Unknown values fall through to detection.
pub fn parse_depth(s: &str) -> Option<ColorDepth> {
    match s.trim().to_ascii_lowercase().as_str() {
        "truecolor" | "24bit" | "24" | "16m" | "rgb" => Some(ColorDepth::TrueColor),
        "256" | "8bit" | "256color" => Some(ColorDepth::Ansi256),
        "16" | "4bit" | "ansi" | "basic" => Some(ColorDepth::Ansi16),
        _ => None,
    }
}

/// Down-convert one color for `depth`. Anything that is already expressible —
/// named ANSI colors, and indexed colors at 256-color depth — passes through
/// untouched, so a user who pins `indexed:N` in `theme.toml` keeps exactly
/// that index.
pub fn quantize(color: Color, depth: ColorDepth) -> Color {
    match depth {
        ColorDepth::TrueColor => color,
        ColorDepth::Ansi256 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(nearest_indexed(r, g, b)),
            other => other,
        },
        ColorDepth::Ansi16 => match color {
            Color::Rgb(r, g, b) => nearest_basic(r, g, b),
            Color::Indexed(idx) if idx >= 16 => {
                let (r, g, b) = indexed_rgb(idx);
                nearest_basic(r, g, b)
            }
            other => other,
        },
    }
}

/// The six levels the xterm 6×6×6 color cube samples each channel at.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Absolute chroma (max channel − min channel) a color needs before it counts
/// as *hued*. Below it, the color is a neutral that merely isn't perfectly
/// balanced, and the grayscale ramp — far finer than the cube's neutrals — is
/// the better answer.
const HUED_CHROMA: i32 = 24;

/// Chroma *relative to* the brightest channel that a color needs before it
/// counts as hued. Both floors have to be met, because either one alone
/// misjudges an end of the range: a dark near-neutral like `(12, 18, 27)` has
/// little chroma but a lot of it relative to its brightness, while a pale tint
/// like `(190, 255, 205)` has the opposite. Neither is really about its hue —
/// the first is a dark gray, the second is off-white — so neither should be
/// forced away from the neutrals.
const HUED_SATURATION: f32 = 0.35;

/// Weight on the lightness term relative to the hue/chroma term. Above 1
/// because two candidates can match a hue equally well while sitting at very
/// different lightnesses, and lightness is what keeps text legible against its
/// background.
const LIGHTNESS_WEIGHT: f32 = 2.0;

/// Nearest 256-color index: the best of the color cube (16..232) and the
/// grayscale ramp (232..256) under [`perceptual_cost`].
///
/// Indices 0..16 are never returned: their hues belong to the user's terminal
/// profile, so a palette slot mapped onto one would drift with the profile
/// instead of staying put.
fn nearest_indexed(r: u8, g: u8, b: u8) -> u8 {
    best_candidate((r, g, b), indexed_candidates(), |_| true).unwrap_or(16)
}

/// Every candidate the 256-color depth may answer with, as `(index, rgb)`.
fn indexed_candidates() -> impl Iterator<Item = (u8, (u8, u8, u8))> {
    (16u8..=255).map(|idx| (idx, indexed_rgb(idx)))
}

/// The candidate with the lowest [`perceptual_cost`], among those `accept`
/// allows. A hued source never accepts a gray: losing the hue loses what the
/// palette was using the color to say.
fn best_candidate<T>(
    src: (u8, u8, u8),
    candidates: impl Iterator<Item = (T, (u8, u8, u8))>,
    accept: impl Fn((u8, u8, u8)) -> bool,
) -> Option<T> {
    let hued = is_hued(src);
    let mut best: Option<(f32, T)> = None;
    for (item, rgb) in candidates {
        if (hued && is_gray(rgb)) || !accept(rgb) {
            continue;
        }
        let cost = perceptual_cost(src, rgb);
        if best.as_ref().is_none_or(|(best_cost, _)| cost < *best_cost) {
            best = Some((cost, item));
        }
    }
    best.map(|(_, item)| item)
}

/// Nearest of the 16 basic colors, returned as ratatui's named variants — which
/// go out as `38;5;0`..`38;5;15`, i.e. the user's own profile entries. The RGB
/// values compared against are the conventional defaults for those slots; a
/// profile that redefines them shifts the rendered hue, which is exactly the
/// bargain at this depth.
fn nearest_basic(r: u8, g: u8, b: u8) -> Color {
    best_candidate((r, g, b), basic_candidates(), |_| true).unwrap_or(Color::Reset)
}

/// The 16 basic colors as candidates, paired with the conventional RGB of each.
fn basic_candidates() -> impl Iterator<Item = (Color, (u8, u8, u8))> {
    (0u8..16).map(|idx| (basic_color(idx), indexed_rgb(idx)))
}

/// The ratatui named color for one of the low 16 indices.
fn basic_color(idx: u8) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// How wrong `candidate` is as a stand-in for `src`, as lightness error plus
/// hue/saturation error rather than plain RGB distance.
///
/// Plain RGB distance is what made a dark green fill land on a dark gray: the
/// ramp's 10-step neutrals sit closer, in raw distance, to a dark hued color
/// than anything in the cube's sparse dark corner does, and it also let a dim
/// green land on a teal because green and blue error trade off freely. Scoring
/// lightness and color-direction separately keeps the hue that carries the
/// palette's meaning.
fn perceptual_cost(src: (u8, u8, u8), candidate: (u8, u8, u8)) -> f32 {
    let d_light = luma(src) - luma(candidate);
    let (sr, sg, sb) = chroma_vector(src);
    let (cr, cg, cb) = chroma_vector(candidate);
    let d_chroma = (sr - cr).powi(2) + (sg - cg).powi(2) + (sb - cb).powi(2);
    LIGHTNESS_WEIGHT * d_light.powi(2) + d_chroma
}

/// How closely two colors have to point the same way out of neutral to count
/// as the same hue, as a cosine: about 18 degrees. Used only when repairing a
/// collision, where the whole point is to move a slot in lightness while
/// leaving its hue alone.
const SAME_HUE_ALIGNMENT: f32 = 0.95;

/// Cosine of the angle between two colors' chroma vectors — 1.0 for the same
/// hue. A neutral has no hue to compare, so it aligns with nothing but another
/// neutral.
fn hue_alignment(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let (av, bv) = (chroma_vector(a), chroma_vector(b));
    let (a_mag, b_mag) = (
        (av.0 * av.0 + av.1 * av.1 + av.2 * av.2).sqrt(),
        (bv.0 * bv.0 + bv.1 * bv.1 + bv.2 * bv.2).sqrt(),
    );
    if a_mag < f32::EPSILON || b_mag < f32::EPSILON {
        return if a_mag < f32::EPSILON && b_mag < f32::EPSILON {
            1.0
        } else {
            -1.0
        };
    }
    ((av.0 * bv.0 + av.1 * bv.1 + av.2 * bv.2) / (a_mag * b_mag)).clamp(-1.0, 1.0)
}

/// Perceived lightness, Rec. 601 weights.
fn luma((r, g, b): (u8, u8, u8)) -> f32 {
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
}

/// The color with its lightness removed, i.e. what direction (and how far) it
/// points away from neutral. Comparing these keeps hue *and* saturation
/// together in one term.
fn chroma_vector(rgb: (u8, u8, u8)) -> (f32, f32, f32) {
    let l = luma(rgb);
    (rgb.0 as f32 - l, rgb.1 as f32 - l, rgb.2 as f32 - l)
}

/// Whether a color's hue is the point of it, and so must not be answered with
/// a gray. See [`HUED_CHROMA`] and [`HUED_SATURATION`].
fn is_hued((r, g, b): (u8, u8, u8)) -> bool {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    let chroma = max - min;
    chroma >= HUED_CHROMA && max > 0 && chroma as f32 / max as f32 >= HUED_SATURATION
}

fn is_gray((r, g, b): (u8, u8, u8)) -> bool {
    r == g && g == b
}

/// RGB of a 256-color index (cube + grayscale ramp; 0..16 use the terminal's
/// own profile, approximated with the conventional xterm defaults).
fn indexed_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => {
            const LOW: [(u8, u8, u8); 16] = [
                (0, 0, 0),
                (170, 0, 0),
                (0, 170, 0),
                (170, 85, 0),
                (0, 0, 170),
                (170, 0, 170),
                (0, 170, 170),
                (170, 170, 170),
                (85, 85, 85),
                (255, 85, 85),
                (85, 255, 85),
                (255, 255, 85),
                (85, 85, 255),
                (255, 85, 255),
                (85, 255, 255),
                (255, 255, 255),
            ];
            LOW[idx as usize]
        }
        16..=231 => {
            let i = idx - 16;
            (
                CUBE_LEVELS[(i / 36) as usize],
                CUBE_LEVELS[((i % 36) / 6) as usize],
                CUBE_LEVELS[(i % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + 10 * (idx - 232);
            (level, level, level)
        }
    }
}

/// A [`quantize`] with a memo, so the 240-candidate search runs once per
/// distinct color rather than once per cell per frame. A frame draws thousands
/// of cells from a few dozen distinct colors, so the cache is tiny and hits
/// almost always.
///
/// The depth is fixed at construction and part of the identity of the cache:
/// keying entries by color alone would serve a 256-color answer to a 16-color
/// terminal the moment one process used both.
pub struct Quantizer {
    depth: ColorDepth,
    cache: std::collections::HashMap<(u8, u8, u8), Color>,
}

impl Quantizer {
    pub fn new(depth: ColorDepth) -> Self {
        Self {
            depth,
            cache: std::collections::HashMap::new(),
        }
    }

    pub fn map(&mut self, color: Color) -> Color {
        if self.depth == ColorDepth::TrueColor {
            return color;
        }
        let depth = self.depth;
        match color {
            Color::Rgb(r, g, b) => *self
                .cache
                .entry((r, g, b))
                .or_insert_with(|| quantize(color, depth)),
            other => quantize(other, depth),
        }
    }

    /// Pull apart palette slots that the theme draws directly against each
    /// other but that quantization has collapsed onto one entry.
    ///
    /// Quantizing colors one at a time cannot prevent this: two colors the
    /// theme keeps distinct may simply have no two entries near them, and each
    /// lookup is individually correct. Contrast, though, is a property of a
    /// *pair* — so pairs that matter are named by the theme and repaired here,
    /// by moving the lighter slot further from the darker one. Everything else
    /// is left to collapse where the palette has nothing better; a flattened
    /// gradient is cosmetic, whereas a fill that merges into what it is drawn
    /// on top of reads as missing.
    pub fn keep_apart(&mut self, pairs: &[(Color, Color)]) {
        if self.depth == ColorDepth::TrueColor {
            return;
        }
        for (first, second) in pairs {
            let (Color::Rgb(..), Color::Rgb(..)) = (first, second) else {
                continue;
            };
            if self.map(*first) != self.map(*second) {
                continue;
            }
            let (Some(first_rgb), Some(second_rgb)) = (rgb_of(*first), rgb_of(*second)) else {
                continue;
            };
            // Move whichever slot is already the lighter one further up, so the
            // pair keeps the lightness order the theme gave it. When the
            // palette has nothing lighter, push the darker slot down instead.
            let (lighter, darker) = if luma(first_rgb) >= luma(second_rgb) {
                (first_rgb, second_rgb)
            } else {
                (second_rgb, first_rgb)
            };
            let Some(collision) = rgb_of(self.map(Color::Rgb(lighter.0, lighter.1, lighter.2)))
            else {
                continue;
            };
            if let Some(moved) = self.nearest_beyond(lighter, luma(collision), Side::Lighter) {
                self.cache.insert(lighter, moved);
            } else if let Some(moved) = self.nearest_beyond(darker, luma(collision), Side::Darker) {
                self.cache.insert(darker, moved);
            }
        }
    }

    /// The best replacement for `src` among candidates strictly beyond
    /// `boundary` in lightness, on the given side.
    ///
    /// Tried twice: first among entries of the same hue, so a repaired slot
    /// still reads as the color the theme asked for, then among anything
    /// distinguishable. The second pass matters at 16 colors, which frequently
    /// has no second entry of a given hue — there, a track that turns cyan
    /// still does its job, and an invisible one doesn't.
    fn nearest_beyond(&self, src: (u8, u8, u8), boundary: f32, side: Side) -> Option<Color> {
        self.search_beyond(src, boundary, side, SAME_HUE_ALIGNMENT)
            .or_else(|| self.search_beyond(src, boundary, side, -1.0))
    }

    fn search_beyond(
        &self,
        src: (u8, u8, u8),
        boundary: f32,
        side: Side,
        min_alignment: f32,
    ) -> Option<Color> {
        let accept = |rgb: (u8, u8, u8)| {
            let beyond = match side {
                Side::Lighter => luma(rgb) > boundary,
                Side::Darker => luma(rgb) < boundary,
            };
            beyond && hue_alignment(src, rgb) >= min_alignment
        };
        match self.depth {
            ColorDepth::TrueColor => None,
            ColorDepth::Ansi256 => {
                best_candidate(src, indexed_candidates(), accept).map(Color::Indexed)
            }
            ColorDepth::Ansi16 => best_candidate(src, basic_candidates(), accept),
        }
    }
}

/// Which way to move a slot that collided with another.
#[derive(Clone, Copy)]
enum Side {
    Lighter,
    Darker,
}

/// The RGB a color stands for, for the palette entries we ourselves emit.
/// `Reset` and the terminal-owned named colors have no fixed RGB.
fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(idx) => Some(indexed_rgb(idx)),
        Color::Black => Some(indexed_rgb(0)),
        Color::Red => Some(indexed_rgb(1)),
        Color::Green => Some(indexed_rgb(2)),
        Color::Yellow => Some(indexed_rgb(3)),
        Color::Blue => Some(indexed_rgb(4)),
        Color::Magenta => Some(indexed_rgb(5)),
        Color::Cyan => Some(indexed_rgb(6)),
        Color::Gray => Some(indexed_rgb(7)),
        Color::DarkGray => Some(indexed_rgb(8)),
        Color::LightRed => Some(indexed_rgb(9)),
        Color::LightGreen => Some(indexed_rgb(10)),
        Color::LightYellow => Some(indexed_rgb(11)),
        Color::LightBlue => Some(indexed_rgb(12)),
        Color::LightMagenta => Some(indexed_rgb(13)),
        Color::LightCyan => Some(indexed_rgb(14)),
        Color::White => Some(indexed_rgb(15)),
        Color::Reset => None,
    }
}

/// A [`ratatui::backend::Backend`] that quantizes every cell color on its way
/// to the wrapped backend.
///
/// This is deliberately the *last* stop before the terminal rather than a
/// transform on the theme: the frame also carries ad-hoc colors from the
/// render code (rain fades, blended highlights) and colors that child
/// harnesses emitted into their own PTY panes. Quantizing here catches all
/// three with one rule. At [`ColorDepth::TrueColor`] the cells are handed
/// through with no copy at all.
pub struct QuantizingBackend<W: std::io::Write> {
    inner: ratatui::backend::CrosstermBackend<W>,
    depth: ColorDepth,
    /// Reused across frames so a downgraded terminal doesn't allocate a
    /// per-cell vector on every draw.
    scratch: Vec<(u16, u16, ratatui::buffer::Cell)>,
    quantizer: Quantizer,
    /// Theme revision the current contrast repairs were computed for.
    palette_revision: Option<u64>,
}

impl<W: std::io::Write> QuantizingBackend<W> {
    pub fn new(writer: W, depth: ColorDepth) -> Self {
        Self {
            inner: ratatui::backend::CrosstermBackend::new(writer),
            depth,
            scratch: Vec::new(),
            quantizer: Quantizer::new(depth),
            palette_revision: None,
        }
    }
}

impl<W: std::io::Write> QuantizingBackend<W> {
    /// Recompute contrast repairs if the palette has changed since the last
    /// call. `pairs` is only consulted when the revision actually moved, so the
    /// steady-state cost is one integer comparison per frame.
    pub fn sync_palette(&mut self, revision: u64, pairs: impl FnOnce() -> Vec<(Color, Color)>) {
        if self.depth == ColorDepth::TrueColor || self.palette_revision == Some(revision) {
            return;
        }
        self.palette_revision = Some(revision);
        // A new palette invalidates every earlier answer, repaired or not.
        self.quantizer = Quantizer::new(self.depth);
        self.quantizer.keep_apart(&pairs());
    }
}

impl<W: std::io::Write> std::io::Write for QuantizingBackend<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: std::io::Write> ratatui::backend::Backend for QuantizingBackend<W> {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        if self.depth == ColorDepth::TrueColor {
            return self.inner.draw(content);
        }
        // `mem::take` so the scratch buffer's allocation survives the
        // borrow of `self.inner` below.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        for (x, y, cell) in content {
            let mut cell = cell.clone();
            cell.fg = self.quantizer.map(cell.fg);
            cell.bg = self.quantizer.map(cell.bg);
            cell.underline_color = self.quantizer.map(cell.underline_color);
            scratch.push((x, y, cell));
        }
        let result = self
            .inner
            .draw(scratch.iter().map(|(x, y, cell)| (*x, *y, cell)));
        self.scratch = scratch;
        result
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::Backend;
    use ratatui::buffer::Cell;

    /// Writer that keeps the bytes a backend emitted readable after the
    /// backend has taken ownership of it (ratatui's own `writer()` accessor is
    /// still unstable).
    #[derive(Clone, Default)]
    struct SharedWriter(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("utf8")
        }
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn apple_terminal_is_256_color() {
        assert_eq!(
            detect_from(env_of(&[
                ("TERM", "xterm-256color"),
                ("TERM_PROGRAM", "Apple_Terminal"),
            ])),
            ColorDepth::Ansi256
        );
    }

    #[test]
    fn known_256_color_terminal_outranks_a_colorterm_claim() {
        // Plenty of shell profiles export COLORTERM=truecolor unconditionally;
        // Apple Terminal still cannot render it, so the terminal's own
        // identification has to win.
        assert_eq!(
            detect_from(env_of(&[
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-256color"),
                ("TERM_PROGRAM", "Apple_Terminal"),
            ])),
            ColorDepth::Ansi256
        );
        // tmux identifies itself rather than its host terminal, so a truecolor
        // terminal behind tmux is not mistaken for Apple Terminal.
        assert_eq!(
            detect_from(env_of(&[
                ("COLORTERM", "truecolor"),
                ("TERM", "tmux-256color"),
                ("TERM_PROGRAM", "tmux"),
            ])),
            ColorDepth::TrueColor
        );
    }

    #[test]
    fn unknown_terminal_defaults_to_truecolor() {
        // COLORTERM is routinely lost over SSH; assuming 256 there would
        // downgrade the majority of capable terminals.
        assert_eq!(
            detect_from(env_of(&[("TERM", "xterm-256color")])),
            ColorDepth::TrueColor
        );
        assert_eq!(detect_from(env_of(&[])), ColorDepth::TrueColor);
    }

    #[test]
    fn direct_terminfo_and_legacy_terms() {
        assert_eq!(
            detect_from(env_of(&[("TERM", "xterm-direct")])),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_from(env_of(&[("TERM", "screen.xterm-256color")])),
            ColorDepth::Ansi256
        );
        // ...unless that screen says it does handle 24-bit color.
        assert_eq!(
            detect_from(env_of(&[
                ("TERM", "screen.xterm-256color"),
                ("COLORTERM", "truecolor"),
            ])),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_from(env_of(&[("TERM", "linux")])),
            ColorDepth::Ansi16
        );
        assert_eq!(detect_from(env_of(&[("TERM", "dumb")])), ColorDepth::Ansi16);
    }

    #[test]
    fn env_override_beats_everything() {
        assert_eq!(
            detect_from(env_of(&[
                (COLOR_ENV, "256"),
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-direct"),
            ])),
            ColorDepth::Ansi256
        );
        assert_eq!(
            detect_from(env_of(&[
                (COLOR_ENV, "truecolor"),
                ("TERM_PROGRAM", "Apple_Terminal"),
            ])),
            ColorDepth::TrueColor
        );
        // Garbage in the override falls back to detection rather than erroring.
        assert_eq!(
            detect_from(env_of(&[
                (COLOR_ENV, "chartreuse"),
                ("TERM_PROGRAM", "Apple_Terminal"),
            ])),
            ColorDepth::Ansi256
        );
    }

    #[test]
    fn truecolor_passes_rgb_through() {
        let c = Color::Rgb(64, 82, 104);
        assert_eq!(quantize(c, ColorDepth::TrueColor), c);
    }

    #[test]
    fn cube_corners_and_levels_map_exactly() {
        // Pure black/white and an exact cube level round-trip to their index.
        assert_eq!(
            quantize(Color::Rgb(0, 0, 0), ColorDepth::Ansi256),
            Color::Indexed(16)
        );
        assert_eq!(
            quantize(Color::Rgb(255, 255, 255), ColorDepth::Ansi256),
            Color::Indexed(231)
        );
        assert_eq!(
            quantize(Color::Rgb(0, 175, 0), ColorDepth::Ansi256),
            Color::Indexed(16 + 6 * 3)
        );
    }

    #[test]
    fn near_gray_prefers_the_grayscale_ramp() {
        // (18,18,18) is on the ramp; the cube's nearest neighbor is much
        // further away, so the ramp must win.
        assert_eq!(
            quantize(Color::Rgb(18, 18, 18), ColorDepth::Ansi256),
            Color::Indexed(233)
        );
        // The light theme's near-white frame background: the cube's white
        // corner is closer than the top of the ramp, so it lands there.
        assert_eq!(
            quantize(Color::Rgb(246, 248, 251), ColorDepth::Ansi256),
            Color::Indexed(231)
        );
    }

    /// Reported on #960: the matrix modeline/minibuffer fill vanished at 256
    /// colors. Its slot is a very dark green, and raw RGB distance answered
    /// with a dark *gray* from the ramp — indistinguishable from the frame
    /// behind it, so the bar read as missing rather than as a different green.
    #[test]
    fn dark_hued_fills_keep_their_hue_instead_of_collapsing_to_gray() {
        let matrix_modeline_bg = Color::Rgb(8, 46, 24);
        let Color::Indexed(idx) = quantize(matrix_modeline_bg, ColorDepth::Ansi256) else {
            panic!("expected an indexed color");
        };
        let (r, g, b) = indexed_rgb(idx);
        assert!(!is_gray((r, g, b)), "{idx} is the gray {r},{g},{b}");
        assert!(g > r && g > b, "expected a green, got {idx} = {r},{g},{b}");
        // And at 16 colors, where the only alternative to a hue is black.
        assert_eq!(
            quantize(matrix_modeline_bg, ColorDepth::Ansi16),
            Color::Green
        );
    }

    #[test]
    fn hues_survive_the_cubes_sparse_dark_corner() {
        // The matrix `dim`/`inactive_highlight_bg` greens are dark enough that
        // channelwise rounding used to swing them to a teal (equal G and B).
        for (r, g, b) in [(32, 112, 58), (28, 78, 42), (24, 96, 48), (18, 92, 42)] {
            let Color::Indexed(idx) = quantize(Color::Rgb(r, g, b), ColorDepth::Ansi256) else {
                panic!("expected an indexed color");
            };
            let got = indexed_rgb(idx);
            assert!(
                got.1 > got.0 && got.1 > got.2,
                "({r},{g},{b}) -> {idx} = {got:?}, which isn't green-dominant"
            );
        }
    }

    #[test]
    fn near_neutrals_still_use_the_fine_grayscale_ramp() {
        // Slightly-blue dark neutrals (the dark_ui frame background) are better
        // served by the ramp's 10-unit steps than by a saturated cube entry.
        for (r, g, b) in [(12, 18, 27), (29, 38, 52), (43, 52, 66)] {
            match quantize(Color::Rgb(r, g, b), ColorDepth::Ansi256) {
                Color::Indexed(idx) => assert!(
                    idx >= 232 || is_gray(indexed_rgb(idx)),
                    "({r},{g},{b}) -> {idx} = {:?}, expected a neutral",
                    indexed_rgb(idx)
                ),
                other => panic!("expected an indexed color, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_memo_agrees_with_the_uncached_search_at_every_depth() {
        for depth in [
            ColorDepth::TrueColor,
            ColorDepth::Ansi256,
            ColorDepth::Ansi16,
        ] {
            let mut q = Quantizer::new(depth);
            for color in [
                Color::Rgb(8, 46, 24),
                Color::Rgb(190, 255, 205),
                Color::Indexed(34),
                Color::Green,
                Color::Reset,
            ] {
                // Twice, so the second call is served from the cache.
                assert_eq!(q.map(color), quantize(color, depth), "{depth:?} {color:?}");
                assert_eq!(q.map(color), quantize(color, depth), "{depth:?} {color:?}");
            }
        }
    }

    /// One quantizer per depth, and no cross-talk between them: the same RGB
    /// has to answer differently for a 256-color and a 16-color terminal.
    #[test]
    fn separate_quantizers_do_not_share_answers() {
        let dark_green = Color::Rgb(8, 46, 24);
        let mut wide = Quantizer::new(ColorDepth::Ansi256);
        let mut narrow = Quantizer::new(ColorDepth::Ansi16);
        let wide_answer = wide.map(dark_green);
        assert_eq!(narrow.map(dark_green), Color::Green);
        assert_ne!(wide_answer, Color::Green);
        assert_eq!(wide.map(dark_green), wide_answer);
    }

    /// Reported against #960: on the matrix theme at 256 colors the context
    /// gauge lost the background on its *remaining* stretch. `modeline_bg`
    /// (a very dark green) and `dim` (a mid green) both quantized to index 22,
    /// so the gauge's track became the same color as the bar it sits on.
    #[test]
    fn the_context_gauge_keeps_a_visible_track_over_the_modeline() {
        let theme = crate::theme::Theme::dark();
        // Individually, the two slots really do collapse together — no
        // per-color rule can prevent that, which is why the pair is repaired.
        assert_eq!(
            quantize(theme.modeline_bg, ColorDepth::Ansi256),
            quantize(theme.dim, ColorDepth::Ansi256),
        );

        let mut q = Quantizer::new(ColorDepth::Ansi256);
        q.keep_apart(&theme.contrast_pairs());
        let bar = q.map(theme.modeline_bg);
        let track = q.map(theme.dim);
        let filled = q.map(theme.modeline_fg);
        assert_ne!(bar, track, "the gauge's track merged into the bar");
        assert_ne!(filled, track);
        assert_ne!(filled, bar);
        // The repair keeps the theme's lightness order: the track is the
        // lighter of the two, before and after.
        let (bar_rgb, track_rgb) = (rgb_of(bar).unwrap(), rgb_of(track).unwrap());
        assert!(
            luma(track_rgb) > luma(bar_rgb),
            "track {track_rgb:?} is not lighter than bar {bar_rgb:?}"
        );
        // ...and it stays a green rather than being shoved onto some other hue.
        assert!(track_rgb.1 > track_rgb.0 && track_rgb.1 > track_rgb.2);
    }

    #[test]
    fn every_shipped_theme_keeps_its_contrast_pairs_apart() {
        let themes = [
            ("matrix dark", crate::theme::Theme::dark()),
            ("matrix light", crate::theme::Theme::light()),
            ("basic dark", crate::theme::Theme::basic_dark()),
            ("basic light", crate::theme::Theme::basic_light()),
            ("dark_ui", crate::theme::Theme::dark_ui()),
            ("light_ui", crate::theme::Theme::light_ui()),
        ];
        for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            for (name, theme) in &themes {
                let pairs = theme.contrast_pairs();
                let mut q = Quantizer::new(depth);
                q.keep_apart(&pairs);
                for (first, second) in &pairs {
                    assert_ne!(
                        q.map(*first),
                        q.map(*second),
                        "{name} at {depth:?}: {first:?} and {second:?} collapsed together"
                    );
                }
            }
        }
    }

    #[test]
    fn keeping_pairs_apart_is_a_no_op_without_a_collision() {
        let theme = crate::theme::Theme::dark();
        let mut plain = Quantizer::new(ColorDepth::Ansi256);
        let mut repaired = Quantizer::new(ColorDepth::Ansi256);
        repaired.keep_apart(&theme.contrast_pairs());
        // Slots outside a repaired pair answer exactly as before.
        for color in [theme.accent, theme.danger, theme.warning, theme.success] {
            assert_eq!(plain.map(color), repaired.map(color), "{color:?}");
        }
    }

    #[test]
    fn a_truecolor_terminal_is_never_repaired() {
        let theme = crate::theme::Theme::dark();
        let mut q = Quantizer::new(ColorDepth::TrueColor);
        q.keep_apart(&theme.contrast_pairs());
        assert_eq!(q.map(theme.dim), theme.dim);
        assert_eq!(q.map(theme.modeline_bg), theme.modeline_bg);
    }

    #[test]
    fn quantized_colors_never_use_the_profile_owned_low_16() {
        for (r, g, b) in [
            (190, 255, 205),
            (32, 112, 58),
            (64, 82, 104),
            (92, 103, 118),
            (8, 46, 24),
            (255, 95, 90),
            (1, 1, 1),
        ] {
            match quantize(Color::Rgb(r, g, b), ColorDepth::Ansi256) {
                Color::Indexed(idx) => assert!(idx >= 16, "({r},{g},{b}) -> {idx}"),
                other => panic!("expected an indexed color, got {other:?}"),
            }
        }
    }

    #[test]
    fn ansi16_maps_to_named_colors_and_folds_indexed_down() {
        // The matrix accent green keeps its hue...
        assert_eq!(
            quantize(Color::Rgb(57, 255, 136), ColorDepth::Ansi16),
            Color::LightGreen
        );
        // ...while the palette's near-white body text reads as plain white:
        // a pale tint is off-white, not a green, so it is not held away from
        // the neutrals the way a saturated color is.
        assert_eq!(
            quantize(Color::Rgb(190, 255, 205), ColorDepth::Ansi16),
            Color::White
        );
        assert_eq!(
            quantize(Color::Rgb(10, 10, 200), ColorDepth::Ansi16),
            Color::Blue
        );
        // 256-color indices have to fold down too, or a `theme.toml`
        // `indexed:N` override would go out unrenderable.
        assert_eq!(
            quantize(Color::Indexed(231), ColorDepth::Ansi16),
            Color::White
        );
        // ...but the low 16 are already renderable and stay put.
        assert_eq!(
            quantize(Color::Indexed(3), ColorDepth::Ansi16),
            Color::Indexed(3)
        );
    }

    #[test]
    fn named_and_reset_colors_are_left_alone() {
        for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            assert_eq!(quantize(Color::Reset, depth), Color::Reset);
            assert_eq!(quantize(Color::Green, depth), Color::Green);
        }
        assert_eq!(
            quantize(Color::Indexed(200), ColorDepth::Ansi256),
            Color::Indexed(200)
        );
    }

    /// The regression this module exists for: on a 256-color terminal no
    /// truecolor introducer may reach the wire, because the terminal re-reads
    /// its channel values as SGR codes.
    #[test]
    fn no_truecolor_sequences_reach_a_downgraded_terminal() {
        for depth in [ColorDepth::Ansi256, ColorDepth::Ansi16] {
            let sink = SharedWriter::default();
            let mut backend = QuantizingBackend::new(sink.clone(), depth);
            let mut cell = Cell::new("x");
            // The exact light-theme slots from issue #949.
            cell.fg = Color::Rgb(92, 103, 118);
            cell.bg = Color::Rgb(64, 82, 104);
            let cells = [(0u16, 0u16, cell)];
            backend
                .draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
                .unwrap();
            std::io::Write::flush(&mut backend).unwrap();
            let out = sink.text();
            assert!(
                !out.contains("38;2"),
                "{depth:?} emitted truecolor fg: {out:?}"
            );
            assert!(
                !out.contains("48;2"),
                "{depth:?} emitted truecolor bg: {out:?}"
            );
        }
    }

    #[test]
    fn truecolor_backend_still_emits_rgb() {
        let sink = SharedWriter::default();
        let mut backend = QuantizingBackend::new(sink.clone(), ColorDepth::TrueColor);
        let mut cell = Cell::new("x");
        cell.fg = Color::Rgb(92, 103, 118);
        let cells = [(0u16, 0u16, cell)];
        backend
            .draw(cells.iter().map(|(x, y, c)| (*x, *y, c)))
            .unwrap();
        std::io::Write::flush(&mut backend).unwrap();
        assert!(sink.text().contains("38;2;92;103;118"), "{:?}", sink.text());
    }
}
