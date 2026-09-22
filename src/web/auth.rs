use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use getrandom::fill;
use sha2::{Digest, Sha256};

const PAIRING_SECONDS: u64 = 5 * 60;

#[derive(Clone, Debug)]
pub(super) struct BrowserPairing {
    pub code: String,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BrowserDeviceStatus {
    pub paired_devices: usize,
    pub pending_pairings: usize,
    pub max_devices: usize,
}

pub(super) struct Authentication {
    pub ticket: Option<String>,
    pub expires_at: u64,
}

pub(super) struct BrowserAuthority {
    ticket_seconds: u64,
    max_devices: usize,
    pairings: HashMap<[u8; 32], u64>,
    tickets: HashMap<[u8; 32], u64>,
}

impl BrowserAuthority {
    pub fn new(ticket_seconds: u64, max_devices: usize) -> Result<(Self, BrowserPairing), String> {
        let mut authority = Self {
            ticket_seconds,
            max_devices,
            pairings: HashMap::new(),
            tickets: HashMap::new(),
        };
        let initial = authority
            .create_pairing()
            .ok_or_else(|| "browser device limit must allow initial pairing".to_string())?;
        Ok((authority, initial))
    }

    pub fn authenticate(
        &mut self,
        code: Option<&str>,
        ticket: Option<&str>,
    ) -> Option<Authentication> {
        self.purge();
        if let Some(ticket) = ticket {
            if let Some(expires_at) = self.tickets.get(&digest(ticket)).copied() {
                if expires_at > unix_now() {
                    return Some(Authentication {
                        ticket: None,
                        expires_at,
                    });
                }
            }
        }
        let code = code?;
        let expires_at = self.pairings.remove(&digest(code))?;
        if expires_at <= unix_now() || self.tickets.len() >= self.max_devices {
            return None;
        }
        let ticket = random_token(32).ok()?;
        let expires_at = unix_now().saturating_add(self.ticket_seconds);
        self.tickets.insert(digest(&ticket), expires_at);
        Some(Authentication {
            ticket: Some(ticket),
            expires_at,
        })
    }

    pub fn create_pairing(&mut self) -> Option<BrowserPairing> {
        self.purge();
        if self.tickets.len() + self.pairings.len() >= self.max_devices {
            return None;
        }
        let code = random_token(24).ok()?;
        let expires_at = unix_now().saturating_add(PAIRING_SECONDS);
        self.pairings.insert(digest(&code), expires_at);
        Some(BrowserPairing { code, expires_at })
    }

    pub fn set_max_devices(&mut self, max_devices: usize) -> bool {
        self.purge();
        if max_devices < self.tickets.len() + self.pairings.len() {
            return false;
        }
        self.max_devices = max_devices;
        true
    }

    pub fn status(&mut self) -> BrowserDeviceStatus {
        self.purge();
        BrowserDeviceStatus {
            paired_devices: self.tickets.len(),
            pending_pairings: self.pairings.len(),
            max_devices: self.max_devices,
        }
    }

    fn purge(&mut self) {
        let now = unix_now();
        self.pairings.retain(|_, expires_at| *expires_at > now);
        self.tickets.retain(|_, expires_at| *expires_at > now);
    }
}

fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

fn random_token(bytes: usize) -> Result<String, getrandom::Error> {
    let mut random = vec![0_u8; bytes];
    fill(&mut random)?;
    Ok(base64_url(&random))
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 0x3f) as usize] as char);
        }
    }
    output
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_is_one_use_and_tickets_are_independent() {
        let (mut authority, initial) = BrowserAuthority::new(600, 2).unwrap();
        let desktop = authority.authenticate(Some(&initial.code), None).unwrap();
        assert!(authority.authenticate(Some(&initial.code), None).is_none());
        assert!(authority
            .authenticate(None, desktop.ticket.as_deref())
            .is_some());

        let phone = authority.create_pairing().unwrap();
        let phone = authority.authenticate(Some(&phone.code), None).unwrap();
        assert_ne!(desktop.ticket, phone.ticket);
        assert_eq!(
            authority.status(),
            BrowserDeviceStatus {
                paired_devices: 2,
                pending_pairings: 0,
                max_devices: 2,
            }
        );
        assert!(authority.create_pairing().is_none());
        assert!(!authority.set_max_devices(1));
    }

    #[test]
    fn url_tokens_use_only_unreserved_characters() {
        for length in 1..=64 {
            let token = random_token(length).unwrap();
            assert!(token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
        }
    }
}
