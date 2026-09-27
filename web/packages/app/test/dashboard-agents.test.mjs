import assert from "node:assert/strict";
import test from "node:test";

import { dashboardAgents } from "../dist/test/dashboard-agents.js";

test("dashboard filters agents and shells while keeping live counts and routes", () => {
  const panes = [
    { pane_id: "1", kind: "terminal", terminal_id: "term-1", is_agent: true, agent: "codex", agent_status: "working", agent_session_title: "Review" },
    { pane_id: "2", kind: "terminal", terminal_id: "term-2", is_agent: true, agent: "claude", agent_status: "idle" },
    { pane_id: "3", kind: "terminal", terminal_id: "term-3", is_agent: false, agent: "zsh", agent_status: "done" },
    { pane_id: "4", kind: "terminal", terminal_id: null, is_agent: false, agent: "zsh" },
    { pane_id: "5", kind: "view", agent_status: "working" },
  ];
  const snapshot = { workspaces: [{ name: "web", tabs: [{ panes }] }] };

  const active = dashboardAgents(snapshot, false);
  assert.equal(active.agentCount, 2);
  assert.equal(active.workingCount, 1);
  assert.deepEqual(active.cards.map(({ pane }) => pane.pane_id), ["1", "2"]);
  assert.equal(active.cards[0].title, "Review");
  assert.equal(active.cards[1].title, "Untitled session");

  const all = dashboardAgents(snapshot, true);
  assert.equal(all.agentCount, 2);
  assert.equal(all.workingCount, 1);
  assert.deepEqual(all.cards.map(({ pane }) => pane.pane_id), ["1", "2", "3", "4"]);
  assert.deepEqual(all.cards.slice(2).map(({ context, title, state, available }) => ({ context, title, state, available })), [
    { context: "Shell · web", title: "Pane 3", state: "shell", available: true },
    { context: "Shell · web", title: "Pane 4", state: "shell", available: false },
  ]);
});
