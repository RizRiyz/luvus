//! The luvus **module** system: an extension is a directory with a
//! `luvus-module.toml` manifest declaring argv commands; luvus runs them as
//! subprocesses with `LUVUS_*` context env and they call back through the same
//! UHP the CLI uses. No SDK, no scripting engine. See docs/13.
//!
//! MOD-1 (this layer): local `link`/`list`/`enable`/`disable`/`unlink` + action
//! invocation with captured, logged output. Panes (MOD-2), event hooks (MOD-3)
//! and git install (MOD-4) build on these pieces.

pub mod context;
pub mod discovery;
pub mod install;
pub mod manifest;
pub mod paths;
pub mod registry;
pub mod runtime;
pub mod settings;

pub use registry::{InstalledModule, ModuleRegistry};
pub use runtime::ModuleCommandLog;

/// Tracks which module + entrypoint a live pane belongs to (MOD-2), so the pane
/// can be auto-untracked on close.
#[derive(Clone)]
pub struct ModulePaneRecord {
    pub module_id: String,
    pub entrypoint: String,
}
