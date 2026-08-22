//! Native Git DIFF review (docs/88).
//!
//! The model, Git parser, row projection, note store, and agent handoff live in
//! this directory so the FILES dock, renderer, and public API remain thin
//! integration layers.

pub mod git;
pub mod message;
pub mod model;
pub mod notes;
pub mod rows;

pub use model::{
    DiffAgentChoice, DiffAgentPicker, DiffColorMode, DiffFile, DiffFileStatus, DiffKey, DiffLayer,
    DiffLayoutPreference, DiffLine, DiffLineKind, DiffListRow, DiffLoad, DiffMarkerStyle,
    DiffSendScope, DiffSide, DiffSnapshot, DiffState, DiffView, FilesMode, LoadedDiff, RepoPath,
};
pub use notes::{NoteKind, NoteState, ReviewNote};

pub const PATCH_BYTE_CAP: usize = 4 * 1024 * 1024;
pub const PATCH_LINE_CAP: usize = 20_000;
pub const PATCH_LINE_BYTE_CAP: usize = 16 * 1024;
pub const DIFF_FILE_CAP: usize = 5_000;
pub const MAX_CONTEXT_LINES: u16 = 20;
/// Plain terminal glyph used to identify native DIFF views in pane and tab chrome.
pub const DIFF_GLYPH: &str = "▲";
pub const NOTE_CAP: usize = 1_000;
pub const NOTE_BODY_CAP: usize = 8 * 1024;
pub const HANDOFF_BYTE_CAP: usize = 64 * 1024;
pub const DIFF_CACHE_BYTE_CAP: usize = 16 * 1024 * 1024;
