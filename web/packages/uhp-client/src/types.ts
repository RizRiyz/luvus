export type JsonObject = Record<string, unknown>;

export interface PaneSnapshot {
  pane_id: string;
  kind: "terminal" | "view";
  focused: boolean;
  cwd?: string;
  terminal_id?: string | null;
  content_revision?: number;
  agent_name?: string | null;
  agent?: string | null;
  agent_status?: "idle" | "working" | "blocked" | "done" | null;
}

export interface TabSnapshot {
  index: number;
  name: string;
  kind: string;
  active: boolean;
  panes: PaneSnapshot[];
}

export interface WorkspaceSnapshot {
  index: number;
  name: string;
  cwd: string;
  branch?: string | null;
  active: boolean;
  tabs: TabSnapshot[];
}

export interface SessionSnapshot {
  type: "session_snapshot";
  session: string;
  server_generation: string;
  event_sequence: number;
  workspaces: WorkspaceSnapshot[];
}

export interface Capabilities {
  type: "uhp_capabilities";
  server_generation: string;
  session: string;
  event_sequence: number;
  methods: string[];
  access?: {
    mode: "read_only" | "control";
    allowed_methods: string[];
  };
  terminal?: {
    capabilities?: string[];
    features?: string[];
  };
}

export interface UhpEvent {
  event: string;
  sequence: number;
  data: JsonObject;
}

export interface TerminalFrame extends UhpEvent {
  event: "terminal.frame";
  data: JsonObject & {
    server_generation: string;
    terminal_id: string;
    pane_id: string;
    content_revision: number;
    ansi: boolean;
    text: string;
    truncated: boolean;
    cursor?: { offset: number; padding_cells: number } | null;
  };
}

export type ConnectionState =
  | "disconnected"
  | "connecting"
  | "authenticating"
  | "synchronizing"
  | "ready"
  | "reconnecting"
  | "expired";
