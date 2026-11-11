//! Kitty graphics for child panes.
//!
//! Placements are anchored to terminal rows and drawn with direct `a=p`
//! commands. Unicode placeholder cells (`U+10EEEE`) and their combining marks
//! are never used to position an image and never reach the host grid: a
//! placeholder becomes one ordinary space so scrolling cannot change text width.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const PLACEHOLDER: &[u8] = &[0xF4, 0x8E, 0xBB, 0xAE];
const MAX_APC_BYTES: usize = 12 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STORED_IMAGES: usize = 32;
const MAX_PLACEMENTS: usize = 128;
const KITTY_CHUNK_BYTES: usize = 3072;
const HOST_IMAGE_ID_BASE: u32 = 10_000;
const SCAN_ABOVE_VIEWPORT: i32 = 256;

thread_local! {
    static HOST_DRAW: RefCell<Arc<AtomicBool>> =
        RefCell::new(Arc::new(AtomicBool::new(false)));
}

/// Engines created on this thread share the server's host-draw flag.
pub(crate) fn install_host_draw(flag: Arc<AtomicBool>) {
    HOST_DRAW.with(|slot| *slot.borrow_mut() = flag);
}

pub(crate) fn current_host_draw() -> Arc<AtomicBool> {
    HOST_DRAW.with(|slot| slot.borrow().clone())
}

/// `CSI 16 t` measures cell pixels. It is not a graphics capability query and
/// is sent only after the host has agreed to draw and ioctl has no size.
pub(crate) fn cell_size_query_required(graphics_ok: bool, ioctl: Option<(u16, u16)>) -> bool {
    graphics_ok && ioctl.is_none() && !cfg!(windows)
}

/// Replace a Kitty Unicode placeholder cluster with one width-1 space.
pub(crate) fn host_cell_symbol(symbol: &str) -> &str {
    if symbol.chars().any(|ch| ch == '\u{10EEEE}') {
        " "
    } else {
        symbol
    }
}

/// Child output with Kitty APC sequences removed is returned separately.
/// Placeholder characters in the plain text are rewritten before the VT parser.
#[derive(Debug, Default)]
pub(crate) struct GraphicsParser {
    state: ParseState,
    pending: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ParseState {
    #[default]
    Ground,
    Esc,
    Apc,
    Kitty,
    KittyEsc,
    Overflow,
    OverflowEsc,
}

#[derive(Debug)]
pub(crate) struct ParsedCommand {
    entries: Vec<(char, String)>,
    payload: Vec<u8>,
}

#[derive(Debug)]
pub(crate) enum StreamEvent {
    Plain(Vec<u8>),
    Command(ParsedCommand),
}

impl GraphicsParser {
    pub(crate) fn is_idle(&self) -> bool {
        self.state == ParseState::Ground && self.pending.is_empty()
    }

    pub(crate) fn drain(&mut self, input: &[u8]) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let mut plain = Vec::new();
        let flush = |plain: &mut Vec<u8>, events: &mut Vec<StreamEvent>| {
            if plain.is_empty() {
                return;
            }
            let mut sanitized = Vec::with_capacity(plain.len());
            emit_plain(plain, &mut sanitized);
            plain.clear();
            if !sanitized.is_empty() {
                events.push(StreamEvent::Plain(sanitized));
            }
        };
        let mut index = 0;
        while index < input.len() {
            match self.state {
                ParseState::Ground => {
                    if input[index] == 0x1b {
                        self.state = ParseState::Esc;
                        index += 1;
                    } else {
                        let end = input[index..]
                            .iter()
                            .position(|byte| *byte == 0x1b)
                            .map_or(input.len(), |offset| index + offset);
                        plain.extend_from_slice(&input[index..end]);
                        index = end;
                    }
                }
                ParseState::Esc => {
                    if input[index] == b'_' {
                        self.state = ParseState::Apc;
                        index += 1;
                    } else {
                        plain.push(0x1b);
                        self.state = ParseState::Ground;
                    }
                }
                ParseState::Apc => {
                    if input[index] == b'G' {
                        self.pending.clear();
                        self.state = ParseState::Kitty;
                        index += 1;
                    } else {
                        plain.extend_from_slice(b"\x1b_");
                        self.state = ParseState::Ground;
                    }
                }
                ParseState::Kitty => {
                    if input[index] == 0x1b {
                        self.state = ParseState::KittyEsc;
                        index += 1;
                    } else if self.pending.len() >= MAX_APC_BYTES {
                        self.pending.clear();
                        self.state = ParseState::Overflow;
                        index += 1;
                    } else {
                        self.pending.push(input[index]);
                        index += 1;
                    }
                }
                ParseState::KittyEsc => {
                    if input[index] == b'\\' {
                        flush(&mut plain, &mut events);
                        if let Some(command) = parse_command(&self.pending) {
                            events.push(StreamEvent::Command(command));
                        }
                        self.pending.clear();
                        self.state = ParseState::Ground;
                        index += 1;
                    } else {
                        if self.pending.len() < MAX_APC_BYTES {
                            self.pending.push(0x1b);
                        }
                        self.state = ParseState::Kitty;
                    }
                }
                ParseState::Overflow => {
                    if input[index] == 0x1b {
                        self.state = ParseState::OverflowEsc;
                    }
                    index += 1;
                }
                ParseState::OverflowEsc => {
                    self.state = if input[index] == b'\\' {
                        ParseState::Ground
                    } else {
                        ParseState::Overflow
                    };
                    index += 1;
                }
            }
        }
        flush(&mut plain, &mut events);
        events
    }
}

fn emit_plain(input: &[u8], out: &mut Vec<u8>) {
    if !input.contains(&0xF4) {
        out.extend_from_slice(input);
        return;
    }
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with(PLACEHOLDER) {
            out.push(b' ');
            index += PLACEHOLDER.len();
            index = skip_zero_width(input, index);
            continue;
        }
        out.push(input[index]);
        index += 1;
    }
}

fn skip_zero_width(input: &[u8], mut index: usize) -> usize {
    while index < input.len() {
        let rest = &input[index..];
        let (valid, stop) = match std::str::from_utf8(rest) {
            Ok(text) => (text, true),
            Err(err) if err.valid_up_to() > 0 => {
                let text = std::str::from_utf8(&rest[..err.valid_up_to()]).unwrap_or("");
                (text, err.error_len().is_some())
            }
            Err(_) => return index,
        };
        let mut consumed = 0;
        for ch in valid.chars() {
            if unicode_width::UnicodeWidthChar::width(ch) != Some(0) {
                return index;
            }
            consumed += ch.len_utf8();
            index += ch.len_utf8();
        }
        if stop || consumed == 0 {
            break;
        }
    }
    index
}

fn parse_command(raw: &[u8]) -> Option<ParsedCommand> {
    let split = raw
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(raw.len());
    let mut entries = Vec::new();
    if split > 0 {
        for part in raw[..split].split(|byte| *byte == b',') {
            let Some(eq) = part.iter().position(|byte| *byte == b'=') else {
                continue;
            };
            if eq != 1 || !part[0].is_ascii_alphabetic() {
                continue;
            }
            let value = String::from_utf8_lossy(&part[eq + 1..]).into_owned();
            entries.push((part[0] as char, value));
        }
    }
    let payload = if split < raw.len() {
        base64_decode(&raw[split + 1..])?
    } else {
        Vec::new()
    };
    Some(ParsedCommand { entries, payload })
}

#[derive(Debug, Clone)]
struct Keys {
    action: char,
    format: u32,
    width: u32,
    height: u32,
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: u32,
    more: bool,
    medium: char,
    compression: char,
    quiet: u32,
    columns: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
    z: i32,
    cursor_policy: u32,
    unicode_placeholder: bool,
    delete: Option<char>,
    feature: Option<char>,
}

impl Keys {
    fn from_entries(entries: &[(char, String)]) -> Self {
        let mut keys = Self {
            action: 't',
            format: 32,
            width: 0,
            height: 0,
            image_id: None,
            image_number: None,
            placement_id: 0,
            more: false,
            medium: 'd',
            compression: '\0',
            quiet: 1,
            columns: 0,
            rows: 0,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            x_offset: 0,
            y_offset: 0,
            z: 0,
            cursor_policy: 0,
            unicode_placeholder: false,
            delete: None,
            feature: None,
        };
        for (key, value) in entries {
            match *key {
                'a' => keys.action = value.chars().next().unwrap_or('t'),
                'f' => keys.format = parse_u32(value).unwrap_or(32),
                's' => keys.width = parse_u32(value).unwrap_or(0),
                'v' => keys.height = parse_u32(value).unwrap_or(0),
                'i' => keys.image_id = parse_u32(value).filter(|id| *id != 0),
                'I' => keys.image_number = parse_u32(value).filter(|id| *id != 0),
                'p' => keys.placement_id = parse_u32(value).unwrap_or(0),
                'm' => keys.more = value == "1",
                't' => keys.medium = value.chars().next().unwrap_or('d'),
                'o' => keys.compression = value.chars().next().unwrap_or('\0'),
                'q' => keys.quiet = parse_u32(value).unwrap_or(1),
                'c' => keys.columns = parse_u32(value).unwrap_or(0),
                'r' => keys.rows = parse_u32(value).unwrap_or(0),
                'x' => keys.source_x = parse_u32(value).unwrap_or(0),
                'y' => keys.source_y = parse_u32(value).unwrap_or(0),
                'w' => keys.source_width = parse_u32(value).unwrap_or(0),
                'h' => keys.source_height = parse_u32(value).unwrap_or(0),
                'X' => keys.x_offset = parse_u32(value).unwrap_or(0),
                'Y' => keys.y_offset = parse_u32(value).unwrap_or(0),
                'z' => keys.z = value.parse().unwrap_or(0),
                'C' => keys.cursor_policy = parse_u32(value).unwrap_or(0),
                'U' => keys.unicode_placeholder = value == "1",
                'd' => keys.delete = value.chars().next(),
                'e' => keys.feature = value.chars().next(),
                _ => {}
            }
        }
        keys
    }

    fn continuation(entries: &[(char, String)]) -> bool {
        !entries.is_empty() && entries.iter().all(|(key, _)| *key == 'm' || *key == 'q')
    }
}

fn parse_u32(value: &str) -> Option<u32> {
    value.parse().ok()
}

#[derive(Debug, Clone)]
struct ImageRecord {
    width: u32,
    height: u32,
    format: u32,
    data: Arc<[u8]>,
    fingerprint: u64,
}

#[derive(Debug, Clone)]
struct Placement {
    anchor: u64,
    column: u16,
    image_id: u32,
    child_placement_id: u32,
    local_placement_id: u32,
    z: i32,
    columns: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
}

#[derive(Debug)]
struct PendingTransfer {
    keys: Keys,
    data: Vec<u8>,
}

/// One image placement the current viewport can see.
#[derive(Debug, Clone)]
pub(crate) struct VisiblePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub z: i32,
    pub viewport_col: i32,
    pub viewport_row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    pub format_code: u32,
    pub data: Arc<[u8]>,
    pub fingerprint: u64,
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub scroll_offset: u32,
}

#[derive(Debug)]
pub(crate) enum GraphicsAction {
    None,
    Reply(Vec<u8>),
    Place(NewPlacement),
    DeleteCursor { free_image: bool },
}

#[derive(Debug)]
pub(crate) struct NewPlacement {
    pub(crate) image_id: u32,
    pub(crate) child_placement_id: u32,
    pub(crate) local_placement_id: u32,
    pub(crate) z: i32,
    pub(crate) columns: u32,
    pub(crate) rows: u32,
    pub(crate) source_x: u32,
    pub(crate) source_y: u32,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) x_offset: u32,
    pub(crate) y_offset: u32,
    pub(crate) move_cursor: bool,
}

/// Images and placements owned by one pane's terminal.
#[derive(Debug)]
pub(crate) struct PaneGraphics {
    parser: GraphicsParser,
    images: HashMap<u32, ImageRecord>,
    image_order: Vec<u32>,
    placements: Vec<Placement>,
    next_anchor: u64,
    next_image_id: u32,
    next_local_placement: u32,
    pending: Option<PendingTransfer>,
    gc_needed: bool,
    /// `0` means the production byte cap. Tests lower it to prove a truncated
    /// transfer is dropped instead of placed.
    image_limit: usize,
    /// Child `a=q` replies OK only while the displaying host has agreed.
    /// Direct parser tests leave this on; the engine sets it from the host.
    queries_enabled: bool,
}

impl Default for PaneGraphics {
    fn default() -> Self {
        Self {
            parser: GraphicsParser::default(),
            images: HashMap::new(),
            image_order: Vec::new(),
            placements: Vec::new(),
            next_anchor: 0,
            next_image_id: 0,
            next_local_placement: 0,
            pending: None,
            gc_needed: false,
            image_limit: 0,
            queries_enabled: true,
        }
    }
}

impl PaneGraphics {
    pub(crate) fn parser_idle(&self) -> bool {
        self.parser.is_idle()
    }

    pub(crate) fn has_placements(&self) -> bool {
        !self.placements.is_empty()
    }

    pub(crate) fn take_gc(&mut self) -> bool {
        std::mem::take(&mut self.gc_needed)
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, input: &[u8], plain: &mut Vec<u8>) -> Vec<GraphicsAction> {
        let mut actions = Vec::new();
        for event in self.parser.drain(input) {
            match event {
                StreamEvent::Plain(bytes) => plain.extend_from_slice(&bytes),
                StreamEvent::Command(command) => actions.extend(self.command(command)),
            }
        }
        actions
    }

    pub(crate) fn drain(&mut self, input: &[u8]) -> Vec<StreamEvent> {
        self.parser.drain(input)
    }

    pub(crate) fn apply(&mut self, command: ParsedCommand) -> Vec<GraphicsAction> {
        self.command(command)
    }

    pub(crate) fn alloc_anchor(&mut self, current: u64) -> u64 {
        if current != 0 {
            return current;
        }
        self.next_anchor = self.next_anchor.wrapping_add(1).max(1);
        self.next_anchor
    }

    pub(crate) fn record(&mut self, anchor: u64, column: u16, placement: NewPlacement) {
        if placement.child_placement_id != 0 {
            self.placements.retain(|existing| {
                !(existing.image_id == placement.image_id
                    && existing.child_placement_id == placement.child_placement_id)
            });
        }
        self.placements.push(Placement {
            anchor,
            column,
            image_id: placement.image_id,
            child_placement_id: placement.child_placement_id,
            local_placement_id: placement.local_placement_id,
            z: placement.z,
            columns: placement.columns,
            rows: placement.rows,
            source_x: placement.source_x,
            source_y: placement.source_y,
            source_width: placement.source_width,
            source_height: placement.source_height,
            x_offset: placement.x_offset,
            y_offset: placement.y_offset,
        });
        if self.placements.len() > MAX_PLACEMENTS {
            let overflow = self.placements.len() - MAX_PLACEMENTS;
            self.placements.drain(0..overflow);
        }
        self.gc_needed = true;
    }

    pub(crate) fn delete_at(&mut self, anchor: u64, column: u16, free_image: bool) {
        let mut removed = Vec::new();
        self.placements.retain(|placement| {
            let end = placement
                .column
                .saturating_add(placement.columns.max(1) as u16);
            let hit = placement.anchor == anchor && column >= placement.column && column < end;
            if hit {
                removed.push(placement.image_id);
            }
            !hit
        });
        if free_image {
            for image_id in removed {
                if !self
                    .placements
                    .iter()
                    .any(|placement| placement.image_id == image_id)
                {
                    self.remove_image(image_id);
                }
            }
        }
    }

    pub(crate) fn retain_anchors(&mut self, live: &HashSet<u64>) {
        self.placements
            .retain(|placement| live.contains(&placement.anchor));
    }

    pub(crate) fn visible(
        &self,
        anchors: &[(u64, i32)],
        cell_width_px: u32,
        cell_height_px: u32,
        scroll_offset: u32,
    ) -> Vec<VisiblePlacement> {
        let mut visible = Vec::new();
        for placement in &self.placements {
            let Some((_, line)) = anchors
                .iter()
                .find(|(anchor, _)| *anchor == placement.anchor)
            else {
                continue;
            };
            let Some(image) = self.images.get(&placement.image_id) else {
                continue;
            };
            let (grid_cols, grid_rows) = grid_span(placement, image, cell_width_px, cell_height_px);
            let pixel_width = grid_cols
                .saturating_mul(cell_width_px)
                .max(image.width)
                .max(1);
            let pixel_height = grid_rows
                .saturating_mul(cell_height_px)
                .max(image.height)
                .max(1);
            visible.push(VisiblePlacement {
                image_id: placement.image_id,
                placement_id: placement.local_placement_id,
                z: placement.z,
                viewport_col: i32::from(placement.column),
                viewport_row: *line,
                grid_cols,
                grid_rows,
                pixel_width,
                pixel_height,
                image_width: image.width,
                image_height: image.height,
                format_code: image.format,
                data: Arc::clone(&image.data),
                fingerprint: image.fingerprint,
                source_x: placement.source_x,
                source_y: placement.source_y,
                source_width: placement.source_width,
                source_height: placement.source_height,
                x_offset: placement.x_offset,
                y_offset: placement.y_offset,
                scroll_offset,
            });
        }
        visible
    }

    fn command(&mut self, command: ParsedCommand) -> Vec<GraphicsAction> {
        if Keys::continuation(&command.entries) {
            return self.continue_transfer(&command);
        }
        self.pending = None;
        let keys = Keys::from_entries(&command.entries);
        if keys.more && keys.medium == 'd' && keys.action != 'q' && keys.action != 'd' {
            self.pending = Some(PendingTransfer {
                keys,
                data: command.payload,
            });
            return Vec::new();
        }
        self.finish(keys, command.payload)
    }

    pub(crate) fn set_queries_enabled(&mut self, enabled: bool) {
        self.queries_enabled = enabled;
    }

    #[cfg(test)]
    fn set_image_limit(&mut self, limit: usize) {
        self.image_limit = limit;
    }

    fn image_limit(&self) -> usize {
        if self.image_limit == 0 {
            MAX_IMAGE_BYTES
        } else {
            self.image_limit
        }
    }

    fn continue_transfer(&mut self, command: &ParsedCommand) -> Vec<GraphicsAction> {
        let more = command
            .entries
            .iter()
            .any(|(key, value)| *key == 'm' && value == "1");
        let limit = self.image_limit();
        let Some(pending) = self.pending.as_mut() else {
            return Vec::new();
        };
        if pending.data.len().saturating_add(command.payload.len()) > limit {
            self.pending = None;
            return vec![GraphicsAction::Reply(error_reply(None, "EBADF"))];
        }
        pending.data.extend_from_slice(&command.payload);
        if more {
            return Vec::new();
        }
        let Some(done) = self.pending.take() else {
            return Vec::new();
        };
        self.finish(done.keys, done.data)
    }

    fn finish(&mut self, keys: Keys, data: Vec<u8>) -> Vec<GraphicsAction> {
        if keys.action == 'q' {
            return vec![self.query_reply(&keys)];
        }
        if keys.action == 'd' {
            if let Some(action) = delete_action(keys.action, keys.delete) {
                return vec![action];
            }
            self.delete(&keys);
            return quiet_ok(&keys).into_iter().collect();
        }
        if keys.medium != 'd' || keys.compression == 'z' {
            return error_action(&keys, "EINVAL").into_iter().collect();
        }
        if keys.action == 'f' {
            let Some(id) = keys.image_id else {
                return error_action(&keys, "EINVAL").into_iter().collect();
            };
            if !self.images.contains_key(&id) || !self.store_image(id, &keys, data) {
                return error_action(&keys, "EINVAL").into_iter().collect();
            }
            return assigned_reply(&keys, Some(id))
                .map(GraphicsAction::Reply)
                .into_iter()
                .collect();
        }
        let image_id = if matches!(keys.action, 't' | 'T') || !data.is_empty() {
            let id = keys.image_id.unwrap_or_else(|| self.allocate_image_id());
            if !self.store_image(id, &keys, data) {
                return error_action(&keys, "EINVAL").into_iter().collect();
            }
            Some(id)
        } else {
            keys.image_id
        };
        let reply = assigned_reply(&keys, image_id);
        if !matches!(keys.action, 'T' | 'p') {
            return reply.map(GraphicsAction::Reply).into_iter().collect();
        }
        let Some(image_id) = image_id.filter(|id| self.images.contains_key(id)) else {
            return error_action(&keys, "ENOENT").into_iter().collect();
        };
        let local_placement_id = if keys.placement_id == 0 {
            self.next_local_placement = self.next_local_placement.wrapping_add(1).max(1);
            self.next_local_placement
        } else {
            keys.placement_id
        };
        let placement = NewPlacement {
            image_id,
            child_placement_id: keys.placement_id,
            local_placement_id,
            z: keys.z,
            columns: keys.columns,
            rows: keys.rows,
            source_x: keys.source_x,
            source_y: keys.source_y,
            source_width: keys.source_width,
            source_height: keys.source_height,
            x_offset: keys.x_offset,
            y_offset: keys.y_offset,
            move_cursor: keys.cursor_policy != 1
                && !keys.unicode_placeholder
                && (keys.columns > 0 || keys.rows > 0),
        };
        let mut actions = Vec::new();
        if let Some(bytes) = reply {
            actions.push(GraphicsAction::Reply(bytes));
        }
        actions.push(GraphicsAction::Place(placement));
        actions
    }

    fn query_reply(&self, keys: &Keys) -> GraphicsAction {
        if !self.queries_enabled {
            let id = keys
                .image_id
                .map(|id| format!("i={id}"))
                .unwrap_or_else(|| "a=q".to_string());
            return GraphicsAction::Reply(format!("\x1b_G{id};ENOTSUPPORTED\x1b\\").into_bytes());
        }
        let Some(id) = keys.image_id else {
            return GraphicsAction::None;
        };
        let mut body = format!("i={id}");
        if keys.feature == Some('T') {
            body.push_str(",e=T");
            if keys.cursor_policy == 1 {
                body.push_str(",C=1");
            }
        }
        GraphicsAction::Reply(format!("\x1b_G{body};OK\x1b\\").into_bytes())
    }

    fn delete(&mut self, keys: &Keys) {
        match keys.delete {
            None | Some('a') => self.placements.clear(),
            Some('A') => {
                self.placements.clear();
                self.images.clear();
                self.image_order.clear();
            }
            Some('i') => {
                if let Some(id) = keys.image_id {
                    self.placements.retain(|placement| {
                        placement.image_id != id
                            || (keys.placement_id != 0
                                && placement.child_placement_id != keys.placement_id)
                    });
                }
            }
            Some('I') => {
                if let Some(id) = keys.image_id {
                    self.placements.retain(|placement| placement.image_id != id);
                    self.remove_image(id);
                }
            }
            Some('p' | 'P') if keys.placement_id != 0 => {
                self.placements
                    .retain(|placement| placement.child_placement_id != keys.placement_id);
            }
            _ => {}
        }
    }

    fn store_image(&mut self, id: u32, keys: &Keys, data: Vec<u8>) -> bool {
        if data.is_empty() || data.len() > self.image_limit() {
            return false;
        }
        let (width, height) = match keys.format {
            24 | 32 => {
                let bpp = if keys.format == 24 { 3 } else { 4 };
                if keys.width == 0
                    || keys.height == 0
                    || data.len() != keys.width as usize * keys.height as usize * bpp
                {
                    return false;
                }
                (keys.width, keys.height)
            }
            100 => match png_size(&data) {
                Some(size) => size,
                None if keys.width > 0 && keys.height > 0 => (keys.width, keys.height),
                None => return false,
            },
            _ => return false,
        };
        let fingerprint = fingerprint(&data);
        if self
            .images
            .insert(
                id,
                ImageRecord {
                    width,
                    height,
                    format: keys.format,
                    data: Arc::from(data.into_boxed_slice()),
                    fingerprint,
                },
            )
            .is_none()
        {
            self.image_order.push(id);
        }
        self.enforce_budget();
        true
    }

    fn allocate_image_id(&mut self) -> u32 {
        loop {
            self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
            if !self.images.contains_key(&self.next_image_id) {
                return self.next_image_id;
            }
        }
    }

    fn remove_image(&mut self, id: u32) {
        self.images.remove(&id);
        self.image_order.retain(|existing| *existing != id);
    }

    fn enforce_budget(&mut self) {
        let mut total: usize = self.images.values().map(|image| image.data.len()).sum();
        while self.images.len() > MAX_STORED_IMAGES || total > MAX_IMAGE_BYTES {
            let Some(oldest) = self.image_order.first().copied() else {
                break;
            };
            if let Some(image) = self.images.remove(&oldest) {
                total = total.saturating_sub(image.data.len());
            }
            self.image_order.remove(0);
            self.placements
                .retain(|placement| placement.image_id != oldest);
        }
    }
}

fn grid_span(
    placement: &Placement,
    image: &ImageRecord,
    cell_width_px: u32,
    cell_height_px: u32,
) -> (u32, u32) {
    let cols = if placement.columns > 0 {
        placement.columns
    } else if cell_width_px > 0 {
        image.width.div_ceil(cell_width_px).max(1)
    } else {
        1
    };
    let rows = if placement.rows > 0 {
        placement.rows
    } else if cell_height_px > 0 {
        image.height.div_ceil(cell_height_px).max(1)
    } else {
        1
    };
    (cols, rows)
}

fn assigned_reply(keys: &Keys, image_id: Option<u32>) -> Option<Vec<u8>> {
    if keys.quiet != 0 && keys.image_number.is_none() {
        return None;
    }
    let id = image_id?;
    let mut body = format!("i={id}");
    if let Some(number) = keys.image_number {
        body.push_str(&format!(",I={number}"));
    }
    if keys.placement_id != 0 && matches!(keys.action, 'T' | 'p') {
        body.push_str(&format!(",p={}", keys.placement_id));
    }
    Some(format!("\x1b_G{body};OK\x1b\\").into_bytes())
}

fn quiet_ok(keys: &Keys) -> Option<GraphicsAction> {
    if keys.quiet != 0 {
        return None;
    }
    let id = keys.image_id.unwrap_or(0);
    Some(GraphicsAction::Reply(
        format!("\x1b_Gi={id};OK\x1b\\").into_bytes(),
    ))
}

fn error_action(keys: &Keys, code: &str) -> Option<GraphicsAction> {
    if keys.quiet >= 2 {
        return None;
    }
    Some(GraphicsAction::Reply(error_reply(keys.image_id, code)))
}

fn error_reply(image_id: Option<u32>, code: &str) -> Vec<u8> {
    match image_id {
        Some(id) => format!("\x1b_Gi={id};{code}\x1b\\").into_bytes(),
        None => format!("\x1b_G;{code}\x1b\\").into_bytes(),
    }
}

fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn fingerprint(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.len().hash(&mut hasher);
    data.hash(&mut hasher);
    hasher.finish()
}

/// Lines above and inside the viewport that may still own a placement anchor.
pub(crate) fn anchor_scan_range(topmost: i32, display_offset: i32, screen_rows: i32) -> (i32, i32) {
    let viewport_top = -display_offset;
    let start = viewport_top
        .saturating_sub(SCAN_ABOVE_VIEWPORT)
        .max(topmost);
    let end = viewport_top.saturating_add(screen_rows);
    (start, end)
}

pub(crate) fn viewport_line(grid_line: i32, display_offset: i32) -> i32 {
    grid_line.saturating_add(display_offset)
}

// ── host placement encoding ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageSignature {
    width: u32,
    height: u32,
    format: u32,
    fingerprint: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PlacementSignature {
    x: u16,
    y: u16,
    cols: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
    z: i32,
    scroll_offset: u32,
}

#[derive(Debug, Clone, Copy)]
struct Clipped {
    x: u16,
    y: u16,
    cols: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
}

/// One pane's visible placement, already in that pane's viewport coordinates.
#[derive(Debug, Clone)]
pub(crate) struct ScreenPlacement {
    pub pane_id: u32,
    pub area_x: u16,
    pub area_y: u16,
    pub area_width: u16,
    pub area_height: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub visible: VisiblePlacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SourceKey {
    pane_id: u32,
    image_id: u32,
}

/// What this client has already uploaded to its host terminal.
#[derive(Debug, Default, Clone)]
pub(crate) struct HostGraphicsCache {
    images: HashMap<u32, ImageSignature>,
    placements: HashMap<(u32, u32), PlacementSignature>,
    sources: HashMap<SourceKey, u32>,
    replay: bool,
}

impl HostGraphicsCache {
    pub(crate) fn request_replay(&mut self) {
        self.replay = true;
    }
}

pub(crate) fn host_image_id(pane_id: u32, image_id: u32, fingerprint: u64) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pane_id.hash(&mut hasher);
    image_id.hash(&mut hasher);
    fingerprint.hash(&mut hasher);
    HOST_IMAGE_ID_BASE.saturating_add((hasher.finish() as u32) % 900_000)
}

fn host_placement_id(pane_id: u32, image_id: u32, placement_id: u32) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pane_id.hash(&mut hasher);
    image_id.hash(&mut hasher);
    placement_id.hash(&mut hasher);
    1 + (hasher.finish() as u32) % 900_000
}

/// Encode the delta that makes the host match `placements`.
///
/// Image ids are derived from the pane and the image bytes, so two panes never
/// share or delete each other's storage. A scroll-offset change deletes the
/// previous placement and puts the clipped rectangle again. Panes missing from
/// `live_panes` lose only their own images.
pub(crate) fn encode_host_frame(
    cache: &mut HostGraphicsCache,
    placements: &[ScreenPlacement],
    live_panes: &HashSet<u32>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut retired = cache
        .sources
        .iter()
        .filter(|(source, _)| !live_panes.contains(&source.pane_id))
        .map(|(source, host_id)| (*source, *host_id))
        .collect::<Vec<_>>();
    retired.sort_unstable_by_key(|(source, host_id)| (source.pane_id, source.image_id, *host_id));
    for (source, host_id) in retired {
        encode_delete_image(&mut out, host_id);
        cache.sources.remove(&source);
        cache.images.remove(&host_id);
        cache.placements.retain(|(id, _), _| *id != host_id);
    }

    let mut desired = Vec::new();
    for placement in placements {
        let Some((clipped, _)) = clip_placement(placement) else {
            continue;
        };
        let host_id = host_image_id(
            placement.pane_id,
            placement.visible.image_id,
            placement.visible.fingerprint,
        );
        let placement_id = host_placement_id(
            placement.pane_id,
            placement.visible.image_id,
            placement.visible.placement_id,
        );
        desired.push((host_id, placement_id, clipped, placement));
    }
    let desired_keys = desired
        .iter()
        .map(|(host_id, placement_id, _, _)| (*host_id, *placement_id))
        .collect::<HashSet<_>>();
    let mut stale = cache
        .placements
        .keys()
        .filter(|key| !desired_keys.contains(key))
        .copied()
        .collect::<Vec<_>>();
    stale.sort_unstable();
    for (host_id, placement_id) in stale {
        encode_delete_placement(&mut out, host_id, placement_id);
        cache.placements.remove(&(host_id, placement_id));
    }

    for (host_id, placement_id, clipped, placement) in desired {
        encode_one(cache, &mut out, host_id, placement_id, clipped, placement);
    }
    cache.replay = false;
    out
}

fn encode_one(
    cache: &mut HostGraphicsCache,
    out: &mut Vec<u8>,
    host_id: u32,
    placement_id: u32,
    clipped: Clipped,
    placement: &ScreenPlacement,
) {
    let signature = ImageSignature {
        width: placement.visible.image_width,
        height: placement.visible.image_height,
        format: placement.visible.format_code,
        fingerprint: placement.visible.fingerprint,
    };
    let placed = placement_signature(
        clipped,
        placement.visible.z,
        placement.visible.scroll_offset,
    );
    let image_current = cache.images.get(&host_id) == Some(&signature);
    let known = cache.placements.get(&(host_id, placement_id)).copied();
    let placement_current = known == Some(placed) && !cache.replay;
    let source = SourceKey {
        pane_id: placement.pane_id,
        image_id: placement.visible.image_id,
    };
    if image_current && placement_current && cache.sources.get(&source) == Some(&host_id) {
        return;
    }
    if !image_current {
        if placement.visible.data.is_empty() {
            return;
        }
        if cache.images.contains_key(&host_id) {
            encode_delete_image(out, host_id);
            cache.placements.retain(|(id, _), _| *id != host_id);
        }
        encode_upload(out, &placement.visible, host_id);
        cache.images.insert(host_id, signature);
    }
    release_superseded(cache, out, source, host_id);
    if known.is_some() && known != Some(placed) {
        encode_delete_placement(out, host_id, placement_id);
    }
    if !placement_current {
        encode_display(out, clipped, host_id, placement_id, placement.visible.z);
    }
    cache.placements.insert((host_id, placement_id), placed);
}

fn release_superseded(
    cache: &mut HostGraphicsCache,
    out: &mut Vec<u8>,
    source: SourceKey,
    host_id: u32,
) {
    let Some(previous) = cache.sources.insert(source, host_id) else {
        return;
    };
    if previous == host_id || cache.sources.values().any(|id| *id == previous) {
        return;
    }
    encode_delete_image(out, previous);
    cache.images.remove(&previous);
    cache.placements.retain(|(id, _), _| *id != previous);
}

fn clip_placement(placement: &ScreenPlacement) -> Option<(Clipped, u32)> {
    if placement.area_width == 0
        || placement.area_height == 0
        || placement.cell_width_px == 0
        || placement.cell_height_px == 0
    {
        return None;
    }
    let visible = &placement.visible;
    if visible.grid_cols == 0 || visible.grid_rows == 0 {
        return None;
    }
    let left = if visible.viewport_col < 0 {
        visible.viewport_col.saturating_neg() as u32
    } else {
        0
    };
    let top = if visible.viewport_row < 0 {
        visible.viewport_row.saturating_neg() as u32
    } else {
        0
    };
    let column = visible.viewport_col.max(0) as u32;
    let row = visible.viewport_row.max(0) as u32;
    if column >= u32::from(placement.area_width) || row >= u32::from(placement.area_height) {
        return None;
    }
    let cols = visible
        .grid_cols
        .saturating_sub(left)
        .min(u32::from(placement.area_width) - column);
    let rows = visible
        .grid_rows
        .saturating_sub(top)
        .min(u32::from(placement.area_height) - row);
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell_w = placement.cell_width_px.max(1);
    let cell_h = placement.cell_height_px.max(1);
    let source_width = if visible.source_width == 0 {
        visible.image_width
    } else {
        visible.source_width
    };
    let source_height = if visible.source_height == 0 {
        visible.image_height
    } else {
        visible.source_height
    };
    let pixel_width = visible
        .pixel_width
        .max(visible.grid_cols.saturating_mul(cell_w))
        .max(1);
    let pixel_height = visible
        .pixel_height
        .max(visible.grid_rows.saturating_mul(cell_h))
        .max(1);
    let source_x = visible.source_x.saturating_add(scale_pixels(
        left.saturating_mul(cell_w),
        source_width,
        pixel_width,
    ));
    let source_y = visible.source_y.saturating_add(scale_pixels(
        top.saturating_mul(cell_h),
        source_height,
        pixel_height,
    ));
    let cropped_width = scale_pixels(cols.saturating_mul(cell_w), source_width, pixel_width)
        .max(1)
        .min(visible.image_width.saturating_sub(source_x));
    let cropped_height = scale_pixels(rows.saturating_mul(cell_h), source_height, pixel_height)
        .max(1)
        .min(visible.image_height.saturating_sub(source_y));
    if cropped_width == 0 || cropped_height == 0 {
        return None;
    }
    Some((
        Clipped {
            x: placement.area_x.saturating_add(column as u16),
            y: placement.area_y.saturating_add(row as u16),
            cols,
            rows,
            source_x,
            source_y,
            source_width: cropped_width,
            source_height: cropped_height,
            x_offset: if left == 0 { visible.x_offset } else { 0 },
            y_offset: if top == 0 { visible.y_offset } else { 0 },
        },
        visible.format_code,
    ))
}

fn placement_signature(clipped: Clipped, z: i32, scroll_offset: u32) -> PlacementSignature {
    PlacementSignature {
        x: clipped.x,
        y: clipped.y,
        cols: clipped.cols,
        rows: clipped.rows,
        source_x: clipped.source_x,
        source_y: clipped.source_y,
        source_width: clipped.source_width,
        source_height: clipped.source_height,
        x_offset: clipped.x_offset,
        y_offset: clipped.y_offset,
        z,
        scroll_offset,
    }
}

fn scale_pixels(value: u32, source: u32, dest: u32) -> u32 {
    ((u64::from(value).saturating_mul(u64::from(source)) / u64::from(dest.max(1)))
        .min(u64::from(u32::MAX))) as u32
}

fn encode_upload(out: &mut Vec<u8>, visible: &VisiblePlacement, host_id: u32) {
    let control = format!(
        "a=t,t=d,f={},s={},v={},i={host_id},q=2",
        visible.format_code, visible.image_width, visible.image_height
    );
    encode_kitty_data(out, &control, &visible.data);
}

fn encode_display(out: &mut Vec<u8>, clipped: Clipped, host_id: u32, placement_id: u32, z: i32) {
    use std::io::Write;
    let _ = write!(out, "\x1b[{};{}H", clipped.y + 1, clipped.x + 1);
    let mut control = format!(
        "a=p,i={host_id},p={placement_id},c={},r={},z={z},C=1,q=2",
        clipped.cols, clipped.rows
    );
    if clipped.source_x > 0 {
        control.push_str(&format!(",x={}", clipped.source_x));
    }
    if clipped.source_y > 0 {
        control.push_str(&format!(",y={}", clipped.source_y));
    }
    if clipped.source_width > 0 {
        control.push_str(&format!(",w={}", clipped.source_width));
    }
    if clipped.source_height > 0 {
        control.push_str(&format!(",h={}", clipped.source_height));
    }
    if clipped.x_offset > 0 {
        control.push_str(&format!(",X={}", clipped.x_offset));
    }
    if clipped.y_offset > 0 {
        control.push_str(&format!(",Y={}", clipped.y_offset));
    }
    let _ = write!(out, "\x1b_G{control};\x1b\\");
}

fn encode_delete_image(out: &mut Vec<u8>, host_id: u32) {
    use std::io::Write;
    let _ = write!(out, "\x1b_Ga=d,d=I,i={host_id},q=2;\x1b\\");
}

fn encode_delete_placement(out: &mut Vec<u8>, host_id: u32, placement_id: u32) {
    use std::io::Write;
    let _ = write!(out, "\x1b_Ga=d,d=i,i={host_id},p={placement_id},q=2;\x1b\\");
}

fn encode_kitty_data(out: &mut Vec<u8>, control: &str, data: &[u8]) {
    let mut chunks = data.chunks(KITTY_CHUNK_BYTES).peekable();
    let Some(first) = chunks.next() else {
        return;
    };
    write_chunk(out, &format!("{control},m={}", more(&mut chunks)), first);
    while let Some(chunk) = chunks.next() {
        write_chunk(out, &format!("m={}", more(&mut chunks)), chunk);
    }
}

fn more<I>(chunks: &mut std::iter::Peekable<I>) -> u8
where
    I: Iterator,
{
    if chunks.peek().is_some() {
        1
    } else {
        0
    }
}

fn write_chunk(out: &mut Vec<u8>, control: &str, data: &[u8]) {
    out.extend_from_slice(b"\x1b_G");
    out.extend_from_slice(control.as_bytes());
    out.push(b';');
    base64_encode(data, out);
    out.extend_from_slice(b"\x1b\\");
}

fn base64_encode(data: &[u8], out: &mut Vec<u8>) {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut index = 0;
    while index + 3 <= data.len() {
        let n = (u32::from(data[index]) << 16)
            | (u32::from(data[index + 1]) << 8)
            | u32::from(data[index + 2]);
        out.push(ALPHA[((n >> 18) & 63) as usize]);
        out.push(ALPHA[((n >> 12) & 63) as usize]);
        out.push(ALPHA[((n >> 6) & 63) as usize]);
        out.push(ALPHA[(n & 63) as usize]);
        index += 3;
    }
    let rest = data.len() - index;
    if rest == 1 {
        let n = u32::from(data[index]) << 16;
        out.push(ALPHA[((n >> 18) & 63) as usize]);
        out.push(ALPHA[((n >> 12) & 63) as usize]);
        out.extend_from_slice(b"==");
    } else if rest == 2 {
        let n = (u32::from(data[index]) << 16) | (u32::from(data[index + 1]) << 8);
        out.push(ALPHA[((n >> 18) & 63) as usize]);
        out.push(ALPHA[((n >> 12) & 63) as usize]);
        out.push(ALPHA[((n >> 6) & 63) as usize]);
        out.push(b'=');
    }
}

fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let filtered = input
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if filtered.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
    let (chunks, _) = filtered.as_chunks::<4>();
    for chunk in chunks {
        let pad = chunk.iter().rev().take_while(|byte| **byte == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                continue;
            }
            n |= u32::from(value(*byte)?) << (18 - 6 * index);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Cursor movement after a direct placement: right by columns and down by rows.
pub(crate) fn cursor_after_image(
    column: u32,
    row: u32,
    width: u32,
    columns: u32,
    rows: u32,
) -> (u32, u32) {
    let width = width.max(1);
    let mut column = column.saturating_add(columns);
    let mut row = row.saturating_add(rows);
    row = row.saturating_add(column / width);
    column %= width;
    (column, row)
}

/// `d=c` / `d=C` need the terminal cursor, so the parser reports them separately.
pub(crate) fn cursor_delete(command_entries_action: char, delete: Option<char>) -> Option<bool> {
    if command_entries_action == 'd' {
        match delete {
            Some('c') => Some(false),
            Some('C') => Some(true),
            _ => None,
        }
    } else {
        None
    }
}

// The delete path above drops cursor deletes. Re-read keys in `finish` by
// converting `c`/`C` into `GraphicsAction::DeleteCursor` before the generic
// delete. This helper keeps that decision testable.
pub(crate) fn delete_action(keys_action: char, delete: Option<char>) -> Option<GraphicsAction> {
    cursor_delete(keys_action, delete).map(|free_image| GraphicsAction::DeleteCursor { free_image })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(width: u32, height: u32, byte: u8) -> Vec<u8> {
        vec![byte; width as usize * height as usize * 4]
    }

    fn visible(viewport_col: i32, viewport_row: i32, scroll: u32) -> VisiblePlacement {
        let data = rgba(30, 30, 9);
        let fingerprint = fingerprint(&data);
        VisiblePlacement {
            image_id: 7,
            placement_id: 3,
            z: 0,
            viewport_col,
            viewport_row,
            grid_cols: 3,
            grid_rows: 3,
            pixel_width: 0,
            pixel_height: 0,
            image_width: 30,
            image_height: 30,
            format_code: 32,
            data: Arc::from(data.into_boxed_slice()),
            fingerprint,
            source_x: 0,
            source_y: 0,
            source_width: 0,
            source_height: 0,
            x_offset: 0,
            y_offset: 0,
            scroll_offset: scroll,
        }
    }

    fn screen(pane: u32, placement: VisiblePlacement) -> ScreenPlacement {
        ScreenPlacement {
            pane_id: pane,
            area_x: 2,
            area_y: 1,
            area_width: 20,
            area_height: 10,
            cell_width_px: 10,
            cell_height_px: 10,
            visible: placement,
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn clipped_placement_crops_negative_viewport_and_keeps_positive_cells() {
        let (inside, _) = clip_placement(&screen(1, visible(2, 2, 0))).unwrap();
        assert_eq!((inside.x, inside.y, inside.cols, inside.rows), (4, 3, 3, 3));
        assert_eq!((inside.source_x, inside.source_y), (0, 0));

        let (cropped, _) = clip_placement(&screen(1, visible(-1, -1, 0))).unwrap();
        assert_eq!(
            (cropped.x, cropped.y, cropped.cols, cropped.rows),
            (2, 1, 2, 2)
        );
        assert_eq!((cropped.source_x, cropped.source_y), (10, 10));
        assert_eq!((cropped.source_width, cropped.source_height), (20, 20));
    }

    #[test]
    fn scroll_offset_change_replaces_placement_without_reuploading() {
        let mut cache = HostGraphicsCache::default();
        let live = HashSet::from([1]);
        let first = encode_host_frame(&mut cache, &[screen(1, visible(0, 0, 0))], &live);
        let first = text(&first);
        assert!(first.contains("a=t"), "{first}");
        assert!(first.contains("a=p"), "{first}");

        assert!(encode_host_frame(&mut cache, &[screen(1, visible(0, 0, 0))], &live).is_empty());

        let moved = encode_host_frame(&mut cache, &[screen(1, visible(0, 4, 3))], &live);
        let moved = text(&moved);
        assert!(moved.contains("a=d,d=i"), "{moved}");
        assert!(moved.contains("a=p"), "{moved}");
        assert!(!moved.contains("a=t"), "{moved}");
        assert!(moved.contains("\x1b[6;3H"), "{moved}");
    }

    #[test]
    fn pane_image_ids_are_isolated_and_close_deletes_only_that_pane() {
        let mut cache = HostGraphicsCache::default();
        let live = HashSet::from([1, 2]);
        let uploaded = encode_host_frame(
            &mut cache,
            &[screen(1, visible(0, 0, 0)), screen(2, visible(1, 1, 0))],
            &live,
        );
        let uploaded = text(&uploaded);
        let id_one = host_image_id(1, 7, screen(1, visible(0, 0, 0)).visible.fingerprint);
        let id_two = host_image_id(2, 7, screen(2, visible(0, 0, 0)).visible.fingerprint);
        assert_ne!(id_one, id_two);
        assert!(uploaded.contains(&format!("i={id_one}")));
        assert!(uploaded.contains(&format!("i={id_two}")));

        let closed = encode_host_frame(
            &mut cache,
            &[screen(2, visible(1, 1, 0))],
            &HashSet::from([2]),
        );
        let closed = text(&closed);
        assert!(closed.contains(&format!("a=d,d=I,i={id_one}")), "{closed}");
        assert!(!closed.contains(&format!("i={id_two}")), "{closed}");
        assert!(cache.sources.contains_key(&SourceKey {
            pane_id: 2,
            image_id: 7
        }));
        assert!(!cache.sources.contains_key(&SourceKey {
            pane_id: 1,
            image_id: 7
        }));
    }

    #[test]
    fn placeholder_characters_are_ordinary_single_width_spaces() {
        assert_eq!(host_cell_symbol("\u{10EEEE}"), " ");
        assert_eq!(host_cell_symbol("\u{10EEEE}\u{0305}\u{030d}"), " ");
        assert_eq!(host_cell_symbol("A"), "A");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(host_cell_symbol("\u{10EEEE}")),
            1
        );

        let mut plain = Vec::new();
        emit_plain("A\u{10EEEE}\u{0305}B".as_bytes(), &mut plain);
        assert_eq!(plain, b"A B");
        assert!(!plain.windows(4).any(|bytes| bytes == PLACEHOLDER));
    }

    #[test]
    fn direct_placement_parses_without_leaking_the_escape_into_text() {
        let pixels = rgba(1, 1, 7);
        let mut payload = Vec::new();
        base64_encode(&pixels, &mut payload);
        let mut command = b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,i=4,q=2;".to_vec();
        command.extend_from_slice(&payload);
        command.extend_from_slice(b"\x1b\\");
        let mut graphics = PaneGraphics::default();
        let mut plain = Vec::new();
        plain.extend_from_slice(b"before");
        let actions = graphics.push(&command, &mut plain);
        plain.extend_from_slice(b"after");
        assert_eq!(plain, b"beforeafter");
        assert!(matches!(actions.as_slice(), [GraphicsAction::Place(_)]));
    }

    #[test]
    fn placement_scrolls_with_its_row_and_leaves_width_one_cells() {
        use crate::terminal::vt::VtEngine;

        let (tx, _rx) = std::sync::mpsc::channel();
        let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
        let pixels = rgba(1, 1, 7);
        let mut payload = Vec::new();
        base64_encode(&pixels, &mut payload);
        let mut command = b"before \x1b_Ga=T,f=32,s=1,v=1,C=1,q=2;".to_vec();
        command.extend_from_slice(&payload);
        command.extend_from_slice(b"\x1b\\ after\n");
        engine.advance(&command);
        engine.finish_output_batch();

        let placed = engine.kitty_visible_placements(10, 20);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].viewport_row, 0);
        let first = engine.screen_rows().remove(0);
        assert!(first.starts_with("before  after"), "{first}");
        assert!(!first.contains('\u{10EEEE}'));
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(first.trim_end()),
            "before  after".len()
        );

        engine.advance(b"\n\n\n\n\n");
        engine.finish_output_batch();
        let scrolled = engine.kitty_visible_placements(10, 20);
        assert_eq!(scrolled.len(), 1);
        assert!(
            scrolled[0].viewport_row < 0,
            "{:?}",
            scrolled[0].viewport_row
        );
        engine.scroll(-scrolled[0].viewport_row);
        let restored = engine.kitty_visible_placements(10, 20);
        assert_eq!(restored[0].viewport_row, 0);
        for row in engine.screen_rows() {
            assert!(!row.contains('\u{10EEEE}'));
        }
    }

    #[test]
    fn feature_query_does_not_advertise_unicode_placeholders() {
        let mut graphics = PaneGraphics::default();
        let mut plain = Vec::new();
        let actions = graphics.push(b"\x1b_Ga=q,i=31,e=T,U=1,C=1\x1b\\", &mut plain);
        match actions.as_slice() {
            [GraphicsAction::Reply(bytes)] => {
                let reply = text(bytes);
                assert!(reply.contains("e=T"), "{reply}");
                assert!(reply.contains("C=1"), "{reply}");
                assert!(!reply.contains("U=1"), "{reply}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(plain.is_empty());
    }

    #[test]
    fn child_query_is_refused_until_the_host_agrees() {
        let mut graphics = PaneGraphics::default();
        graphics.set_queries_enabled(false);
        let mut plain = Vec::new();
        let actions = graphics.push(b"\x1b_Ga=q,i=31,e=T,C=1\x1b\\", &mut plain);
        match actions.as_slice() {
            [GraphicsAction::Reply(bytes)] => {
                let reply = text(bytes);
                assert!(reply.contains("ENOTSUPPORTED"), "{reply}");
                assert!(!reply.contains("OK"), "{reply}");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!cell_size_query_required(false, None));
        assert!(!cell_size_query_required(true, Some((8, 16))));
        assert_eq!(cell_size_query_required(true, None), !cfg!(windows));
    }

    #[test]
    fn host_query_follows_the_display_flag() {
        use crate::terminal::vt::VtEngine;
        use std::sync::atomic::{AtomicBool, Ordering};

        let flag = Arc::new(AtomicBool::new(false));
        install_host_draw(flag.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
        let query = b"\x1b_Ga=q,i=31\x1b\\";
        engine.advance(query);
        engine.finish_output_batch();
        let refused = text(&input_bytes(rx.try_recv().unwrap()));
        assert!(refused.contains("ENOTSUPPORTED"), "{refused}");
        flag.store(true, Ordering::Release);
        engine.advance(query);
        engine.finish_output_batch();
        let accepted = text(&input_bytes(rx.try_recv().unwrap()));
        assert!(accepted.contains("OK"), "{accepted}");
        flag.store(false, Ordering::Release);
    }

    #[test]
    fn animation_frame_updates_the_saved_image_for_a_later_client() {
        use crate::terminal::vt::VtEngine;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut engine = crate::terminal::vt::alacritty::AlacrittyEngine::new(20, 5, tx, 64 * 1024);
        let first = rgba(1, 1, 1);
        let second = rgba(1, 1, 2);
        engine.advance(&transmit(b'a', b'T', 4, &first, false));
        engine.finish_output_batch();
        engine.advance(&transmit(b'a', b'f', 4, &second, false));
        engine.finish_output_batch();
        let placed = engine.kitty_visible_placements(10, 20);
        assert_eq!(placed.len(), 1);
        assert_eq!(&*placed[0].data, second.as_slice());

        let mut cache = HostGraphicsCache::default();
        let replay = encode_host_frame(
            &mut cache,
            &[screen(1, placed[0].clone())],
            &HashSet::from([1]),
        );
        let replay = text(&replay);
        assert!(replay.contains("a=t"), "{replay}");
        let mut encoded = Vec::new();
        base64_encode(&second, &mut encoded);
        assert!(
            replay.contains(std::str::from_utf8(&encoded).unwrap()),
            "{replay}"
        );
    }

    #[test]
    fn interrupted_or_oversized_chunks_are_not_placed() {
        let mut graphics = PaneGraphics::default();
        let mut plain = Vec::new();
        let start = b"\x1b_Ga=T,f=32,s=1,v=1,i=9,m=1;AAAA\x1b\\";
        assert!(graphics.push(start, &mut plain).is_empty());
        let interrupted = graphics.push(b"\x1b_Ga=q,i=1\x1b\\", &mut plain);
        assert!(interrupted
            .iter()
            .all(|action| !matches!(action, GraphicsAction::Place(_))));
        let place = graphics.push(b"\x1b_Ga=p,i=9,c=1,r=1\x1b\\", &mut plain);
        assert!(
            matches!(place.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("ENOENT")),
            "{place:?}"
        );

        graphics.set_image_limit(4);
        assert!(graphics
            .push(b"\x1b_Ga=T,f=32,s=1,v=1,i=8,m=1;AAAA\x1b\\", &mut plain)
            .is_empty());
        let overflow = graphics.push(b"\x1b_Gm=0;AAAA\x1b\\", &mut plain);
        assert!(
            matches!(overflow.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("EBADF")),
            "{overflow:?}"
        );
        let place = graphics.push(b"\x1b_Ga=p,i=8,c=1,r=1\x1b\\", &mut plain);
        assert!(
            matches!(place.as_slice(), [GraphicsAction::Reply(bytes)] if text(bytes).contains("ENOENT")),
            "{place:?}"
        );
    }

    #[test]
    fn unknown_cell_size_does_not_place() {
        let mut placement = screen(1, visible(0, 0, 0));
        placement.cell_width_px = 0;
        placement.cell_height_px = 0;
        let bytes = encode_host_frame(
            &mut HostGraphicsCache::default(),
            &[placement],
            &HashSet::from([1]),
        );
        assert!(bytes.is_empty());
    }

    fn transmit(key: u8, action: u8, id: u32, pixels: &[u8], more: bool) -> Vec<u8> {
        let mut payload = Vec::new();
        base64_encode(pixels, &mut payload);
        let more = if more { ",m=1" } else { "" };
        let mut command = format!(
            "\x1b_G{}={},f=32,s=1,v=1,c=1,r=1,i={id},C=1,q=2{more};",
            key as char, action as char
        )
        .into_bytes();
        command.extend_from_slice(&payload);
        command.extend_from_slice(b"\x1b\\");
        command
    }

    fn input_bytes(action: crate::terminal::pty::InputAction) -> Vec<u8> {
        match action {
            crate::terminal::pty::InputAction::Bytes(bytes) => bytes,
            crate::terminal::pty::InputAction::Submit { .. } => Vec::new(),
        }
    }
}
