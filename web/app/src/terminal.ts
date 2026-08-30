export interface TerminalLocator {
  server_generation: string;
  terminal_id: string;
  pane_id: string;
  title: string;
  location: string;
}

interface InventoryTerminal {
  terminal_id?: unknown;
  pane_id?: unknown;
  workspace?: { name?: unknown; index?: unknown };
  tab?: { name?: unknown; index?: unknown };
  terminal_title?: unknown;
  label?: unknown;
}

export interface TerminalFrame {
  text: string;
  contentRevision: number;
  truncated: boolean;
}

export type TerminalAction = "type_literal" | "submit_text" | "send_key";

export function terminalLocators(value: unknown): Map<string, TerminalLocator> {
  const inventory = value as { server_generation?: unknown; terminals?: unknown };
  if (
    typeof inventory?.server_generation !== "string" ||
    !/^[a-f0-9]{32}$/.test(inventory.server_generation) ||
    !Array.isArray(inventory.terminals)
  ) {
    throw new Error("Luvus returned an invalid terminal inventory");
  }
  const terminals = new Map<string, TerminalLocator>();
  for (const candidate of inventory.terminals) {
    const entry = candidate as InventoryTerminal;
    if (
      typeof entry?.terminal_id !== "string" ||
      !/^[a-f0-9]{32}$/.test(entry.terminal_id) ||
      typeof entry.pane_id !== "string" ||
      !/^[1-9][0-9]{0,9}$/.test(entry.pane_id)
    ) {
      continue;
    }
    const workspace = text(entry.workspace?.name) || `workspace ${number(entry.workspace?.index)}`;
    const tab = text(entry.tab?.name) || `tab ${number(entry.tab?.index)}`;
    terminals.set(entry.pane_id, {
      server_generation: inventory.server_generation,
      terminal_id: entry.terminal_id,
      pane_id: entry.pane_id,
      title: text(entry.label) || text(entry.terminal_title) || `pane ${entry.pane_id}`,
      location: `${workspace} · ${tab} · pane ${entry.pane_id}`,
    });
  }
  return terminals;
}

export function terminalFrame(value: unknown): TerminalFrame | null {
  const envelope = value as { event?: unknown; data?: Record<string, unknown> };
  if (envelope?.event !== "terminal.frame" || !envelope.data) return null;
  const revision = envelope.data.content_revision;
  if (
    typeof envelope.data.text !== "string" ||
    !Number.isSafeInteger(revision) ||
    Number(revision) < 0 ||
    typeof envelope.data.truncated !== "boolean"
  ) {
    throw new Error("Luvus returned an invalid terminal frame");
  }
  return {
    text: envelope.data.text,
    contentRevision: Number(revision),
    truncated: envelope.data.truncated,
  };
}

function text(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function number(value: unknown): string {
  return Number.isSafeInteger(value) ? String(value) : "?";
}
