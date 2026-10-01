import assert from "node:assert/strict";
import test from "node:test";
import { filterTerminalPanes, filterWorkspacePanes, TerminalPaneRecency, TerminalWorkspaceSelection, terminalPaneLabel, terminalPaneOptions, terminalWorkspaceOptions } from "../dist/test/terminal-pane-options.js";

const pane = (id, fields = {}) => ({ pane_id: id, terminal_id: `terminal-${id}`, kind: "terminal", focused: false, ...fields });
const snapshot = (panes) => ({ session: "preview", server_generation: "generation-1", workspaces: [{ index: 0, name: "sudos", cwd: "/work/sudos", tabs: [{ index: 0, name: "core", panes }] }] });

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

test("desktop filters keep all detected agents, exclude shells by default, and preserve pane routes", () => {
  const options = terminalPaneOptions(snapshot([
    pane("1", { is_agent: true, agent: "codex", agent_status: "working" }),
    pane("2", { is_agent: true, agent: "claude", agent_status: "idle" }),
    pane("3", { is_agent: false, agent: "zsh", agent_status: "working" }),
    pane("4", { agent: "old-agent", agent_status: "done" }),
  ]));
  assert.deepEqual(filterTerminalPanes(options, false).map(({ pane }) => pane.pane_id), ["1", "2"]);
  assert.equal(filterTerminalPanes(options, true), options);
  assert.equal(filterTerminalPanes(options, false)[0], options[0]);
});

test("workspace-scoped panes include all its tabs, keep recency, and exclude other projects", () => {
  const first = pane("1", { is_agent: true }), shell = pane("2"), second = pane("3", { is_agent: true });
  const data = snapshot([first]);
  data.workspaces[0].tabs.push({ index: 1, name: "review", panes: [shell] });
  data.workspaces.push({ index: 3, name: "sudos", cwd: "/other", tabs: [{ index: 0, name: "core", panes: [second] }] });
  const recent = new TerminalPaneRecency();
  recent.opened(data, shell);
  recent.opened(data, second);
  const options = recent.options(data);
  const scoped = filterWorkspacePanes(options, data.workspaces[0]);
  assert.deepEqual(scoped.map(({ pane }) => pane.pane_id), ["2", "1"]);
  assert.deepEqual(filterTerminalPanes(scoped, false).map(({ pane }) => pane.pane_id), ["1"]);
  assert.deepEqual(filterWorkspacePanes(options, data.workspaces[1]).map(({ pane }) => pane.pane_id), ["3"]);
  assert.deepEqual(filterWorkspacePanes(options, undefined), []);
  assert.deepEqual(options.map(({ pane }) => pane.pane_id), ["3", "2", "1"]);
});

test("Web workspace selection ignores TUI focus changes and follows the selected live pane", () => {
  const native = pane("1", { focused: true }), selected = pane("2");
  const data = snapshot([native]);
  data.workspaces[0].active = true;
  data.workspaces.push({ index: 3, name: "other", cwd: "/other", active: false, tabs: [{ index: 5, name: "review", panes: [selected] }] });
  const selection = new TerminalWorkspaceSelection();
  assert.equal(selection.resolve(data).workspace, data.workspaces[0]);
  selection.select(data, selected);
  assert.equal(selection.resolve(data).workspace, data.workspaces[1]);
  assert.equal(selection.resolve(data).pane, selected);
  native.agent_session_title = "Updated native title";
  assert.equal(selection.resolve(data).workspace, data.workspaces[1]);
  data.workspaces[1].tabs[0].panes = [];
  data.workspaces[0].tabs[0].panes.push(selected);
  assert.equal(selection.resolve(data).workspace, data.workspaces[0]);
  assert.equal(data.workspaces[1].active, false);
});

test("closing a Web terminal retains its workspace, but a removed workspace cannot reuse its index", () => {
  const native = pane("1"), selected = pane("2"), remaining = pane("3");
  const data = snapshot([native]);
  data.workspaces[0].active = true;
  data.workspaces.push({ index: 3, name: "other", cwd: "/other", tabs: [{ index: 5, name: "review", panes: [selected, remaining] }] });
  const selection = new TerminalWorkspaceSelection();
  selection.select(data, selected);
  data.workspaces[1].tabs[0].panes = [remaining];
  assert.equal(selection.resolve(data).workspace, data.workspaces[1]);
  assert.equal(selection.resolve(data).pane, remaining);
  data.workspaces[1].cwd = "/replacement";
  assert.equal(selection.resolve(data).workspace, data.workspaces[0]);
});

test("Web workspace selection resets for a new session or server and guards empty snapshots", () => {
  const first = pane("1"), selected = pane("2");
  const data = snapshot([first]);
  data.workspaces[0].active = true;
  data.workspaces.push({ index: 3, name: "other", cwd: "/other", tabs: [{ index: 5, name: "review", panes: [selected] }] });
  const selection = new TerminalWorkspaceSelection();
  selection.select(data, selected);
  assert.equal(selection.resolve({ ...data, session: "other-session" }).workspace, data.workspaces[0]);
  selection.select(data, selected);
  assert.equal(selection.resolve({ ...data, server_generation: "replacement" }).workspace, data.workspaces[0]);
  selection.select(data, selected);
  selection.select(data, { ...selected, terminal_id: "unrecognized" });
  assert.equal(selection.resolve(data).workspace, data.workspaces[1]);
  assert.equal(selection.resolve({ ...data, workspaces: [] }), undefined);
});

test("workspace navigation highlights the Web terminal, not the native active workspace", () => {
  const native = pane("1", { focused: true }), current = pane("2");
  const data = snapshot([native]);
  data.workspaces[0].active = true;
  data.workspaces.push({ index: 3, name: "other", cwd: "/other", active: false,
    tabs: [{ index: 5, name: "review", active: false, panes: [current] }] });
  const options = terminalWorkspaceOptions(data, current);
  assert.deepEqual(options.map(({ index, selected }) => [index, selected]), [[0, false], [3, true]]);
  assert.equal(options[1].tabs[0].index, 5);
  assert.equal(options[1].tabs[0].selected, true);
  assert.equal(options[1].tabs[0].target, current);
  assert.equal(data.workspaces[0].active, true);
  assert.equal(data.workspaces[1].active, false);
});

test("tab navigation chooses current, then focused, then first live terminal", () => {
  const first = pane("1"), focused = pane("2", { focused: true }), current = pane("3");
  const data = snapshot([pane("view", { kind: "view", focused: true }), pane("closed", { terminal_id: null }), first, focused, current]);
  assert.equal(terminalWorkspaceOptions(data, current)[0].tabs[0].target, current);
  assert.equal(terminalWorkspaceOptions(data, undefined)[0].tabs[0].target, focused);
  focused.focused = false;
  assert.equal(terminalWorkspaceOptions(data, undefined)[0].tabs[0].target, first);
  assert.equal(terminalWorkspaceOptions(data, undefined)[0].tabs[0].paneCount, 3);
  const replacement = { ...current, terminal_id: "replaced" };
  assert.equal(terminalWorkspaceOptions(data, replacement)[0].selected, false);
});

test("workspace navigation retains empty and native-only tabs without selectable targets", () => {
  const data = snapshot([]);
  data.workspaces[0].name = " null ";
  data.workspaces[0].tabs.push({ index: 1, name: null, panes: [pane("view", { kind: "view" })] });
  const [workspace] = terminalWorkspaceOptions(data, undefined);
  assert.equal(workspace.name, "Workspace 1");
  assert.equal(workspace.target, undefined);
  assert.deepEqual(workspace.tabs.map(({ name, paneCount, target }) => [name, paneCount, target]), [["core", 0, undefined], ["Tab 2", 0, undefined]]);
  assert.deepEqual(terminalWorkspaceOptions({ ...data, workspaces: [] }, undefined), []);
});

test("workspace sidebar projects branch and path without inventing missing Git data", () => {
  const current = pane("1");
  const data = snapshot([current]);
  data.workspaces[0].branch = "  feature/sidebar-ui  ";
  const [option] = terminalWorkspaceOptions(data, current);
  assert.equal(option.name, "sudos");
  assert.equal(option.branch, "feature/sidebar-ui");
  assert.equal(option.path, "/work/sudos");
  assert.equal(option.target, current);
  for (const branch of [undefined, null, " null ", ""]) {
    data.workspaces[0].branch = branch;
    assert.equal(terminalWorkspaceOptions(data, current)[0].branch, "");
  }
});

test("workspace selection prefers the Web tab, then the native active tab, then an available terminal", () => {
  const first = pane("1"), active = pane("2"), current = pane("3");
  const data = snapshot([first]);
  data.workspaces[0].tabs.push({ index: 2, name: "active", active: true, panes: [active, current] });
  assert.equal(terminalWorkspaceOptions(data, current)[0].target, current);
  assert.equal(terminalWorkspaceOptions(data, undefined)[0].target, active);
  active.terminal_id = null;
  current.kind = "view";
  assert.equal(terminalWorkspaceOptions(data, undefined)[0].target, first);
});

test("web-opened terminals lead the list without duplicating entries or following native focus", () => {
  const panes = [pane("1", { is_agent: true }), pane("2"), pane("3", { is_agent: true })];
  const data = snapshot(panes);
  const recent = new TerminalPaneRecency();
  const ids = () => recent.options(data).map(({ pane }) => pane.pane_id);
  assert.deepEqual(ids(), ["1", "2", "3"]);
  recent.opened(data, panes[2]);
  recent.opened(data, panes[1]);
  assert.deepEqual(ids(), ["2", "3", "1"]);
  recent.opened(data, panes[2]);
  panes[0].focused = true;
  panes[2].agent_session_title = "Updated title";
  assert.deepEqual(ids(), ["3", "2", "1"]);
  assert.deepEqual(filterTerminalPanes(recent.options(data), false).map(({ pane }) => pane.pane_id), ["3", "1"]);
});

test("web recency cannot transfer to a replaced terminal, another session, or server generation", () => {
  const first = pane("1"), second = pane("2");
  const data = snapshot([first, second]);
  const recent = new TerminalPaneRecency();
  recent.opened(data, second);
  second.terminal_id = "replacement-terminal";
  assert.deepEqual(recent.options(data).map(({ pane }) => pane.pane_id), ["1", "2"]);
  recent.opened(data, second);
  const other = { ...data, session: "other" };
  assert.deepEqual(recent.options(other).map(({ pane }) => pane.pane_id), ["1", "2"]);
  recent.opened(other, second);
  assert.deepEqual(recent.options({ ...other, server_generation: "generation-2" }).map(({ pane }) => pane.pane_id), ["1", "2"]);
});

test("closed terminals are pruned and unavailable native views are never added to web recency", () => {
  const first = pane("1"), second = pane("2");
  const data = snapshot([first, second]);
  const recent = new TerminalPaneRecency();
  recent.opened(data, second);
  data.workspaces[0].tabs[0].panes = [first];
  assert.deepEqual(recent.options(data).map(({ pane }) => pane.pane_id), ["1"]);
  data.workspaces[0].tabs[0].panes.push(second);
  recent.opened(data, { ...second, kind: "view" });
  recent.opened(data, { ...second, terminal_id: null });
  assert.deepEqual(recent.options(data).map(({ pane }) => pane.pane_id), ["1", "2"]);
});

test("web recency stays bounded while keeping unopened terminals in layout order", () => {
  const panes = Array.from({ length: 130 }, (_, index) => pane(String(index)));
  const data = snapshot(panes);
  const recent = new TerminalPaneRecency();
  for (const item of panes) recent.opened(data, item);
  const ordered = recent.options(data).map(({ pane }) => pane.pane_id);
  assert.deepEqual(ordered.slice(0, 128), panes.slice(2).reverse().map(({ pane_id }) => pane_id));
  assert.deepEqual(ordered.slice(128), ["0", "1"]);
});
