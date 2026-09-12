//! The kitty graphics protocol, to the depth a multiplexer owes a child.
//!
//! Luvus owns no pixels — the terminal displaying a client does. So a pane
//! cannot draw an image itself, and everything here is about connecting a
//! child that wants to draw one with a terminal that can.
//!
//! Two things follow from that.
//!
//! **The support question has a real answer.** The protocol's test is a query
//! action (`a=q`) followed by a primary device attributes request: answering
//! only the DA1 declares no graphics support. Luvus answers `OK` exactly when
//! the foreground client's terminal answered the same question, and declines
//! otherwise. Staying silent instead would be a valid answer but a poor one —
//! the child cannot tell it apart from a slow terminal or a multiplexer that
//! swallowed the sequence, so it waits out a timeout and then guesses. A child
//! that guesses optimistically paints image bytes at a terminal that cannot
//! render them, and the user sees garbage.
//!
//! **Commands are forwarded, not interpreted.** Luvus does not decode pixels.
//! A graphics command is opaque bytes that mean something to the terminal
//! holding the image, and passing it through unchanged is what lets that
//! terminal learn the image. Only commands that do not decide *where* the
//! image appears are passed on; position comes from the placeholder cells the
//! child writes into the grid, which are ordinary text that Luvus already
//! clips, scrolls, and reflows. See [`is_forwardable`].

pub(crate) mod placeholder;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use crate::terminal::theme_probe::CellSize;

/// What the terminals displaying the attached clients can do with an image.
///
/// One value is shared by every pane rather than copied into each, because it
/// describes the clients, not the pane. A child asks whether graphics work
/// synchronously, in the middle of parsing its own output, and the answer has
/// to be correct at that instant — a pane created between two clients
/// attaching must not be left believing something the others do not.
///
/// Panes stay honest when nothing here is known: a support query is declined,
/// and a pane reports no pixel size rather than a made-up one.
#[derive(Clone, Debug, Default)]
pub struct HostGraphics(Arc<HostGraphicsState>);

#[derive(Debug, Default)]
struct HostGraphicsState {
    /// Bit 0: a renderer can receive commands. Bit 1: the foreground can
    /// acknowledge new queries. Publish both together when ownership changes.
    clients: AtomicU8,
    /// Set by any pane that queued a command. Lets a render pass decide with
    /// one atomic load whether it is worth walking the panes at all, instead
    /// of locking every engine on every frame to find nothing.
    pending: AtomicBool,
    /// Cell width and height in pixels, packed into one word so a pane reads a
    /// consistent pair rather than a width from one client and a height from
    /// another. Zero means no client has reported one.
    cell_size: AtomicU32,
}

impl HostGraphics {
    pub(crate) fn supported(&self) -> bool {
        self.0.clients.load(Ordering::Relaxed) & 1 != 0
    }

    /// A single local display owns both negotiation and command delivery.
    pub(crate) fn set(&self, supported: bool) {
        self.set_clients(supported, supported);
    }

    /// New queries follow foreground ownership; existing streams may still
    /// reach a passive renderer. Updating one must not briefly enable the other.
    pub(crate) fn set_clients(&self, foreground: bool, any_renderer: bool) {
        let clients = u8::from(any_renderer) | (u8::from(foreground && any_renderer) << 1);
        self.0.clients.store(clients, Ordering::Relaxed);
    }

    pub(crate) fn query_supported(&self) -> bool {
        self.0.clients.load(Ordering::Relaxed) & 2 != 0
    }

    /// Pixel size of one cell, as reported by the terminal showing a client.
    ///
    /// A program that draws an image asks its pane how many pixels it has, and
    /// the pane can only answer if it knows this. `None` means no attached
    /// client's terminal reported one, and the pane says so rather than
    /// guessing — a made-up size renders an image at the wrong scale.
    pub(crate) fn cell_size(&self) -> Option<CellSize> {
        let packed = self.0.cell_size.load(Ordering::Relaxed);
        CellSize::unpack(packed)
    }

    /// Record a cell size, returning whether it changed.
    ///
    /// Panes carry this in their window size, which only changes when they are
    /// told to, so the caller has to know when to tell them.
    pub(crate) fn set_cell_size(&self, cell_size: Option<CellSize>) -> bool {
        let packed = cell_size.map_or(0, CellSize::pack);
        self.0.cell_size.swap(packed, Ordering::Relaxed) != packed
    }

    pub(crate) fn mark_pending(&self) {
        self.0.pending.store(true, Ordering::Release);
    }

    /// Non-consuming fence for a projection made after the last collection.
    /// Only the app thread consumes the flag; producers set it before their
    /// changed graphics/grid can be observed outside the engine lock.
    pub(crate) fn pending(&self) -> bool {
        self.0.pending.load(Ordering::Acquire)
    }

    /// Whether any pane may be holding commands, clearing the flag so a
    /// command queued during the collection that follows is not missed.
    ///
    /// Clearing before collecting is deliberate: a pane that queues in between
    /// sets the flag again and is picked up by the next pass. Clearing after
    /// would discard exactly that command.
    pub(crate) fn take_pending(&self) -> bool {
        self.0.pending.swap(false, Ordering::AcqRel)
    }
}

/// Most bytes of graphics commands a pane may hold between two frames, and the
/// most commands across them. Whichever is reached first stops collection.
///
/// This is a forwarding queue, not an image cache: it holds what a pane's child
/// emitted since the last frame was sent. A transmission arrives base64-encoded
/// in chunks of at most 4 KiB, so the budget carries a large image comfortably
/// while keeping a runaway child from growing a pane without limit.
const MAX_PENDING_GRAPHICS_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING_GRAPHICS_COMMANDS: usize = 4_096;

/// Most bytes of images a pane keeps so a client attaching later can be taught
/// them, and the most images across them.
///
/// A pane's grid outlives the client that was watching it, and the placeholder
/// cells in it name images by id. A client that attaches afterwards is sent
/// those cells, so it has to be sent the images they name or it would be asked
/// to draw something it was never given. Only the newest version of each image
/// is worth keeping — an id names one image, and a child that redraws replaces
/// it — so this holds a working set, not a history.
const MAX_RETAINED_GRAPHICS_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_RETAINED_IMAGES: usize = 64;

/// Graphics commands a pane's child emitted, waiting to reach the terminals
/// that can draw them.
///
/// Luvus does not decode any of this. A command is opaque bytes that mean
/// something to the terminal holding the pixels, and forwarding it verbatim is
/// what lets that terminal learn the image. Where the image then appears is
/// decided by the placeholder cells in the grid, not by these bytes.
#[derive(Default)]
pub(crate) struct GraphicsQueue {
    commands: Vec<Vec<u8>>,
    /// Cells owed to images whose sender expected the terminal to position
    /// them. Applied by the engine, which owns the grid.
    placements: Vec<Placement>,
    bytes: usize,
    /// Set when the budget was reached. A chunked transmission only means
    /// anything whole, so an overflowing queue is abandoned rather than
    /// forwarded with a hole in it.
    overflowed: bool,
    /// Whether the transfer currently being chunked is one worth forwarding.
    /// A continuation chunk carries only `m`, repeating none of the keys that
    /// made the decision, so the decision has to be remembered.
    forwarding_transfer: bool,
    /// Continuations precede any placement commands generated by a resize
    /// while the child's transfer is incomplete.
    continuation_index: Option<usize>,
    /// The images this pane has taught, newest last, for clients yet to attach.
    retained: Vec<RetainedImage>,
    retained_bytes: usize,
    /// Which retained image the transfer in progress is being collected into.
    retaining: Option<u32>,
    /// The virtual rectangle last forwarded for each image Luvus positions
    /// itself. A change is preceded by deleting the image's placements —
    /// see [`GraphicsQueue::clear_stale_virtual_rect`].
    virtual_rects: Vec<VirtualRect>,
}

/// The rectangle a terminal was last told to fit one image into.
struct VirtualRect {
    image_id: u32,
    columns: usize,
    rows: usize,
}

/// Everything a terminal needs in order to draw one image the pane holds.
struct RetainedImage {
    id: u32,
    /// The commands that carried the image, in the order they arrived. A large
    /// transmission arrives in chunks, and means nothing until all of them do.
    commands: Vec<Vec<u8>>,
    /// The placement that goes with it, when one was sent after the
    /// transmission. One is enough: a newer placement with the same id
    /// replaces the older on arrival, so only the newest is worth keeping.
    placement: Option<Vec<u8>>,
    bytes: usize,
}

/// What a command adds to the images a pane keeps.
enum Retain {
    /// Carries an image, replacing whatever that id held before.
    Open(u32),
    /// Adds a placement to an image already held, which is only worth keeping
    /// alongside the transmission it refers to.
    Place(u32),
    /// Continues the transfer already being collected.
    Chunk,
    /// Teaches no image, so there is nothing to keep.
    No,
}

impl GraphicsQueue {
    /// Record a graphics command if it is one Luvus can safely pass on.
    ///
    /// `payload` is the APC body with the `G` introducer stripped. `cursor` is
    /// where the grid cursor stood when the command arrived, which is where a
    /// terminal placing the image directly would have put its top-left corner.
    pub(crate) fn push(
        &mut self,
        payload: &[u8],
        cursor: (i32, usize),
        cell_size: Option<CellSize>,
    ) {
        let Some(control) = ControlData::parse(payload) else {
            return;
        };

        if control.is_continuation {
            // Chunks belong to the transfer that opened them, which already
            // carried every key the decision was made on.
            if self.forwarding_transfer {
                self.queue(payload, Retain::Chunk);
            }
            if !control.more {
                self.forwarding_transfer = false;
                self.continuation_index = None;
                self.retaining = None;
            }
            return;
        }

        // A new child command before the final chunk is an invalid transfer.
        // Do not let its prefix escape when the replacement command completes.
        if self.forwarding_transfer {
            self.commands.clear();
            self.placements.clear();
            self.bytes = 0;
            self.continuation_index = None;
            if let Some(id) = self.retaining {
                self.forget(id);
            }
        }
        let handling = classify(&control, payload, cell_size);
        // A rewritten opening chunk still opens the transfer its later chunks
        // belong to; those are forwarded as they came.
        self.forwarding_transfer = control.more && !matches!(handling, Handling::Drop);

        match handling {
            Handling::Drop => {}
            Handling::Forward => {
                let retain = self.plan_retention(&control);
                self.queue(payload, retain);
            }
            Handling::Virtualize {
                payload,
                mut placement,
            } => {
                placement.line = cursor.0;
                placement.column = cursor.1;
                self.clear_stale_virtual_rect(&placement);
                let retain = self.plan_retention(&control);
                self.queue(&payload, retain);
                self.placements.push(placement);
            }
        }
        self.continuation_index = self.forwarding_transfer.then_some(self.commands.len());
    }

    /// Delete a terminal's placements of an image whose virtual rectangle is
    /// about to change, so the command that follows leaves it exactly one.
    ///
    /// The protocol says re-transmitting an id deletes its placements, but a
    /// terminal that only replaces the image data — Ghostty 1.3 does — leaves
    /// the old virtual placement standing beside the new one and may draw
    /// either; the image then appears fitted into the rectangle it had
    /// before. Deleting by id first is explicit, and harmless where the
    /// terminal already did it. Only the rectangle matters here: the same
    /// rectangle re-sent every frame, as a streaming child does, costs nothing.
    fn clear_stale_virtual_rect(&mut self, placement: &Placement) {
        let known = self
            .virtual_rects
            .iter_mut()
            .find(|rect| rect.image_id == placement.image_id);
        match known {
            Some(rect) if rect.columns == placement.columns && rect.rows == placement.rows => {
                return;
            }
            Some(rect) => {
                rect.columns = placement.columns;
                rect.rows = placement.rows;
            }
            None => {
                if self.virtual_rects.len() == MAX_RETAINED_IMAGES {
                    self.virtual_rects.remove(0);
                }
                self.virtual_rects.push(VirtualRect {
                    image_id: placement.image_id,
                    columns: placement.columns,
                    rows: placement.rows,
                });
            }
        }
        // `d=i` takes the placements and leaves the image data, which the
        // command that follows either replaces or places again.
        let delete = format!("a=d,d=i,i={},q=2", placement.image_id);
        self.queue(delete.as_bytes(), Retain::No);
    }

    /// Fit an image the terminal already holds into a new rectangle.
    ///
    /// For the engine that stretched an image's cells to a resized pane: the
    /// cells alone change nothing on a terminal still fitting the image into
    /// the old rectangle. This is the protocol's own resize — the same
    /// placement id replaces the placement — and a client attaching later
    /// is taught the image with this placement rather than the older one.
    pub(crate) fn replace_virtual_rect(&mut self, placement: &Placement) {
        self.clear_stale_virtual_rect(placement);
        let place = format!(
            "a=p,i={},p={},U=1,c={},r={},q=2",
            placement.image_id, placement.placement_id, placement.columns, placement.rows
        );
        self.queue(place.as_bytes(), Retain::Place(placement.image_id));
    }

    /// Decide what one command leaves behind for a client yet to attach, and
    /// apply the deletions that take images away again.
    ///
    /// A delete aimed at an id by name is honoured; other scopes select by
    /// position or z-order, which only the terminal holding the image can
    /// resolve. Those are left alone, so an image may outlive its placements
    /// in this working set. That costs a little of the budget and shows
    /// nothing: what a client draws is decided by the placeholder cells in the
    /// grid, and those are gone once the child stops writing them.
    fn plan_retention(&mut self, control: &ControlData) -> Retain {
        match control.action {
            b'd' => {
                match (control.delete_scope, control.image_id) {
                    (b'A' | b'a', _) => self.forget_all(),
                    (b'I' | b'i', Some(id)) => self.forget(id),
                    _ => {}
                }
                Retain::No
            }
            b't' | b'T' => match control.image_id {
                Some(id) => Retain::Open(id),
                // Without an id the terminal assigns one and reports it back to
                // the child. Luvus does not read that reply, so it cannot name
                // the image later either.
                None => Retain::No,
            },
            b'p' => control.image_id.map_or(Retain::No, Retain::Place),
            _ => Retain::No,
        }
    }

    /// Wrap one command back into an APC sequence and hold it, within budget.
    fn queue(&mut self, payload: &[u8], retain: Retain) {
        if self.overflowed {
            return;
        }
        let length = payload.len() + 5;
        if self.commands.len() == MAX_PENDING_GRAPHICS_COMMANDS
            || self.bytes.saturating_add(length) > MAX_PENDING_GRAPHICS_BYTES
        {
            self.overflowed = true;
            self.commands = Vec::new();
            self.placements = Vec::new();
            self.bytes = 0;
            if let Some(id) = self.retaining {
                self.forget(id);
            }
            self.continuation_index = None;
            return;
        }
        let mut command = Vec::with_capacity(length);
        command.extend_from_slice(b"\x1b_G");
        command.extend_from_slice(payload);
        command.extend_from_slice(b"\x1b\\");
        self.bytes += command.len();
        let continuation = matches!(retain, Retain::Chunk);
        self.retain(&command, retain);
        if let Some(index) = self.continuation_index.as_mut().filter(|_| continuation) {
            self.commands.insert(*index, command);
            *index += 1;
        } else {
            self.commands.push(command);
        }
    }

    /// Keep a command so it can be replayed to a client that attaches later.
    fn retain(&mut self, command: &[u8], retain: Retain) {
        let placing = matches!(retain, Retain::Place(_));
        let id = match retain {
            Retain::No => return,
            Retain::Chunk => match self.retaining {
                Some(id) => id,
                None => return,
            },
            Retain::Open(id) => {
                self.forget(id);
                self.retained.push(RetainedImage {
                    id,
                    commands: Vec::new(),
                    placement: None,
                    bytes: 0,
                });
                self.retaining = Some(id);
                id
            }
            Retain::Place(id) => id,
        };

        let Some(image) = self.retained.iter_mut().find(|image| image.id == id) else {
            return;
        };
        if placing {
            // The newest placement is the only one a terminal ends up with,
            // so it is the only one worth teaching a client that attaches.
            if let Some(previous) = image.placement.take() {
                image.bytes -= previous.len();
                self.retained_bytes -= previous.len();
            }
            image.placement = Some(command.to_vec());
        } else {
            image.commands.push(command.to_vec());
        }
        image.bytes += command.len();
        self.retained_bytes += command.len();
        self.enforce_retention_budget();
    }

    /// Drop the oldest images until the working set is back within budget.
    fn enforce_retention_budget(&mut self) {
        while self.retained_bytes > MAX_RETAINED_GRAPHICS_BYTES
            || self.retained.len() > MAX_RETAINED_IMAGES
        {
            // The image being collected is the one the pane is about to show,
            // so evicting it would trade a stale image for no image at all.
            let Some(index) = self
                .retained
                .iter()
                .position(|image| Some(image.id) != self.retaining)
            else {
                return;
            };
            let dropped = self.retained.remove(index);
            self.retained_bytes -= dropped.bytes;
        }
    }

    fn forget(&mut self, id: u32) {
        if let Some(index) = self.retained.iter().position(|image| image.id == id) {
            let dropped = self.retained.remove(index);
            self.retained_bytes -= dropped.bytes;
        }
        if self.retaining == Some(id) {
            self.retaining = None;
        }
    }

    fn forget_all(&mut self) {
        self.retained.clear();
        self.retained_bytes = 0;
        self.retaining = None;
    }

    /// Every image this pane holds, in the order a terminal must learn them.
    pub(crate) fn retained(&self) -> Vec<Vec<u8>> {
        self.retained
            .iter()
            .filter(|image| !(self.forwarding_transfer && self.retaining == Some(image.id)))
            .flat_map(|image| image.commands.iter().chain(image.placement.iter()).cloned())
            .collect()
    }

    /// Deliver complete transfers only. Another pane may be drained immediately
    /// afterwards, so exposing even an opening chunk would let its commands
    /// interrupt this image on the shared host terminal.
    pub(crate) fn drain(&mut self) -> Vec<Vec<u8>> {
        if self.overflowed {
            self.forwarding_transfer = false;
            self.continuation_index = None;
        } else if self.forwarding_transfer {
            return Vec::new();
        }
        self.bytes = 0;
        self.overflowed = false;
        std::mem::take(&mut self.commands)
    }

    /// Take the cells owed to images placed at the cursor.
    pub(crate) fn drain_placements(&mut self) -> Vec<Placement> {
        if self.forwarding_transfer {
            return Vec::new();
        }
        std::mem::take(&mut self.placements)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Most bytes of graphics one client may have waiting behind its socket
/// writer, and the most entries across them.
///
/// The budget matches a pane's queue: coalescing means a client can never owe
/// more than the panes emitted, and one image is the largest thing that has to
/// fit whole.
const MAX_BACKLOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_BACKLOG_ENTRIES: usize = 256;

/// Graphics commands one client has yet to be sent, coalesced by image id.
///
/// A terminal has to learn an image before it is shown the cells naming it, so
/// what a pane emits cannot simply be pushed at a client that is behind: its
/// grid still holds the previous placement, and an image that arrives ahead of
/// the frame is drawn into the old rectangle — the wrong size, anchored
/// top-left, with the pane blank around it. Commands wait here for the frame
/// they belong to.
///
/// They are coalesced because an id names one image. A child that redraws
/// replaces what its id means, so an older command for that id is not merely
/// wasteful: a child streaming frames reuses the file the command refers to,
/// and sending the old command teaches the terminal today's pixels under
/// yesterday's instructions. Only the newest command for an id is worth
/// sending — and a chunked transfer is kept whole, because it means nothing in
/// halves.
///
/// Overflow asks for a resync rather than a partial delivery. The entries
/// depend on each other, and a client given some of them would draw an image it
/// was taught in part; re-teaching it every image its panes still hold is the
/// only state that is certainly right.
#[derive(Default)]
pub(crate) struct GraphicsBacklog {
    entries: Vec<BacklogEntry>,
    bytes: usize,
    /// The entry continuation chunks belong to, named by sequence rather than
    /// by position so that a delete removing an entry cannot silently redirect
    /// the rest of a transfer into another image's.
    collecting: Option<Collecting>,
    next_seq: u64,
    needs_resync: bool,
}

/// One image's pending commands, or one command that names no image.
struct BacklogEntry {
    /// The image this entry teaches, when its command named one. An entry
    /// without an id stands on its own: nothing can replace it, because
    /// nothing can be said to supersede it.
    id: Option<u32>,
    commands: Vec<Vec<u8>>,
    bytes: usize,
    seq: u64,
}

/// The transfer being chunked, and the image it carries.
#[derive(Clone, Copy)]
struct Collecting {
    id: Option<u32>,
    seq: u64,
}

impl GraphicsBacklog {
    /// Add what a render pass collected from the panes.
    pub(crate) fn push(&mut self, commands: &[Vec<u8>]) {
        for command in commands {
            self.push_one(command);
        }
    }

    fn push_one(&mut self, command: &[u8]) {
        let Some(control) = payload(command).and_then(ControlData::parse) else {
            // Not a command this module wrapped, or control data it cannot
            // read. Luvus does not interpret these bytes, so the honest thing
            // is to pass them on in order rather than to decide they are junk.
            self.open(None, command, false);
            return;
        };

        if control.is_continuation {
            self.chunk(command, control.more);
            return;
        }

        match control.action {
            // A transmission replaces the image its id names, and with it
            // everything still waiting to teach that id.
            b't' | b'T' => {
                if let Some(id) = control.image_id {
                    self.forget(id);
                }
                self.open(control.image_id, command, control.more);
            }
            // A placement only means anything alongside the image it places,
            // so it travels with that image when one is still waiting.
            b'p' => match control.image_id.and_then(|id| self.sequence_of(id)) {
                Some(seq) => self.append(seq, command, control.more),
                None => self.open(control.image_id, command, control.more),
            },
            b'd' => {
                match (control.delete_scope, control.image_id) {
                    (b'A' | b'a', _) => self.forget_all(),
                    (b'I' | b'i', Some(id)) => self.forget(id),
                    _ => {}
                }
                // The delete itself still has to arrive: it is the only thing
                // that takes back an image the client already learned.
                self.open(None, command, false);
            }
            // Anything else — a query, a key this build has never heard of —
            // is kept where it arrived. The terminal is the one that has to
            // make sense of it.
            _ => self.open(None, command, false),
        }
    }

    /// Add a chunk to the transfer being collected.
    ///
    /// That transfer may have been delivered since its opening chunk arrived,
    /// in which case a fresh entry carries the rest: the chunks reach the
    /// terminal in order either way, and it is reading one transfer. A chunk
    /// with no transfer at all is dropped — it has no beginning to belong to,
    /// and a terminal cannot make an image out of a middle.
    fn chunk(&mut self, command: &[u8], more: bool) {
        let Some(collecting) = self.collecting else {
            return;
        };
        if self.entries.iter().any(|entry| entry.seq == collecting.seq) {
            self.append(collecting.seq, command, more);
        } else {
            self.open(collecting.id, command, more);
        }
    }

    /// Start an entry, and remember it while more of its transfer is coming.
    fn open(&mut self, id: Option<u32>, command: &[u8], more: bool) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.bytes = self.bytes.saturating_add(command.len());
        self.entries.push(BacklogEntry {
            id,
            commands: vec![command.to_vec()],
            bytes: command.len(),
            seq,
        });
        self.collecting = more.then_some(Collecting { id, seq });
        self.enforce_budget();
    }

    fn append(&mut self, seq: u64, command: &[u8], more: bool) {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.seq == seq) else {
            return;
        };
        entry.commands.push(command.to_vec());
        entry.bytes += command.len();
        let id = entry.id;
        self.bytes = self.bytes.saturating_add(command.len());
        self.collecting = more.then_some(Collecting { id, seq });
        self.enforce_budget();
    }

    fn sequence_of(&self, id: u32) -> Option<u64> {
        self.entries
            .iter()
            .find(|entry| entry.id == Some(id))
            .map(|entry| entry.seq)
    }

    fn forget(&mut self, id: u32) {
        let mut dropped = 0;
        self.entries.retain(|entry| {
            let keep = entry.id != Some(id);
            if !keep {
                dropped += entry.bytes;
            }
            keep
        });
        self.bytes = self.bytes.saturating_sub(dropped);
        if self
            .collecting
            .is_some_and(|collecting| collecting.id == Some(id))
        {
            self.collecting = None;
        }
    }

    fn forget_all(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.collecting = None;
    }

    /// Abandon everything once the budget is reached, and ask to be re-taught.
    fn enforce_budget(&mut self) {
        if self.bytes <= MAX_BACKLOG_BYTES && self.entries.len() <= MAX_BACKLOG_ENTRIES {
            return;
        }
        self.entries.clear();
        self.bytes = 0;
        self.collecting = None;
        self.needs_resync = true;
    }

    /// Take everything waiting, in the order the panes emitted it.
    ///
    /// A transfer still being chunked stays remembered, so its remaining chunks
    /// are collected into a new entry rather than dropped for having lost the
    /// one they opened with.
    pub(crate) fn take(&mut self) -> Vec<Vec<u8>> {
        self.bytes = 0;
        std::mem::take(&mut self.entries)
            .into_iter()
            .flat_map(|entry| entry.commands)
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether this client has to be taught its panes' images again, clearing
    /// the request so it is answered once.
    pub(crate) fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.needs_resync)
    }
}

/// One command's control data and body, with the APC wrapper [`GraphicsQueue`]
/// put around it stripped back off.
fn payload(command: &[u8]) -> Option<&[u8]> {
    command
        .strip_prefix(b"\x1b_G".as_slice())?
        .strip_suffix(b"\x1b\\".as_slice())
}

/// What Luvus does with one graphics command.
enum Handling {
    /// Not something Luvus can pass on, so the child's command stops here.
    Drop,
    /// Safe exactly as written.
    Forward,
    /// Safe once its placement is made virtual. The rewritten command carries
    /// the same image, positioned by cells Luvus writes into its own grid.
    Virtualize {
        payload: Vec<u8>,
        placement: Placement,
    },
}

/// Cells Luvus must write so a forwarded image appears where its sender meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) image_id: u32,
    /// The placement id the forwarded command carries. Keyed together with
    /// the image id, it is what lets a later command with the same pair
    /// replace this placement instead of standing beside it.
    pub(crate) placement_id: u32,
    pub(crate) columns: usize,
    pub(crate) rows: usize,
    /// Grid position of the cursor when the command arrived, which is the
    /// image's top-left corner.
    pub(crate) line: i32,
    pub(crate) column: usize,
    /// Whether the cursor should end up past the image, as it would on a
    /// terminal that placed it directly. `C=1` asks for it to stay put.
    pub(crate) move_cursor: bool,
}

/// Decide what to do with a command that opens a transfer.
///
/// A command that does not decide *where* an image appears passes through: a
/// transmission with a virtual placement (`U=1`) is a prototype that draws
/// nothing until placeholder cells refer to it, and transmit-only and delete
/// place nothing at all.
///
/// A command that asks for the image at the cursor cannot be forwarded as it
/// stands. The receiving terminal's cursor belongs to Luvus's own rendering,
/// not to this pane, so the image would land in the wrong place, ignore the
/// pane's edges, and survive a split as a ghost — the mistake tmux made first
/// and had to undo. Instead the placement is made virtual and Luvus writes the
/// placeholder cells itself, which is how tmux eventually fixed it: the cells
/// are ordinary text, so the machinery that already clips, scrolls and reflows
/// text carries the image with them.
fn classify(control: &ControlData, payload: &[u8], cell_size: Option<CellSize>) -> Handling {
    match control.action {
        b'T' | b'p' if control.virtual_placement => Handling::Forward,
        b'T' | b'p' => virtualize(control, payload, cell_size),
        // Transmit without displaying, and delete, place nothing by themselves.
        b't' | b'd' => Handling::Forward,
        _ => Handling::Drop,
    }
}

/// Rewrite a placement at the cursor into a virtual one of the same size.
///
/// The rectangle comes from the sender when it named one (`c=`/`r=`), and
/// otherwise from the image's pixel size divided by the terminal's cell size.
/// Without either the image cannot be turned into cells, so it is dropped
/// rather than guessed at: a wrong rectangle draws the image at the wrong
/// scale, which is worse than not drawing it.
fn virtualize(control: &ControlData, payload: &[u8], cell_size: Option<CellSize>) -> Handling {
    // The placeholder cells carry the image id in their foreground color, so
    // an image without one cannot be addressed by them at all.
    let Some(image_id) = control.image_id else {
        return Handling::Drop;
    };

    let cells = |pixels: Option<u32>, per_cell: u16| {
        // Round up: a part-covered cell still shows part of the image.
        pixels.map(|pixels| pixels.div_ceil(u32::from(per_cell.max(1))) as usize)
    };
    let columns = control
        .columns
        .map(|columns| columns as usize)
        .or_else(|| cells(control.width_px, cell_size?.width));
    let rows = control
        .rows
        .map(|rows| rows as usize)
        .or_else(|| cells(control.height_px, cell_size?.height));
    let (Some(columns), Some(rows)) = (columns, rows) else {
        return Handling::Drop;
    };

    // A rectangle the protocol cannot address in diacritics is refused rather
    // than clipped, so a partial image never masquerades as a whole one.
    if columns == 0
        || rows == 0
        || columns > placeholder::MAX_EXTENT
        || rows > placeholder::MAX_EXTENT
    {
        return Handling::Drop;
    }

    // The child's own placement id is kept; one that named none gets 1. A
    // placement with an id is the protocol's way to resize without flicker:
    // the next command with the same (image id, placement id) replaces it.
    // Without one, a terminal adds a fresh placement every frame and may draw
    // any of them — Ghostty 1.3 drew the first, so a resized image kept
    // fitting into the rectangle it had before the resize.
    let placement_id = control.placement_id.unwrap_or(1);
    Handling::Virtualize {
        payload: rewrite_placement(payload, columns, rows, placement_id),
        placement: Placement {
            image_id,
            placement_id,
            columns,
            rows,
            // Filled in by the caller, which knows where the cursor was.
            line: 0,
            column: 0,
            move_cursor: !control.cursor_stays,
        },
    }
}

/// The same command with its placement keys replaced by a virtual placement
/// carrying `placement_id`.
///
/// Everything else is preserved byte for byte, including keys Luvus does not
/// understand: the protocol keeps growing, and the terminal receiving this is
/// the one that has to make sense of the image.
fn rewrite_placement(payload: &[u8], columns: usize, rows: usize, placement_id: u32) -> Vec<u8> {
    let (control, body) = match payload.iter().position(|byte| *byte == b';') {
        Some(end) => (&payload[..end], Some(&payload[end + 1..])),
        None => (payload, None),
    };

    let mut rewritten = Vec::with_capacity(payload.len() + 24);
    for pair in control.split(|byte| *byte == b',') {
        if pair.is_empty() {
            continue;
        }
        // Drop the keys that describe the old placement; the rest is the image.
        if matches!(pair.first(), Some(b'p' | b'C' | b'U' | b'c' | b'r'))
            && pair.get(1) == Some(&b'=')
        {
            continue;
        }
        if !rewritten.is_empty() {
            rewritten.push(b',');
        }
        rewritten.extend_from_slice(pair);
    }
    if !rewritten.is_empty() {
        rewritten.push(b',');
    }
    rewritten.extend_from_slice(format!("U=1,p={placement_id},c={columns},r={rows}").as_bytes());
    if let Some(body) = body {
        rewritten.push(b';');
        rewritten.extend_from_slice(body);
    }
    rewritten
}

/// Reply Luvus owes the child for one kitty graphics command, if any.
///
/// `payload` is the APC body with the `G` introducer already stripped, as
/// captured by the terminal engine. Returns `None` for every command that is
/// not a query and for a query whose sender asked to be left alone.
pub(crate) fn query_reply(payload: &[u8], supported: bool) -> Option<Vec<u8>> {
    let control = ControlData::parse(payload)?;

    if control.action != b'q' {
        // Transmission, placement, and deletion are answered by the terminal
        // that actually holds the image, not here.
        return None;
    }
    if control.quiet >= 2 {
        // `q=2` suppresses even errors. Honour it: a client that asked for
        // silence must not receive a reply it is not reading.
        return None;
    }

    // The spec keys an acknowledgement to the image id when the sender chose
    // one. A query without an id is answered against the action instead, which
    // is what other multiplexers emit and what clients match on.
    let mut reply = b"\x1b_G".to_vec();
    match control.image_id {
        Some(id) => {
            reply.extend_from_slice(b"i=");
            reply.extend_from_slice(id.to_string().as_bytes());
        }
        None => reply.extend_from_slice(b"a=q"),
    }
    reply.extend_from_slice(if supported {
        b";OK\x1b\\".as_slice()
    } else {
        b";ENOTSUPPORTED:foreground client cannot draw images\x1b\\".as_slice()
    });
    Some(reply)
}

/// The keys of a graphics command's control data that Luvus acts on.
struct ControlData {
    action: u8,
    image_id: Option<u32>,
    /// `p=`: the placement id. Placements are keyed by (image id, placement
    /// id), and one sent without an id is a new placement every time.
    placement_id: Option<u32>,
    quiet: u8,
    /// `U=1`: the placement is a prototype and draws nothing on its own.
    virtual_placement: bool,
    /// `s=` and `v=`: the source image's pixel dimensions.
    width_px: Option<u32>,
    height_px: Option<u32>,
    /// `c=` and `r=`: the cell rectangle the sender chose for the image.
    columns: Option<u32>,
    rows: Option<u32>,
    /// `C=1`: the cursor is to stay where it is instead of moving past the
    /// image. A full-window image sets this so it cannot force a scroll.
    cursor_stays: bool,
    /// `m=1`: more chunks of this transfer follow.
    more: bool,
    /// `d=`: what a delete is aimed at. Uppercase frees the image's data as
    /// well as its placements; lowercase leaves the image behind.
    delete_scope: u8,
    /// A chunk that continues a transfer rather than opening one. The protocol
    /// requires such a chunk to carry only `m` and optionally `q`, so anything
    /// else present means this command stands on its own.
    is_continuation: bool,
}

impl ControlData {
    /// Parse the `key=value,...` prefix of a graphics command.
    ///
    /// Returns `None` when the control data is malformed rather than guessing:
    /// an unparsable command is one Luvus has no business answering. Unknown
    /// keys are skipped, because the protocol keeps adding them and a command
    /// carrying one is still a valid command.
    fn parse(payload: &[u8]) -> Option<Self> {
        let control = match payload.iter().position(|byte| *byte == b';') {
            Some(end) => &payload[..end],
            None => payload,
        };

        // `a=T` is the protocol default, so a command that omits the action is
        // a transmit-and-display, never a query.
        let mut parsed = ControlData {
            action: b'T',
            image_id: None,
            placement_id: None,
            quiet: 0,
            virtual_placement: false,
            width_px: None,
            height_px: None,
            columns: None,
            rows: None,
            cursor_stays: false,
            more: false,
            // `d=a` is the protocol default for a delete.
            delete_scope: b'a',
            is_continuation: true,
        };

        for pair in control.split(|byte| *byte == b',') {
            if pair.is_empty() {
                continue;
            }
            let split = pair.iter().position(|byte| *byte == b'=')?;
            let (key, value) = (&pair[..split], &pair[split + 1..]);
            // Every key in the protocol is a single character.
            let [key] = key else {
                return None;
            };
            // `m` and `q` are the only keys a continuation chunk may carry.
            if !matches!(key, b'm' | b'q') {
                parsed.is_continuation = false;
            }
            match key {
                b'a' => parsed.action = *value.first()?,
                b'i' => parsed.image_id = Some(parse_u32(value)?).filter(|id| *id != 0),
                b'p' => parsed.placement_id = Some(parse_u32(value)?).filter(|id| *id != 0),
                b'q' => parsed.quiet = parse_u32(value)?.min(u32::from(u8::MAX)) as u8,
                b'U' => parsed.virtual_placement = parse_u32(value)? != 0,
                b's' => parsed.width_px = Some(parse_u32(value)?),
                b'v' => parsed.height_px = Some(parse_u32(value)?),
                b'c' => parsed.columns = Some(parse_u32(value)?),
                b'r' => parsed.rows = Some(parse_u32(value)?),
                b'C' => parsed.cursor_stays = parse_u32(value)? != 0,
                b'm' => parsed.more = parse_u32(value)? != 0,
                b'd' => parsed.delete_scope = *value.first()?,
                _ => {}
            }
        }

        // A command carrying no keys at all opens a transfer rather than
        // continuing one.
        if control.is_empty() {
            parsed.is_continuation = false;
        }
        Some(parsed)
    }
}

/// Parse an unsigned control-data value, rejecting anything that is not one.
fn parse_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() {
        return None;
    }
    let mut parsed: u32 = 0;
    for byte in value {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        parsed = parsed.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No attached client can draw, which is the state a pane is in until one
    /// that can attaches.
    fn reply(payload: &str) -> Option<String> {
        query_reply(payload.as_bytes(), false).map(|reply| String::from_utf8(reply).unwrap())
    }

    fn reply_when_supported(payload: &str) -> Option<String> {
        query_reply(payload.as_bytes(), true).map(|reply| String::from_utf8(reply).unwrap())
    }

    #[test]
    fn query_with_image_id_is_answered_against_that_id() {
        // The exact probe terminal-browser sends.
        let answer = reply("i=4207,a=q,t=d,f=24,s=1,v=1;AAAA").expect("a query is answered");
        assert!(
            answer.starts_with("\x1b_Gi=4207;"),
            "reply must be keyed to the queried id: {answer:?}"
        );
        assert!(
            answer.contains("ENOTSUPPORTED"),
            "reply must decline, not acknowledge: {answer:?}"
        );
        assert!(answer.ends_with("\x1b\\"), "reply must be ST-terminated");
        assert!(
            !answer.contains(";OK"),
            "claiming support would make clients paint unrenderable bytes"
        );
    }

    #[test]
    fn a_query_is_acknowledged_once_a_client_can_draw() {
        let answer =
            reply_when_supported("i=4207,a=q,t=d,f=24,s=1,v=1;AAAA").expect("a query is answered");
        assert_eq!(
            answer, "\x1b_Gi=4207;OK\x1b\\",
            "the spec's acknowledgement, keyed to the queried id"
        );
        assert_eq!(
            reply_when_supported("a=q").as_deref(),
            Some("\x1b_Ga=q;OK\x1b\\")
        );
    }

    #[test]
    fn support_never_turns_a_non_query_into_a_reply() {
        // Only the terminal holding the image answers a transmission.
        for payload in ["a=T,f=100,s=1,v=1;AAAA", "a=p,i=1", "a=d,d=A"] {
            assert_eq!(reply_when_supported(payload), None, "{payload:?}");
        }
        assert_eq!(
            reply_when_supported("a=q,i=7,q=2"),
            None,
            "q=2 stays silent"
        );
    }

    #[test]
    fn query_without_image_id_is_answered_against_the_action() {
        let answer = reply("a=q").expect("a query is answered");
        assert!(answer.starts_with("\x1b_Ga=q;"), "{answer:?}");
    }

    #[test]
    fn zero_image_id_is_not_an_id() {
        // `i=0` means "unassigned" in the protocol, not "image number zero".
        let answer = reply("a=q,i=0").expect("a query is answered");
        assert!(answer.starts_with("\x1b_Ga=q;"), "{answer:?}");
    }

    #[test]
    fn non_query_commands_are_silent() {
        for payload in [
            "a=T,f=100,s=1,v=1;AAAA", // transmit and display
            "f=100;AAAA",             // action omitted: defaults to transmit
            "a=p,i=1",                // place
            "a=d,d=A",                // delete
            "a=f,i=1",                // animation frame
        ] {
            assert_eq!(reply(payload), None, "must not answer {payload:?}");
        }
    }

    #[test]
    fn quiet_two_suppresses_the_reply() {
        assert_eq!(reply("a=q,i=7,q=2"), None);
        // `q=1` suppresses only success; an error still reaches the sender.
        assert!(reply("a=q,i=7,q=1").is_some());
    }

    #[test]
    fn malformed_control_data_is_never_answered() {
        for payload in [
            "a",                    // no value
            "a=q,i",                // missing separator after a valid pair
            "a=q,=9",               // empty key
            "aa=q",                 // multi-character key
            "a=q,i=99999999999999", // id beyond the protocol's range
            "a=q,i=-1",             // negative where unsigned is required
            "a=q,i=",               // empty value
            "a=q,i=12x",            // trailing garbage
        ] {
            assert_eq!(reply(payload), None, "must not answer {payload:?}");
            assert_eq!(
                reply_when_supported(payload),
                None,
                "must not answer {payload:?}"
            );
        }
    }

    #[test]
    fn unknown_keys_do_not_invalidate_a_query() {
        // The protocol keeps growing keys; a query carrying one Luvus has never
        // heard of is still a query.
        assert!(reply("a=q,i=9,z=-1,U=1,X=3").is_some());
    }

    /// A cell of the size a real terminal reports.
    const CELL: CellSize = CellSize {
        width: 14,
        height: 34,
    };

    /// Queue commands the way the engine does, and report both what goes to the
    /// terminal and what Luvus owes its own grid.
    fn queued_at(
        payloads: &[&str],
        cursor: (i32, usize),
        cell_size: Option<CellSize>,
    ) -> (Vec<String>, Vec<Placement>) {
        let mut queue = GraphicsQueue::default();
        for payload in payloads {
            queue.push(payload.as_bytes(), cursor, cell_size);
        }
        let commands = queue
            .drain()
            .into_iter()
            .map(|command| String::from_utf8(command).unwrap())
            .collect();
        (commands, queue.drain_placements())
    }

    fn queued(payloads: &[&str]) -> Vec<String> {
        queued_at(payloads, (0, 0), Some(CELL)).0
    }

    #[test]
    fn a_virtual_placement_is_forwarded_because_the_grid_positions_it() {
        // `U=1` creates a prototype that draws nothing on its own; the
        // placeholder cells decide where it lands.
        assert_eq!(queued(&["a=T,U=1,i=1,c=2,r=2,f=100;AAAA"]).len(), 1);
        assert_eq!(queued(&["a=p,U=1,i=1,c=2,r=2"]).len(), 1);
        // Transmit-only and delete place nothing by themselves.
        assert_eq!(queued(&["a=t,i=1,f=100;AAAA"]).len(), 1);
        assert_eq!(queued(&["a=d,d=I,i=1"]).len(), 1);
    }

    #[test]
    fn a_placement_at_the_cursor_is_rewritten_instead_of_forwarded_as_it_stands() {
        // Forwarding this as written is the mistake tmux made first and had to
        // undo: the image lands wherever the outer terminal's cursor happens to
        // be, ignores the pane, and survives a split as a ghost. Made virtual,
        // the placeholder cells decide where it goes.
        let (commands, placements) = queued_at(
            &["a=T,f=32,s=28,v=68,t=d,i=7,p=1,C=1,q=2;AAAA"],
            (3, 5),
            Some(CELL),
        );
        assert_eq!(
            commands.len(),
            2,
            "a rectangle the terminal has not been told about is preceded by a \
             delete of whatever it holds for the id: {commands:?}"
        );
        assert_eq!(commands[0], "\x1b_Ga=d,d=i,i=7,q=2\x1b\\");
        let command = &commands[1];
        assert!(
            command.contains("U=1,p=1,c=2,r=2"),
            "28x68 pixels over a 14x34 cell is 2x2 cells, under the child's own \
             placement id: {command:?}"
        );
        assert!(
            !command.contains("C=1"),
            "the placement at the cursor must not survive the rewrite: {command:?}"
        );
        assert!(
            command.contains("i=7")
                && command.contains("f=32")
                && command.ends_with(";AAAA\u{1b}\\"),
            "everything else is preserved byte for byte: {command:?}"
        );

        assert_eq!(
            placements,
            vec![Placement {
                image_id: 7,
                placement_id: 1,
                columns: 2,
                rows: 2,
                line: 3,
                column: 5,
                move_cursor: false,
            }],
            "the cells go where the cursor was, and C=1 keeps it there"
        );
    }

    /// A child that streams re-sends the same placement every frame. The
    /// delete that clears a stale rectangle must not ride along with each of
    /// them — only with a rectangle the terminal has not been told about.
    #[test]
    fn a_rectangle_the_terminal_already_holds_is_not_deleted_again() {
        let same = "a=T,f=32,s=28,v=68,t=d,i=7,p=1,C=1,q=2;AAAA";
        let (commands, _) = queued_at(&[same, same, same], (0, 0), Some(CELL));
        assert_eq!(
            commands.len(),
            4,
            "one delete, then the three transmissions: {commands:?}"
        );
        assert!(commands[0].contains("a=d,d=i,i=7"));
        assert!(commands[1..].iter().all(|command| command.contains("a=T")));

        // A new size is a new rectangle. Ghostty 1.3 keeps the old virtual
        // placement beside the new one and draws whichever it finds first,
        // so the terminal's placements for the id go before the new one.
        let wider = "a=T,f=32,s=56,v=68,t=d,i=7,p=1,C=1,q=2;BBBB";
        let (commands, _) = queued_at(&[same, wider], (0, 0), Some(CELL));
        assert_eq!(commands.len(), 4, "{commands:?}");
        assert!(commands[2].contains("a=d,d=i,i=7"), "{commands:?}");
        assert!(commands[3].contains("U=1,p=1,c=4,r=2"), "{commands:?}");
    }

    /// The engine stretched an image's cells to a resized pane. The terminal
    /// has to be told the new rectangle or it keeps fitting the image into
    /// the old one, and a client attaching later must learn the newest.
    #[test]
    fn replacing_a_virtual_rectangle_re_places_the_image_and_is_what_is_retained() {
        let stretched = |columns, rows| Placement {
            image_id: 7,
            placement_id: 1,
            columns,
            rows,
            line: 0,
            column: 0,
            move_cursor: false,
        };
        let mut queue = GraphicsQueue::default();
        queue.push(
            b"a=T,f=32,s=28,v=68,t=d,i=7,p=1,C=1,q=2;AAAA",
            (0, 0),
            Some(CELL),
        );
        queue.drain();

        queue.replace_virtual_rect(&stretched(5, 3));
        let commands: Vec<String> = queue
            .drain()
            .into_iter()
            .map(|command| String::from_utf8(command).unwrap())
            .collect();
        assert_eq!(
            commands,
            vec![
                "\x1b_Ga=d,d=i,i=7,q=2\x1b\\".to_string(),
                "\x1b_Ga=p,i=7,p=1,U=1,c=5,r=3,q=2\x1b\\".to_string(),
            ],
            "the protocol's own resize: the same placement id, a new rectangle"
        );

        let retained = queue.retained();
        assert_eq!(
            retained.len(),
            2,
            "the transmission and its one placement: {retained:?}"
        );
        assert!(String::from_utf8(retained[1].clone())
            .unwrap()
            .contains("c=5,r=3"));

        // Another resize replaces the retained placement rather than adding
        // one: a later client is taught the image where it is now.
        queue.replace_virtual_rect(&stretched(6, 3));
        let retained = queue.retained();
        assert_eq!(retained.len(), 2, "{retained:?}");
        assert!(String::from_utf8(retained[1].clone())
            .unwrap()
            .contains("c=6,r=3"));
    }

    #[test]
    fn a_sender_that_names_its_own_rectangle_is_taken_at_its_word() {
        // `c=`/`r=` say what the sender wants in cells, so no division is
        // needed and no cell size has to be known.
        let (commands, placements) = queued_at(&["a=T,i=9,c=4,r=3;AAAA"], (0, 0), None);
        assert!(commands[1].contains("U=1,p=1,c=4,r=3"), "{commands:?}");
        assert_eq!(placements[0].columns, 4);
        assert_eq!(placements[0].rows, 3);
        assert!(
            placements[0].move_cursor,
            "without C=1 the cursor ends past the image, as on a real terminal"
        );
    }

    #[test]
    fn an_image_that_cannot_be_measured_is_dropped_rather_than_guessed_at() {
        // Drawing at the wrong scale is worse than not drawing: the user sees a
        // stretched image and no reason for it.
        assert!(
            queued_at(&["a=T,f=32,s=28,v=68,i=7;AAAA"], (0, 0), None)
                .0
                .is_empty(),
            "pixel dimensions are useless without the terminal's cell size"
        );
        assert!(
            queued(&["a=T,f=100,i=7;AAAA"]).is_empty(),
            "a PNG that carries its size only inside itself cannot be measured"
        );
        assert!(
            queued(&["a=T,f=32,s=28,v=68;AAAA"]).is_empty(),
            "placeholder cells address an image by id, so an image needs one"
        );
    }

    #[test]
    fn a_chunked_transfer_is_kept_or_dropped_whole() {
        // Only the first chunk carries the keys that decide this; the rest
        // carry `m` alone, so the decision has to survive across them.
        let forwarded = queued(&["a=T,U=1,i=1,c=2,r=2,f=100,m=1;AAAA", "m=1;BBBB", "m=0;CCCC"]);
        assert_eq!(forwarded.len(), 3, "a whole transfer is forwarded: half of");
        assert!(forwarded[2].contains("CCCC"));

        // A refused transfer must not leak its continuation chunks, which
        // would reach the terminal as a transfer with no beginning.
        assert!(
            queued(&["a=T,i=1,f=100,m=1;AAAA", "m=1;BBBB", "m=0;CCCC"]).is_empty(),
            "a refused transfer must not leak its chunks"
        );
    }

    #[test]
    fn a_transfer_that_follows_a_refused_one_is_judged_on_its_own() {
        let forwarded = queued(&[
            "a=T,i=1,f=100;AAAA",             // refused, and complete
            "a=T,U=1,i=2,c=1,r=1,f=100;BBBB", // must still be forwarded
        ]);
        assert_eq!(forwarded.len(), 1);
        assert!(forwarded[0].contains("BBBB"));
    }

    #[test]
    fn an_oversized_transfer_is_abandoned_rather_than_forwarded_with_a_hole() {
        let mut queue = GraphicsQueue::default();
        let payload = format!("a=t,i=1,m=1;{}", "A".repeat(4096));
        let command = format!("\x1b_G{payload}\x1b\\");
        queue.push(payload.as_bytes(), (0, 0), Some(CELL));
        let continuation = format!("m=1;{}", "A".repeat(4096));
        // Enough chunks to pass the byte budget several times over.
        for _ in 0..(MAX_PENDING_GRAPHICS_BYTES / command.len() + 8) {
            queue.push(continuation.as_bytes(), (0, 0), Some(CELL));
        }
        assert!(
            queue.is_empty(),
            "a partial transfer means nothing to the terminal, so none is kept"
        );

        // The queue recovers: the next image is not punished for the last one.
        queue.drain();
        queue.push(b"a=t,i=2,f=100;AAAA", (0, 0), Some(CELL));
        assert_eq!(queue.drain().len(), 1);
    }

    #[test]
    fn draining_leaves_the_queue_ready_to_collect_again() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=1,f=100;AAAA", (0, 0), Some(CELL));
        assert_eq!(queue.drain().len(), 1);
        assert!(queue.drain().is_empty(), "a command is delivered once");
    }

    #[test]
    fn concurrent_panes_deliver_whole_transfers_across_frame_drains() {
        let mut first = GraphicsQueue::default();
        let mut second = GraphicsQueue::default();
        first.push(b"a=t,i=1,m=1;AAAA", (0, 0), Some(CELL));
        second.push(b"a=t,i=2,m=1;BBBB", (0, 0), Some(CELL));
        assert!(first.drain().is_empty());
        assert!(second.drain().is_empty());
        assert!(
            first.retained().is_empty(),
            "late attach must not learn a partial image"
        );
        second.push(b"m=0;CCCC", (0, 0), Some(CELL));
        let mut delivered = second.drain();
        first.push(b"m=0;DDDD", (0, 0), Some(CELL));
        delivered.extend(first.drain());
        assert_eq!(
            delivered,
            vec![
                wrapped("a=t,i=2,m=1;BBBB"),
                wrapped("m=0;CCCC"),
                wrapped("a=t,i=1,m=1;AAAA"),
                wrapped("m=0;DDDD")
            ]
        );
        assert_eq!(first.retained().len(), 2);
    }

    #[test]
    fn resize_commands_wait_until_an_inline_transfer_finishes() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=1,m=1;AAAA", (0, 0), Some(CELL));
        queue.queue(b"a=p,U=1,i=1,c=2,r=2", Retain::Place(1));
        assert!(queue.drain().is_empty());
        queue.push(b"m=0;BBBB", (0, 0), Some(CELL));
        assert_eq!(
            queue.drain(),
            vec![
                wrapped("a=t,i=1,m=1;AAAA"),
                wrapped("m=0;BBBB"),
                wrapped("a=p,U=1,i=1,c=2,r=2")
            ]
        );
    }

    #[test]
    fn a_new_child_command_discards_its_abandoned_image_prefix() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=1,m=1;AAAA", (0, 0), Some(CELL));
        queue.push(b"a=t,i=2;BBBB", (0, 0), Some(CELL));
        assert_eq!(queue.drain(), vec![wrapped("a=t,i=2;BBBB")]);
        assert_eq!(queue.retained(), vec![wrapped("a=t,i=2;BBBB")]);
    }

    #[test]
    fn overflowing_partial_transfer_is_never_replayed_or_resumed() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=1,m=1;AAAA", (0, 0), Some(CELL));
        let chunk = format!("m=1;{}", "A".repeat(4096));
        for _ in 0..(MAX_PENDING_GRAPHICS_BYTES / 4096 + 1) {
            queue.push(chunk.as_bytes(), (0, 0), Some(CELL));
            assert!(queue.retained().is_empty());
        }
        assert!(queue.drain().is_empty());
        queue.push(b"m=0;BBBB", (0, 0), Some(CELL));
        assert!(queue.drain().is_empty());
        queue.push(b"a=t,i=2;CCCC", (0, 0), Some(CELL));
        assert_eq!(queue.drain(), vec![wrapped("a=t,i=2;CCCC")]);
    }

    /// Draining hands an image to the clients watching now. A client that
    /// attaches afterwards is sent the same grid, so it must be able to learn
    /// the same images — otherwise its cells name nothing.
    #[test]
    fn an_image_outlives_the_delivery_that_took_it_to_todays_clients() {
        let mut queue = GraphicsQueue::default();
        queue.push(
            b"a=T,i=7,f=32,s=38,v=84,U=1,c=2,r=2;DATA",
            (0, 0),
            Some(CELL),
        );

        assert_eq!(queue.drain().len(), 1, "delivered to the clients attached");
        assert!(queue.drain().is_empty(), "and delivered only once");
        assert_eq!(
            queue.retained(),
            vec![b"\x1b_Ga=T,i=7,f=32,s=38,v=84,U=1,c=2,r=2;DATA\x1b\\".to_vec()],
            "but still available to a client that has yet to attach"
        );
        assert_eq!(
            queue.retained().len(),
            1,
            "and available to every later client, not just the first"
        );
    }

    /// The child positioned this one itself, so what a later client is taught
    /// has to be the rewritten command — the original would put the image
    /// wherever Luvus's own cursor happens to be.
    #[test]
    fn a_rewritten_placement_is_what_a_later_client_learns() {
        let mut queue = GraphicsQueue::default();
        // Two cells wide and two tall at `CELL`.
        queue.push(b"a=T,i=7,f=32,s=28,v=68,C=1;DATA", (3, 5), Some(CELL));
        queue.drain();

        let retained = String::from_utf8(queue.retained().concat()).unwrap();
        assert!(retained.contains("U=1,p=1,c=2,r=2"), "{retained}");
        assert!(!retained.contains("C=1"), "{retained}");
        assert!(
            !retained.contains("a=d"),
            "a delete teaches nothing and is not kept for a later client: {retained}"
        );
    }

    /// A transfer split into chunks means nothing until every chunk arrives,
    /// so a later client has to be given all of them, in order.
    #[test]
    fn a_chunked_image_is_kept_whole() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=7,f=100,m=1;AAAA", (0, 0), Some(CELL));
        queue.push(b"m=1;BBBB", (0, 0), Some(CELL));
        queue.push(b"m=0;CCCC", (0, 0), Some(CELL));
        queue.drain();

        assert_eq!(
            queue.retained(),
            vec![
                b"\x1b_Ga=t,i=7,f=100,m=1;AAAA\x1b\\".to_vec(),
                b"\x1b_Gm=1;BBBB\x1b\\".to_vec(),
                b"\x1b_Gm=0;CCCC\x1b\\".to_vec(),
            ]
        );
    }

    /// An id names one image. A child that redraws replaces what that id means,
    /// and a client attaching later wants what the grid shows now, not a
    /// stale frame that happened to come first.
    #[test]
    fn redrawing_an_image_replaces_the_one_a_later_client_learns() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=7,f=100;OLD", (0, 0), Some(CELL));
        queue.push(b"a=t,i=7,f=100;NEW", (0, 0), Some(CELL));
        queue.drain();

        assert_eq!(
            queue.retained(),
            vec![b"\x1b_Ga=t,i=7,f=100;NEW\x1b\\".to_vec()],
            "one image per id, and it is the current one"
        );
    }

    /// A child that deletes an image is saying the pane no longer shows it.
    /// Handing it to the next client would be handing over something the child
    /// has already taken back.
    #[test]
    fn a_deleted_image_is_not_handed_to_a_later_client() {
        let mut queue = GraphicsQueue::default();
        queue.push(b"a=t,i=7,f=100;AAAA", (0, 0), Some(CELL));
        queue.push(b"a=t,i=8,f=100;BBBB", (0, 0), Some(CELL));

        queue.push(b"a=d,d=I,i=7", (0, 0), Some(CELL));
        let left = String::from_utf8(queue.retained().concat()).unwrap();
        assert!(!left.contains("AAAA"), "{left}");
        assert!(left.contains("BBBB"), "{left}");

        queue.push(b"a=d,d=A", (0, 0), Some(CELL));
        assert!(queue.retained().is_empty(), "delete-all clears the set");
    }

    /// A child that draws continuously must not grow a pane without limit. The
    /// working set is bounded, and the newest images are the ones the grid is
    /// most likely to still be naming.
    #[test]
    fn the_images_kept_for_later_clients_stay_within_budget() {
        let mut queue = GraphicsQueue::default();
        let body = "A".repeat(4096);
        for id in 1..=(MAX_RETAINED_IMAGES + 20) {
            queue.push(
                format!("a=t,i={id},f=100;{body}").as_bytes(),
                (0, 0),
                Some(CELL),
            );
            queue.drain();
        }

        let retained = queue.retained();
        assert!(retained.len() <= MAX_RETAINED_IMAGES, "{}", retained.len());
        assert!(queue.retained_bytes <= MAX_RETAINED_GRAPHICS_BYTES);
        let newest = String::from_utf8(retained.concat()).unwrap();
        assert!(
            newest.contains(&format!("i={}", MAX_RETAINED_IMAGES + 20)),
            "the image drawn last is the one to keep"
        );
    }

    #[test]
    fn pending_is_cleared_before_collection_so_a_racing_command_is_not_lost() {
        let graphics = HostGraphics::default();
        assert!(!graphics.pending());
        assert!(!graphics.take_pending(), "nothing queued yet");
        graphics.mark_pending();
        assert!(graphics.pending());
        assert!(
            graphics.pending(),
            "checking a projection must not consume the flag"
        );
        assert!(graphics.take_pending());
        assert!(!graphics.pending());
        assert!(!graphics.take_pending(), "the flag is consumed");
    }

    /// A command as a pane hands it over: the APC wrapper is what the backlog
    /// has to look through to find the image an entry belongs to.
    fn wrapped(payload: &str) -> Vec<u8> {
        let mut command = b"\x1b_G".to_vec();
        command.extend_from_slice(payload.as_bytes());
        command.extend_from_slice(b"\x1b\\");
        command
    }

    fn backlogged(payloads: &[&str]) -> Vec<String> {
        let mut backlog = GraphicsBacklog::default();
        let commands: Vec<Vec<u8>> = payloads.iter().copied().map(wrapped).collect();
        backlog.push(&commands);
        backlog
            .take()
            .into_iter()
            .map(|command| String::from_utf8(command).unwrap())
            .collect()
    }

    /// An id names one image. The command that carried the older version is
    /// worse than wasted work: a child that streams frames reuses the file it
    /// refers to, so sending it teaches the terminal the newest pixels under
    /// instructions written for an older frame.
    #[test]
    fn a_redrawn_image_replaces_the_command_still_waiting_for_its_id() {
        let waiting = backlogged(&[
            "a=T,U=1,i=7,c=2,r=2,f=100;OLD",
            "a=T,U=1,i=7,c=2,r=2,f=100;NEW",
        ]);
        assert_eq!(waiting.len(), 1, "one command per id: {waiting:?}");
        assert!(waiting[0].contains("NEW"), "{waiting:?}");

        // Another id is another image, and both are owed.
        let two = backlogged(&["a=T,U=1,i=7,c=1,r=1;AAAA", "a=T,U=1,i=8,c=1,r=1;BBBB"]);
        assert_eq!(two.len(), 2, "{two:?}");
    }

    /// A transfer means nothing in halves, so coalescing must never leave a
    /// terminal holding part of one.
    #[test]
    fn a_chunked_transfer_is_kept_whole_and_replaced_whole() {
        let chunks = backlogged(&["a=T,U=1,i=7,c=1,r=1,m=1;AAAA", "m=1;BBBB", "m=0;CCCC"]);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert!(chunks[2].contains("CCCC"), "{chunks:?}");

        let redrawn = backlogged(&[
            "a=T,U=1,i=7,c=1,r=1,m=1;AAAA",
            "m=1;BBBB",
            "m=0;CCCC",
            "a=T,U=1,i=7,c=1,r=1;NEW",
        ]);
        assert_eq!(
            redrawn.len(),
            1,
            "every chunk of the replaced transfer goes with it: {redrawn:?}"
        );
        assert!(redrawn[0].contains("NEW"), "{redrawn:?}");
    }

    /// The remaining chunks of a transfer already delivered still belong to it.
    /// They are collected on their own rather than dropped for having lost the
    /// chunk they opened with — the terminal is reading one transfer, and the
    /// chunks reach it in order either way.
    #[test]
    fn chunks_that_follow_a_delivery_are_still_collected() {
        let mut backlog = GraphicsBacklog::default();
        backlog.push(&[wrapped("a=T,U=1,i=7,c=1,r=1,m=1;AAAA")]);
        assert_eq!(backlog.take().len(), 1);

        backlog.push(&[wrapped("m=0;BBBB")]);
        let rest = backlog.take();
        assert_eq!(rest.len(), 1, "the tail of the transfer is not dropped");
        assert_eq!(rest[0], wrapped("m=0;BBBB"));

        // A chunk with no transfer at all has no beginning to belong to.
        let mut orphan = GraphicsBacklog::default();
        orphan.push(&[wrapped("m=0;BBBB")]);
        assert!(orphan.is_empty(), "a middle is not an image");
    }

    /// A placement is only worth sending alongside the image it places, so it
    /// travels with that image when one is still waiting.
    #[test]
    fn a_placement_joins_the_image_it_places() {
        let together = backlogged(&["a=T,U=1,i=7,c=1,r=1;AAAA", "a=p,U=1,i=7,c=1,r=1"]);
        assert_eq!(together.len(), 2, "{together:?}");
        assert!(together[0].contains("AAAA") && together[1].contains("a=p"));

        // Replacing the image replaces the placement that came with it.
        let redrawn = backlogged(&[
            "a=T,U=1,i=7,c=1,r=1;AAAA",
            "a=p,U=1,i=7,c=1,r=1",
            "a=T,U=1,i=7,c=1,r=1;NEW",
        ]);
        assert_eq!(redrawn.len(), 1, "{redrawn:?}");

        // A placement for an image the client already has stands on its own.
        let alone = backlogged(&["a=p,U=1,i=7,c=1,r=1"]);
        assert_eq!(alone.len(), 1, "{alone:?}");
    }

    /// A delete takes back an image the client may already have learned, so it
    /// has to reach the terminal even though what it deletes never will.
    #[test]
    fn a_delete_drops_what_it_deletes_and_is_itself_delivered() {
        let one = backlogged(&[
            "a=T,U=1,i=7,c=1,r=1;AAAA",
            "a=T,U=1,i=8,c=1,r=1;BBBB",
            "a=d,d=I,i=7",
        ]);
        assert_eq!(one.len(), 2, "{one:?}");
        assert!(
            !one.iter().any(|command| command.contains("AAAA")),
            "{one:?}"
        );
        assert!(one[0].contains("BBBB") && one[1].contains("a=d"), "{one:?}");

        let all = backlogged(&[
            "a=T,U=1,i=7,c=1,r=1;AAAA",
            "a=T,U=1,i=8,c=1,r=1;BBBB",
            "a=d,d=A",
        ]);
        assert_eq!(all, vec![String::from_utf8(wrapped("a=d,d=A")).unwrap()]);
    }

    /// The entries depend on each other, so a client given some of them would
    /// draw an image it was taught in part. Overflow asks to be taught the
    /// panes' whole working set again instead.
    #[test]
    fn an_overflowing_backlog_is_abandoned_and_asks_to_be_taught_again() {
        let mut backlog = GraphicsBacklog::default();
        for id in 1..=(MAX_BACKLOG_ENTRIES + 1) {
            backlog.push(&[wrapped(&format!("a=T,U=1,i={id},c=1,r=1;AAAA"))]);
        }
        assert!(backlog.is_empty(), "nothing partial is kept");
        assert!(backlog.take().is_empty());

        // The byte budget stops a few large images just as the entry count
        // stops many small ones.
        let body = "A".repeat(64 * 1024);
        let mut heavy = GraphicsBacklog::default();
        let mut images = 0;
        while images == 0 || !heavy.is_empty() {
            images += 1;
            assert!(images <= MAX_BACKLOG_ENTRIES, "the byte budget stops first");
            heavy.push(&[wrapped(&format!("a=T,U=1,i={images},c=1,r=1;{body}"))]);
        }
        assert!(images * body.len() >= MAX_BACKLOG_BYTES);
        assert!(heavy.take_resync());

        assert!(backlog.take_resync(), "the client has to be re-taught");
        assert!(!backlog.take_resync(), "and asked for it once");

        // The backlog recovers: the next image is not punished for the last.
        backlog.push(&[wrapped("a=T,U=1,i=1,c=1,r=1;AAAA")]);
        assert_eq!(backlog.take().len(), 1);
    }
}
