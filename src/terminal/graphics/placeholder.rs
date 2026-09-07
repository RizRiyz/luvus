//! Unicode placeholder cells for the kitty graphics protocol.
//!
//! A client can display an image by writing ordinary text: the private-use
//! character `U+10EEEE`, carrying the image id in its foreground color and its
//! coordinates within the image in combining diacritics. Because those are just
//! cells, the redraw machinery that already scrolls, clips, and reflows text
//! moves the image too, without knowing anything about graphics.
//!
//! That property is why the protocol offers this form at all, and why a
//! multiplexer wants it. tmux first relayed the graphics escape codes outward
//! instead and had to replace that: a forwarded image does not clip to its
//! pane, does not scroll with the text, and leaves a ghost behind when a pane
//! splits. None of it is fixable without putting the image in the grid.
//!
//! Luvus does not yet render these cells, but they already reach its grid,
//! because a client that believes it is talking to a graphics-capable terminal
//! writes them as plain text. They must therefore be kept out of extracted
//! text: a placeholder is an image cell, not something anyone typed or printed.

use std::borrow::Cow;

/// The placeholder character itself.
///
/// A cell holding this is an image cell. Its combining marks encode a
/// coordinate rather than an accent, so they carry no meaning as text either.
pub(crate) const PLACEHOLDER: char = '\u{10eeee}';

/// Replace placeholder cells in already-extracted text with blanks.
///
/// Prefer filtering per cell where the grid is still available: a cell knows
/// exactly which marks are its own. This exists for text that came back from
/// the terminal engine as a finished string, where the only way to tell a
/// coordinate mark from the surrounding text is that it follows a placeholder
/// and occupies no column of its own.
///
/// Each image cell becomes one space, so a selection that spanned an image
/// keeps the alignment of the text around it.
pub(crate) fn strip(text: &str) -> Cow<'_, str> {
    if !text.contains(PLACEHOLDER) {
        return Cow::Borrowed(text);
    }

    let mut stripped = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != PLACEHOLDER {
            stripped.push(character);
            continue;
        }
        stripped.push(' ');
        // The coordinate diacritics trail the placeholder and take no column.
        while characters
            .peek()
            .is_some_and(|next| unicode_width::UnicodeWidthChar::width(*next) == Some(0))
        {
            characters.next();
        }
    }
    Cow::Owned(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_placeholder_occupies_exactly_one_cell() {
        // The scheme maps one placeholder grapheme to one image cell. A
        // private-use character is East Asian "ambiguous", so a build that
        // resolved it to two columns would silently halve every image's width.
        assert_eq!(unicode_width::UnicodeWidthChar::width(PLACEHOLDER), Some(1));
    }

    #[test]
    fn an_image_cell_becomes_one_blank_and_takes_its_marks_with_it() {
        // Two image cells of a 2x1 placement, as a client writes them.
        let selection = "before \u{10eeee}\u{0305}\u{0305}\u{10eeee}\u{0305}\u{030d} after";
        assert_eq!(strip(selection), "before    after");
    }

    #[test]
    fn text_without_an_image_is_returned_untouched() {
        let plain = "an ordinary line";
        assert!(
            matches!(strip(plain), Cow::Borrowed(_)),
            "the common path must not allocate"
        );
        assert_eq!(strip(plain), plain);
    }

    #[test]
    fn combining_marks_on_real_text_are_preserved() {
        // Only marks trailing a placeholder are coordinates. An accent on a
        // letter, or an emoji's variation selector, is text the user selected.
        let accented = "e\u{0301} \u{1f5a5}\u{fe0f}";
        assert_eq!(strip(accented), accented);
    }

    #[test]
    fn a_placeholder_without_marks_still_becomes_a_blank() {
        // The protocol lets a cell inherit its coordinates from the cell to its
        // left, so a bare placeholder is a legitimate image cell.
        assert_eq!(strip("a\u{10eeee}b"), "a b");
    }
}
