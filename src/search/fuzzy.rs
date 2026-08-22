//! Small deterministic fuzzy scorer owned by Luvus (docs/90 FIND-1).

use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct PreparedText {
    original: Arc<str>,
    folded: Vec<u8>,
    /// One bit per ASCII byte. Unicode candidates derive boundaries only while
    /// being scored, keeping the cached catalog close to two source copies.
    boundaries: Vec<u8>,
    ascii: bool,
}

impl PreparedText {
    pub fn new(text: &str) -> Self {
        Self::from_shared(Arc::<str>::from(text))
    }

    pub fn from_shared(original: Arc<str>) -> Self {
        let ascii = original.is_ascii();
        let folded = if ascii {
            original
                .bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .collect()
        } else {
            original.to_lowercase().into_bytes()
        };
        let mut boundaries = vec![0u8; original.len().div_ceil(8)];
        if ascii {
            let bytes = original.as_bytes();
            for index in 0..bytes.len() {
                let boundary = index == 0
                    || matches!(
                        bytes[index - 1],
                        b'/' | b'\\' | b'-' | b'_' | b'.' | b' ' | b'\t'
                    )
                    || (bytes[index - 1].is_ascii_lowercase() && bytes[index].is_ascii_uppercase());
                if boundary {
                    boundaries[index / 8] |= 1 << (index % 8);
                }
            }
        }
        Self {
            original,
            folded,
            boundaries,
            ascii,
        }
    }

    #[cfg(test)]
    pub fn original(&self) -> &str {
        &self.original
    }

    fn is_boundary(&self, index: usize) -> bool {
        self.boundaries
            .get(index / 8)
            .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
    }

    pub(crate) fn index_bytes(&self) -> usize {
        self.folded.len().saturating_add(self.boundaries.len())
    }
}

#[derive(Clone, Debug)]
struct Token {
    folded: String,
    exact: String,
}

#[derive(Clone, Debug)]
pub struct FuzzyQuery {
    tokens: Vec<Token>,
    case_sensitive: bool,
}

pub struct FuzzyField<'a> {
    pub text: &'a PreparedText,
    pub weight: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyScore {
    pub value: i64,
    pub field: usize,
    pub byte_positions: Vec<usize>,
    output_safe: bool,
}

impl FuzzyScore {
    /// Output rows are much noisier than short navigation labels. Exact,
    /// prefix, and contiguous hits are always safe; an ordered subsequence is
    /// safe only when its characters occupy at least half of the matched span.
    pub fn output_safe(&self) -> bool {
        self.output_safe
    }
}

impl FuzzyQuery {
    pub fn new(query: &str, case_sensitive: bool) -> Self {
        let tokens = query
            .split_whitespace()
            .filter(|token| !token.is_empty())
            .map(|token| Token {
                folded: token.to_lowercase(),
                exact: token.to_string(),
            })
            .collect();
        Self {
            tokens,
            case_sensitive,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub fn char_count(&self) -> usize {
        self.tokens
            .iter()
            .map(|token| token.folded.chars().count())
            .sum()
    }

    pub fn score(&self, fields: &[FuzzyField<'_>]) -> Option<FuzzyScore> {
        if self.tokens.is_empty() || fields.is_empty() {
            return None;
        }
        let mut total = 0i64;
        let mut primary_field = usize::MAX;
        let mut primary_positions = Vec::new();
        let mut output_safe = true;
        for token in &self.tokens {
            let mut best: Option<(i64, usize, Vec<usize>, bool)> = None;
            for (field_index, field) in fields.iter().enumerate() {
                let scored =
                    if field.text.ascii && token.exact.is_ascii() && token.folded.is_ascii() {
                        let hay = if self.case_sensitive {
                            field.text.original.as_bytes()
                        } else {
                            field.text.folded.as_slice()
                        };
                        let needle = if self.case_sensitive {
                            token.exact.as_bytes()
                        } else {
                            token.folded.as_bytes()
                        };
                        score_one(
                            hay,
                            needle,
                            |index| field.text.is_boundary(index),
                            field.weight,
                        )
                    } else {
                        let (hay, bytes, boundaries) =
                            unicode_chars(&field.text.original, self.case_sensitive);
                        let needle = if self.case_sensitive {
                            token.exact.chars().collect::<Vec<_>>()
                        } else {
                            token.folded.chars().collect::<Vec<_>>()
                        };
                        score_one(
                            &hay,
                            &needle,
                            |index| boundaries.get(index).copied().unwrap_or(false),
                            field.weight,
                        )
                        .map(|(score, positions, safe)| {
                            (
                                score,
                                positions
                                    .into_iter()
                                    .filter_map(|index| bytes.get(index).copied())
                                    .collect(),
                                safe,
                            )
                        })
                    };
                let Some((score, positions, safe)) = scored else {
                    continue;
                };
                let candidate = (score, field_index, positions, safe);
                if best.as_ref().is_none_or(|current| candidate.0 > current.0) {
                    best = Some(candidate);
                }
            }
            let (score, field, positions, safe) = best?;
            total = total.saturating_add(score);
            output_safe &= safe;
            if primary_field == usize::MAX || field < primary_field {
                primary_field = field;
                primary_positions = positions;
            } else if field == primary_field {
                primary_positions.extend(positions);
                primary_positions.sort_unstable();
                primary_positions.dedup();
            }
        }
        Some(FuzzyScore {
            value: total,
            field: primary_field,
            byte_positions: primary_positions,
            output_safe,
        })
    }
}

fn score_one<T: Eq>(
    hay: &[T],
    needle: &[T],
    boundary: impl Fn(usize) -> bool,
    field_weight: i64,
) -> Option<(i64, Vec<usize>, bool)> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    if hay == needle {
        return Some((12_000 + field_weight, (0..needle.len()).collect(), true));
    }
    if hay.starts_with(needle) {
        return Some((
            9_000 + field_weight - hay.len() as i64,
            (0..needle.len()).collect(),
            true,
        ));
    }
    if let Some(start) = hay
        .windows(needle.len())
        .position(|window| window == needle)
    {
        return Some((
            6_500 + field_weight + if boundary(start) { 300 } else { 0 }
                - start as i64 * 4
                - (hay.len() - needle.len()) as i64,
            (start..start + needle.len()).collect(),
            true,
        ));
    }

    let mut positions = Vec::with_capacity(needle.len());
    let mut cursor = 0usize;
    let mut score = 2_000 + field_weight;
    for item in needle {
        let relative = hay[cursor..]
            .iter()
            .position(|candidate| candidate == item)?;
        let position = cursor + relative;
        if positions
            .last()
            .is_some_and(|previous| *previous + 1 == position)
        {
            score += 180;
        }
        if boundary(position) {
            score += 120;
        }
        score -= relative as i64 * 18;
        positions.push(position);
        cursor = position + 1;
    }
    let span = positions.last().copied().unwrap_or(0) - positions[0] + 1;
    score -= (span.saturating_sub(needle.len()) as i64) * 12;
    score -= (hay.len().saturating_sub(needle.len()) as i64).min(400);
    let output_safe = needle.len().saturating_mul(2) >= span;
    Some((score, positions, output_safe))
}

fn unicode_chars(text: &str, case_sensitive: bool) -> (Vec<char>, Vec<usize>, Vec<bool>) {
    let mut chars = Vec::new();
    let mut bytes = Vec::new();
    let mut boundaries = Vec::new();
    let mut previous = None;
    for (byte, ch) in text.char_indices() {
        let boundary = previous.is_none_or(|p: char| {
            matches!(p, '/' | '\\' | '-' | '_' | '.' | ' ' | '\t')
                || (p.is_lowercase() && ch.is_uppercase())
        });
        if case_sensitive {
            chars.push(ch);
            bytes.push(byte);
            boundaries.push(boundary);
        } else {
            for lower in ch.to_lowercase() {
                chars.push(lower);
                bytes.push(byte);
                boundaries.push(boundary);
            }
        }
        previous = Some(ch);
    }
    (chars, bytes, boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, text: &str) -> Option<FuzzyScore> {
        let text = PreparedText::new(text);
        FuzzyQuery::new(query, false).score(&[FuzzyField {
            text: &text,
            weight: 0,
        }])
    }

    #[test]
    fn exact_prefix_contiguous_and_subsequence_order() {
        let exact = score("api", "api").unwrap().value;
        let prefix = score("api", "api-client").unwrap().value;
        let contiguous = score("api", "my-api-client").unwrap().value;
        let sparse = score("api", "a-path-item").unwrap().value;
        assert!(exact > prefix && prefix > contiguous && contiguous > sparse);
    }

    #[test]
    fn output_rejects_sparse_but_keeps_dense_subsequences() {
        assert!(score("bld", "build failed").unwrap().output_safe());
        assert!(!score("fzpn1fl", "fuzzy-benchmark-pane-1-auth-failure")
            .unwrap()
            .output_safe());
        assert!(score("auth", "fuzzy benchmark auth failure")
            .unwrap()
            .output_safe());
    }

    #[test]
    fn path_and_camel_boundaries_beat_unstructured_gaps() {
        assert!(
            score("sui", "src/ui/input.rs").unwrap().value
                > score("sui", "some_unrelated_item").unwrap().value
        );
        assert!(
            score("gsc", "GlobalSearchCatalog").unwrap().value
                > score("gsc", "long generic source code").unwrap().value
        );
    }

    #[test]
    fn tokens_may_match_different_fields() {
        let a = PreparedText::new("tests");
        let b = PreparedText::new("default > api");
        assert!(FuzzyQuery::new("api test", false)
            .score(&[
                FuzzyField {
                    text: &a,
                    weight: 10
                },
                FuzzyField {
                    text: &b,
                    weight: 0
                },
            ])
            .is_some());
    }

    #[test]
    fn unicode_highlights_use_original_byte_boundaries() {
        let text = PreparedText::new("Ångström");
        let got = FuzzyQuery::new("ång", false)
            .score(&[FuzzyField {
                text: &text,
                weight: 0,
            }])
            .unwrap();
        assert_eq!(got.byte_positions, vec![0, 2, 3]);
        for byte in got.byte_positions {
            assert!(text.original().is_char_boundary(byte));
        }
    }

    #[test]
    fn case_sensitive_matching_is_strict() {
        let text = PreparedText::new("ReadMe");
        assert!(FuzzyQuery::new("RM", true)
            .score(&[FuzzyField {
                text: &text,
                weight: 0
            }])
            .is_some());
        assert!(FuzzyQuery::new("rm", true)
            .score(&[FuzzyField {
                text: &text,
                weight: 0
            }])
            .is_none());
    }

    #[test]
    fn ascii_catalog_storage_stays_close_to_two_source_copies() {
        let text = PreparedText::new(&"x".repeat(8192));
        assert_eq!(text.folded.len(), 8192);
        assert_eq!(text.boundaries.len(), 1024);
    }
}
