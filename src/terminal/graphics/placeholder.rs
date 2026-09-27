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
//! Cells arrive here two ways. A client that knows it is talking through a
//! multiplexer writes them itself, as plain text. A client that believes it has
//! the terminal to itself instead asks for the image to be placed at the
//! cursor, and Luvus builds the cells on its behalf — see
//! [`crate::terminal::graphics`].
//!
//! Either way they must be kept out of extracted text: a placeholder is an
//! image cell, not something anyone typed or printed.

use std::borrow::Cow;

/// The placeholder character itself.
///
/// A cell holding this is an image cell. Its combining marks encode a
/// coordinate rather than an accent, so they carry no meaning as text either.
pub(crate) const PLACEHOLDER: char = '\u{10eeee}';

/// Diacritics encoding a row or column number, indexed by that number.
///
/// This is kitty's `rowcolumn-diacritics.txt` verbatim: the combining
/// characters of class 230 that have no decomposition mapping, so normalizing a
/// placeholder cell cannot fuse it into a different character. A terminal
/// decodes a coordinate by position in exactly this order, so the list must
/// never be reordered or trimmed.
#[rustfmt::skip]
const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}',
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}',
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}',
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}',
    '\u{1d187}', '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}',
    '\u{1d242}', '\u{1d243}', '\u{1d244}',
];

/// Largest row or column a placeholder can address.
///
/// The table length is a hard protocol ceiling, not a Luvus limit. No terminal
/// is anywhere near 297 cells tall, but an image is refused rather than
/// truncated if one ever were.
pub(crate) const MAX_EXTENT: usize = DIACRITICS.len();

/// The diacritic encoding `index` as a row or column number.
pub(crate) fn diacritic(index: usize) -> Option<char> {
    DIACRITICS.get(index).copied()
}

/// Whether one cell's symbol is an image cell rather than text.
///
/// The base character decides it, so the cell is recognized whichever
/// coordinate marks it carries — including none, which the protocol allows for
/// a cell that inherits its coordinates from its left neighbour.
pub(crate) fn is_placeholder(symbol: &str) -> bool {
    symbol.starts_with(PLACEHOLDER)
}

/// Replace placeholder cells in already-extracted text with blanks.
///
/// Prefer filtering per cell where the grid is still available: a cell knows
/// exactly which marks are its own. This exists for text that came back from
/// the terminal engine as a finished string, where the only way to tell a
/// coordinate mark from the surrounding text is that it follows a placeholder
/// and belongs to the protocol's coordinate table.
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
            .is_some_and(|next| DIACRITICS.contains(next))
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
    fn the_table_matches_the_values_the_protocol_publishes() {
        // kitty's specification writes 0, 1 and 2 out explicitly.
        assert_eq!(diacritic(0), Some('\u{0305}'));
        assert_eq!(diacritic(1), Some('\u{030d}'));
        assert_eq!(diacritic(2), Some('\u{030e}'));
        assert_eq!(diacritic(296), Some('\u{1d244}'));
        assert_eq!(diacritic(297), None, "past the table is not addressable");
        assert_eq!(MAX_EXTENT, 297);
    }

    #[test]
    fn every_diacritic_is_distinct() {
        // A repeat would silently alias two coordinates onto each other.
        let mut sorted = DIACRITICS;
        sorted.sort_unstable();
        let mut unique = sorted.to_vec();
        unique.dedup();
        assert_eq!(unique.len(), DIACRITICS.len());
    }

    #[test]
    fn every_diacritic_is_zero_width() {
        // The scheme relies on one placeholder grapheme staying one cell wide,
        // however many coordinate marks it carries.
        for (index, mark) in DIACRITICS.iter().enumerate() {
            assert_eq!(
                unicode_width::UnicodeWidthChar::width(*mark),
                Some(0),
                "diacritic {index} ({mark:?}) would take a cell of its own"
            );
        }
    }

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

    /// Zero width alone does not make a character a kitty coordinate.
    #[test]
    fn review_stripping_preserves_non_coordinate_zero_width_text() {
        for character in ['\u{200b}', '\u{200d}', '\u{0301}', '\u{fe0f}'] {
            assert!(!DIACRITICS.contains(&character));
            let selected = format!("a{PLACEHOLDER}\u{0305}\u{030d}{character}b");
            assert_eq!(strip(&selected), format!("a {character}b"));
        }
    }

    #[test]
    fn an_image_cell_is_recognized_by_its_base_character() {
        assert!(is_placeholder("\u{10eeee}"));
        assert!(is_placeholder("\u{10eeee}\u{0305}\u{030d}"));
        assert!(!is_placeholder("x"));
        assert!(!is_placeholder(""));
        // A bare combining mark is not an image cell.
        assert!(!is_placeholder("\u{0305}"));
    }

    #[test]
    fn a_placeholder_without_marks_still_becomes_a_blank() {
        // The protocol lets a cell inherit its coordinates from the cell to its
        // left, so a bare placeholder is a legitimate image cell.
        assert_eq!(strip("a\u{10eeee}b"), "a b");
    }
}
