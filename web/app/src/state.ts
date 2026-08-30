export interface SessionSnapshot {
  type: "session_snapshot";
  session: string;
  server_generation: string;
  event_sequence: number;
  workspaces: Workspace[];
}

export interface Workspace {
  index: number;
  name: string;
  cwd: string;
  branch?: string | null;
  active: boolean;
  tabs: Tab[];
}

export interface Tab {
  index: number;
  name?: string | null;
  kind: string;
  active: boolean;
  panes: Pane[];
}

export interface Pane {
  pane_id: string;
  kind: string;
  focused: boolean;
  name?: string | null;
}

export function asSnapshot(value: unknown): SessionSnapshot {
  const snapshot = value as Partial<SessionSnapshot>;
  if (
    snapshot.type !== "session_snapshot" ||
    typeof snapshot.session !== "string" ||
    typeof snapshot.server_generation !== "string" ||
    !Number.isSafeInteger(snapshot.event_sequence) ||
    !Array.isArray(snapshot.workspaces)
  ) {
    throw new Error("Luvus returned an invalid session snapshot");
  }
  return snapshot as SessionSnapshot;
}

export function eventSequence(value: unknown): number | null {
  const sequence = (value as { sequence?: unknown })?.sequence;
  return Number.isSafeInteger(sequence) && Number(sequence) > 0 ? Number(sequence) : null;
}
