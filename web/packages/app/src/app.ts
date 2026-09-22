import { BridgeClient, BridgeError, LiveSession, type PaneSnapshot, type SessionSnapshot } from "@luvus/uhp-client";
import { button, element } from "./dom.js";
import { TerminalView } from "./terminal-view.js";

const TICKET_KEY = "luvus.web.ticket";

export class WebApp {
  #bridge: BridgeClient;
  #session: LiveSession;
  #terminal: TerminalView | undefined;

  constructor(private readonly root: HTMLElement) {
    const pair = consumePairingFragment();
    if (pair) sessionStorage.removeItem(TICKET_KEY);
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    this.#bridge = new BridgeClient(`${scheme}//${location.host}/bridge`, () => ({
      ...(sessionStorage.getItem(TICKET_KEY) ? { ticket: sessionStorage.getItem(TICKET_KEY)! } : {}),
      ...(!sessionStorage.getItem(TICKET_KEY) && pair ? { code: pair } : {}),
    }), (ticket) => sessionStorage.setItem(TICKET_KEY, ticket));
    this.#session = new LiveSession(this.#bridge);
    this.#session.addEventListener("state", () => this.#render());
    this.#session.addEventListener("snapshot", () => this.#render());
  }

  async start(): Promise<void> {
    this.#render();
    try {
      await this.#session.start();
    } catch (error) {
      this.#showError(error);
    }
  }

  #render(): void {
    if (this.#terminal) return;
    const snapshot = this.#session.snapshot;
    this.root.replaceChildren(
      element("div", { className: "shell" },
        this.#header(snapshot),
        snapshot ? this.#dashboard(snapshot) : this.#connecting(),
      ),
    );
  }

  #header(snapshot: SessionSnapshot | undefined): HTMLElement {
    return element("header", { className: "topbar" },
      element("div", { className: "brand" }, element("span", { className: "mark", text: "L" }), element("strong", { text: snapshot?.session || "Luvus" })),
      element("span", { className: `connection ${this.#session.state}`, text: this.#session.state }),
    );
  }

  #connecting(): HTMLElement {
    return element("section", { className: "empty-state" },
      element("div", { className: "pulse" }),
      element("h1", { text: this.#session.state === "expired" ? "Access expired" : "Connecting to Luvus" }),
      element("p", { text: this.#session.state === "expired" ? "Restart the bridge to generate a new pairing link." : "Authenticating and reconciling the live session." }),
      this.#session.state === "expired" ? button("Clear ticket", "primary", () => {
        sessionStorage.removeItem(TICKET_KEY);
        location.reload();
      }) : undefined,
    );
  }

  #dashboard(snapshot: SessionSnapshot): HTMLElement {
    const agents = snapshot.workspaces.flatMap((workspace) => workspace.tabs.flatMap((tab) => tab.panes.map((pane) => ({ pane, workspace: workspace.name })))).filter(({ pane }) => pane.agent);
    const workspaces = snapshot.workspaces.map((workspace) => element("article", { className: `workspace-card${workspace.active ? " active" : ""}` },
      element("div", { className: "card-title" },
        element("div", {}, element("h2", { text: workspace.name }), element("p", { text: workspace.branch || workspace.cwd })),
        element("span", { className: "count", text: String(workspace.tabs.length) }),
      ),
      ...workspace.tabs.map((tab) => element("div", { className: "tab-row" },
        element("div", {}, element("strong", { text: tab.name }), element("small", { text: tab.kind.replaceAll("_", " ") })),
        element("div", { className: "pane-list" }, ...tab.panes.map((pane) => this.#paneButton(snapshot, pane))),
      )),
    ));
    return element("div", { className: "dashboard" },
      element("section", { className: "hero" },
        element("div", {}, element("p", { className: "eyebrow", text: "MISSION CONTROL" }), element("h1", { text: `${snapshot.workspaces.length} workspace${snapshot.workspaces.length === 1 ? "" : "s"} online` })),
        button("Refresh", "ghost", () => void this.#session.refresh().catch((error) => this.#showError(error))),
      ),
      agents.length ? element("section", { className: "section" },
        element("h2", { className: "section-title", text: "Agents" }),
        element("div", { className: "agent-grid" }, ...agents.map(({ pane, workspace }) => element("button", {
          className: "agent-card",
          attrs: { type: "button" },
          on: { click: () => this.#openTerminal(snapshot, pane) },
        }, element("span", { className: `agent-dot ${pane.agent_status || "idle"}` }), element("div", {}, element("strong", { text: pane.agent_name || pane.agent || "Agent" }), element("small", { text: `${workspace} · ${pane.agent_status || "unknown"}` }))))),
      ) : undefined,
      element("section", { className: "section" },
        element("h2", { className: "section-title", text: "Workspaces" }),
        element("div", { className: "workspace-grid" }, ...workspaces),
      ),
    );
  }

  #paneButton(snapshot: SessionSnapshot, pane: PaneSnapshot): HTMLElement {
    if (pane.kind !== "terminal" || !pane.terminal_id) return element("span", { className: "pane view", text: "View" });
    return element("button", {
      className: `pane${pane.focused ? " focused" : ""}`,
      attrs: { type: "button" },
      on: { click: () => this.#openTerminal(snapshot, pane) },
    }, element("span", { text: pane.agent_name || pane.agent || `Pane ${pane.pane_id}` }), element("small", { text: pane.agent_status || "terminal" }));
  }

  #openTerminal(snapshot: SessionSnapshot, pane: PaneSnapshot): void {
    const allowed = this.#session.allowedMethods;
    const control = allowed.has("terminal.backend.control");
    if (!control && !allowed.has("terminal.backend.observe")) return;
    this.#terminal?.destroy();
    const terminal = new TerminalView(this.#bridge, snapshot.server_generation, pane, control, () => {
      terminal.destroy();
      this.#terminal = undefined;
      this.#render();
    });
    this.#terminal = terminal;
    this.root.replaceChildren(terminal.root);
    void terminal.start().catch((error) => this.#showError(error));
  }

  #showError(error: unknown): void {
    const message = error instanceof BridgeError || error instanceof Error ? error.message : "Unexpected connection error";
    const toast = element("div", { className: "toast", text: message });
    this.root.append(toast);
    setTimeout(() => toast.remove(), 5_000);
  }
}

function consumePairingFragment(): string | undefined {
  const params = new URLSearchParams(location.hash.slice(1));
  const pair = params.get("pair") || undefined;
  if (pair) history.replaceState(null, "", `${location.pathname}${location.search}`);
  return pair;
}
