use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use getrandom::fill;
use sha2::{Digest, Sha256};

const PAIRING_SECONDS: u64 = 5 * 60;
/// How long a device must have had no open connection before the operator's
/// terminal may reclaim its slot. An open tab reconnects within about ten
/// seconds, so this keeps a briefly disconnected tab from losing its access.
const RECONNECT_GRACE_SECONDS: u64 = 30;

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
    /// The highest limit a browser may choose: the operator's `--max-devices`.
    pub limit_ceiling: usize,
}

pub(super) struct Authentication {
    pub ticket: Option<String>,
    pub expires_at: u64,
    /// The ticket this connection holds, whether presented or newly issued.
    /// Used only to count live connections per device.
    pub ticket_digest: [u8; 32],
    pairing: Option<([u8; 32], u64)>,
}

/// Why a requested device limit was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LimitError {
    /// Lower than the devices and unspent links already counted.
    BelowUsed,
    /// Higher than the limit the operator chose when starting the bridge. A
    /// browser may tighten that limit but never raise it.
    AboveCeiling,
}

/// The slot freed to make room for an operator's pairing link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Reclaimed {
    /// The limit had room; nothing was revoked.
    Nothing,
    /// The oldest pairing link that nobody had used yet.
    UnusedLink,
    /// The device that had been disconnected longest.
    IdleDevice,
}

/// Why the operator could not get a pairing link.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Full {
    /// Devices that disconnected too recently to reclaim; they may be open
    /// tabs about to reconnect.
    pub reconnecting: usize,
}

struct Ticket {
    expires_at: u64,
    /// Open WebSocket connections authenticated with this ticket.
    live: usize,
    /// When `live` last dropped to zero, or when the ticket was issued.
    idle_since: Option<u64>,
}

pub(super) struct BrowserAuthority {
    ticket_seconds: u64,
    max_devices: usize,
    /// The operator's `--max-devices` value, fixed for the bridge's lifetime.
    ceiling: usize,
    reconnect_grace: u64,
    pairings: HashMap<[u8; 32], u64>,
    tickets: HashMap<[u8; 32], Ticket>,
}

impl BrowserAuthority {
    pub fn new(ticket_seconds: u64, max_devices: usize) -> Result<(Self, BrowserPairing), String> {
        let mut authority = Self {
            ticket_seconds,
            max_devices,
            ceiling: max_devices,
            reconnect_grace: RECONNECT_GRACE_SECONDS,
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
        // A valid ticket always wins, and does not spend a code sent with it:
        // reopening a used pairing link must not cost a working device its
        // access, and an unspent code stays available for another device.
        if let Some(ticket) = ticket {
            let ticket_digest = digest(ticket);
            if let Some(existing) = self.tickets.get(&ticket_digest) {
                if existing.expires_at > unix_now() {
                    return Some(Authentication {
                        ticket: None,
                        expires_at: existing.expires_at,
                        ticket_digest,
                        pairing: None,
                    });
                }
            }
        }
        let code = code?;
        let pairing = digest(code);
        let pairing_expires_at = self.pairings.remove(&pairing)?;
        if pairing_expires_at <= unix_now() || self.tickets.len() >= self.max_devices {
            return None;
        }
        let ticket = random_token(32).ok()?;
        let expires_at = unix_now().saturating_add(self.ticket_seconds);
        let ticket_digest = digest(&ticket);
        self.tickets.insert(
            ticket_digest,
            Ticket {
                expires_at,
                live: 0,
                idle_since: Some(unix_now()),
            },
        );
        Some(Authentication {
            ticket: Some(ticket),
            expires_at,
            ticket_digest,
            pairing: Some((pairing, pairing_expires_at)),
        })
    }

    pub fn rollback(&mut self, authentication: Authentication) {
        let Some((pairing, expires_at)) = authentication.pairing else {
            return;
        };
        if let Some(ticket) = authentication.ticket {
            self.tickets.remove(&digest(&ticket));
        }
        if expires_at > unix_now() {
            self.pairings.insert(pairing, expires_at);
        }
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

    /// Create a pairing link on behalf of the operator at the terminal running
    /// the bridge. When the limit is full, exactly one slot is freed: the
    /// oldest unused pairing link, which the new link replaces, or else the
    /// device disconnected longest, since a closed tab can never reuse its
    /// per-tab ticket. Connected devices, and devices that disconnected too
    /// recently to tell apart from a reconnecting tab, are never revoked.
    pub fn create_operator_pairing(&mut self) -> Result<(BrowserPairing, Reclaimed), Full> {
        self.purge();
        let mut reclaimed = Reclaimed::Nothing;
        if self.tickets.len() + self.pairings.len() >= self.max_devices {
            let now = unix_now();
            let oldest_link = self
                .pairings
                .iter()
                .min_by_key(|(_, expires_at)| **expires_at)
                .map(|(pairing, _)| *pairing);
            let idle_device = self
                .tickets
                .iter()
                .filter(|(_, ticket)| ticket.live == 0)
                .filter_map(|(digest, ticket)| Some((digest, ticket.idle_since?)))
                .filter(|(_, since)| now.saturating_sub(*since) >= self.reconnect_grace)
                .min_by_key(|(_, since)| *since)
                .map(|(digest, _)| *digest);
            if let Some(pairing) = oldest_link {
                self.pairings.remove(&pairing);
                reclaimed = Reclaimed::UnusedLink;
            } else if let Some(digest) = idle_device {
                self.tickets.remove(&digest);
                reclaimed = Reclaimed::IdleDevice;
            }
        }
        match self.create_pairing() {
            Some(pairing) => Ok((pairing, reclaimed)),
            None => Err(Full {
                reconnecting: self.tickets.values().filter(|t| t.live == 0).count(),
            }),
        }
    }

    /// Record an open connection for a ticket. Pair with [`Self::disconnect`].
    pub fn connect(&mut self, ticket_digest: &[u8; 32]) {
        if let Some(ticket) = self.tickets.get_mut(ticket_digest) {
            ticket.live += 1;
            ticket.idle_since = None;
        }
    }

    pub fn disconnect(&mut self, ticket_digest: &[u8; 32]) {
        if let Some(ticket) = self.tickets.get_mut(ticket_digest) {
            ticket.live = ticket.live.saturating_sub(1);
            if ticket.live == 0 {
                ticket.idle_since = Some(unix_now());
            }
        }
    }

    pub fn set_max_devices(&mut self, max_devices: usize) -> Result<(), LimitError> {
        self.purge();
        if max_devices > self.ceiling {
            return Err(LimitError::AboveCeiling);
        }
        if max_devices < self.tickets.len() + self.pairings.len() {
            return Err(LimitError::BelowUsed);
        }
        self.max_devices = max_devices;
        Ok(())
    }

    pub fn status(&mut self) -> BrowserDeviceStatus {
        self.purge();
        BrowserDeviceStatus {
            paired_devices: self.tickets.len(),
            pending_pairings: self.pairings.len(),
            max_devices: self.max_devices,
            limit_ceiling: self.ceiling,
        }
    }

    fn purge(&mut self) {
        let now = unix_now();
        self.pairings.retain(|_, expires_at| *expires_at > now);
        self.tickets.retain(|_, ticket| ticket.expires_at > now);
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
                limit_ceiling: 2,
            }
        );
        assert!(authority.create_pairing().is_none());
        assert_eq!(authority.set_max_devices(1), Err(LimitError::BelowUsed));
    }

    #[test]
    fn a_valid_ticket_wins_without_spending_a_code_sent_with_it() {
        let (mut authority, initial) = BrowserAuthority::new(600, 2).unwrap();
        let device = authority.authenticate(Some(&initial.code), None).unwrap();
        let spare = authority.create_pairing().unwrap();

        // Reopening a spent link from a tab that still holds its ticket.
        let again = authority
            .authenticate(Some(&initial.code), device.ticket.as_deref())
            .expect("the ticket still authenticates");
        assert!(again.ticket.is_none(), "no new ticket is issued");
        assert_eq!(again.ticket_digest, device.ticket_digest);

        // A valid ticket sent with an unspent code leaves that code unspent.
        authority
            .authenticate(Some(&spare.code), device.ticket.as_deref())
            .unwrap();
        assert!(authority.authenticate(Some(&spare.code), None).is_some());
    }

    #[test]
    fn a_browser_may_lower_but_never_raise_the_operator_limit() {
        let (mut authority, _) = BrowserAuthority::new(600, 3).unwrap();
        assert_eq!(authority.set_max_devices(4), Err(LimitError::AboveCeiling));
        assert_eq!(authority.set_max_devices(8), Err(LimitError::AboveCeiling));
        assert_eq!(authority.set_max_devices(2), Ok(()));
        assert_eq!(
            authority.set_max_devices(3),
            Ok(()),
            "back up to the ceiling"
        );
        assert_eq!(authority.status().limit_ceiling, 3);
    }

    #[test]
    fn operator_pairing_replaces_the_oldest_unused_link_first() {
        // Default limit: the initial link plus one more fill both slots.
        let (mut authority, initial) = BrowserAuthority::new(600, 2).unwrap();
        let (second, reclaimed) = authority.create_operator_pairing().unwrap();
        assert_eq!(reclaimed, Reclaimed::Nothing);
        let now = unix_now();
        authority.pairings.insert(digest(&initial.code), now + 100); // oldest
        authority.pairings.insert(digest(&second.code), now + 200);

        let (third, reclaimed) = authority.create_operator_pairing().unwrap();
        assert_eq!(reclaimed, Reclaimed::UnusedLink);
        assert!(authority.authenticate(Some(&initial.code), None).is_none());
        assert!(authority.authenticate(Some(&second.code), None).is_some());
        assert_eq!(authority.status().pending_pairings, 1);
        assert!(authority.authenticate(Some(&third.code), None).is_some());
    }

    #[test]
    fn operator_pairing_reclaims_one_long_disconnected_device() {
        let (mut authority, initial) = BrowserAuthority::new(600, 3).unwrap();
        authority.reconnect_grace = 0;
        let open = authority.authenticate(Some(&initial.code), None).unwrap();
        authority.connect(&open.ticket_digest);
        let mut closed = Vec::new();
        for since in [20, 10] {
            let link = authority.create_pairing().unwrap();
            let device = authority.authenticate(Some(&link.code), None).unwrap();
            authority.connect(&device.ticket_digest);
            authority.disconnect(&device.ticket_digest); // its tab was closed
            authority
                .tickets
                .get_mut(&device.ticket_digest)
                .unwrap()
                .idle_since = Some(since);
            closed.push(device);
        }
        assert!(authority.create_pairing().is_none(), "the limit is full");

        let (_, reclaimed) = authority.create_operator_pairing().unwrap();
        assert_eq!(reclaimed, Reclaimed::IdleDevice);
        let still = |authority: &mut BrowserAuthority, device: &Authentication| {
            authority
                .authenticate(None, device.ticket.as_deref())
                .is_some()
        };
        assert!(!still(&mut authority, &closed[1]), "disconnected longest");
        assert!(still(&mut authority, &closed[0]), "only one slot is freed");
        assert!(still(&mut authority, &open), "connected devices stay");
    }

    #[test]
    fn operator_pairing_spares_a_tab_that_may_be_reconnecting() {
        let (mut authority, initial) = BrowserAuthority::new(600, 1).unwrap();
        let tab = authority.authenticate(Some(&initial.code), None).unwrap();
        authority.connect(&tab.ticket_digest);
        authority.disconnect(&tab.ticket_digest); // waiting to reconnect

        assert_eq!(
            authority.create_operator_pairing().unwrap_err(),
            Full { reconnecting: 1 }
        );
        assert!(authority
            .authenticate(None, tab.ticket.as_deref())
            .is_some());
    }

    #[test]
    fn operator_pairing_spares_a_device_between_pairing_and_connecting() {
        let (mut authority, initial) = BrowserAuthority::new(600, 1).unwrap();
        let tab = authority.authenticate(Some(&initial.code), None).unwrap();
        assert!(authority.create_operator_pairing().is_err());
        assert!(authority
            .authenticate(None, tab.ticket.as_deref())
            .is_some());
    }

    #[test]
    fn operator_pairing_never_revokes_a_connected_device() {
        let (mut authority, initial) = BrowserAuthority::new(600, 1).unwrap();
        authority.reconnect_grace = 0;
        let open = authority.authenticate(Some(&initial.code), None).unwrap();
        authority.connect(&open.ticket_digest);

        assert_eq!(
            authority.create_operator_pairing().unwrap_err(),
            Full { reconnecting: 0 }
        );
        assert!(authority
            .authenticate(None, open.ticket.as_deref())
            .is_some());
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

    #[test]
    fn rolled_back_pairing_can_authenticate_again() {
        let (mut authority, initial) = BrowserAuthority::new(600, 1).unwrap();
        let authentication = authority.authenticate(Some(&initial.code), None).unwrap();
        authority.rollback(authentication);

        assert_eq!(authority.status().pending_pairings, 1);
        assert!(authority.authenticate(Some(&initial.code), None).is_some());
    }
}
