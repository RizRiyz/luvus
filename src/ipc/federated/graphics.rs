//! Tracks only graphics actually painted by this disposable host client. The
//! server owns pixels and replay; client modals remove placements, never assets.
use std::io::{self, Write};

use crate::terminal::kitty_graphics::{HOST_IMAGE_ID_BASE as FIRST, HOST_IMAGE_ID_LAST};
const COUNT: usize = (HOST_IMAGE_ID_LAST - FIRST + 1) as usize;

#[derive(Default)]
pub(super) struct HostPlacements {
    // Lazy fixed-size bitset for the server's bounded host image-id namespace.
    // No per-image/pixel/frame clones and no inventory for inactive endpoints.
    images: Vec<u64>,
    /// Only populated words are visited at a modal/surface boundary.
    words: std::collections::BTreeSet<usize>,
}

impl HostPlacements {
    pub(super) fn reset(&mut self) {
        for word in &self.words {
            self.images[*word] = 0;
        }
        self.words.clear();
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        for part in bytes.split(|byte| *byte == 0x1b) {
            let Some(body) = part.strip_prefix(b"_G") else {
                continue;
            };
            let Some(end) = body.iter().position(|byte| *byte == b';') else {
                continue;
            };
            let Ok(header) = std::str::from_utf8(&body[..end]) else {
                continue;
            };
            let (mut action, mut delete, mut image) = ("", "", None);
            for field in header.split(',') {
                if let Some((key, value)) = field.split_once('=') {
                    match key {
                        "a" => action = value,
                        "d" => delete = value,
                        "i" => image = value.parse::<u32>().ok(),
                        _ => {}
                    }
                }
            }
            let Some(index) = image
                .and_then(|id| id.checked_sub(FIRST))
                .map(|id| id as usize)
                .filter(|index| *index < COUNT)
            else {
                continue;
            };
            if action == "p" {
                if self.images.is_empty() {
                    self.images.resize(COUNT.div_ceil(64), 0);
                }
                self.images[index / 64] |= 1 << (index % 64);
                self.words.insert(index / 64);
            } else if action == "d" && matches!(delete, "i" | "I") {
                // A placement-specific delete cannot prove the image has no
                // other fragments. Keeping that bit makes modal deletion safe.
                if !header.split(',').any(|field| field.starts_with("p="))
                    && !self.images.is_empty()
                {
                    self.images[index / 64] &= !(1 << (index % 64));
                    if self.images[index / 64] == 0 {
                        self.words.remove(&(index / 64));
                    }
                }
            }
        }
    }

    pub(super) fn hide(&mut self, out: &mut impl Write) -> io::Result<()> {
        for word in &self.words {
            let mut bits = self.images[*word];
            while bits != 0 {
                let bit = bits.trailing_zeros();
                let id = FIRST + (*word * 64) as u32 + bit;
                // Lowercase i removes this image's placements, preserving
                // retained pixel data and every image not owned by this view.
                write!(out, "\x1b_Ga=d,d=i,i={id},q=2;\x1b\\")?;
                bits &= bits - 1;
            }
        }
        out.flush()?;
        self.reset();
        Ok(())
    }
}
