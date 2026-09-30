import type { PaneSnapshot, SessionSnapshot } from "@luvus/uhp-client";
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
