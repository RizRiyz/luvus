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
pub(crate) const HOST_IMAGE_ID_BASE: u32 = 10_000;
pub(crate) const HOST_IMAGE_ID_LAST: u32 = HOST_IMAGE_ID_BASE + 899_999;
/// Explicit cell spans and natural image dimensions share this ceiling, so
/// even 1px cells cannot produce an unbounded cursor move or anchor lookback.
const MAX_PLACEMENT_CELLS: u32 = 4_096;
/// Bound the decoded surface as well as compressed/transmitted storage.
const MAX_DECODED_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Graphics bytes in one frame stay far under the 64 MiB client frame cap.
pub(crate) const GRAPHICS_FRAME_BUDGET: usize = 16 * 1024 * 1024;
/// Grid reflow uses this range for anchors that do not exist yet.
pub(crate) const GRAPHICS_TEMP_ANCHOR: u64 = 1 << 62;

thread_local! {
    static HOST_DRAW: RefCell<Arc<AtomicBool>> =
        RefCell::new(Arc::new(AtomicBool::new(false)));
    static RUNTIME_CELLS: RefCell<Arc<RuntimeCells>> = RefCell::new(Arc::new(RuntimeCells::default()));
}

/// Engines created on this thread share the server's host-draw flag.
pub(crate) fn install_host_draw(flag: Arc<AtomicBool>) {
    HOST_DRAW.with(|slot| *slot.borrow_mut() = flag);
}

/// Foreground runtime geometry is one atomic pair, independent of projection.
/// Engines share it before PTY startup, including while backend creation waits.
#[derive(Default)]
pub(crate) struct RuntimeCells(std::sync::atomic::AtomicU32);

impl RuntimeCells {
    pub(crate) fn set(&self, width: u16, height: u16) {
        self.0.store(
            (u32::from(width) << 16) | u32::from(height),
            std::sync::atomic::Ordering::Release,
        );
    }

    pub(crate) fn get(&self) -> (u16, u16) {
        let pair = self.0.load(std::sync::atomic::Ordering::Acquire);
        ((pair >> 16) as u16, pair as u16)
    }
}

/// Bind construction on this thread to the app's current geometry source.
pub(crate) fn install_runtime_source(cells: Arc<RuntimeCells>) {
    RUNTIME_CELLS.with(|slot| *slot.borrow_mut() = cells);
}

pub(crate) fn runtime_source() -> Arc<RuntimeCells> {
    RUNTIME_CELLS.with(|slot| slot.borrow().clone())
}

#[cfg(test)]
pub(crate) fn install_runtime_cells(cells: (u16, u16)) {
    runtime_source().set(cells.0, cells.1);
}

#[cfg(test)]
pub(crate) fn current_runtime_cells() -> (u16, u16) {
    runtime_source().get()
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

mod parser;
#[cfg(test)]
use parser::emit_plain;
pub(crate) use parser::{GraphicsParser, ParsedCommand, StreamEvent};

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
            quiet: 0,
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

    fn supported_transfer(&self) -> bool {
        self.medium == 'd' && self.compression == '\0' && matches!(self.format, 24 | 32 | 100)
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
    /// Carried for the child command. Source cropping uses the cell grid,
    /// not this native size, so a shrunk image that fits is not cropped.
    #[allow(dead_code)]
    pub pixel_width: u32,
    #[allow(dead_code)]
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
    DeleteVisible { free_image: bool },
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
    pub(crate) fn has_placements(&self) -> bool {
        !self.placements.is_empty()
    }

    pub(crate) fn take_gc(&mut self) -> bool {
        std::mem::take(&mut self.gc_needed)
    }

    pub(crate) fn parser_holds_text(&self) -> bool {
        self.parser.holds_text()
    }

    pub(crate) fn retained_images(&self) -> Vec<(u32, u64)> {
        self.image_order
            .iter()
            .filter_map(|id| self.images.get(id).map(|image| (*id, image.fingerprint)))
            .collect()
    }

    pub(crate) fn lookback_rows(&self, cell_height_px: u32) -> i32 {
        let rows = self
            .placements
            .iter()
            .map(|placement| self.placement_rows(placement, cell_height_px))
            .max()
            .unwrap_or(0);
        i32::try_from(rows).unwrap_or(i32::MAX)
    }

    pub(crate) fn cursor_span(
        &self,
        placement: &NewPlacement,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Option<(u32, u32)> {
        if cell_width_px == 0 || cell_height_px == 0 {
            return None;
        }
        let image = self.images.get(&placement.image_id)?;
        let width = source_extent(image.width, placement.source_x, placement.source_width);
        let height = source_extent(image.height, placement.source_y, placement.source_height);
        let cols = placement_axis_span(placement.columns, width, cell_width_px);
        let rows = placement_axis_span(placement.rows, height, cell_height_px);
        Some((cols, rows))
    }

    pub(crate) fn delete_covering(
        &mut self,
        cursor_line: i32,
        cursor_col: u16,
        anchors: &[(i32, u64)],
        cell_pixels: (u32, u32),
        free_image: bool,
    ) {
        let spans = self
            .placements
            .iter()
            .filter_map(|p| {
                self.images.get(&p.image_id).map(|image| {
                    (
                        (p.image_id, p.local_placement_id),
                        grid_span(p, image, cell_pixels.0, cell_pixels.1),
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        let mut removed = Vec::new();
        let mut keep = Vec::with_capacity(self.placements.len());
        for placement in self.placements.drain(..) {
            let Some(anchor_line) = anchors
                .iter()
                .find_map(|(line, anchor)| (*anchor == placement.anchor).then_some(*line))
            else {
                keep.push(placement);
                continue;
            };
            let dy = cursor_line.saturating_sub(anchor_line);
            let (cols, rows) = spans
                .get(&(placement.image_id, placement.local_placement_id))
                .copied()
                .unwrap_or((1, 1));
            let rows = rows.min(i32::MAX as u32) as i32;
            let end = u32::from(placement.column).saturating_add(cols);
            let hit = dy >= 0
                && dy < rows
                && cursor_col >= placement.column
                && u32::from(cursor_col) < end;
            if hit {
                removed.push(placement.image_id);
            } else {
                keep.push(placement);
            }
        }
        self.placements = keep;
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

    /// Delete only placements intersecting the live screen, not scrollback.
    pub(crate) fn delete_visible(&mut self, visible_ids: &HashSet<(u32, u32)>, free_image: bool) {
        let mut removed = HashSet::new();
        self.placements.retain(|p| {
            let hit = visible_ids.contains(&(p.image_id, p.local_placement_id));
            if hit {
                removed.insert(p.image_id);
            }
            !hit
        });
        if free_image {
            for id in removed {
                if !self.placements.iter().any(|p| p.image_id == id) {
                    self.remove_image(id);
                }
            }
        }
    }

    /// Move placements when a resize merges or wraps their row.
    ///
    /// A temporary destination anchor (`>= GRAPHICS_TEMP_ANCHOR`) is replaced
    /// with a real one. The returned pair is `(temporary, real)`; `real == 0`
    /// means the temporary row anchor should be cleared.
    pub(crate) fn reflow_move_prefix(
        &mut self,
        anchor: u64,
        prefix: u32,
        dest_column: u32,
        dest_anchor: u64,
        entire: bool,
    ) -> Option<(u64, u64)> {
        let (dest, remap) = self.resolve_dest(dest_anchor);
        for placement in &mut self.placements {
            if placement.anchor != anchor {
                continue;
            }
            if entire || u32::from(placement.column) < prefix {
                placement.column = placement.column.saturating_add(dest_column as u16);
                placement.anchor = dest;
            } else {
                placement.column = placement.column.saturating_sub(prefix as u16);
            }
        }
        remap
    }

    pub(crate) fn reflow_move_suffix(
        &mut self,
        anchor: u64,
        at: u32,
        dest_anchor: u64,
    ) -> Option<(u64, u64)> {
        let moved = self
            .placements
            .iter()
            .any(|placement| placement.anchor == anchor && u32::from(placement.column) >= at);
        if !moved {
            return (dest_anchor >= GRAPHICS_TEMP_ANCHOR).then_some((dest_anchor, 0));
        }
        let (dest, remap) = self.resolve_dest(dest_anchor);
        for placement in &mut self.placements {
            if placement.anchor == anchor && u32::from(placement.column) >= at {
                placement.column = placement.column.saturating_sub(at as u16);
                placement.anchor = dest;
            }
        }
        remap
    }

    pub(crate) fn reflow_shift(&mut self, anchor: u64, delta: i32) {
        if anchor == 0 || delta == 0 {
            return;
        }
        for placement in &mut self.placements {
            if placement.anchor != anchor {
                continue;
            }
            if delta > 0 {
                placement.column = placement.column.saturating_add(delta as u16);
            } else {
                placement.column = placement.column.saturating_sub(delta.unsigned_abs() as u16);
            }
        }
    }

    fn resolve_dest(&mut self, dest_anchor: u64) -> (u64, Option<(u64, u64)>) {
        if dest_anchor < GRAPHICS_TEMP_ANCHOR {
            (dest_anchor, None)
        } else {
            let real = self.alloc_anchor(0);
            (real, Some((dest_anchor, real)))
        }
    }

    fn placement_rows(&self, placement: &Placement, cell_height_px: u32) -> u32 {
        let Some(image) = self.images.get(&placement.image_id) else {
            return 1;
        };
        grid_span(placement, image, 0, cell_height_px).1
    }

    fn allocate_local_placement(&mut self, explicit: u32) -> u32 {
        if explicit != 0 {
            self.displace_auto_placement(explicit);
            return explicit;
        }
        self.next_free_placement()
    }

    fn displace_auto_placement(&mut self, explicit: u32) {
        let taken = self.placements.iter().any(|placement| {
            placement.child_placement_id == 0 && placement.local_placement_id == explicit
        });
        if !taken {
            return;
        }
        let replacement = self.next_free_placement();
        for placement in &mut self.placements {
            if placement.child_placement_id == 0 && placement.local_placement_id == explicit {
                placement.local_placement_id = replacement;
            }
        }
    }

    fn next_free_placement(&mut self) -> u32 {
        loop {
            self.next_local_placement = self.next_local_placement.wrapping_add(1).max(1);
            let id = self.next_local_placement;
            let taken = self.placements.iter().any(|placement| {
                placement.local_placement_id == id || placement.child_placement_id == id
            });
            if !taken {
                return id;
            }
        }
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

    /// Query replies are independent of pending transfers and cursor state.
    pub(crate) fn immediate_query(&self, command: &ParsedCommand) -> GraphicsAction {
        self.query_reply(&Keys::from_entries(&command.entries))
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
            if command.payload.len() > self.image_limit() {
                return error_action(&keys, "EBADF").into_iter().collect();
            }
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
            let keys = self.pending.take().map(|pending| pending.keys);
            return keys
                .and_then(|keys| error_action(&keys, "EBADF"))
                .into_iter()
                .collect();
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
            if matches!(keys.delete, None | Some('a' | 'A')) {
                let mut actions = vec![GraphicsAction::DeleteVisible {
                    free_image: keys.delete == Some('A'),
                }];
                if let Some(reply) = quiet_ok(&keys) {
                    actions.push(reply);
                }
                return actions;
            }
            if let Some(action) = delete_action(keys.action, keys.delete) {
                return vec![action];
            }
            self.delete(&keys);
            return quiet_ok(&keys).into_iter().collect();
        }
        if !keys.supported_transfer() {
            return error_action(&keys, "EINVAL").into_iter().collect();
        }
        // Animation frames have separate compositing/selection semantics. Until
        // those are supported, never acknowledge a frame by replacing the base.
        if matches!(keys.action, 'f' | 'a' | 'c') {
            return error_action(&keys, "ENOTSUPPORTED").into_iter().collect();
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
        if keys.columns > MAX_PLACEMENT_CELLS || keys.rows > MAX_PLACEMENT_CELLS {
            return error_action(&keys, "EINVAL").into_iter().collect();
        }
        let local_placement_id = self.allocate_local_placement(keys.placement_id);
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
            move_cursor: keys.cursor_policy != 1 && !keys.unicode_placeholder,
        };
        let mut actions = Vec::new();
        if let Some(bytes) = reply {
            actions.push(GraphicsAction::Reply(bytes));
        }
        actions.push(GraphicsAction::Place(placement));
        actions
    }

    /// Report pane capabilities, not host-only media/codec support. Queries
    /// never store pixels and honor q=1 success/q=2 all-response suppression.
    fn query_reply(&self, keys: &Keys) -> GraphicsAction {
        if !self.queries_enabled || !keys.supported_transfer() {
            if keys.quiet >= 2 {
                return GraphicsAction::None;
            }
            let id = keys
                .image_id
                .map(|id| format!("i={id}"))
                .unwrap_or_else(|| "a=q".to_string());
            return GraphicsAction::Reply(format!("\x1b_G{id};ENOTSUPPORTED\x1b\\").into_bytes());
        }
        let Some(id) = keys.image_id else {
            return GraphicsAction::None;
        };
        if keys.quiet != 0 {
            return GraphicsAction::None;
        }
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
                let Some(expected) = rgb_byte_len(keys.width, keys.height, bpp) else {
                    return false;
                };
                if keys.width == 0 || keys.height == 0 || data.len() != expected {
                    return false;
                }
                (keys.width, keys.height)
            }
            100 => match png_size(&data) {
                Some(size) => size,
                None => return false,
            },
            _ => return false,
        };
        if !bounded_image_size(width, height) {
            return false;
        }
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
        } else {
            // Retransmission replaces storage, not every old placement's pixels.
            self.placements.retain(|placement| placement.image_id != id);
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
    (
        placement_axis_span(
            placement.columns,
            source_extent(image.width, placement.source_x, placement.source_width),
            cell_width_px,
        ),
        placement_axis_span(
            placement.rows,
            source_extent(image.height, placement.source_y, placement.source_height),
            cell_height_px,
        ),
    )
}

/// One span calculation for rendering, cursor movement and anchor lookback.
/// Accepted images and explicit spans are bounded before reaching this helper.
fn placement_axis_span(explicit: u32, source: u32, cell_pixels: u32) -> u32 {
    if explicit > 0 {
        explicit
    } else if cell_pixels > 0 {
        source.div_ceil(cell_pixels).max(1)
    } else {
        1
    }
}

/// Resolve a crop against the remaining pixels, including omitted w/h.
fn source_extent(full: u32, offset: u32, extent: u32) -> u32 {
    let remaining = full.saturating_sub(offset);
    if extent == 0 {
        remaining
    } else {
        extent.min(remaining)
    }
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

fn rgb_byte_len(width: u32, height: u32, bytes_per_pixel: usize) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(bytes_per_pixel)
}

fn error_reply(image_id: Option<u32>, code: &str) -> Vec<u8> {
    match image_id {
        Some(id) => format!("\x1b_Gi={id};{code}\x1b\\").into_bytes(),
        None => format!("\x1b_G;{code}\x1b\\").into_bytes(),
    }
}

mod png;
use png::png_size;

fn bounded_image_size(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_PLACEMENT_CELLS
        && height <= MAX_PLACEMENT_CELLS
        && rgb_byte_len(width, height, 4).is_some_and(|bytes| bytes <= MAX_DECODED_IMAGE_BYTES)
}

fn fingerprint(data: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.len().hash(&mut hasher);
    data.hash(&mut hasher);
    hasher.finish()
}

/// Lines above and inside the viewport that may still own a placement anchor.
pub(crate) fn anchor_scan_range(
    topmost: i32,
    display_offset: i32,
    screen_rows: i32,
    lookback: i32,
) -> (i32, i32) {
    let viewport_top = -display_offset;
    let start = viewport_top.saturating_sub(lookback.max(0)).max(topmost);
    let end = viewport_top.saturating_add(screen_rows);
    (start, end)
}

pub(crate) fn viewport_line(grid_line: i32, display_offset: i32) -> i32 {
    grid_line.saturating_add(display_offset)
}

mod occlusion;
pub(crate) use occlusion::Occlusion;

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
pub(crate) struct SourceKey {
    pub(crate) pane_id: u32,
    pub(crate) image_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HostPlacementKey {
    pane_id: u32,
    image_id: u32,
    placement_id: u32,
    piece: Option<ratatui::layout::Rect>,
}

/// What this client has already uploaded to its host terminal.
///
/// Ids are allocated, never hashed. A dropped frame must discard the staged
/// copy of this cache so the next attempt uploads the pixels again.
#[derive(Debug, Default, Clone)]
pub(crate) struct HostGraphicsCache {
    images: HashMap<u32, ImageSignature>,
    placements: HashMap<(u32, u32), PlacementSignature>,
    sources: HashMap<SourceKey, u32>,
    host_placements: HashMap<HostPlacementKey, u32>,
    next_host_image: u32,
    next_host_placement: u32,
    replay: bool,
    remainder: bool,
    /// `0` uses [`GRAPHICS_FRAME_BUDGET`]. Tests lower it to prove a second
    /// image waits for the next frame.
    budget_override: usize,
    /// Zero uses the full protocol range; tests exercise exhaustion with two ids.
    placement_id_limit: u32,
}

impl HostGraphicsCache {
    pub(crate) fn request_replay(&mut self) {
        self.replay = true;
    }

    pub(crate) fn remainder(&self) -> bool {
        self.remainder
    }

    #[cfg(test)]
    pub(crate) fn placement_rects(&self) -> Vec<(u32, ratatui::layout::Rect)> {
        self.host_placements
            .iter()
            .filter_map(|(key, id)| {
                let host = self.sources.get(&SourceKey {
                    pane_id: key.pane_id,
                    image_id: key.image_id,
                })?;
                let p = self.placements.get(&(*host, *id))?;
                Some((
                    key.pane_id,
                    ratatui::layout::Rect::new(p.x, p.y, p.cols as u16, p.rows as u16),
                ))
            })
            .collect()
    }

    #[cfg(test)]
    fn host_id(&self, pane_id: u32, image_id: u32) -> Option<u32> {
        self.sources.get(&SourceKey { pane_id, image_id }).copied()
    }
}

/// Encode the delta that makes the host match `placements`.
///
/// Host image ids are allocated per pane and image. A pane that closes, or a
/// pane that drops an image, deletes only that record. `replay` uploads the
/// pixels again because the host screen was cleared. When several images do
/// not fit in one frame, `remainder` stays set and the unsent images are left
/// unmarked so the next frame continues.
#[cfg(test)]
pub(crate) fn encode_host_frame(
    cache: &mut HostGraphicsCache,
    placements: &[ScreenPlacement],
    live_panes: &HashSet<u32>,
    inspected: &HashSet<u32>,
    retained: &HashMap<SourceKey, u64>,
) -> Vec<u8> {
    encode_occluded_host_frame(
        cache,
        placements,
        live_panes,
        inspected,
        retained,
        &Occlusion::default(),
    )
}

pub(crate) fn encode_occluded_host_frame(
    cache: &mut HostGraphicsCache,
    placements: &[ScreenPlacement],
    live_panes: &HashSet<u32>,
    inspected: &HashSet<u32>,
    retained: &HashMap<SourceKey, u64>,
    occlusion: &Occlusion,
) -> Vec<u8> {
    let mut out = Vec::new();
    cache.remainder = false;
    if cache.replay {
        // The host no longer has the pixels. Keep the ids, forget the upload.
        cache.images.clear();
        cache.placements.clear();
    }
    let on_screen = placements
        .iter()
        .map(|placement| SourceKey {
            pane_id: placement.pane_id,
            image_id: placement.visible.image_id,
        })
        .collect::<HashSet<_>>();
    let mut retired = cache
        .sources
        .iter()
        .filter(|(source, _)| {
            if !live_panes.contains(&source.pane_id) {
                return true;
            }
            inspected.contains(&source.pane_id)
                && !on_screen.contains(source)
                && !retained.contains_key(source)
        })
        .map(|(source, host_id)| (*source, *host_id))
        .collect::<Vec<_>>();
    retired.sort_unstable_by_key(|(source, host_id)| (source.pane_id, source.image_id, *host_id));
    for (source, host_id) in retired {
        forget_source(cache, &mut out, source, host_id);
    }

    let pieces = occlusion::frame_pieces(placements, occlusion);
    let desired_placements = pieces
        .iter()
        .map(|(key, _, _)| *key)
        .collect::<HashSet<_>>();
    // Delete before reusing an id. Otherwise a stale delete can erase the new
    // placement allocated the same id in this frame.
    let retired = cache
        .host_placements
        .iter()
        .filter(|(key, _)| !desired_placements.contains(key))
        .map(|(key, id)| (*key, *id))
        .collect::<Vec<_>>();
    for (key, id) in retired {
        if let Some(host) = cache
            .sources
            .get(&SourceKey {
                pane_id: key.pane_id,
                image_id: key.image_id,
            })
            .copied()
        {
            if cache.placements.remove(&(host, id)).is_some() {
                encode_delete_placement(&mut out, host, id);
            }
        }
        cache.host_placements.remove(&key);
    }
    let mut planned = Vec::new();
    for (key, clipped, placement) in pieces {
        let source = SourceKey {
            pane_id: placement.pane_id,
            image_id: placement.visible.image_id,
        };
        let signature = ImageSignature {
            width: placement.visible.image_width,
            height: placement.visible.image_height,
            format: placement.visible.format_code,
            fingerprint: placement.visible.fingerprint,
        };
        let host_id = host_image_id_for(cache, &mut out, source, signature);
        let Some(placement_id) = host_placement_id_for(cache, key) else {
            // Exhaustion is bounded; never spin on the server event loop.
            continue;
        };
        planned.push((host_id, placement_id, clipped, placement, signature));
    }
    let planned_keys = planned
        .iter()
        .map(|(host_id, placement_id, _, _, _)| (*host_id, *placement_id))
        .collect::<HashSet<_>>();
    let mut stale = cache
        .placements
        .keys()
        .filter(|key| !planned_keys.contains(key))
        .copied()
        .collect::<Vec<_>>();
    stale.sort_unstable();
    for (host_id, placement_id) in stale {
        encode_delete_placement(&mut out, host_id, placement_id);
        cache.placements.remove(&(host_id, placement_id));
    }

    // Remove changed geometry before the upload budget can stop delivery. An
    // old fragment must never remain over a newly moved menu on a partial frame.
    for (host, id, clipped, placement, _) in &planned {
        let desired = placement_signature(
            *clipped,
            placement.visible.z,
            placement.visible.scroll_offset,
        );
        if cache
            .placements
            .get(&(*host, *id))
            .is_some_and(|old| *old != desired)
        {
            encode_delete_placement(&mut out, *host, *id);
            cache.placements.remove(&(*host, *id));
        }
    }
    for (host_id, placement_id, clipped, placement, signature) in planned {
        let image_current = cache.images.get(&host_id) == Some(&signature);
        if image_current
            && cache.placements.get(&(host_id, placement_id))
                == Some(&placement_signature(
                    clipped,
                    placement.visible.z,
                    placement.visible.scroll_offset,
                ))
        {
            // Unchanged fragments spend no command budget. Otherwise a nearly
            // full frame could repeatedly stop before its remaining pieces.
            continue;
        }
        let estimate = upload_estimate(&placement.visible, image_current);
        let budget = if cache.budget_override == 0 {
            GRAPHICS_FRAME_BUDGET
        } else {
            cache.budget_override
        };
        if !out.is_empty() && out.len().saturating_add(estimate) > budget {
            cache.remainder = true;
            break;
        }
        encode_one(
            cache,
            &mut out,
            EncodeJob {
                host_id,
                placement_id,
                clipped,
                placement,
                signature,
                image_current,
            },
        );
    }
    cache.replay = false;
    out
}

fn upload_estimate(visible: &VisiblePlacement, image_current: bool) -> usize {
    let place = 192;
    if image_current {
        place
    } else {
        visible.data.len().saturating_add(2) / 3 * 4 + 256 + place
    }
}

struct EncodeJob<'a> {
    host_id: u32,
    placement_id: u32,
    clipped: Clipped,
    placement: &'a ScreenPlacement,
    signature: ImageSignature,
    image_current: bool,
}

fn encode_one(cache: &mut HostGraphicsCache, out: &mut Vec<u8>, job: EncodeJob<'_>) {
    let EncodeJob {
        host_id,
        placement_id,
        clipped,
        placement,
        signature,
        image_current,
    } = job;
    let placed = placement_signature(
        clipped,
        placement.visible.z,
        placement.visible.scroll_offset,
    );
    let known = cache.placements.get(&(host_id, placement_id)).copied();
    let placement_current = known == Some(placed);
    if image_current && placement_current {
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
    if known.is_some() && known != Some(placed) {
        encode_delete_placement(out, host_id, placement_id);
    }
    if !placement_current {
        encode_display(out, clipped, host_id, placement_id, placement.visible.z);
    }
    cache.placements.insert((host_id, placement_id), placed);
}

fn host_image_id_for(
    cache: &mut HostGraphicsCache,
    out: &mut Vec<u8>,
    source: SourceKey,
    signature: ImageSignature,
) -> u32 {
    let existing = cache.sources.get(&source).copied();
    if let Some(existing) = existing {
        let known = cache.images.get(&existing).copied();
        if known.is_none_or(|known| known == signature) {
            return existing;
        }
        forget_source(cache, out, source, existing);
    }
    let id = allocate_host_image(cache);
    cache.sources.insert(source, id);
    id
}

fn host_placement_id_for(cache: &mut HostGraphicsCache, key: HostPlacementKey) -> Option<u32> {
    if let Some(id) = cache.host_placements.get(&key).copied() {
        return Some(id);
    }
    let id = allocate_host_placement(cache)?;
    cache.host_placements.insert(key, id);
    Some(id)
}

fn allocate_host_image(cache: &mut HostGraphicsCache) -> u32 {
    loop {
        cache.next_host_image = cache.next_host_image.saturating_add(1);
        if !(HOST_IMAGE_ID_BASE..=HOST_IMAGE_ID_LAST).contains(&cache.next_host_image) {
            cache.next_host_image = HOST_IMAGE_ID_BASE;
        }
        let id = cache.next_host_image;
        let used = cache.images.contains_key(&id)
            || cache.sources.values().any(|existing| *existing == id);
        if !used {
            return id;
        }
    }
}

fn allocate_host_placement(cache: &mut HostGraphicsCache) -> Option<u32> {
    let limit = if cache.placement_id_limit == 0 {
        900_000
    } else {
        cache.placement_id_limit
    };
    if cache.host_placements.len() >= limit as usize {
        return None;
    }
    for _ in 0..limit {
        cache.next_host_placement = cache.next_host_placement.saturating_add(1);
        if cache.next_host_placement == 0 || cache.next_host_placement > limit {
            cache.next_host_placement = 1;
        }
        let id = cache.next_host_placement;
        if !cache
            .host_placements
            .values()
            .any(|existing| *existing == id)
        {
            return Some(id);
        }
    }
    None
}

fn forget_source(
    cache: &mut HostGraphicsCache,
    out: &mut Vec<u8>,
    source: SourceKey,
    host_id: u32,
) {
    encode_delete_image(out, host_id);
    cache.sources.remove(&source);
    cache.images.remove(&host_id);
    cache.placements.retain(|(id, _), _| *id != host_id);
    cache
        .host_placements
        .retain(|key, _| key.pane_id != source.pane_id || key.image_id != source.image_id);
}

/// Clip destination cells to the pane and map every cut through the same
/// absolute offset-aware source edges used for overlay fragments.
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
    let fully_inside =
        left == 0 && top == 0 && cols == visible.grid_cols && rows == visible.grid_rows;
    let (source_x, source_y, source_width, source_height, x_offset, y_offset) = if fully_inside {
        // The whole shrunk image is on screen. Keep the child's source
        // rectangle; do not turn a scaled image into a top-left crop.
        (
            visible.source_x,
            visible.source_y,
            visible.source_width,
            visible.source_height,
            visible.x_offset,
            visible.y_offset,
        )
    } else {
        let rect = ratatui::layout::Rect::new(
            placement.area_x.saturating_add(column as u16),
            placement.area_y.saturating_add(row as u16),
            cols as u16,
            rows as u16,
        );
        let cropped = occlusion::crop_piece(placement, rect)?;
        (
            cropped.source_x,
            cropped.source_y,
            cropped.source_width,
            cropped.source_height,
            cropped.x_offset,
            cropped.y_offset,
        )
    };
    Some((
        Clipped {
            x: placement.area_x.saturating_add(column as u16),
            y: placement.area_y.saturating_add(row as u16),
            cols,
            rows,
            source_x,
            source_y,
            source_width,
            source_height,
            x_offset,
            y_offset,
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
mod bounds_tests;

#[cfg(test)]
mod tests;
