use std::fmt;

pub const MAX_FIELDS: usize = 8;
pub const MAX_SAFE_ID_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggerKind {
    Server,
    Client,
}

impl LoggerKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Server,
    Client,
    Local,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Client => "client",
            Self::Local => "local",
        }
    }

    pub(crate) const fn permits(self, logger: LoggerKind) -> bool {
        matches!(self, Self::Local)
            || matches!(
                (self, logger),
                (Self::Server, LoggerKind::Server) | (Self::Client, LoggerKind::Client)
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Listener {
    Uhp,
    Client,
}

impl Listener {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Uhp => "uhp",
            Self::Client => "client",
        }
    }
}

// These closed enums are the on-disk schema. Some variants are emitted only by
// platform-specific or uncommon failure paths, so a normal host need not
// construct every variant.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Ok,
    Error,
    Rejected,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Rejected => "rejected",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnKind {
    Shell,
    Command,
    Resume,
    Module,
    Deferred,
}

impl SpawnKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Command => "command",
            Self::Resume => "resume",
            Self::Module => "module",
            Self::Deferred => "deferred",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitClass {
    Exited,
    Signaled,
    Unknown,
}

impl ExitClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signaled => "signaled",
            Self::Unknown => "unknown",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Done,
}

impl AgentState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authority {
    Hook,
    Process,
    Text,
    None,
}

impl Authority {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::Process => "process",
            Self::Text => "text",
            Self::None => "none",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Worker {
    Git,
    Files,
    Sessions,
    Search,
    Diff,
    Update,
    Module,
    Pty,
}

impl Worker {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Files => "files",
            Self::Sessions => "sessions",
            Self::Search => "search",
            Self::Diff => "diff",
            Self::Update => "update",
            Self::Module => "module",
            Self::Pty => "pty",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    VersionMismatch,
    Handshake,
    Eof,
    Empty,
    Io,
    Parse,
    Protocol,
    Overflow,
    Lock,
}

impl Reason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::VersionMismatch => "version_mismatch",
            Self::Handshake => "handshake",
            Self::Eof => "eof",
            Self::Empty => "empty",
            Self::Io => "io",
            Self::Parse => "parse",
            Self::Protocol => "protocol",
            Self::Overflow => "overflow",
            Self::Lock => "lock",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SafeId {
    bytes: [u8; MAX_SAFE_ID_BYTES],
    len: u8,
}

impl SafeId {
    pub fn new(value: &str) -> Option<Self> {
        if value.is_empty()
            || value.len() > MAX_SAFE_ID_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
            })
        {
            return None;
        }
        let mut bytes = [0; MAX_SAFE_ID_BYTES];
        bytes[..value.len()].copy_from_slice(value.as_bytes());
        Some(Self {
            bytes,
            len: value.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)])
            .expect("SafeId stores validated ASCII")
    }
}

impl fmt::Debug for SafeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SafeId")
            .field(&self.as_str())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FieldKey {
    Role,
    Listener,
    Outcome,
    ErrorCode,
    Method,
    RequestId,
    IdOmitted,
    PaneId,
    WorkspaceIndex,
    TabIndex,
    ClientId,
    Cols,
    Rows,
    ProtocolVersion,
    SpawnKind,
    ExitClass,
    Agent,
    AgentState,
    FromState,
    Authority,
    ModuleId,
    Worker,
    RestoreWorkspaces,
    RestoreTabs,
    RestorePanes,
    RestoreSkipped,
    DurationMs,
    Dropped,
    Reason,
}

impl FieldKey {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Role => "role",
            Self::Listener => "listener",
            Self::Outcome => "outcome",
            Self::ErrorCode => "error_code",
            Self::Method => "method",
            Self::RequestId => "request_id",
            Self::IdOmitted => "id_omitted",
            Self::PaneId => "pane_id",
            Self::WorkspaceIndex => "workspace_index",
            Self::TabIndex => "tab_index",
            Self::ClientId => "client_id",
            Self::Cols => "cols",
            Self::Rows => "rows",
            Self::ProtocolVersion => "protocol_version",
            Self::SpawnKind => "spawn_kind",
            Self::ExitClass => "exit_class",
            Self::Agent => "agent",
            Self::AgentState => "agent_state",
            Self::FromState => "from_state",
            Self::Authority => "authority",
            Self::ModuleId => "module_id",
            Self::Worker => "worker",
            Self::RestoreWorkspaces => "restore_workspaces",
            Self::RestoreTabs => "restore_tabs",
            Self::RestorePanes => "restore_panes",
            Self::RestoreSkipped => "restore_skipped",
            Self::DurationMs => "duration_ms",
            Self::Dropped => "dropped",
            Self::Reason => "reason",
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Role(Role),
    Listener(Listener),
    Outcome(Outcome),
    ErrorCode(SafeId),
    Method(SafeId),
    RequestId(SafeId),
    IdOmitted(bool),
    PaneId(u64),
    WorkspaceIndex(u64),
    TabIndex(u64),
    ClientId(u64),
    Cols(u64),
    Rows(u64),
    ProtocolVersion(u64),
    SpawnKind(SpawnKind),
    ExitClass(ExitClass),
    Agent(SafeId),
    AgentState(AgentState),
    FromState(AgentState),
    Authority(Authority),
    ModuleId(SafeId),
    Worker(Worker),
    RestoreWorkspaces(u64),
    RestoreTabs(u64),
    RestorePanes(u64),
    RestoreSkipped(u64),
    DurationMs(u64),
    Dropped(u64),
    Reason(Reason),
}

impl Field {
    pub(crate) const fn key(self) -> FieldKey {
        match self {
            Self::Role(_) => FieldKey::Role,
            Self::Listener(_) => FieldKey::Listener,
            Self::Outcome(_) => FieldKey::Outcome,
            Self::ErrorCode(_) => FieldKey::ErrorCode,
            Self::Method(_) => FieldKey::Method,
            Self::RequestId(_) => FieldKey::RequestId,
            Self::IdOmitted(_) => FieldKey::IdOmitted,
            Self::PaneId(_) => FieldKey::PaneId,
            Self::WorkspaceIndex(_) => FieldKey::WorkspaceIndex,
            Self::TabIndex(_) => FieldKey::TabIndex,
            Self::ClientId(_) => FieldKey::ClientId,
            Self::Cols(_) => FieldKey::Cols,
            Self::Rows(_) => FieldKey::Rows,
            Self::ProtocolVersion(_) => FieldKey::ProtocolVersion,
            Self::SpawnKind(_) => FieldKey::SpawnKind,
            Self::ExitClass(_) => FieldKey::ExitClass,
            Self::Agent(_) => FieldKey::Agent,
            Self::AgentState(_) => FieldKey::AgentState,
            Self::FromState(_) => FieldKey::FromState,
            Self::Authority(_) => FieldKey::Authority,
            Self::ModuleId(_) => FieldKey::ModuleId,
            Self::Worker(_) => FieldKey::Worker,
            Self::RestoreWorkspaces(_) => FieldKey::RestoreWorkspaces,
            Self::RestoreTabs(_) => FieldKey::RestoreTabs,
            Self::RestorePanes(_) => FieldKey::RestorePanes,
            Self::RestoreSkipped(_) => FieldKey::RestoreSkipped,
            Self::DurationMs(_) => FieldKey::DurationMs,
            Self::Dropped(_) => FieldKey::Dropped,
            Self::Reason(_) => FieldKey::Reason,
        }
    }

    pub(crate) fn json_value(self) -> serde_json::Value {
        match self {
            Self::Role(value) => value.as_str().into(),
            Self::Listener(value) => value.as_str().into(),
            Self::Outcome(value) => value.as_str().into(),
            Self::ErrorCode(value)
            | Self::Method(value)
            | Self::RequestId(value)
            | Self::Agent(value)
            | Self::ModuleId(value) => value.as_str().into(),
            Self::IdOmitted(value) => value.into(),
            Self::PaneId(value)
            | Self::WorkspaceIndex(value)
            | Self::TabIndex(value)
            | Self::ClientId(value)
            | Self::Cols(value)
            | Self::Rows(value)
            | Self::ProtocolVersion(value)
            | Self::RestoreWorkspaces(value)
            | Self::RestoreTabs(value)
            | Self::RestorePanes(value)
            | Self::RestoreSkipped(value)
            | Self::DurationMs(value)
            | Self::Dropped(value) => value.into(),
            Self::SpawnKind(value) => value.as_str().into(),
            Self::ExitClass(value) => value.as_str().into(),
            Self::AgentState(value) | Self::FromState(value) => value.as_str().into(),
            Self::Authority(value) => value.as_str().into(),
            Self::Worker(value) => value.as_str().into(),
            Self::Reason(value) => value.as_str().into(),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventKind {
    ServerStart,
    ServerStop,
    ServerReady,
    ListenerBind,
    ListenerBindFailed,
    PersistRestore,
    PersistSave,
    PersistSaveFailed,
    PersistCleared,
    ServerClientAttach,
    ServerClientHandshakeRejected,
    ServerClientResize,
    ServerClientDetach,
    WorkspaceOpen,
    WorkspaceClose,
    TabOpen,
    TabClose,
    PaneOpen,
    PaneClose,
    PtySpawnFailed,
    PtyInputRejected,
    PtyExit,
    PtyResize,
    AgentIdentity,
    AgentState,
    AgentAuthority,
    ManifestReload,
    UpdateCheck,
    WorkerFailed,
    ClientStart,
    ClientStop,
    ClientConnect,
    ClientConnectFailed,
    ClientHandshake,
    ClientHandshakeRejected,
    ClientDisconnect,
    ClientResize,
    ClientFrameError,
    ClientRenderFailed,
    LogWriteRecovered,
    UhpConnectionOpen,
    UhpConnectionClose,
    UhpRequestStart,
    UhpRequestComplete,
    UhpRequestFailed,
    UhpRequestRejected,
    UhpSubscriptionOpen,
    UhpSubscriptionClose,
}

impl EventKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::ServerStart => "server.start",
            Self::ServerStop => "server.stop",
            Self::ServerReady => "server.ready",
            Self::ListenerBind => "listener.bind",
            Self::ListenerBindFailed => "listener.bind_failed",
            Self::PersistRestore => "persist.restore",
            Self::PersistSave => "persist.save",
            Self::PersistSaveFailed => "persist.save_failed",
            Self::PersistCleared => "persist.cleared",
            Self::ServerClientAttach => "client.attach",
            Self::ServerClientHandshakeRejected => "client.handshake_rejected",
            Self::ServerClientResize => "client.resize",
            Self::ServerClientDetach => "client.detach",
            Self::WorkspaceOpen => "workspace.open",
            Self::WorkspaceClose => "workspace.close",
            Self::TabOpen => "tab.open",
            Self::TabClose => "tab.close",
            Self::PaneOpen => "pane.open",
            Self::PaneClose => "pane.close",
            Self::PtySpawnFailed => "pty.spawn_failed",
            Self::PtyInputRejected => "pty.input_rejected",
            Self::PtyExit => "pty.exit",
            Self::PtyResize => "pty.resize",
            Self::AgentIdentity => "agent.identity",
            Self::AgentState => "agent.state",
            Self::AgentAuthority => "agent.authority",
            Self::ManifestReload => "manifest.reload",
            Self::UpdateCheck => "update.check",
            Self::WorkerFailed => "worker.failed",
            Self::ClientStart => "client.start",
            Self::ClientStop => "client.stop",
            Self::ClientConnect => "client.connect",
            Self::ClientConnectFailed => "client.connect_failed",
            Self::ClientHandshake => "client.handshake",
            Self::ClientHandshakeRejected => "client.handshake_rejected",
            Self::ClientDisconnect => "client.disconnect",
            Self::ClientResize => "client.resize",
            Self::ClientFrameError => "client.frame_error",
            Self::ClientRenderFailed => "client.render_failed",
            Self::LogWriteRecovered => "log.write_recovered",
            Self::UhpConnectionOpen => "uhp.connection.open",
            Self::UhpConnectionClose => "uhp.connection.close",
            Self::UhpRequestStart => "uhp.request.start",
            Self::UhpRequestComplete => "uhp.request.complete",
            Self::UhpRequestFailed => "uhp.request.failed",
            Self::UhpRequestRejected => "uhp.request.rejected",
            Self::UhpSubscriptionOpen => "uhp.subscription.open",
            Self::UhpSubscriptionClose => "uhp.subscription.close",
        }
    }

    pub const fn level(self) -> Level {
        match self {
            Self::ListenerBindFailed
            | Self::PtySpawnFailed
            | Self::ClientConnectFailed
            | Self::ClientHandshakeRejected
            | Self::ClientRenderFailed => Level::Error,
            Self::PersistSaveFailed
            | Self::PtyInputRejected
            | Self::WorkerFailed
            | Self::ClientDisconnect
            | Self::ClientFrameError
            | Self::LogWriteRecovered
            | Self::UhpRequestFailed
            | Self::UhpRequestRejected => Level::Warn,
            Self::PersistSave
            | Self::ServerClientResize
            | Self::TabOpen
            | Self::TabClose
            | Self::PtyResize
            | Self::ClientResize
            | Self::UhpConnectionOpen
            | Self::UhpConnectionClose
            | Self::UhpRequestStart
            | Self::UhpRequestComplete => Level::Debug,
            _ => Level::Info,
        }
    }

    pub const fn logger(self) -> Option<LoggerKind> {
        match self {
            Self::ClientStart
            | Self::ClientStop
            | Self::ClientConnect
            | Self::ClientConnectFailed
            | Self::ClientHandshake
            | Self::ClientHandshakeRejected
            | Self::ClientDisconnect
            | Self::ClientResize
            | Self::ClientFrameError
            | Self::ClientRenderFailed => Some(LoggerKind::Client),
            Self::LogWriteRecovered => None,
            _ => Some(LoggerKind::Server),
        }
    }

    pub(crate) const fn allows(self, key: FieldKey) -> bool {
        use EventKind as E;
        use FieldKey as F;
        match self {
            E::ServerStart | E::ClientStart => matches!(key, F::Role),
            E::ServerReady => matches!(
                key,
                F::RestoreWorkspaces | F::RestoreTabs | F::RestorePanes | F::RestoreSkipped
            ),
            E::ListenerBind => matches!(key, F::Listener),
            E::ListenerBindFailed => matches!(key, F::Listener | F::ErrorCode),
            E::PersistRestore => matches!(
                key,
                F::Outcome
                    | F::RestoreWorkspaces
                    | F::RestoreTabs
                    | F::RestorePanes
                    | F::RestoreSkipped
            ),
            E::PersistSave => matches!(key, F::Outcome),
            E::PersistSaveFailed => matches!(key, F::ErrorCode),
            E::PersistCleared => matches!(key, F::Reason),
            E::ServerClientAttach => {
                matches!(key, F::ClientId | F::Cols | F::Rows | F::ProtocolVersion)
            }
            E::ServerClientHandshakeRejected => {
                matches!(key, F::Reason | F::ProtocolVersion)
            }
            E::ServerClientResize => matches!(key, F::ClientId | F::Cols | F::Rows),
            E::ServerClientDetach => matches!(key, F::ClientId | F::Reason),
            E::WorkspaceOpen | E::WorkspaceClose => matches!(key, F::WorkspaceIndex),
            E::TabOpen | E::TabClose => matches!(key, F::WorkspaceIndex | F::TabIndex),
            E::PaneOpen => matches!(key, F::PaneId | F::SpawnKind),
            E::PaneClose => matches!(key, F::PaneId),
            E::PtyInputRejected => matches!(key, F::PaneId),
            E::PtySpawnFailed => matches!(key, F::PaneId | F::SpawnKind | F::ErrorCode),
            E::PtyExit => matches!(key, F::PaneId | F::ExitClass),
            E::PtyResize => matches!(key, F::PaneId | F::Cols | F::Rows),
            E::AgentIdentity => matches!(key, F::PaneId | F::Agent | F::Authority | F::IdOmitted),
            E::AgentState => matches!(
                key,
                F::PaneId | F::Agent | F::FromState | F::AgentState | F::IdOmitted
            ),
            E::AgentAuthority => matches!(
                key,
                F::PaneId | F::Agent | F::Authority | F::Outcome | F::IdOmitted
            ),
            E::ManifestReload | E::UpdateCheck => matches!(key, F::Outcome),
            E::WorkerFailed => matches!(key, F::Worker | F::ErrorCode),
            E::ClientConnectFailed | E::ClientRenderFailed => matches!(key, F::ErrorCode),
            E::ClientHandshake => matches!(key, F::ProtocolVersion | F::Cols | F::Rows),
            E::ClientHandshakeRejected => matches!(key, F::Reason | F::ProtocolVersion),
            E::ClientDisconnect => matches!(key, F::Reason),
            E::ClientResize => matches!(key, F::Cols | F::Rows),
            E::ClientFrameError => matches!(key, F::ErrorCode),
            E::LogWriteRecovered => matches!(key, F::ErrorCode | F::Dropped),
            E::UhpRequestStart => matches!(key, F::RequestId | F::Method | F::IdOmitted),
            E::UhpRequestComplete | E::UhpRequestFailed => matches!(
                key,
                F::RequestId | F::Method | F::Outcome | F::ErrorCode | F::DurationMs | F::IdOmitted
            ),
            E::UhpRequestRejected => matches!(
                key,
                F::RequestId | F::Method | F::ErrorCode | F::DurationMs | F::IdOmitted
            ),
            E::UhpSubscriptionOpen => matches!(key, F::RequestId | F::Method | F::IdOmitted),
            E::UhpSubscriptionClose => {
                matches!(key, F::RequestId | F::Method | F::Reason | F::IdOmitted)
            }
            E::ServerStop
            | E::ClientStop
            | E::ClientConnect
            | E::UhpConnectionOpen
            | E::UhpConnectionClose => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_id_accepts_only_the_public_identifier_alphabet() {
        assert_eq!(
            SafeId::new("pane.read:42").unwrap().as_str(),
            "pane.read:42"
        );
        assert!(SafeId::new("").is_none());
        assert!(SafeId::new("contains space").is_none());
        assert!(SafeId::new(&"a".repeat(65)).is_none());
    }

    #[test]
    fn event_field_allowlist_rejects_unrelated_metadata() {
        assert!(EventKind::PaneOpen.allows(FieldKey::PaneId));
        assert!(!EventKind::PaneOpen.allows(FieldKey::RequestId));
        assert!(EventKind::UhpRequestComplete.allows(FieldKey::DurationMs));
        assert!(!EventKind::UhpRequestComplete.allows(FieldKey::PaneId));
        assert_eq!(EventKind::UhpRequestComplete.level(), Level::Debug);
        assert_eq!(EventKind::UhpRequestFailed.level(), Level::Warn);
        assert!(EventKind::UhpRequestFailed.allows(FieldKey::ErrorCode));
    }
}
