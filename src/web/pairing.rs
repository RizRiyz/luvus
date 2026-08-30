use std::time::{Duration, Instant};

const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const MAX_ATTEMPTS: u8 = 5;

#[derive(Debug)]
pub(super) struct Pairing {
    code: String,
    expires_at: Instant,
    attempts: u8,
    used: bool,
}

impl Pairing {
    pub(super) fn new(ttl: Duration) -> Result<Self, String> {
        let mut entropy = [0_u8; 8];
        getrandom::fill(&mut entropy)
            .map_err(|error| format!("operating-system RNG failed: {error}"))?;

        // Twelve base32 symbols carry exactly 60 uniformly random bits. The
        // alphabet omits visually ambiguous characters for mobile entry.
        let mut bits = u64::from_le_bytes(entropy);
        let mut raw = String::with_capacity(12);
        for _ in 0..12 {
            raw.push(ALPHABET[(bits & 31) as usize] as char);
            bits >>= 5;
        }
        let code = format!("{}-{}-{}", &raw[..4], &raw[4..8], &raw[8..]);
        Ok(Self {
            code,
            expires_at: Instant::now() + ttl,
            attempts: 0,
            used: false,
        })
    }

    pub(super) fn display_code(&self) -> &str {
        &self.code
    }

    pub(super) fn consume(&mut self, candidate: &str) -> bool {
        if self.used || self.attempts >= MAX_ATTEMPTS || Instant::now() >= self.expires_at {
            return false;
        }
        self.attempts = self.attempts.saturating_add(1);
        if constant_time_eq(self.code.as_bytes(), candidate.as_bytes()) {
            self.used = true;
            true
        } else {
            false
        }
    }
}

fn constant_time_eq(expected: &[u8], candidate: &[u8]) -> bool {
    let mut difference = expected.len() ^ candidate.len();
    let length = expected.len().max(candidate.len());
    for index in 0..length {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = candidate.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_has_twelve_unambiguous_base32_symbols() {
        let pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let symbols: String = pairing
            .display_code()
            .chars()
            .filter(|character| *character != '-')
            .collect();
        assert_eq!(symbols.len(), 12);
        assert!(symbols.bytes().all(|byte| ALPHABET.contains(&byte)));
    }

    #[test]
    fn pairing_is_one_use() {
        let mut pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let code = pairing.display_code().to_string();
        assert!(pairing.consume(&code));
        assert!(!pairing.consume(&code));
    }

    #[test]
    fn pairing_exhausts_after_five_failures() {
        let mut pairing = Pairing::new(Duration::from_secs(60)).unwrap();
        let code = pairing.display_code().to_string();
        for _ in 0..MAX_ATTEMPTS {
            assert!(!pairing.consume("WRONG-CODE"));
        }
        assert!(!pairing.consume(&code));
    }

    #[test]
    fn expired_pairing_is_rejected() {
        let mut pairing = Pairing::new(Duration::ZERO).unwrap();
        let code = pairing.display_code().to_string();
        assert!(!pairing.consume(&code));
    }

    #[test]
    fn constant_time_comparison_checks_length_and_contents() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"samf"));
        assert!(!constant_time_eq(b"same", b"same-longer"));
    }
}
