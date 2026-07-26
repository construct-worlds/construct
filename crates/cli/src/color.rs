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

impl ColorDepth {
    /// Short modeline tag; `None` when nothing was downgraded.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::TrueColor => None,
            Self::Ansi256 => Some("256c"),
            Self::Ansi16 => Some("16c"),
        }
    }
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
            Color::Rgb(r, g, b) => Color::Indexed(nearest_cube_or_gray(r, g, b)),
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

/// Nearest 256-color index, choosing between the color cube (16..232) and the
/// grayscale ramp (232..256) by squared RGB distance. Indices 0..16 are never
/// returned: their hues belong to the user's terminal profile, so a palette
/// slot mapped onto one would drift with the profile instead of staying put.
fn nearest_cube_or_gray(r: u8, g: u8, b: u8) -> u8 {
    let cube_idx = |c: u8| -> usize {
        CUBE_LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| (**level as i32 - c as i32).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (cube_idx(r), cube_idx(g), cube_idx(b));
    let cube = 16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8;
    let cube_dist = dist(
        (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]),
        (r, g, b),
    );

    // Grayscale ramp: levels 8, 18, .. 238 at indices 232..256.
    let avg = (r as u32 + g as u32 + b as u32) / 3;
    let step = ((avg as i32 - 8).clamp(0, 238) as f32 / 10.0).round() as i32;
    let step = step.clamp(0, 23) as u8;
    let gray_level = 8 + 10 * step;
    let gray_dist = dist((gray_level, gray_level, gray_level), (r, g, b));

    if gray_dist < cube_dist {
        232 + step
    } else {
        cube
    }
}

/// Nearest of the 16 basic colors, returned as ratatui's named variants — which
/// go out as `38;5;0`..`38;5;15`, i.e. the user's own profile entries. The RGB
/// values compared against are the conventional defaults for those slots; a
/// profile that redefines them shifts the rendered hue, which is exactly the
/// bargain at this depth.
fn nearest_basic(r: u8, g: u8, b: u8) -> Color {
    const BASIC: [(Color, (u8, u8, u8)); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::Red, (170, 0, 0)),
        (Color::Green, (0, 170, 0)),
        (Color::Yellow, (170, 85, 0)),
        (Color::Blue, (0, 0, 170)),
        (Color::Magenta, (170, 0, 170)),
        (Color::Cyan, (0, 170, 170)),
        (Color::Gray, (170, 170, 170)),
        (Color::DarkGray, (85, 85, 85)),
        (Color::LightRed, (255, 85, 85)),
        (Color::LightGreen, (85, 255, 85)),
        (Color::LightYellow, (255, 255, 85)),
        (Color::LightBlue, (85, 85, 255)),
        (Color::LightMagenta, (255, 85, 255)),
        (Color::LightCyan, (85, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];
    BASIC
        .iter()
        .min_by_key(|(_, rgb)| dist(*rgb, (r, g, b)))
        .map(|(color, _)| *color)
        .unwrap_or(Color::Reset)
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

fn dist((r1, g1, b1): (u8, u8, u8), (r2, g2, b2): (u8, u8, u8)) -> i32 {
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;
    dr * dr + dg * dg + db * db
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
}

impl<W: std::io::Write> QuantizingBackend<W> {
    pub fn new(writer: W, depth: ColorDepth) -> Self {
        Self {
            inner: ratatui::backend::CrosstermBackend::new(writer),
            depth,
            scratch: Vec::new(),
        }
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
            cell.fg = quantize(cell.fg, self.depth);
            cell.bg = quantize(cell.bg, self.depth);
            cell.underline_color = quantize(cell.underline_color, self.depth);
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
        // ...while the palette's near-white body text reads as plain white,
        // which is the honest answer at four bits.
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
