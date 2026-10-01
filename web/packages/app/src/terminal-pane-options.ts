import type { PaneSnapshot, SessionSnapshot, WorkspaceSnapshot } from "@luvus/uhp-client";
import { agentCardTitle, displayText } from "./dashboard-agents.js";

export interface TerminalPaneOption {
  pane: PaneSnapshot;
  title: string;
  agentName: string;
  context: string;
  path: string;
}

export function terminalPaneOptions(snapshot: SessionSnapshot): TerminalPaneOption[] {
  return snapshot.workspaces.flatMap((workspace, workspaceIndex) => {
    const workspaceName = displayText(workspace.name, `Workspace ${workspaceIndex + 1}`);
    return workspace.tabs.flatMap((tab, tabIndex) => {
      const tabName = displayText(tab.name, `Tab ${tabIndex + 1}`);
      return tab.panes.flatMap((pane, paneIndex) => {
        if (pane.kind !== "terminal" || !pane.terminal_id) return [];
        return [{
          pane,
          title: pane.is_agent === true ? agentCardTitle(pane).title : "",
          agentName: displayText(pane.agent_name, displayText(pane.agent, pane.is_agent === true ? "Agent" : `Terminal ${paneIndex + 1}`)),
          context: `${workspaceName} / ${tabName}`,
          path: displayText(pane.cwd, displayText(workspace.cwd, "Terminal")),
        }];
      });
    });
  });
}

export function terminalPaneLabel(option: TerminalPaneOption): string {
  return option.title ? `${option.title} - ${option.agentName}` : option.agentName;
}

/** Match the dashboard filter without treating stale shell metadata as an agent. */
export function filterTerminalPanes(options: TerminalPaneOption[], showShells: boolean): TerminalPaneOption[] {
  return showShells ? options : options.filter(({ pane }) => pane.is_agent === true);
}

/** Scope navigation without changing the global, bounded Web recency order. */
export function filterWorkspacePanes<T extends { pane: PaneSnapshot }>(options: T[], workspace: WorkspaceSnapshot | undefined): T[] {
  const ids = new Set(workspace?.tabs.flatMap((tab) => tab.panes.map((pane) => pane.pane_id)) ?? []);
  return options.filter(({ pane }) => ids.has(pane.pane_id));
}

/** The Web selection survives native focus changes, but never a different server. */
export class TerminalWorkspaceSelection {
  #selected: { session: string; generation: string; index: number; cwd: string; paneId: string; terminalId: string } | undefined;

  select(snapshot: SessionSnapshot, pane: PaneSnapshot): void {
    if (pane.kind !== "terminal" || !pane.terminal_id) return;
    const workspace = snapshot.workspaces.find((candidate) => candidate.tabs.some((tab) => tab.panes.some(
      (current) => current.kind === "terminal" && current.pane_id === pane.pane_id && current.terminal_id === pane.terminal_id,
    )));
    if (workspace) this.#selected = {
      session: snapshot.session, generation: snapshot.server_generation, index: workspace.index,
      cwd: workspace.cwd, paneId: pane.pane_id, terminalId: pane.terminal_id,
    };
  }

  resolve(snapshot: SessionSnapshot): { workspace: WorkspaceSnapshot; pane: PaneSnapshot | undefined } | undefined {
    const selected = this.#selected;
    let workspace: WorkspaceSnapshot | undefined;
    if (selected?.session === snapshot.session && selected.generation === snapshot.server_generation) {
      for (const candidate of snapshot.workspaces) {
        for (const tab of candidate.tabs) {
          const pane = tab.panes.find((current) => current.kind === "terminal"
            && current.pane_id === selected.paneId && current.terminal_id === selected.terminalId);
          if (pane) {
            selected.index = candidate.index;
            selected.cwd = candidate.cwd;
            return { workspace: candidate, pane };
          }
        }
      }
      // Closing one terminal should not switch to a different project. Guard
      // the path too, since a closed workspace can cause index renumbering.
      workspace = snapshot.workspaces.find((candidate) => candidate.index === selected.index && candidate.cwd === selected.cwd);
    }
    if (!workspace) {
      this.#selected = undefined;
      workspace = snapshot.workspaces.find((candidate) => candidate.active) ?? snapshot.workspaces[0];
    }
    if (!workspace) return undefined;
    const activeTab = workspace.tabs.find((tab) => tab.active) ?? workspace.tabs[0];
    const panes = activeTab?.panes.filter((pane) => pane.kind === "terminal" && pane.terminal_id) ?? [];
    const pane = panes.find((candidate) => candidate.focused) ?? panes[0]
      ?? workspace.tabs.flatMap((tab) => tab.panes).find((candidate) => candidate.kind === "terminal" && candidate.terminal_id);
    return { workspace, pane };
  }
}

export interface TerminalWorkspaceOption {
  index: number;
  name: string;
  branch: string;
  path: string;
  selected: boolean;
  target: PaneSnapshot | undefined;
  tabs: Array<{
    index: number;
    name: string;
    selected: boolean;
    paneCount: number;
    target: PaneSnapshot | undefined;
  }>;
}

/** Browser navigation uses live terminal identities, not the TUI's active tab. */
export function terminalWorkspaceOptions(snapshot: SessionSnapshot, selected: PaneSnapshot | undefined): TerminalWorkspaceOption[] {
  return snapshot.workspaces.map((workspace, workspacePosition) => {
    const tabs = workspace.tabs.map((tab, tabPosition) => {
      const panes = tab.panes.filter((pane) => pane.kind === "terminal" && pane.terminal_id);
      const current = panes.find((pane) => pane.pane_id === selected?.pane_id && pane.terminal_id === selected?.terminal_id);
      return {
        index: tab.index,
        name: displayText(tab.name, `Tab ${tabPosition + 1}`),
        selected: Boolean(current),
        paneCount: panes.length,
        target: current ?? panes.find((pane) => pane.focused) ?? panes[0],
      };
    });
    return {
      index: workspace.index,
      name: displayText(workspace.name, `Workspace ${workspacePosition + 1}`),
      branch: displayText(workspace.branch, ""),
      path: displayText(workspace.cwd, "Path unavailable"),
      selected: tabs.some((tab) => tab.selected),
      target: tabs.find((tab) => tab.selected)?.target
        ?? tabs[workspace.tabs.findIndex((tab) => tab.active)]?.target
        ?? tabs.find((tab) => tab.target)?.target,
      tabs,
    };
  });
}

/** Bounded, browser-owned recency; native focus and live updates never promote panes. */
export class TerminalPaneRecency {
  #session: string | undefined;
  #generation: string | undefined;
  #recent: string[] = [];

  opened(snapshot: SessionSnapshot, pane: PaneSnapshot): void {
    this.#selectServer(snapshot);
    if (pane.kind !== "terminal" || !pane.terminal_id) return;
    this.#recent = [pane.terminal_id, ...this.#recent.filter((id) => id !== pane.terminal_id)].slice(0, 128);
  }

  options(snapshot: SessionSnapshot): TerminalPaneOption[] {
    this.#selectServer(snapshot);
    const options = terminalPaneOptions(snapshot);
    if (!this.#recent.length) return options;
    const terminals = new Map(options.map((option) => [option.pane.terminal_id, option]));
    this.#recent = this.#recent.filter((id) => terminals.has(id));
    const ordered: TerminalPaneOption[] = [];
    for (const id of this.#recent) {
      const option = terminals.get(id);
      if (option) ordered.push(option);
      terminals.delete(id);
    }
    // Unopened terminals keep their workspace/tab order.
    return [...ordered, ...options.filter((option) => terminals.has(option.pane.terminal_id))];
  }

  #selectServer(snapshot: SessionSnapshot): void {
    if (snapshot.session === this.#session && snapshot.server_generation === this.#generation) return;
    this.#session = snapshot.session;
    this.#generation = snapshot.server_generation;
    this.#recent = [];
  }
}
