//! Shared Unicode case semantics for exact and fuzzy search.
//!
//! Case-insensitive search uses Unicode's default simple C/S folds. Each source
//! scalar produces exactly one folded scalar, so callers can retain original
//! UTF-8 byte offsets without normalization or expansion mapping.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FoldedChar {
    pub source_byte: usize,
    pub source: char,
    pub folded: char,
}

/// Fold one scalar while preserving strict matching in case-sensitive mode.
pub(crate) fn fold_char(ch: char, case_sensitive: bool) -> char {
    if case_sensitive {
        ch
    } else {
        ::casefold::simple_fold_char(ch)
    }
}

/// Fold text with the same one-to-one scalar contract used by every search.
pub(crate) fn fold_text(text: &str) -> String {
    text.chars()
        .map(::casefold::simple_fold_char)
        .collect::<String>()
}

/// Project folded scalars while retaining their original UTF-8 byte positions.
pub(crate) fn chars_with_source(
    text: &str,
    case_sensitive: bool,
) -> impl Iterator<Item = FoldedChar> + '_ {
    text.char_indices()
        .map(move |(source_byte, source)| FoldedChar {
            source_byte,
            source,
            folded: fold_char(source, case_sensitive),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_simple_fold_contract_is_shared_and_scalar_preserving() {
        assert_eq!(fold_text("IΣςſ"), "iσσs");
        assert_eq!(fold_text("ıß"), "ıß");
        assert_ne!(fold_text("i"), fold_text("ı"));
        assert_ne!(fold_text("ss"), fold_text("ß"));
    }

    #[test]
    fn folded_chars_keep_original_utf8_offsets() {
        let projected = chars_with_source("Åςı", false).collect::<Vec<_>>();
        assert_eq!(
            projected,
            vec![
                FoldedChar {
                    source_byte: 0,
                    source: 'Å',
                    folded: 'å',
                },
                FoldedChar {
                    source_byte: 2,
                    source: 'ς',
                    folded: 'σ',
                },
                FoldedChar {
                    source_byte: 4,
                    source: 'ı',
                    folded: 'ı',
                },
            ]
        );
    }
}
