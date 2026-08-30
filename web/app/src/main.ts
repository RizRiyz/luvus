import { connectTailcat } from "./adapters/tailcat";
import { forgetCredentials, safeError } from "./security";
import { asSnapshot, eventSequence, type SessionSnapshot } from "./state";
import {
  terminalFrame,
  terminalLocators,
  type TerminalAction,
  type TerminalLocator,
} from "./terminal";
import { UhpClient } from "./uhp";
import type { ByteTransportClient } from "./transport";

const form = required<HTMLFormElement>("connect-form");
const addressInput = required<HTMLInputElement>("address");
const portInput = required<HTMLInputElement>("port");
const pairingInput = required<HTMLInputElement>("pairing");
const connectButton = required<HTMLButtonElement>("connect");
const refreshButton = required<HTMLButtonElement>("refresh");
const disconnectButton = required<HTMLButtonElement>("disconnect");
const status = required<HTMLElement>("status");
const statusText = required<HTMLElement>("status-text");
const expires = required<HTMLElement>("expires");
const dashboard = required<HTMLElement>("dashboard");
const sessionCard = required<HTMLElement>("session-card");
const workspaceList = required<HTMLElement>("workspaces");
const agentList = required<HTMLElement>("agents");
const activity = required<HTMLElement>("activity");
const authority = required<HTMLElement>("authority");
const promptDialog = required<HTMLDialogElement>("prompt-dialog");
const promptForm = required<HTMLFormElement>("prompt-form");
const promptTarget = required<HTMLElement>("prompt-target");
const promptText = required<HTMLTextAreaElement>("prompt-text");
const promptCancel = required<HTMLButtonElement>("prompt-cancel");
const promptSend = required<HTMLButtonElement>("prompt-send");
const terminalDialog = required<HTMLDialogElement>("terminal-dialog");
const terminalTitle = required<HTMLElement>("terminal-title");
const terminalLocation = required<HTMLElement>("terminal-location");
const terminalMode = required<HTMLElement>("terminal-mode");
const terminalOutput = required<HTMLElement>("terminal-output");
const terminalState = required<HTMLElement>("terminal-state");
const terminalInputForm = required<HTMLFormElement>("terminal-input-form");
const terminalInput = required<HTMLTextAreaElement>("terminal-input");
const terminalType = required<HTMLButtonElement>("terminal-type");
const terminalSend = required<HTMLButtonElement>("terminal-send");
const terminalClose = required<HTMLButtonElement>("terminal-close");
const terminalKeys = required<HTMLElement>("terminal-keys");

let transport: ByteTransportClient | null = null;
let uhp: UhpClient | null = null;
let snapshot: SessionSnapshot | null = null;
let expiresAt = 0;
let lastSequence = 0;
let refreshTimer = 0;
let expiryTimer = 0;
let controlEnabled = false;
let terminalControlEnabled = false;
let selectedAgentPane = "";
let terminals = new Map<string, TerminalLocator>();
let selectedTerminal: TerminalLocator | null = null;

form.addEventListener("submit", (event) => {
  event.preventDefault();
  void connect();
});
refreshButton.addEventListener("click", () => void refresh());
disconnectButton.addEventListener("click", disconnect);
workspaceList.addEventListener("click", (event) => void handleWorkspaceAction(event));
agentList.addEventListener("click", handleAgentAction);
promptCancel.addEventListener("click", closePrompt);
promptForm.addEventListener("submit", (event) => void sendAgentPrompt(event));
terminalInputForm.addEventListener("submit", (event) => void sendTerminalText(event, "submit_text"));
terminalType.addEventListener("click", (event) => void sendTerminalText(event, "type_literal"));
terminalClose.addEventListener("click", closeTerminal);
terminalKeys.addEventListener("click", (event) => void sendTerminalKey(event));
terminalOutput.addEventListener("keydown", (event) => void handleTerminalKeydown(event));
window.addEventListener("beforeunload", disconnect);

async function connect(): Promise<void> {
  disconnect();
  setBusy(true);
  setStatus("loading", "Loading encrypted transport…");
  try {
    const address = addressInput.value.trim();
    const port = Number(portInput.value);
    const code = pairingInput.value.trim().toUpperCase();
    if (!address.startsWith("tc") || address.length > 12 * 1024) throw new Error("Enter the Tailcat address shown by `luvus web`.");
    if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("Enter the port shown by `luvus web`.");
    if (!/^[2-9A-HJ-NP-Z]{4}-[2-9A-HJ-NP-Z]{4}-[2-9A-HJ-NP-Z]{4}$/.test(code)) {
      throw new Error("Enter the pairing code shown by `luvus web`.");
    }
    transport = await connectTailcat(address, (message) => setStatus("loading", message));
    setStatus("loading", "Pairing with Luvus…");
    uhp = new UhpClient(transport, port);
    const paired = await uhp.pair(code);
    expiresAt = paired.expiresAt;
    controlEnabled = paired.scopes.includes("workspace") && paired.scopes.includes("agent");
    terminalControlEnabled = paired.scopes.includes("terminal");
    addressInput.value = "";
    pairingInput.value = "";
    await refresh();
    await subscribe();
    dashboard.hidden = false;
    refreshButton.hidden = false;
    disconnectButton.hidden = false;
    setConnectedStatus();
    startExpiryClock();
  } catch (error) {
    disconnect();
    setStatus("error", safeError(error));
  } finally {
    setBusy(false);
  }
}

async function refresh(): Promise<void> {
  if (!uhp) return;
  refreshButton.disabled = true;
  try {
    const [capabilities, snapshotValue, agents, terminalInventory] = await Promise.all([
      uhp.request("uhp.capabilities"),
      uhp.request("session.snapshot"),
      uhp.request("agent.list"),
      uhp.request("terminal.backend.inventory"),
    ]);
    snapshot = asSnapshot(snapshotValue);
    terminals = terminalLocators(terminalInventory);
    if (selectedTerminal && !terminals.has(selectedTerminal.pane_id)) closeTerminal();
    lastSequence = Math.max(lastSequence, snapshot.event_sequence);
    renderSnapshot(snapshot, capabilities, agents);
  } catch (error) {
    setStatus("error", safeError(error));
    throw error;
  } finally {
    refreshButton.disabled = false;
  }
}

async function subscribe(): Promise<void> {
  if (!uhp || !snapshot) return;
  await uhp.subscribe(
    snapshot.event_sequence,
    (sequence) => {
      lastSequence = Math.max(lastSequence, sequence);
    },
    (value) => {
      const sequence = eventSequence(value);
      if (sequence === null) {
        setStatus("error", "Invalid event received; refreshing state…");
        scheduleRefresh(true);
        return;
      }
      if (sequence <= lastSequence) return;
      const gap = sequence !== lastSequence + 1;
      lastSequence = sequence;
      renderActivity(value);
      scheduleRefresh(gap);
    },
    (error) => {
      setStatus("error", `${safeError(error)} · reconnect to continue`);
    },
  );
}

function scheduleRefresh(immediate: boolean): void {
  window.clearTimeout(refreshTimer);
  refreshTimer = window.setTimeout(
    () => void refresh().catch(() => undefined),
    immediate ? 0 : 200,
  );
}

function renderSnapshot(value: SessionSnapshot, capabilities: unknown, agents: unknown): void {
  const methods = (capabilities as { methods?: unknown[] })?.methods;
  sessionCard.innerHTML = "";
  sessionCard.append(
    metric("Session", value.session || "default"),
    metric("Workspaces", String(value.workspaces.length)),
    metric("Event fence", String(value.event_sequence)),
    metric("Read methods", String(Array.isArray(methods) ? methods.length : "—")),
  );

  workspaceList.innerHTML = "";
  if (value.workspaces.length === 0) workspaceList.append(empty("No open workspaces"));
  for (const workspace of value.workspaces) {
    const item = document.createElement("article");
    item.className = "workspace";
    const heading = document.createElement("div");
    heading.className = "workspace-heading";
    const title = document.createElement("h3");
    title.textContent = workspace.name || `Workspace ${workspace.index}`;
    heading.append(title);
    if (controlEnabled) {
      heading.append(actionButton(workspace.active ? "Focused" : "Focus", "workspace", {
        workspace: String(workspace.index - 1),
      }, workspace.active));
    }
    const meta = document.createElement("p");
    meta.textContent = `${workspace.branch || "no branch"} · ${workspace.tabs.length} tab${workspace.tabs.length === 1 ? "" : "s"}`;
    const tabs = document.createElement("div");
    tabs.className = "tab-grid";
    for (const tab of workspace.tabs) {
      const tabItem = document.createElement("div");
      tabItem.className = "tab-entry";
      const badge = controlEnabled
        ? actionButton(tab.name || `tab ${tab.index}`, "tab", {
            workspace: String(workspace.index - 1),
            tab: String(tab.index),
          }, workspace.active && tab.active)
        : document.createElement("span");
      badge.classList.add("tab");
      if (tab.active) badge.classList.add("active");
      if (!controlEnabled) badge.textContent = tab.name || `tab ${tab.index}`;
      const paneGrid = document.createElement("div");
      paneGrid.className = "pane-grid";
      for (const pane of tab.panes) {
        const paneLabel = pane.name || `${pane.kind} ${pane.pane_id}`;
        const paneActions = document.createElement("div");
        paneActions.className = "pane-actions";
        if (controlEnabled) {
          const button = actionButton(paneLabel, "pane", { pane: pane.pane_id }, pane.focused);
          button.classList.add("pane-chip");
          paneActions.append(button);
        } else {
          const paneBadge = document.createElement("span");
          paneBadge.className = pane.focused ? "pane-chip active" : "pane-chip";
          paneBadge.textContent = paneLabel;
          paneActions.append(paneBadge);
        }
        if (terminals.has(pane.pane_id)) {
          const terminal = actionButton(
            terminalControlEnabled ? "Control" : "View",
            "terminal",
            { pane: pane.pane_id },
          );
          terminal.classList.add("terminal-action");
          paneActions.append(terminal);
        }
        paneGrid.append(paneActions);
      }
      tabItem.append(badge, paneGrid);
      tabs.append(tabItem);
    }
    item.append(heading, meta, tabs);
    workspaceList.append(item);
  }

  agentList.innerHTML = "";
  const entries = extractAgentEntries(agents);
  if (entries.length === 0) agentList.append(empty("No detected agent sessions"));
  for (const entry of entries) {
    const row = document.createElement("div");
    row.className = "agent-row";
    const name = String(entry.name || entry.agent || entry.kind || "agent");
    const state = String(entry.status || entry.state || "unknown");
    row.append(metric(name, state));
    const pane = typeof entry.pane === "string" ? entry.pane : "";
    if (controlEnabled && pane) {
      const prompt = actionButton("Prompt", "prompt", { pane, agent: name });
      prompt.classList.add("agent-action");
      row.append(prompt);
    }
    agentList.append(row);
  }
}

async function handleWorkspaceAction(event: Event): Promise<void> {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>("button[data-action]");
  if (!button || !uhp) return;
  if (button.dataset.action === "terminal") {
    await openTerminal(button.dataset.pane || "");
    return;
  }
  if (!controlEnabled) return;
  button.disabled = true;
  try {
    const action = button.dataset.action;
    if (action === "workspace") {
      await uhp.request("workspace.focus", { workspace: Number(button.dataset.workspace) });
    } else if (action === "tab") {
      await uhp.request("workspace.focus", { workspace: Number(button.dataset.workspace) });
      await uhp.request("tab.focus", { tab: Number(button.dataset.tab) });
    } else if (action === "pane") {
      await uhp.request("pane.focus", { pane: button.dataset.pane });
    } else {
      return;
    }
    activity.textContent = `${action} focused from web control`;
    await refresh();
    setConnectedStatus();
  } catch (error) {
    setStatus("error", safeError(error));
  } finally {
    button.disabled = false;
  }
}

async function openTerminal(pane: string): Promise<void> {
  const locator = terminals.get(pane);
  if (!uhp || !locator) return;
  uhp.closeTerminal();
  selectedTerminal = locator;
  terminalTitle.textContent = locator.title;
  terminalLocation.textContent = locator.location;
  terminalMode.textContent = terminalControlEnabled ? "INTERACTIVE" : "OBSERVE ONLY";
  terminalMode.dataset.mode = terminalControlEnabled ? "control" : "read";
  terminalOutput.textContent = "Connecting to terminal…";
  terminalState.textContent = "opening stream";
  terminalInputForm.hidden = !terminalControlEnabled;
  terminalKeys.hidden = !terminalControlEnabled;
  terminalDialog.showModal();
  try {
    await uhp.openTerminal(
      locator,
      terminalControlEnabled,
      (value) => {
        try {
          const frame = terminalFrame(value);
          if (frame) {
            terminalOutput.textContent = frame.text;
            terminalState.textContent = `revision ${frame.contentRevision}${frame.truncated ? " · truncated" : ""}`;
            return;
          }
          const event = (value as { event?: unknown })?.event;
          if (event === "terminal.resync_required") {
            throw new Error("Terminal output changed too quickly; reopen it to resynchronize");
          }
        } catch (error) {
          terminalState.textContent = safeError(error);
        }
      },
      (error) => {
        terminalState.textContent = safeError(error);
        terminalMode.textContent = "DISCONNECTED";
      },
    );
    terminalState.textContent = "stream connected";
    terminalOutput.focus();
  } catch (error) {
    terminalState.textContent = safeError(error);
    terminalMode.textContent = "FAILED";
  }
}

async function sendTerminalText(event: Event, action: "type_literal" | "submit_text"): Promise<void> {
  event.preventDefault();
  const text = terminalInput.value;
  if (!uhp || !selectedTerminal || !terminalControlEnabled || !text) return;
  terminalType.disabled = true;
  terminalSend.disabled = true;
  try {
    await uhp.terminalAction(action, { text });
    terminalInput.value = "";
    terminalState.textContent = action === "submit_text" ? "text submitted" : "text typed";
  } catch (error) {
    terminalState.textContent = safeError(error);
  } finally {
    terminalType.disabled = false;
    terminalSend.disabled = false;
    terminalInput.focus();
  }
}

async function sendTerminalKey(event: Event): Promise<void> {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>("button[data-key]");
  const key = button?.dataset.key;
  if (!button || !key) return;
  button.disabled = true;
  try {
    await sendTerminalAction("send_key", { key });
  } finally {
    button.disabled = false;
  }
}

async function handleTerminalKeydown(event: KeyboardEvent): Promise<void> {
  if (!terminalControlEnabled || !selectedTerminal) return;
  const key = browserTerminalKey(event);
  if (!key) return;
  event.preventDefault();
  await sendTerminalAction("send_key", { key });
}

async function sendTerminalAction(action: TerminalAction, params: Record<string, string>): Promise<void> {
  if (!uhp || !selectedTerminal || !terminalControlEnabled) return;
  try {
    await uhp.terminalAction(action, params);
    terminalState.textContent = `${params.key || action} sent`;
  } catch (error) {
    terminalState.textContent = safeError(error);
  }
}

function browserTerminalKey(event: KeyboardEvent): string | null {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "c") return "ctrl-c";
  const keys: Record<string, string> = {
    Enter: "enter",
    Escape: "escape",
    Tab: event.shiftKey ? "backtab" : "tab",
    ArrowUp: "up",
    ArrowDown: "down",
    ArrowLeft: "left",
    ArrowRight: "right",
    Home: "home",
    End: "end",
    Backspace: "backspace",
    Delete: "delete",
    PageUp: "pageup",
    PageDown: "pagedown",
  };
  return keys[event.key] || null;
}

function closeTerminal(): void {
  uhp?.closeTerminal();
  selectedTerminal = null;
  terminalInput.value = "";
  terminalOutput.textContent = "";
  if (terminalDialog.open) terminalDialog.close();
}

function handleAgentAction(event: Event): void {
  const button = (event.target as Element | null)?.closest<HTMLButtonElement>('button[data-action="prompt"]');
  if (!button || !controlEnabled) return;
  selectedAgentPane = button.dataset.pane || "";
  promptTarget.textContent = `${button.dataset.agent || "agent"} · pane ${selectedAgentPane}`;
  promptText.value = "";
  promptDialog.showModal();
  promptText.focus();
}

async function sendAgentPrompt(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  const text = promptText.value.trim();
  if (!uhp || !controlEnabled || !selectedAgentPane || !text) return;
  promptSend.disabled = true;
  try {
    await uhp.request("agent.prompt", { target: selectedAgentPane, text, wait: false });
    activity.textContent = `prompt submitted to pane ${selectedAgentPane}`;
    closePrompt();
    setConnectedStatus();
  } catch (error) {
    setStatus("error", safeError(error));
  } finally {
    promptSend.disabled = false;
  }
}

function closePrompt(): void {
  selectedAgentPane = "";
  promptText.value = "";
  promptDialog.close();
}

function actionButton(
  label: string,
  action: string,
  data: Record<string, string> = {},
  disabled = false,
): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "control-action";
  button.textContent = label;
  button.dataset.action = action;
  for (const [key, value] of Object.entries(data)) button.dataset[key] = value;
  button.disabled = disabled;
  return button;
}

function extractAgentEntries(value: unknown): Record<string, unknown>[] {
  if (Array.isArray(value)) return value.filter(isRecord);
  if (!isRecord(value)) return [];
  for (const key of ["agents", "items", "sessions"]) {
    const entries = value[key];
    if (Array.isArray(entries)) return entries.filter(isRecord);
  }
  return [];
}

function renderActivity(value: unknown): void {
  const event = isRecord(value) && typeof value.event === "string" ? value.event : "state changed";
  activity.textContent = event;
}

function disconnect(): void {
  window.clearTimeout(refreshTimer);
  window.clearInterval(expiryTimer);
  closeTerminal();
  uhp?.closeEvents();
  uhp = null;
  transport?.close();
  transport = null;
  snapshot = null;
  lastSequence = 0;
  expiresAt = 0;
  controlEnabled = false;
  terminalControlEnabled = false;
  terminals.clear();
  authority.textContent = "READ ONLY";
  authority.dataset.mode = "read";
  if (promptDialog.open) closePrompt();
  forgetCredentials();
  dashboard.hidden = true;
  refreshButton.hidden = true;
  disconnectButton.hidden = true;
  expires.textContent = "";
}

function setConnectedStatus(): void {
  authority.textContent = controlEnabled ? "CONTROL ENABLED" : "READ ONLY";
  authority.dataset.mode = controlEnabled ? "control" : "read";
  setStatus("connected", controlEnabled ? "Connected · control" : "Connected · read-only");
}

function startExpiryClock(): void {
  const tick = () => {
    const remaining = Math.max(0, expiresAt * 1000 - Date.now());
    const minutes = Math.ceil(remaining / 60_000);
    expires.textContent = remaining === 0 ? "expired" : `${minutes}m remaining`;
    if (remaining === 0) {
      disconnect();
      setStatus("error", "Web access expired. Start `luvus web` again.");
    }
  };
  tick();
  expiryTimer = window.setInterval(tick, 10_000);
}

function setBusy(busy: boolean): void {
  connectButton.disabled = busy;
  addressInput.disabled = busy;
  portInput.disabled = busy;
  pairingInput.disabled = busy;
}

function setStatus(kind: "loading" | "connected" | "error", message: string): void {
  status.dataset.kind = kind;
  statusText.textContent = message;
}

function metric(label: string, value: string): HTMLElement {
  const element = document.createElement("div");
  element.className = "metric";
  const key = document.createElement("span");
  key.textContent = label;
  const content = document.createElement("strong");
  content.textContent = value;
  element.append(key, content);
  return element;
}

function empty(text: string): HTMLElement {
  const element = document.createElement("p");
  element.className = "empty";
  element.textContent = text;
  return element;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function required<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing interface element: ${id}`);
  return element as T;
}
