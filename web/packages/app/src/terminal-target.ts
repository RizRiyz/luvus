import type { PaneSnapshot, SessionSnapshot } from "@luvus/uhp-client";

export interface TerminalTarget {
  serverGeneration: string;
  pane: PaneSnapshot;
}

type PaneRoute = {
  workspace: number;
  tab: number;
  pane: number;
};

export class TerminalTargetTracker {
  readonly session: string;
  #generation: string;
  #paneId: string;
  #terminalId: string;
  #route: PaneRoute;

  constructor(snapshot: SessionSnapshot, pane: PaneSnapshot) {
    const route = findPane(snapshot, pane.pane_id, pane.terminal_id);
    if (!pane.terminal_id || !route) throw new Error("Pane has no live terminal route");
    this.session = snapshot.session;
    this.#generation = snapshot.server_generation;
    this.#paneId = pane.pane_id;
    this.#terminalId = pane.terminal_id;
    this.#route = route;
  }

  resolve(snapshot: SessionSnapshot): TerminalTarget | undefined {
    if (snapshot.session !== this.session) return undefined;
    const route = snapshot.server_generation === this.#generation
      ? findPane(snapshot, this.#paneId, this.#terminalId)
      : this.#route;
    if (!route) return undefined;
    const pane = paneAt(snapshot, route);
    if (!pane?.terminal_id || pane.kind !== "terminal") return undefined;
    this.#generation = snapshot.server_generation;
    this.#paneId = pane.pane_id;
    this.#terminalId = pane.terminal_id;
    this.#route = route;
    return { serverGeneration: snapshot.server_generation, pane };
  }
}

function findPane(snapshot: SessionSnapshot, paneId: string, terminalId: string | null | undefined): PaneRoute | undefined {
  if (!terminalId) return undefined;
  for (let workspace = 0; workspace < snapshot.workspaces.length; workspace += 1) {
    const tabs = snapshot.workspaces[workspace]!.tabs;
    for (let tab = 0; tab < tabs.length; tab += 1) {
      const panes = tabs[tab]!.panes;
      const pane = panes.findIndex((candidate) => candidate.pane_id === paneId && candidate.terminal_id === terminalId);
      if (pane >= 0) return { workspace, tab, pane };
    }
  }
  return undefined;
}

function paneAt(snapshot: SessionSnapshot, route: PaneRoute): PaneSnapshot | undefined {
  return snapshot.workspaces[route.workspace]?.tabs[route.tab]?.panes[route.pane];
}
