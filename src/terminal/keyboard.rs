//! Luvus-owned keyboard protocol state shared by the VT and input layers.

use bitflags::bitflags;

bitflags! {
    /// Flags negotiated through Kitty's progressive keyboard enhancement protocol.
    ///
    /// Luvus retains every currently defined protocol bit even when its input
    /// event model cannot yet implement that bit's encoding behavior.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
    pub struct KittyKeyboardFlags: u8 {
        const DISAMBIGUATE_ESCAPE_CODES = 1 << 0;
        const REPORT_EVENT_TYPES = 1 << 1;
        const REPORT_ALTERNATE_KEYS = 1 << 2;
        const REPORT_ALL_KEYS_AS_ESCAPE_CODES = 1 << 3;
        const REPORT_ASSOCIATED_TEXT = 1 << 4;
    }
}

impl KittyKeyboardFlags {
    /// Report-all implies the unambiguous encoding used by the first flag.
    pub fn disambiguates_escape_codes(self) -> bool {
        self.intersects(Self::DISAMBIGUATE_ESCAPE_CODES | Self::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
    }

    pub fn reports_event_types(self) -> bool {
        self.contains(Self::REPORT_EVENT_TYPES)
    }

    #[allow(dead_code)]
    pub fn reports_alternate_keys(self) -> bool {
        self.contains(Self::REPORT_ALTERNATE_KEYS)
    }

    pub fn reports_all_keys(self) -> bool {
        self.contains(Self::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
    }

    #[allow(dead_code)]
    pub fn reports_associated_text(self) -> bool {
        self.contains(Self::REPORT_ASSOCIATED_TEXT)
    }
}

/// Keyboard protocol selected by the application running in a pane.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeyboardProtocol {
    #[default]
    Legacy,
    Kitty {
        flags: KittyKeyboardFlags,
    },
}

impl KeyboardProtocol {
    /// Build protocol state from the Kitty bits retained by the VT engine.
    pub fn from_kitty_flags(flags: KittyKeyboardFlags) -> Self {
        if flags.is_empty() {
            Self::Legacy
        } else {
            Self::Kitty { flags }
        }
    }

    pub fn kitty_flags(self) -> KittyKeyboardFlags {
        match self {
            Self::Legacy => KittyKeyboardFlags::empty(),
            Self::Kitty { flags } => flags,
        }
    }

    pub fn disambiguates_escape_codes(self) -> bool {
        self.kitty_flags().disambiguates_escape_codes()
    }

    pub fn reports_event_types(self) -> bool {
        self.kitty_flags().reports_event_types()
    }

    #[allow(dead_code)]
    pub fn reports_alternate_keys(self) -> bool {
        self.kitty_flags().reports_alternate_keys()
    }

    pub fn reports_all_keys(self) -> bool {
        self.kitty_flags().reports_all_keys()
    }

    #[allow(dead_code)]
    pub fn reports_associated_text(self) -> bool {
        self.kitty_flags().reports_associated_text()
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyboardProtocol, KittyKeyboardFlags};

    #[test]
    fn zero_kitty_flags_are_legacy() {
        assert_eq!(
            KeyboardProtocol::from_kitty_flags(KittyKeyboardFlags::empty()),
            KeyboardProtocol::Legacy
        );
    }

    #[test]
    fn semantic_helpers_preserve_disambiguate_and_report_all_behavior() {
        let disambiguate =
            KeyboardProtocol::from_kitty_flags(KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES);
        assert!(disambiguate.disambiguates_escape_codes());
        assert!(!disambiguate.reports_event_types());
        assert!(!disambiguate.reports_alternate_keys());
        assert!(!disambiguate.reports_all_keys());
        assert!(!disambiguate.reports_associated_text());

        let report_all =
            KeyboardProtocol::from_kitty_flags(KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
        assert!(report_all.disambiguates_escape_codes());
        assert!(!report_all.reports_event_types());
        assert!(!report_all.reports_alternate_keys());
        assert!(report_all.reports_all_keys());
        assert!(!report_all.reports_associated_text());
    }

    #[test]
    fn raw_unknown_kitty_bits_are_retained_by_the_typed_value() {
        let flags = KittyKeyboardFlags::from_bits_retain(0b1000_0000);
        assert_eq!(
            KeyboardProtocol::from_kitty_flags(flags)
                .kitty_flags()
                .bits(),
            0b1000_0000
        );
    }
}
