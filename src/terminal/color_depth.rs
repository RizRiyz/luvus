//! Display-client color policy, independent of the server and child PTYs.
//!
//! `COLORTERM` is commonly lost across SSH. Its absence is not evidence that a
//! color terminal cannot display RGB; quantizing then permanently loses colors
//! before the terminal (or an intervening multiplexer) can render them.

pub fn truecolor_supported() -> bool {
    truecolor_supported_with(|key| std::env::var(key).ok())
}

fn truecolor_supported_with(mut read_env: impl FnMut(&str) -> Option<String>) -> bool {
    if let Some(color_term) = read_env("COLORTERM").filter(|value| !value.trim().is_empty()) {
        // Keep the explicit limited-color opt-out, including the historical
        // behavior for non-RGB COLORTERM values. No terminal query reads stdin.
        return matches!(
            color_term.trim().to_ascii_lowercase().as_str(),
            "truecolor" | "24bit"
        );
    }

    let term = read_env("TERM").unwrap_or_default().to_ascii_lowercase();
    if matches!(
        term.as_str(),
        "dumb"
            | "ansi"
            | "linux"
            | "cons25"
            | "cons25r"
            | "vt100"
            | "vt102"
            | "vt220"
            | "vt320"
            | "vt420"
            | "vt520"
    ) {
        return false;
    }

    // Preserve the conservative fallback for the native Apple Terminal path;
    // an explicit RGB COLORTERM above can opt in on newer compatible versions.
    if read_env("TERM_PROGRAM").as_deref() == Some("Apple_Terminal") {
        return false;
    }

    // Windows Terminal need not set TERM or COLORTERM for native clients.
    if read_env("WT_SESSION").is_some_and(|value| !value.is_empty()) {
        return true;
    }

    // TERM describes a terminal family/indexed palette, not an RGB ceiling.
    // In particular, xterm-256color is also used by truecolor terminals over
    // SSH. Preserve RGB for these clients; genuinely limited color terminals
    // can request the existing quantizer with COLORTERM=256color.
    term.ends_with("-256color")
        || term.ends_with("-direct")
        || term.ends_with("-truecolor")
        || term.ends_with("-24bit")
        || matches!(
            term.as_str(),
            "alacritty" | "xterm-kitty" | "foot" | "wezterm"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supports(values: &[(&str, &str)]) -> bool {
        truecolor_supported_with(|key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        })
    }

    #[test]
    fn missing_colorterm_preserves_rgb_over_ssh() {
        for term in [
            "xterm-256color",
            "xterm-direct",
            "screen-256color",
            "tmux-256color",
        ] {
            assert!(
                supports(&[("TERM", term), ("SSH_TTY", "/dev/pts/1")]),
                "{term}"
            );
        }
        assert!(supports(&[("TERM", "xterm-256color"), ("COLORTERM", "")]));
    }

    #[test]
    fn explicit_colorterm_controls_color_depth() {
        for value in ["truecolor", "24bit", "TRUECOLOR", " 24bit "] {
            assert!(supports(&[("COLORTERM", value)]), "{value}");
        }
        for value in ["256color", "8bit", "16color", "not-truecolor"] {
            assert!(
                !supports(&[("TERM", "xterm-direct"), ("COLORTERM", value)]),
                "{value}"
            );
        }
    }

    #[test]
    fn limited_and_unknown_terminals_keep_the_fallback() {
        assert!(!supports(&[]));
        assert!(!supports(&[("TERM", "")]));
        assert!(!supports(&[("TERM", "unknown")]));
        for term in [
            "dumb", "ansi", "linux", "cons25", "cons25r", "vt100", "vt102", "vt220", "vt320",
            "vt420", "vt520",
        ] {
            assert!(!supports(&[("TERM", term)]), "{term}");
        }
        assert!(!supports(&[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "Apple_Terminal")
        ]));
        assert!(supports(&[
            ("TERM_PROGRAM", "Apple_Terminal"),
            ("COLORTERM", "truecolor")
        ]));
    }

    #[test]
    fn native_windows_terminal_has_an_explicit_rgb_identity() {
        assert!(supports(&[("WT_SESSION", "test-session")]));
        assert!(!supports(&[("WT_SESSION", "")]));
        assert!(!supports(&[
            ("WT_SESSION", "test-session"),
            ("COLORTERM", "256color")
        ]));
        assert!(!supports(&[
            ("WT_SESSION", "test-session"),
            ("TERM", "dumb")
        ]));
    }
}
