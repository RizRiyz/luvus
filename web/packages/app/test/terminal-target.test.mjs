import assert from "node:assert/strict";
import test from "node:test";

import { TerminalTargetTracker } from "../dist/test/terminal-target.js";

const pane = (paneId, terminalId) => ({
  pane_id: paneId,
  terminal_id: terminalId,
  kind: "terminal",
  focused: false,
});

const snapshot = (session, generation, panes) => ({
  type: "session_snapshot",
  session,
  server_generation: generation,
  event_sequence: 0,
  workspaces: [{
    index: 0,
    name: "workspace",
    cwd: "/workspace",
    active: true,
    tabs: [{ index: 1, name: "tab", kind: "terminal", active: true, panes }],
  }],
});

test("terminal target follows its route across a server generation", () => {
  const first = pane("1", "terminal-a");
  const initial = snapshot("default", "generation-a", [first, pane("2", "terminal-b")]);
  const tracker = new TerminalTargetTracker(initial, first);

  const moved = snapshot("default", "generation-a", [pane("2", "terminal-b"), first]);
  assert.equal(tracker.resolve(moved)?.pane.terminal_id, "terminal-a");

  const restored = snapshot("default", "generation-b", [
    pane("10", "terminal-c"),
    pane("11", "terminal-restored"),
  ]);
  assert.deepEqual(tracker.resolve(restored), {
    serverGeneration: "generation-b",
    pane: restored.workspaces[0].tabs[0].panes[1],
  });
});

test("terminal target never crosses into another session", () => {
  const first = pane("1", "terminal-a");
  const tracker = new TerminalTargetTracker(snapshot("default", "generation-a", [first]), first);
  assert.equal(tracker.resolve(snapshot("review", "generation-b", [pane("1", "terminal-b")])), undefined);
});
