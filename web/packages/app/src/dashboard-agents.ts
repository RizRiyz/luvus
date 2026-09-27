import type { PaneSnapshot, SessionSnapshot } from "@luvus/uhp-client";

export interface DashboardAgentCard {
  pane: PaneSnapshot;
  context: string;
  title: string;
  state: string;
  titleAbsent: boolean;
  available: boolean;
}

export function dashboardAgents(snapshot: SessionSnapshot, showShells: boolean): {
  agentCount: number;
  workingCount: number;
  cards: DashboardAgentCard[];
} {
  const terminals = snapshot.workspaces.flatMap((workspace, workspaceIndex) =>
    workspace.tabs.flatMap((tab) => tab.panes
      .filter((pane) => pane.kind === "terminal")
      .map((pane) => ({ pane, workspace: displayText(workspace.name, `Workspace ${workspaceIndex + 1}`) }))),
  );
  const agents = terminals.filter(({ pane }) => pane.is_agent === true);
  return {
    agentCount: agents.length,
    workingCount: agents.filter(({ pane }) => pane.agent_status === "working").length,
    cards: (showShells ? terminals : agents).map(({ pane, workspace }) => {
      const isAgent = pane.is_agent === true;
      const sessionTitle = displayText(pane.agent_session_title, "");
      return {
        pane,
        context: isAgent
          ? `${displayText(pane.agent_name, displayText(pane.agent, "Agent"))} · ${workspace}`
          : `Shell · ${workspace}`,
        title: isAgent ? sessionTitle || "Untitled session" : `Pane ${pane.pane_id}`,
        state: isAgent ? displayText(pane.agent_status, "unknown") : "shell",
        titleAbsent: isAgent && !sessionTitle,
        available: Boolean(pane.terminal_id),
      };
    }),
  };
}

export function displayText(value: unknown, fallback: string): string {
  if (typeof value !== "string") return fallback;
  const text = value.trim();
  return text && text.toLowerCase() !== "null" ? text : fallback;
}
