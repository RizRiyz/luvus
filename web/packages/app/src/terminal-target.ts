import type { PaneSnapshot, SessionSnapshot } from "@luvus/uhp-client";

export interface TerminalTarget {
  serverGeneration: string;
  pane: PaneSnapshot;
}

type LiveTerminalPane = PaneSnapshot & {
  kind: "terminal";
  terminal_id: string;
};

export class TerminalTargetTracker {
  readonly session: string;
  readonly serverGeneration: string;
  #paneId: string;
  #terminalId: string;

  constructor(snapshot: SessionSnapshot, pane: PaneSnapshot) {
    if (!pane.terminal_id || !findPane(snapshot, pane.pane_id, pane.terminal_id)) {
      throw new Error("Pane has no live terminal route");
    }
    this.session = snapshot.session;
    this.serverGeneration = snapshot.server_generation;
    this.#paneId = pane.pane_id;
    this.#terminalId = pane.terminal_id;
  }

  resolve(snapshot: SessionSnapshot): TerminalTarget | undefined {
    if (snapshot.session !== this.session || snapshot.server_generation !== this.serverGeneration) {
      return undefined;
    }
    const pane = findPane(snapshot, this.#paneId, this.#terminalId);
    if (!pane) return undefined;
    this.#paneId = pane.pane_id;
    this.#terminalId = pane.terminal_id;
    return { serverGeneration: snapshot.server_generation, pane };
  }
}

function findPane(snapshot: SessionSnapshot, paneId: string, terminalId: string | null | undefined): LiveTerminalPane | undefined {
  if (!terminalId) return undefined;
  for (const workspace of snapshot.workspaces) {
    for (const tab of workspace.tabs) {
      const pane = tab.panes.find((candidate): candidate is LiveTerminalPane => (
        candidate.kind === "terminal"
        && candidate.pane_id === paneId
        && candidate.terminal_id === terminalId
      ));
      if (pane) return pane;
    }
  }
  return undefined;
}
