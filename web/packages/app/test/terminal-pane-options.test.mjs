import assert from "node:assert/strict";
import test from "node:test";
import { terminalPaneLabel, terminalPaneOptions } from "../dist/test/terminal-pane-options.js";

const pane = (id, fields = {}) => ({ pane_id: id, terminal_id: `terminal-${id}`, kind: "terminal", focused: false, ...fields });
const snapshot = (panes) => ({ workspaces: [{ name: "sudos", cwd: "/work/sudos", tabs: [{ name: "core", panes }] }] });

test("pane picker shows session title before agent name and keeps path separate", () => {
  const agent = pane("1", { is_agent: true, agent: "claude", agent_session_title: "  Review clipboard support  ", cwd: "/work/sudos/web" });
  const [option] = terminalPaneOptions(snapshot([agent]));
  assert.equal(option.pane, agent);
  assert.equal(option.title, "Review clipboard support");
  assert.equal(option.agentName, "claude");
  assert.equal(terminalPaneLabel(option), "Review clipboard support - claude");
  assert.equal(option.path, "/work/sudos/web");
  assert.equal(option.context, "sudos / core");
});

test("missing titles use the same fallback as agent cards; shells have no stale title", () => {
  const options = terminalPaneOptions(snapshot([
    pane("1", { is_agent: true, agent_name: "Codex", agent_session_title: null }),
    pane("2", { is_agent: true, agent: "codex", agent_session_title: " null " }),
    pane("3", { is_agent: false, agent: "zsh", agent_session_title: "Old agent title" }),
    pane("4"),
  ]));
  assert.deepEqual(options.map(terminalPaneLabel), ["Untitled session - Codex", "Untitled session - codex", "zsh", "Terminal 4"]);
  assert.ok(options.every((option) => option.path === "/work/sudos"));
});

test("pane picker excludes native views and unavailable terminals, preserving all live destinations", () => {
  const first = pane("1"), second = pane("2");
  const data = snapshot([first, pane("3", { kind: "view" }), pane("4", { terminal_id: null })]);
  data.workspaces.push({ name: "other", cwd: "/other", tabs: [{ name: "review", panes: [second] }] });
  const options = terminalPaneOptions(data);
  assert.deepEqual(options.map(({ pane }) => pane), [first, second]);
  assert.equal(options[1].context, "other / review");
  assert.equal(options[1].path, "/other");
});

test("fresh picker labels reflect title changes without truncating source data", () => {
  const agent = pane("7", { is_agent: true, agent: "codex", agent_session_title: "Initial title" });
  const data = snapshot([agent]);
  assert.equal(terminalPaneLabel(terminalPaneOptions(data)[0]), "Initial title - codex");
  agent.agent_session_title = "最新タイトル 🧭 " + "long title ".repeat(20);
  assert.equal(terminalPaneOptions(data)[0].title, agent.agent_session_title.trim());
  assert.ok(terminalPaneLabel(terminalPaneOptions(data)[0]).endsWith(" - codex"));
});
