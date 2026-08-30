//! Effective pane appearance exposed to child terminal applications.
//!
//! Static themes provide an explicit dark/light preference. The virtual
//! Terminal theme instead follows the last host-terminal probe. Each VT engine
//! owns a copy so OSC 11 and DEC mode 2031 stay correct without process-global
//! mutable state.

use ratatui::style::Color;

use crate::terminal::theme_probe::TerminalColors;
use crate::theme::format::Appearance as ThemeAppearance;

/// Bundled fallback pane background (Quattro Rally mantle) used until a
/// Terminal-theme host probe is available.
pub const DEFAULT_BG: [u8; 3] = [0x1e, 0x20, 0x30];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Dark,
    Light,
}

impl ColorScheme {
    /// Infer the virtual Terminal theme's scheme from its probed foreground and
    /// background. Static themes use their declared appearance instead.
    pub fn from_terminal_colors(colors: &TerminalColors) -> Self {
        if luminance(colors.bg) < luminance(colors.fg) {
            Self::Dark
        } else {
            Self::Light
        }
    }

    pub fn is_dark(self) -> bool {
        self == Self::Dark
    }

    pub fn dsr(self) -> &'static [u8] {
        match self {
            Self::Dark => b"\x1b[?997;1n",
            Self::Light => b"\x1b[?997;2n",
        }
    }
}

/// The background and declared color-scheme preference visible to one child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneAppearance {
    pub background: [u8; 3],
    pub scheme: ColorScheme,
}

impl PaneAppearance {
    pub fn resolve(
        background_color: Color,
        declared: ThemeAppearance,
        probed: Option<Self>,
    ) -> Self {
        let background = rgb_color(background_color)
            .or_else(|| probed.map(|appearance| appearance.background))
            .unwrap_or(DEFAULT_BG);
        let scheme = match declared {
            ThemeAppearance::Dark => ColorScheme::Dark,
            ThemeAppearance::Light => ColorScheme::Light,
            ThemeAppearance::Terminal => probed
                .map(|appearance| appearance.scheme)
                .unwrap_or(ColorScheme::Dark),
        };
        Self { background, scheme }
    }

    pub fn from_terminal_colors(colors: &TerminalColors) -> Self {
        Self {
            background: colors.bg,
            scheme: ColorScheme::from_terminal_colors(colors),
        }
    }
}

impl Default for PaneAppearance {
    fn default() -> Self {
        Self {
            background: DEFAULT_BG,
            scheme: ColorScheme::Dark,
        }
    }
}

fn luminance(rgb: [u8; 3]) -> f32 {
    0.2126 * (rgb[0] as f32 / 255.0)
        + 0.7152 * (rgb[1] as f32 / 255.0)
        + 0.0722 * (rgb[2] as f32 / 255.0)
}

fn rgb_color(color: Color) -> Option<[u8; 3]> {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_probed_terminal_colors_from_their_contrast() {
        let mut colors = TerminalColors {
            fg: [0xca, 0xd3, 0xf5],
            bg: [0x1e, 0x20, 0x30],
            palette: [[0; 3]; 16],
        };
        assert_eq!(
            ColorScheme::from_terminal_colors(&colors),
            ColorScheme::Dark
        );
        colors.fg = [0x4c, 0x4f, 0x69];
        colors.bg = [0xef, 0xf1, 0xf5];
        assert_eq!(
            ColorScheme::from_terminal_colors(&colors),
            ColorScheme::Light
        );
        assert_eq!(ColorScheme::Dark.dsr(), b"\x1b[?997;1n");
        assert_eq!(ColorScheme::Light.dsr(), b"\x1b[?997;2n");
    }

    #[test]
    fn static_theme_declaration_outranks_color_guessing() {
        assert_eq!(
            PaneAppearance::resolve(Color::Rgb(0xee, 0xee, 0xee), ThemeAppearance::Dark, None,)
                .scheme,
            ColorScheme::Dark
        );
    }

    #[test]
    fn terminal_reset_uses_probe_then_fallback() {
        let probed = PaneAppearance {
            background: [0xf2, 0xe5, 0xbc],
            scheme: ColorScheme::Light,
        };
        assert_eq!(
            PaneAppearance::resolve(Color::Reset, ThemeAppearance::Terminal, Some(probed)),
            probed
        );
        assert_eq!(
            PaneAppearance::resolve(Color::Reset, ThemeAppearance::Terminal, None),
            PaneAppearance::default()
        );
    }
}
