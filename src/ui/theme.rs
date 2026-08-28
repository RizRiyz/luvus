//! Color palette. "luvus vr46" — a near-black dark UI with a fluorescent
//! stabilo/Valentino-Rossi green accent for active/selected elements.

use crate::terminal::theme_probe::TerminalColors;
use ratatui::style::Color;

// ── Theme derivation from terminal colors ───────────────────────────────────

fn luminance(rgb: [u8; 3]) -> f32 {
    0.2126 * (rgb[0] as f32 / 255.0)
        + 0.7152 * (rgb[1] as f32 / 255.0)
        + 0.0722 * (rgb[2] as f32 / 255.0)
}

fn blend_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> Color {
    let f = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).clamp(0.0, 255.0) as u8;
    Color::Rgb(f(0), f(1), f(2))
}

fn pal(c: [u8; 3]) -> Color {
    Color::Rgb(c[0], c[1], c[2])
}

impl Theme {
    /// The intentionally soft composer treatment used by Luvus's default
    /// Quattro Rally palette. Preserve its original low-contrast character.
    pub fn subtle_composer_surface(&self) -> Color {
        match (self.mantle, self.surface0) {
            (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
                let blend = |a: u8, b: u8| (a as i16 + ((b as i16 - a as i16) * 28) / 100) as u8;
                Color::Rgb(blend(ar, br), blend(ag, bg), blend(ab, bb))
            }
            (_, surface) if surface != self.mantle => surface,
            _ => self.surface1,
        }
    }

    /// A zero-probe terminal theme. `Reset` follows the terminal's configured
    /// foreground/background and ANSI indices follow its palette, so selecting
    /// Terminal never flashes or falls back to a bundled luvus theme. A
    /// successful OSC probe replaces this with the richer derived palette.
    pub fn terminal_native() -> Self {
        let ansi = Color::Indexed;
        Theme {
            crust: Color::Reset,
            mantle: Color::Reset,
            base: Color::Reset,
            surface0: Color::Reset,
            surface1: ansi(8),
            overlay0: ansi(8),
            overlay1: ansi(7),
            subtext0: ansi(8),
            subtext1: ansi(7),
            text: Color::Reset,
            accent: ansi(4),
            sel_bg: ansi(8),
            border: ansi(8),
            border_focus: ansi(7),
            green: ansi(2),
            mint: ansi(6),
            amber: ansi(3),
            coral: ansi(1),
        }
    }

    pub fn from_terminal(c: &TerminalColors) -> Self {
        let (bg, fg) = (c.bg, c.fg);
        let is_dark = luminance(bg) < luminance(fg);
        // palette[8] (bright black) is the terminal designer's chosen "dim/elevated"
        // color — blend toward it for surfaces so the tint matches the scheme.
        let dim = c.palette[8];
        let base = Self::from_terminal_base(c);

        if is_dark {
            Theme {
                // OSC 11 reports only RGB, not the terminal background's alpha.
                // Painting that RGB explicitly makes transparent terminals
                // opaque, so primary surfaces must keep using the default color.
                crust: Color::Reset,
                mantle: Color::Reset,
                surface0: Color::Reset,
                surface1: blend_rgb(bg, dim, 0.70),
                subtext0: blend_rgb(bg, fg, 0.62),
                subtext1: blend_rgb(bg, fg, 0.78),
                sel_bg: blend_rgb(bg, c.palette[4], 0.18),
                border: blend_rgb(bg, dim, 0.90),
                border_focus: blend_rgb(bg, fg, 0.38),
                ..base
            }
        } else {
            Theme {
                crust: Color::Reset,
                mantle: Color::Reset,
                surface0: Color::Reset,
                surface1: blend_rgb(bg, dim, 0.60),
                subtext0: blend_rgb(bg, fg, 0.55),
                subtext1: blend_rgb(bg, fg, 0.70),
                sel_bg: blend_rgb(bg, c.palette[4], 0.15),
                border: blend_rgb(bg, dim, 0.85),
                border_focus: blend_rgb(bg, fg, 0.45),
                ..base
            }
        }
    }

    fn from_terminal_base(c: &TerminalColors) -> Self {
        let (bg, fg) = (c.bg, c.fg);
        Theme {
            crust: Color::Reset,
            mantle: Color::Reset,
            base: Color::Reset,
            surface0: Color::Reset,
            surface1: Color::Reset,
            overlay0: blend_rgb(bg, fg, 0.28),
            overlay1: blend_rgb(bg, fg, 0.40),
            subtext0: Color::Reset,
            subtext1: Color::Reset,
            text: pal(fg),
            accent: pal(c.palette[4]),
            sel_bg: Color::Reset,
            border: Color::Reset,
            border_focus: Color::Reset,
            green: pal(c.palette[2]),
            mint: pal(c.palette[6]),
            amber: pal(c.palette[3]),
            coral: pal(c.palette[1]),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    // surfaces (dark → light)
    pub crust: Color,
    pub mantle: Color,
    pub base: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub overlay0: Color,
    pub overlay1: Color,
    // text
    pub subtext0: Color,
    pub subtext1: Color,
    pub text: Color,
    // accents
    pub accent: Color,       // fluo green — brand, focus, active/selected
    pub sel_bg: Color,       // selection background (dark green tint)
    pub border: Color,       // pane border (unfocused) — grey
    pub border_focus: Color, // pane border (focused) — light grey/white
    pub green: Color,
    pub mint: Color,
    pub amber: Color,
    pub coral: Color,
}

impl Theme {
    /// A clearly raised—but still quiet—surface for the Codex composer.
    ///
    /// Palette surfaces are not equally spaced: in some themes `surface0` is
    /// almost the pane background, while in others it is already prominent.
    /// Pick the nearest semantic surface with enough range, then blend just far
    /// enough to reach a consistent visible step. Terminal-native and 256-color
    /// themes retain their own palette indices instead of borrowing RGB colors.
    pub fn composer_surface(&self) -> Color {
        let Color::Rgb(ar, ag, ab) = self.mantle else {
            return [self.surface1, self.surface0, self.base, self.overlay0]
                .into_iter()
                .find(|&surface| surface != self.mantle)
                .unwrap_or(self.mantle);
        };

        let distance = |color: Color| match color {
            Color::Rgb(r, g, b) => ar.abs_diff(r).max(ag.abs_diff(g)).max(ab.abs_diff(b)),
            _ => 0,
        };
        let candidates = [self.surface1, self.surface0, self.base, self.overlay0];
        let target = candidates
            .into_iter()
            .find(|&color| distance(color) >= 24)
            .or_else(|| candidates.into_iter().max_by_key(|&color| distance(color)))
            .unwrap_or(self.surface1);

        match target {
            Color::Rgb(br, bg, bb) => {
                let range = distance(target).max(1) as i16;
                // Aim for an 18-step channel difference. The bounds prevent a
                // distant surface becoming loud or a close one staying muddy.
                let percent = (1800 / range).clamp(28, 70);
                let blend =
                    |a: u8, b: u8| (a as i16 + ((b as i16 - a as i16) * percent) / 100) as u8;
                Color::Rgb(blend(ar, br), blend(ag, bg), blend(ab, bb))
            }
            surface => surface,
        }
    }

    pub fn noir() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x07, 0x07, 0x09),
            mantle: rgb(0x11, 0x11, 0x16), // pane background + inactive tabs + tab bar
            base: rgb(0x20, 0x20, 0x28),   // sidebar — a bit lighter than the pane
            surface0: rgb(0x1a, 0x1a, 0x20),
            surface1: rgb(0x25, 0x25, 0x2d),
            overlay0: rgb(0x4a, 0x4a, 0x54),
            overlay1: rgb(0x68, 0x68, 0x73),
            subtext0: rgb(0x93, 0x93, 0x9f),
            subtext1: rgb(0xb6, 0xb6, 0xc0),
            text: rgb(0xe7, 0xe7, 0xed),
            accent: rgb(0xc6, 0xff, 0x1a), // stabilo / VR46 fluo green
            sel_bg: rgb(0x33, 0x45, 0x0e), // dark green selection — stands out from the sidebar
            // Borders are filled (background), so keep the unfocused frame subtle
            // and let the focused one stand out by brightness alone.
            border: rgb(0x38, 0x38, 0x40), // subtle grey (unfocused pane frame)
            border_focus: rgb(0x8c, 0x8c, 0x96), // medium grey (focused pane frame)
            green: rgb(0x8f, 0xbc, 0x7a),  // sage (idle)
            mint: rgb(0x6f, 0xc6, 0xa3),   // mint (done)
            amber: rgb(0xe0, 0x9a, 0x4d),  // amber-orange (working)
            coral: rgb(0xe0, 0x6c, 0x66),  // coral (blocked)
        }
    }

    /// Quattro Rally — Catppuccin Macchiato's soft dark base under a rally-gold
    /// accent, plus olive and red state colors. After the omarchy "quattro"
    /// palette (Audi Quattro rally livery: gold on dark, red flourishes).
    pub fn quattro_rally() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x18, 0x19, 0x26),  // Macchiato crust
            mantle: rgb(0x1e, 0x20, 0x30), // pane background (Macchiato mantle)
            base: rgb(0x24, 0x27, 0x3a),   // sidebar (Macchiato base)
            surface0: rgb(0x36, 0x3a, 0x4f),
            surface1: rgb(0x49, 0x4d, 0x64),
            overlay0: rgb(0x6e, 0x73, 0x8d),
            overlay1: rgb(0x80, 0x87, 0xa2),
            subtext0: rgb(0xa5, 0xad, 0xcb),
            subtext1: rgb(0xb8, 0xc0, 0xe0),
            text: rgb(0xca, 0xd3, 0xf5), // white (Catppuccin Macchiato text)
            accent: rgb(0xdb, 0xc6, 0x6f), // rally gold — brand, focus, selected
            sel_bg: rgb(0x3a, 0x34, 0x16), // dark gold selection
            // Gold frames, like matrix/ember tint theirs to their accent rather
            // than inheriting a neutral grey. Same hue as the accent (~47°) at the
            // same *luminance* the Macchiato greys had, so the unfocused/focused
            // step reads exactly as before — only the colour changed.
            border: rgb(0x5a, 0x51, 0x30), // dim gold (unfocused pane frame)
            border_focus: rgb(0x9a, 0x8a, 0x50), // bright gold (focused pane frame)
            green: rgb(0x94, 0xa1, 0x43),  // moss / olive (idle)
            mint: rgb(0xb8, 0xcf, 0x6a),   // yellow-green (done)
            amber: rgb(0xe0, 0xa1, 0x54),  // amber (working)
            coral: rgb(0xcf, 0x5a, 0x44),  // rally red (blocked)
        }
    }

    /// A near-black grayscale variant — no color, just contrast.
    pub fn mono() -> Self {
        let g = |v| Color::Rgb(v, v, v);
        Theme {
            crust: g(0x07),
            mantle: g(0x12),
            base: g(0x1e),
            surface0: g(0x18),
            surface1: g(0x28),
            overlay0: g(0x4a),
            overlay1: g(0x68),
            subtext0: g(0x93),
            subtext1: g(0xb6),
            text: g(0xec),
            accent: g(0xea), // near-white accent
            sel_bg: g(0x33),
            border: g(0x52),
            border_focus: g(0x90),
            // states by brightness: idle dim → blocked bright.
            green: g(0x82),
            mint: g(0xa6),
            amber: g(0xc8),
            coral: g(0xe6),
        }
    }

    /// Catppuccin **Mocha** — the dark flavour of the palette whose semantic
    /// names (crust/mantle/base/surface/overlay/subtext) this `Theme` borrows.
    /// Upstream hex values; the accent is Mocha's signature mauve.
    pub fn mocha() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x11, 0x11, 0x1b),
            mantle: rgb(0x18, 0x18, 0x25), // pane background
            base: rgb(0x1e, 0x1e, 0x2e),   // sidebar — Mocha's own `base`
            surface0: rgb(0x31, 0x32, 0x44),
            surface1: rgb(0x45, 0x47, 0x5a),
            overlay0: rgb(0x6c, 0x70, 0x86),
            overlay1: rgb(0x7f, 0x84, 0x9c),
            subtext0: rgb(0xa6, 0xad, 0xc8),
            subtext1: rgb(0xba, 0xc2, 0xde),
            text: rgb(0xcd, 0xd6, 0xf4),
            accent: rgb(0xcb, 0xa6, 0xf7), // mauve
            sel_bg: rgb(0x3f, 0x33, 0x59), // dark mauve tint
            border: rgb(0x45, 0x47, 0x5a),
            border_focus: rgb(0x7f, 0x84, 0x9c),
            green: rgb(0xa6, 0xe3, 0xa1), // idle
            mint: rgb(0x94, 0xe2, 0xd5),  // done (teal)
            amber: rgb(0xfa, 0xb3, 0x87), // working (peach)
            coral: rgb(0xf3, 0x8b, 0xa8), // blocked (red)
        }
    }

    /// Catppuccin **Macchiato** — one step lighter than Mocha. Upstream hexes.
    pub fn macchiato() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x18, 0x19, 0x26),
            mantle: rgb(0x1e, 0x20, 0x30),
            base: rgb(0x24, 0x27, 0x3a),
            surface0: rgb(0x36, 0x3a, 0x4f),
            surface1: rgb(0x49, 0x4d, 0x64),
            overlay0: rgb(0x6e, 0x73, 0x8d),
            overlay1: rgb(0x80, 0x87, 0xa2),
            subtext0: rgb(0xa5, 0xad, 0xcb),
            subtext1: rgb(0xb8, 0xc0, 0xe0),
            text: rgb(0xca, 0xd3, 0xf5),
            accent: rgb(0xc6, 0xa0, 0xf6), // mauve
            sel_bg: rgb(0x3b, 0x32, 0x54),
            border: rgb(0x49, 0x4d, 0x64),
            border_focus: rgb(0x80, 0x87, 0xa2),
            green: rgb(0xa6, 0xda, 0x95),
            mint: rgb(0x8b, 0xd5, 0xca),
            amber: rgb(0xf5, 0xa9, 0x7f),
            coral: rgb(0xed, 0x87, 0x96),
        }
    }

    /// Catppuccin **Frappé** — the lightest of the three dark flavours.
    pub fn frappe() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x23, 0x26, 0x34),
            mantle: rgb(0x29, 0x2c, 0x3c),
            base: rgb(0x30, 0x34, 0x46),
            surface0: rgb(0x41, 0x45, 0x59),
            surface1: rgb(0x51, 0x57, 0x6d),
            overlay0: rgb(0x73, 0x79, 0x94),
            overlay1: rgb(0x83, 0x8b, 0xa7),
            subtext0: rgb(0xa5, 0xad, 0xce),
            subtext1: rgb(0xb5, 0xbf, 0xe2),
            text: rgb(0xc6, 0xd0, 0xf5),
            accent: rgb(0xca, 0x9e, 0xe6), // mauve
            sel_bg: rgb(0x46, 0x3e, 0x5e),
            border: rgb(0x51, 0x57, 0x6d),
            border_focus: rgb(0x83, 0x8b, 0xa7),
            green: rgb(0xa6, 0xd1, 0x89),
            mint: rgb(0x81, 0xc8, 0xbe),
            amber: rgb(0xef, 0x9f, 0x76),
            coral: rgb(0xe7, 0x82, 0x84),
        }
    }

    /// Gruvbox **dark** — retro warm earth tones. Upstream hex values; the accent
    /// is Gruvbox's bright yellow rather than its orange, which would sit too
    /// close to `redsands` once downsampled to 256 colors.
    pub fn gruvbox() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x1d, 0x20, 0x21),  // bg0_h
            mantle: rgb(0x28, 0x28, 0x28), // bg — pane background
            base: rgb(0x3c, 0x38, 0x36),   // bg1 — sidebar
            surface0: rgb(0x32, 0x30, 0x2f),
            surface1: rgb(0x50, 0x49, 0x45),
            overlay0: rgb(0x66, 0x5c, 0x54),
            overlay1: rgb(0x7c, 0x6f, 0x64),
            subtext0: rgb(0xa8, 0x99, 0x84),
            subtext1: rgb(0xbd, 0xae, 0x93),
            text: rgb(0xeb, 0xdb, 0xb2),
            accent: rgb(0xfa, 0xbd, 0x2f), // bright yellow
            sel_bg: rgb(0x45, 0x3d, 0x21), // dark amber tint
            border: rgb(0x50, 0x49, 0x45),
            border_focus: rgb(0x92, 0x83, 0x74),
            green: rgb(0xb8, 0xbb, 0x26), // idle
            mint: rgb(0x8e, 0xc0, 0x7c),  // done (aqua)
            amber: rgb(0xfe, 0x80, 0x19), // working (orange)
            coral: rgb(0xfb, 0x49, 0x34), // blocked (bright red)
        }
    }

    /// Gruvbox **light** — the same earth tones on cream, for light terminals.
    /// Surfaces run lightest-first here, like `latte`.
    pub fn gruvbox_light() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0xfb, 0xf1, 0xc7),  // bg0
            mantle: rgb(0xf2, 0xe5, 0xbc), // pane background
            base: rgb(0xeb, 0xdb, 0xb2),   // bg1 — sidebar
            surface0: rgb(0xd5, 0xc4, 0xa1),
            surface1: rgb(0xbd, 0xae, 0x93),
            overlay0: rgb(0xa8, 0x99, 0x84),
            overlay1: rgb(0x92, 0x83, 0x74),
            subtext0: rgb(0x66, 0x5c, 0x54),
            subtext1: rgb(0x50, 0x49, 0x45),
            text: rgb(0x3c, 0x38, 0x36),
            accent: rgb(0xaf, 0x3a, 0x03), // burnt orange, dark enough for cream
            sel_bg: rgb(0xe0, 0xc6, 0x8a),
            border: rgb(0xbd, 0xae, 0x93),
            border_focus: rgb(0x7c, 0x6f, 0x64),
            green: rgb(0x79, 0x74, 0x0e), // idle
            mint: rgb(0x42, 0x7b, 0x58),  // done (aqua)
            amber: rgb(0xb5, 0x76, 0x14), // working
            coral: rgb(0x9d, 0x00, 0x06), // blocked
        }
    }

    /// A warm light palette (Catppuccin-Latte-ish) for light terminals.
    pub fn latte() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0xef, 0xf1, 0xf5),
            mantle: rgb(0xe6, 0xe9, 0xef),
            base: rgb(0xdc, 0xe0, 0xe8),
            surface0: rgb(0xcc, 0xd0, 0xda),
            surface1: rgb(0xbc, 0xc0, 0xcc),
            overlay0: rgb(0x9c, 0xa0, 0xb0),
            overlay1: rgb(0x7c, 0x80, 0x90),
            subtext0: rgb(0x6c, 0x6f, 0x85),
            subtext1: rgb(0x50, 0x52, 0x6c),
            text: rgb(0x4c, 0x4f, 0x69),
            accent: rgb(0x40, 0xa0, 0x2b), // green, darkened for light bg
            sel_bg: rgb(0xc6, 0xe8, 0xa8),
            border: rgb(0xac, 0xb0, 0xbe),
            border_focus: rgb(0x7c, 0x80, 0x90),
            green: rgb(0x40, 0xa0, 0x2b),
            mint: rgb(0x17, 0x92, 0x99),
            amber: rgb(0xdf, 0x8e, 0x1d),
            coral: rgb(0xd2, 0x0f, 0x39),
        }
    }

    /// "Sky" — a light **paper** palette: a soft, faintly cool off-white
    /// background with slate ink and a sky-blue accent, for light terminals.
    /// Surfaces run lightest-first, like `latte` and `gruvbox_light`.
    pub fn sky() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0xf5, 0xf8, 0xfc),  // paper — page background
            mantle: rgb(0xeb, 0xf1, 0xf9), // pane background
            base: rgb(0xe1, 0xea, 0xf5),   // sidebar
            surface0: rgb(0xd6, 0xe1, 0xf0),
            surface1: rgb(0xc4, 0xd3, 0xe8),
            overlay0: rgb(0x93, 0xa4, 0xbd),
            overlay1: rgb(0x74, 0x84, 0x9e),
            subtext0: rgb(0x56, 0x65, 0x79),
            subtext1: rgb(0x42, 0x50, 0x5f),
            text: rgb(0x2f, 0x3a, 0x48),   // slate ink
            accent: rgb(0x24, 0x77, 0xc7), // sky blue, dark enough for the paper
            sel_bg: rgb(0xcb, 0xe2, 0xf7), // light sky selection
            border: rgb(0xc0, 0xcd, 0xe0),
            border_focus: rgb(0x6a, 0x7a, 0x92),
            green: rgb(0x3f, 0x8f, 0x56), // idle
            mint: rgb(0x1c, 0x8f, 0x88),  // done
            amber: rgb(0xc0, 0x7d, 0x12), // working
            coral: rgb(0xcc, 0x3b, 0x52), // blocked
        }
    }

    /// "Ocean" — a deep cmd/PowerShell blue with a bright cyan accent.
    pub fn ocean() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x02, 0x10, 0x2a),
            mantle: rgb(0x06, 0x1d, 0x42),
            base: rgb(0x0c, 0x2e, 0x5e),
            surface0: rgb(0x05, 0x22, 0x4a),
            surface1: rgb(0x12, 0x3a, 0x6e),
            overlay0: rgb(0x35, 0x58, 0x86),
            overlay1: rgb(0x52, 0x76, 0xa4),
            subtext0: rgb(0x8f, 0xa8, 0xc8),
            subtext1: rgb(0xb8, 0xcc, 0xe6),
            text: rgb(0xe8, 0xf2, 0xff),
            accent: rgb(0x46, 0xc6, 0xff), // bright cyan
            sel_bg: rgb(0x12, 0x3e, 0x76),
            border: rgb(0x2a, 0x4e, 0x80),
            border_focus: rgb(0x5a, 0x86, 0xc0),
            green: rgb(0x6f, 0xcf, 0x97),
            mint: rgb(0x4f, 0xd6, 0xc8),
            amber: rgb(0xf2, 0xc1, 0x4e),
            coral: rgb(0xf0, 0x6a, 0x6a),
        }
    }

    /// "Homebrew" — the classic green-on-black terminal.
    pub fn homebrew() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x00, 0x00, 0x00),
            mantle: rgb(0x04, 0x0a, 0x04),
            base: rgb(0x08, 0x16, 0x08),
            surface0: rgb(0x06, 0x10, 0x06),
            surface1: rgb(0x0e, 0x24, 0x0e),
            overlay0: rgb(0x1e, 0x4c, 0x1e),
            overlay1: rgb(0x2e, 0x6c, 0x2e),
            subtext0: rgb(0x22, 0xb4, 0x22),
            subtext1: rgb(0x2e, 0xe0, 0x2e),
            text: rgb(0x3c, 0xff, 0x3c),   // bright green
            accent: rgb(0x00, 0xff, 0x41), // neon green
            sel_bg: rgb(0x0a, 0x3a, 0x0a),
            border: rgb(0x1a, 0x4e, 0x1a),
            border_focus: rgb(0x2e, 0xa8, 0x2e),
            green: rgb(0x35, 0xe0, 0x35),
            mint: rgb(0x5e, 0xff, 0xb0),
            amber: rgb(0xc8, 0xff, 0x3c),
            coral: rgb(0xff, 0x60, 0x50),
        }
    }

    /// "Red Sands" — warm dark red with a bright orange accent.
    pub fn red_sands() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x1f, 0x0a, 0x06),
            mantle: rgb(0x38, 0x12, 0x0c),
            base: rgb(0x4e, 0x1c, 0x12),
            surface0: rgb(0x2c, 0x0e, 0x08),
            surface1: rgb(0x5c, 0x26, 0x1a),
            overlay0: rgb(0x8a, 0x4a, 0x38),
            overlay1: rgb(0xa8, 0x68, 0x54),
            subtext0: rgb(0xc8, 0x9a, 0x80),
            subtext1: rgb(0xdc, 0xba, 0x9c),
            text: rgb(0xf2, 0xda, 0xba),   // warm tan
            accent: rgb(0xff, 0x8a, 0x3c), // orange
            sel_bg: rgb(0x70, 0x2e, 0x1e),
            border: rgb(0x7c, 0x3c, 0x2a),
            border_focus: rgb(0xba, 0x6e, 0x4c),
            green: rgb(0xa6, 0xbf, 0x5e),
            mint: rgb(0x5e, 0xc8, 0xa8),
            amber: rgb(0xff, 0xb4, 0x54),
            coral: rgb(0xff, 0x6a, 0x5a),
        }
    }

    /// "Grass" — a deep green field with pale-yellow text.
    pub fn grass() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x05, 0x20, 0x12),
            mantle: rgb(0x0a, 0x36, 0x1d),
            base: rgb(0x10, 0x4c, 0x28),
            surface0: rgb(0x08, 0x2c, 0x18),
            surface1: rgb(0x15, 0x58, 0x30),
            overlay0: rgb(0x36, 0x78, 0x50),
            overlay1: rgb(0x54, 0x96, 0x6c),
            subtext0: rgb(0xa8, 0xc8, 0x98),
            subtext1: rgb(0xc8, 0xe0, 0xae),
            text: rgb(0xff, 0xf0, 0xa5),   // pale yellow
            accent: rgb(0xbe, 0xe6, 0x32), // lime
            sel_bg: rgb(0x1a, 0x6c, 0x3a),
            border: rgb(0x28, 0x76, 0x48),
            border_focus: rgb(0x56, 0xa6, 0x6c),
            green: rgb(0x8f, 0xd0, 0x7a),
            mint: rgb(0x5e, 0xd6, 0xb0),
            amber: rgb(0xf2, 0xc8, 0x4e),
            coral: rgb(0xf0, 0x6a, 0x5a),
        }
    }

    /// "Dracula" — the famous dark theme: indigo with a violet accent.
    pub fn dracula() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x1a, 0x1b, 0x23),
            mantle: rgb(0x28, 0x2a, 0x36),
            base: rgb(0x33, 0x36, 0x47),
            surface0: rgb(0x21, 0x22, 0x2c),
            surface1: rgb(0x3c, 0x3f, 0x52),
            overlay0: rgb(0x56, 0x58, 0x69),
            overlay1: rgb(0x62, 0x72, 0xa4),
            subtext0: rgb(0xa0, 0xa4, 0xc0),
            subtext1: rgb(0xcc, 0xce, 0xe0),
            text: rgb(0xf8, 0xf8, 0xf2),
            accent: rgb(0xbd, 0x93, 0xf9), // purple
            sel_bg: rgb(0x44, 0x47, 0x5a),
            border: rgb(0x44, 0x47, 0x5a),
            border_focus: rgb(0x62, 0x72, 0xa4),
            green: rgb(0x50, 0xfa, 0x7b),
            mint: rgb(0x8b, 0xe9, 0xfd),
            amber: rgb(0xff, 0xb8, 0x6c),
            coral: rgb(0xff, 0x55, 0x55),
        }
    }

    /// "Nord" — a cool arctic blue-grey palette.
    pub fn nord() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x24, 0x29, 0x33),
            mantle: rgb(0x2e, 0x34, 0x40),
            base: rgb(0x3b, 0x42, 0x52),
            surface0: rgb(0x29, 0x2f, 0x3a),
            surface1: rgb(0x43, 0x4c, 0x5e),
            overlay0: rgb(0x4c, 0x56, 0x6a),
            overlay1: rgb(0x61, 0x6e, 0x88),
            subtext0: rgb(0xb0, 0xb8, 0xc8),
            subtext1: rgb(0xd8, 0xde, 0xe9),
            text: rgb(0xec, 0xef, 0xf4),
            accent: rgb(0x88, 0xc0, 0xd0), // frost cyan
            sel_bg: rgb(0x3b, 0x4a, 0x5e),
            border: rgb(0x43, 0x4c, 0x5e),
            border_focus: rgb(0x6a, 0x76, 0x90),
            green: rgb(0xa3, 0xbe, 0x8c),
            mint: rgb(0x8f, 0xbc, 0xbb),
            amber: rgb(0xeb, 0xcb, 0x8b),
            coral: rgb(0xbf, 0x61, 0x6a),
        }
    }

    /// "Sunset" — a neon synthwave palette: deep purple with a hot-pink accent.
    pub fn sunset() -> Self {
        let rgb = |r, g, b| Color::Rgb(r, g, b);
        Theme {
            crust: rgb(0x16, 0x0a, 0x1e),
            mantle: rgb(0x22, 0x10, 0x30),
            base: rgb(0x2e, 0x16, 0x40),
            surface0: rgb(0x1c, 0x0d, 0x2a),
            surface1: rgb(0x3a, 0x1d, 0x50),
            overlay0: rgb(0x5a, 0x3a, 0x70),
            overlay1: rgb(0x7a, 0x5a, 0x90),
            subtext0: rgb(0xb8, 0x9a, 0xd0),
            subtext1: rgb(0xd6, 0xbc, 0xe8),
            text: rgb(0xf6, 0xe8, 0xff),
            accent: rgb(0xff, 0x5f, 0xd0), // hot pink
            sel_bg: rgb(0x4a, 0x24, 0x66),
            border: rgb(0x4a, 0x2a, 0x64),
            border_focus: rgb(0x8a, 0x5a, 0xa8),
            green: rgb(0x5f, 0xe0, 0xa8),
            mint: rgb(0x5f, 0xd6, 0xe0),
            amber: rgb(0xff, 0xb5, 0x4f),
            coral: rgb(0xff, 0x5f, 0x8f),
        }
    }

    /// Downsample every color to the nearest xterm-256 index, for terminals
    /// without truecolor support.
    pub fn to_256(&self) -> Theme {
        let c = crate::ipc::protocol::to_256;
        Theme {
            crust: c(self.crust),
            mantle: c(self.mantle),
            base: c(self.base),
            surface0: c(self.surface0),
            surface1: c(self.surface1),
            overlay0: c(self.overlay0),
            overlay1: c(self.overlay1),
            subtext0: c(self.subtext0),
            subtext1: c(self.subtext1),
            text: c(self.text),
            accent: c(self.accent),
            sel_bg: c(self.sel_bg),
            border: c(self.border),
            border_focus: c(self.border_focus),
            green: c(self.green),
            mint: c(self.mint),
            amber: c(self.amber),
            coral: c(self.coral),
        }
    }
}

/// Built-in palette names, in display order (first is the default).
pub const THEMES: &[&str] = &[
    // The default leads the list, so Settings -> Theme opens on it.
    "quattro-rally",
    "noir",
    "ocean",
    "dracula",
    "nord",
    "sky",
    "catppuccin-mocha",
    "catppuccin-macchiato",
    "catppuccin-frappe",
    "gruvbox",
    "sunset",
    "homebrew",
    "grass",
    "redsands",
    "catppuccin-latte",
    "gruvbox-light",
    "mono",
    // Terminal is capability-derived rather than a bundled palette, and is
    // newest, so keep it after the built-in themes.
    "terminal",
];

/// Map a stored theme name onto its current registry name. The Catppuccin
/// palettes were originally registered as bare `latte`/`mocha`; a config written
/// then must keep working rather than silently falling back to `noir`.
pub fn canonical(name: &str) -> &str {
    match name {
        "latte" => "catppuccin-latte",
        "mocha" => "catppuccin-mocha",
        "macchiato" => "catppuccin-macchiato",
        "frappe" => "catppuccin-frappe",
        "gruvboxlight" => "gruvbox-light",
        other => other,
    }
}

pub fn by_name(name: &str) -> Theme {
    match name {
        // The client-supplied RGB palette is applied after startup. Until then,
        // use terminal-native Reset/ANSI colors rather than a bundled palette.
        "terminal" => Theme::terminal_native(),
        "ocean" => Theme::ocean(),
        "homebrew" => Theme::homebrew(),
        "redsands" => Theme::red_sands(),
        "grass" => Theme::grass(),
        "dracula" => Theme::dracula(),
        "nord" => Theme::nord(),
        "catppuccin-mocha" | "mocha" => Theme::mocha(),
        "catppuccin-macchiato" | "macchiato" => Theme::macchiato(),
        "catppuccin-frappe" | "frappe" => Theme::frappe(),
        "gruvbox" => Theme::gruvbox(),
        "gruvbox-light" | "gruvboxlight" => Theme::gruvbox_light(),
        "quattro-rally" => Theme::quattro_rally(),
        "sunset" => Theme::sunset(),
        "catppuccin-latte" | "latte" => Theme::latte(),
        "sky" => Theme::sky(),
        "mono" => Theme::mono(),
        "noir" => Theme::noir(),
        // Anything unrecognised falls back to the default palette.
        _ => Theme::quattro_rally(),
    }
}

pub fn describe(name: &str) -> &'static str {
    match name {
        "terminal" => "inferred from your terminal",
        "ocean" => "deep cmd-blue, cyan accent",
        "homebrew" => "classic green-on-black",
        "redsands" => "warm dark red, orange accent",
        "grass" => "green field, pale-yellow text",
        "dracula" => "indigo dark, violet accent",
        "nord" => "cool arctic blue-grey",
        "catppuccin-mocha" | "mocha" => "darkest Catppuccin, mauve",
        "catppuccin-macchiato" | "macchiato" => "softer dark Catppuccin",
        "catppuccin-frappe" | "frappe" => "lightest dark Catppuccin",
        "gruvbox" => "retro warm dark, yellow accent",
        "gruvbox-light" | "gruvboxlight" => "Gruvbox on cream, burnt orange",
        "quattro-rally" => "soft dark, rally-gold accent",
        "sunset" => "neon synthwave, hot-pink",
        "catppuccin-latte" | "latte" => "light Catppuccin, warm",
        "sky" => "light paper, sky-blue accent",
        "mono" => "grayscale, no accent color",
        _ => "near-black, fluo-green accent",
    }
}

/// Agent / pane activity state. Drives sidebar glyphs and colors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Blocked,
    Working,
    Done,
    Idle,
    Unknown,
}

impl State {
    pub fn dot(self) -> &'static str {
        match self {
            State::Idle | State::Unknown => "○",
            _ => "●",
        }
    }

    pub fn color(self, t: &Theme) -> Color {
        match self {
            State::Blocked => t.coral,
            State::Working => t.amber,
            State::Done => t.mint,
            State::Idle => t.green,
            State::Unknown => t.overlay0,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Blocked => "blocked",
            State::Working => "working",
            State::Done => "done",
            State::Idle => "idle",
            State::Unknown => "—",
        }
    }
}

/// The working spinner's frames: a braille mark rotating around the **middle**
/// of the cell, so it sits level with the `○`/`●` dots on neighbouring rows.
///
/// A braille cell has four dot rows, and which rows a glyph uses decides where it
/// sits vertically. The classic CLI spinner (`⠋⠙⠹…`) uses dots 1-6 (the upper
/// three rows) and visibly rides high; the bottom two rows (dots 3/6/7/8) ride
/// just as visibly low. These frames use dots **2/3/5/6** — rows two and three —
/// which is the vertical centre of the cell.
///
/// Each frame is one edge of that 2x2 dot square, so the mark sweeps clockwise:
/// left → top → right → bottom. Every frame is exactly two dots, so the spinner
/// keeps constant weight as it turns (no pulsing).
///
/// Glyph choice matters for alignment, so don't swap these casually:
///
/// * **Every frame is the same width.** Braille (U+2800..=U+28FF) is East Asian
///   *Neutral* — exactly one column in every terminal, whatever its
///   ambiguous-width setting. The old half-circle set (`◐◓◑◒`) mixed classes:
///   `◐`/`◑` are *Ambiguous* (2 cells wherever a terminal draws ambiguous glyphs
///   wide) while `◒`/`◓` are Narrow, so the icon changed size every other frame
///   and drifted against the 1-column slot ratatui reserves for it.
/// * **Every frame carries the same ink.** All ten are six-dot patterns, so the
///   spinner never looks like it grows or shrinks as it turns.
/// * **Font coverage is effectively universal.** Braille is *the* spinner block,
///   so there is no fallback to another face at a different size.
///
/// [`state_glyphs_are_one_column`] guards these properties.
const FRAMES: [&str; 4] = ["⠆", "⠒", "⠰", "⠤"];

/// Frames in one full revolution. Iterate this instead of hardcoding a count so
/// changing the animation can't silently desync a caller or a test.
pub const SPINNER_FRAMES: u64 = FRAMES.len() as u64;

/// One frame of the "working" spinner, advanced by `App.spinner` while an agent
/// is working — a busy agent shows live motion instead of a static `●`.
pub fn spinner_frame(n: u64) -> &'static str {
    FRAMES[(n % SPINNER_FRAMES) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    // The agent-state icon sits in a one-column slot: ratatui lays the row out
    // with `UnicodeWidthStr::width` (== 1 for all of these), so any glyph a
    // terminal draws two cells wide pushes the label right and breaks the column.
    // Requiring `width_cjk == 1` too keeps every icon *unambiguously* narrow, so
    // the slot is honored even where ambiguous-width glyphs render wide — and
    // every spinner frame stays exactly the same size as the static dots.
    #[test]
    fn state_glyphs_are_one_column() {
        use unicode_width::UnicodeWidthStr;
        let mut glyphs: Vec<&str> = [
            State::Blocked,
            State::Working,
            State::Done,
            State::Idle,
            State::Unknown,
        ]
        .iter()
        .map(|s| s.dot())
        .collect();
        glyphs.extend((0..SPINNER_FRAMES).map(spinner_frame));

        for g in glyphs {
            assert_eq!(g.chars().count(), 1, "{g:?} must be a single glyph");
            assert_eq!(g.width(), 1, "{g:?} must occupy one column");
        }

        // The regression this guards: the spinner animates *in place*, so if its
        // frames disagree on East Asian width the icon visibly changes size as it
        // turns (the old `◐◓◑◒` mixed Ambiguous `◐`/`◑` with Narrow `◒`/`◓`).
        // Every frame must sit in the same width class as every other.
        let widths: std::collections::HashSet<usize> = (0..SPINNER_FRAMES)
            .map(|i| spinner_frame(i).width_cjk())
            .collect();
        assert_eq!(
            widths.len(),
            1,
            "spinner frames disagree on East Asian width, so the icon changes \
             size mid-animation: {:?}",
            (0..SPINNER_FRAMES)
                .map(|i| (spinner_frame(i), spinner_frame(i).width_cjk()))
                .collect::<Vec<_>>()
        );
        // The static dots must likewise agree with each other, so an idle row and
        // a blocked row never sit at different widths.
        assert_eq!(
            State::Idle.dot().width_cjk(),
            State::Blocked.dot().width_cjk(),
            "the idle and active dots must be the same width class"
        );

        // The four frames must be distinct, or the spinner would stutter…
        let frames: std::collections::HashSet<&str> =
            (0..SPINNER_FRAMES).map(spinner_frame).collect();
        assert_eq!(
            frames.len() as u64,
            SPINNER_FRAMES,
            "spinner frames must all differ"
        );
        // …and it must cycle with that period.
        assert_eq!(spinner_frame(0), spinner_frame(SPINNER_FRAMES));
    }

    #[test]
    fn theme_registry_is_consistent() {
        // Every listed palette resolves, has a description, and a distinct accent
        // (guards against adding a name to THEMES but forgetting by_name/describe).
        let mut swatches = std::collections::HashSet::new();
        // The picker swatch is (background, accent) and is downsampled to 256
        // colors on non-truecolor terminals — the *pair* must stay distinct, or two
        // rows look identical. A pair rather than the accent alone, because the
        // Catppuccin flavours deliberately share an accent and differ by surface.
        let mut swatches_256 = std::collections::HashSet::new();
        for &name in THEMES {
            assert!(!describe(name).is_empty(), "{name} needs a description");
            // Probe depends on runtime env — just verify it resolves without panic.
            if name == "terminal" {
                let _ = by_name(name);
                continue;
            }
            let pal = by_name(name);
            assert!(
                swatches.insert(format!("{:?}{:?}", pal.base, pal.accent)),
                "{name} should have a distinct background+accent swatch"
            );
            let px = crate::ipc::protocol::to_256(pal.base);
            let pa = crate::ipc::protocol::to_256(pal.accent);
            assert!(
                swatches_256.insert(format!("{px:?}{pa:?}")),
                "{name}'s swatch must stay distinct after 256-color downsampling"
            );
        }
        assert!(THEMES.len() >= 15, "the new palettes are registered");
        assert_eq!(THEMES.last(), Some(&"terminal"));

        // Legacy config values must keep resolving to the same palette.
        for (old, new) in [
            ("mocha", "catppuccin-mocha"),
            ("latte", "catppuccin-latte"),
            ("gruvboxlight", "gruvbox-light"),
        ] {
            assert_eq!(canonical(old), new, "{old} should map to {new}");
            assert_eq!(
                format!("{:?}", by_name(old).accent),
                format!("{:?}", by_name(new).accent),
                "{old} must still resolve to the {new} palette"
            );
        }
    }

    #[test]
    fn from_terminal_preserves_the_default_background() {
        let dark = TerminalColors {
            fg: [0xee, 0xee, 0xee],
            bg: [0x1a, 0x1a, 0x2e],
            palette: crate::terminal::theme_probe::default_ansi_palette(
                [0xee; 3],
                [0x1a, 0x1a, 0x2e],
            ),
        };
        let light = TerminalColors {
            fg: [0x33, 0x33, 0x33],
            bg: [0xf5, 0xf5, 0xf0],
            palette: crate::terminal::theme_probe::default_ansi_palette(
                [0x33; 3],
                [0xf5, 0xf5, 0xf0],
            ),
        };

        for colors in [&dark, &light] {
            let theme = Theme::from_terminal(colors);
            assert_eq!(theme.crust, Color::Reset);
            assert_eq!(theme.mantle, Color::Reset);
            assert_eq!(theme.base, Color::Reset);
            assert_eq!(theme.surface0, Color::Reset);
            assert_eq!(theme.text, pal(colors.fg));
            assert_eq!(theme.accent, pal(colors.palette[4]));
        }
    }

    #[test]
    fn terminal_fallback_uses_the_terminal_palette() {
        let terminal = by_name("terminal");
        assert_eq!(terminal.base, Color::Reset);
        assert_eq!(terminal.text, Color::Reset);
        assert_eq!(terminal.accent, Color::Indexed(4));
        assert_eq!(terminal.green, Color::Indexed(2));
        assert_eq!(terminal.mint, Color::Indexed(6));
        assert_eq!(terminal.amber, Color::Indexed(3));
        assert_eq!(terminal.coral, Color::Indexed(1));

        let quattro = by_name("quattro-rally");
        assert_ne!(terminal.base, quattro.base);
        assert_ne!(terminal.accent, quattro.accent);
    }

    #[test]
    fn composer_surface_adapts_to_every_theme_and_256_color_variant() {
        for &name in THEMES {
            let theme = by_name(name);
            let composer = theme.composer_surface();
            assert_ne!(
                composer, theme.mantle,
                "{name} needs a visible composer surface"
            );
            if let (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) = (theme.mantle, composer) {
                let distance = ar.abs_diff(br).max(ag.abs_diff(bg)).max(ab.abs_diff(bb));
                assert!(
                    distance >= 15,
                    "{name} composer is still too close to its pane background: {distance}"
                );
            }

            let reduced = theme.to_256();
            assert_ne!(
                reduced.composer_surface(),
                reduced.mantle,
                "{name} needs a visible 256-color composer surface"
            );
        }

        let default = by_name("quattro-rally");
        assert_eq!(
            default.subtle_composer_surface(),
            Color::Rgb(0x24, 0x27, 0x38),
            "the default theme keeps its original softer composer treatment"
        );
    }
}

#[cfg(all(test, feature = "dev-tools"))]
mod theme_css {
    /// Dev tool (not a CI check), mirroring `generate_preview`: emit every
    /// palette in [`super::THEMES`] as CSS custom properties for the website's
    /// theme picker, so luvus.dev shows the *real* palettes rather than
    /// hand-copied approximations that drift.
    ///
    /// `cargo test --features dev-tools emit_theme_css -- --nocapture`
    #[test]
    fn emit_theme_css() {
        fn hex(c: ratatui::style::Color) -> String {
            match c {
                ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                other => format!("/* {other:?} */ #000000"),
            }
        }
        for name in super::THEMES {
            let t = super::by_name(name);
            // `html[...]` outranks the pages' own `:root` block, so a picked
            // theme wins without relying on stylesheet order.
            // `html[...]` outranks the pages' own `:root` block, so a picked
            // theme wins without relying on stylesheet order.
            println!("html[data-bh-theme='{name}'] {{");
            // Page surfaces, darkest first.
            println!("  --bg: {};", hex(t.crust));
            println!("  --bg2: {};", hex(t.mantle));
            println!("  --base: {};", hex(t.base));
            println!("  --surface: {};", hex(t.surface0));
            // The site's hairline is `surface1`; `border` is the brighter rule
            // luvus uses for focused pane edges.
            println!("  --line: {};", hex(t.surface1));
            println!("  --border: {};", hex(t.border));
            println!("  --border-focus: {};", hex(t.border_focus));
            println!("  --overlay0: {};", hex(t.overlay0));
            println!("  --overlay1: {};", hex(t.overlay1));
            println!("  --sub: {};", hex(t.subtext0));
            println!("  --sub2: {};", hex(t.subtext1));
            println!("  --text: {};", hex(t.text));
            println!("  --accent: {};", hex(t.accent));
            println!("  --sel: {};", hex(t.sel_bg));
            println!("  --green: {};", hex(t.green));
            println!("  --mint: {};", hex(t.mint));
            println!("  --amber: {};", hex(t.amber));
            println!("  --coral: {};", hex(t.coral));
            println!("}}");
            // The website's theme-picker swatches, so the site never has to
            // juggle `data-bh-theme` in JS just to read an accent colour.
            println!(
                ".td-sw[data-theme='{name}'] {{ background: {}; }}",
                hex(t.accent)
            );
        }
    }
}
