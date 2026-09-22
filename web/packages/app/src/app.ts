import { BridgeClient, BridgeError, LiveSession, type PaneSnapshot, type SessionSnapshot } from "@luvus/uhp-client";
import { button, element } from "./dom.js";
import { pairingQrDataUrl } from "./pairing-qr.js";
import { TerminalView } from "./terminal-view.js";

const TICKET_KEY = "luvus.web.ticket";

type DeviceStatus = {
  type: "browser_device_status";
  paired_devices: number;
  pending_pairings: number;
  max_devices: number;
};

type DevicePairing = {
  type: "browser_device_pairing";
  code: string;
  expires_at: number;
  url?: string;
  devices: DeviceStatus;
};

export class WebApp {
  #bridge: BridgeClient;
  #session: LiveSession;
  #terminal: TerminalView | undefined;
  #devices: DeviceStatus | undefined;
  #devicePanelOpen = false;
  #deviceLoading = false;
  #pairingUrl: string | undefined;

  constructor(private readonly root: HTMLElement) {
    const pair = consumePairingFragment();
    if (pair) sessionStorage.removeItem(TICKET_KEY);
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    this.#bridge = new BridgeClient(`${scheme}//${location.host}/bridge`, () => ({
      ...(sessionStorage.getItem(TICKET_KEY) ? { ticket: sessionStorage.getItem(TICKET_KEY)! } : {}),
      ...(!sessionStorage.getItem(TICKET_KEY) && pair ? { code: pair } : {}),
    }), (ticket) => sessionStorage.setItem(TICKET_KEY, ticket));
    this.#bridge.addEventListener("devices", (event) => {
      try {
        this.#devices = asDeviceStatus((event as CustomEvent).detail);
        this.#render();
      } catch (error) {
        this.#showError(error);
      }
    });
    this.#session = new LiveSession(this.#bridge);
    this.#session.addEventListener("state", () => {
      this.#render();
      if (this.#session.state === "ready" && !this.#devices) void this.#refreshDevices();
    });
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
        snapshot && this.#devicePanelOpen ? this.#devicePanel() : undefined,
      ),
    );
  }

  #header(snapshot: SessionSnapshot | undefined): HTMLElement {
    return element("header", { className: "topbar" },
      element("div", { className: "topbar-inner" },
        element("div", { className: "brand" },
          element("img", { className: "mark", attrs: { src: "/mark.svg", alt: "" } }),
          element("strong", { text: "Luvus Web" }),
        ),
        element("div", { className: "topbar-actions" },
          snapshot ? button(
            this.#devices ? `${this.#devices.paired_devices}/${this.#devices.max_devices} devices` : "Devices",
            "device-button",
            () => {
              this.#devicePanelOpen = true;
              this.#render();
              void this.#refreshDevices();
            },
          ) : undefined,
          element("span", { className: `connection ${this.#session.state}`, text: this.#session.state }),
        ),
      ),
    );
  }

  #connecting(): HTMLElement {
    return element("section", { className: "empty-state" },
      element("div", { className: "pulse" }),
      element("h1", { text: this.#session.state === "expired" ? "Access expired" : "Connecting to Luvus" }),
      element("p", { text: this.#session.state === "expired" ? "This device ticket expired, or this one-use pairing link was already used. Ask a connected device to create a new link." : "Authenticating and reconciling the live session." }),
    );
  }

  #devicePanel(): HTMLElement {
    const status = this.#devices;
    const used = status ? status.paired_devices + status.pending_pairings : 1;
    const select = element("select", {
      className: "device-limit",
      attrs: { "aria-label": "Maximum paired devices", ...(this.#deviceLoading ? { disabled: "" } : {}) },
      on: { change: (event) => void this.#setDeviceLimit(Number((event.currentTarget as HTMLSelectElement).value)) },
    });
    for (let limit = 1; limit <= 8; limit += 1) {
      select.append(element("option", {
        text: String(limit),
        attrs: {
          value: String(limit),
          ...(status?.max_devices === limit ? { selected: "" } : {}),
          ...(limit < used ? { disabled: "" } : {}),
        },
      }));
    }
    const pairButton = button(
      this.#deviceLoading ? "Creating…" : used >= (status?.max_devices ?? 1) ? "Device limit reached" : "Pair another device",
      "primary device-pair",
      () => void this.#createDevicePairing(),
    );
    pairButton.disabled = this.#deviceLoading || !status || used >= status.max_devices;
    const panel = element("section", { className: "device-panel", attrs: { role: "dialog", "aria-modal": "true", "aria-labelledby": "device-title" } },
      element("div", { className: "device-panel-head" },
        element("div", {},
          element("p", { className: "eyebrow", text: "BROWSER ACCESS" }),
          element("h2", { text: "Connected devices", attrs: { id: "device-title" } }),
        ),
        button("Close", "device-close", () => this.#closeDevicePanel()),
      ),
      element("p", { className: "device-copy", text: status
        ? `${status.paired_devices} authorized${status.pending_pairings ? ` · ${status.pending_pairings} link pending` : ""}`
        : "Loading device access…" }),
      element("label", { className: "device-limit-row" },
        element("span", { text: "Maximum devices" }),
        select,
      ),
      element("p", { className: "device-help", text: "Each device receives its own ticket. Pairing links work once and expire after five minutes." }),
      this.#pairingUrl ? this.#pairingCard(this.#pairingUrl) : pairButton,
    );
    const overlay = element("div", {
      className: "device-overlay",
      on: { click: (event) => { if (event.target === event.currentTarget) this.#closeDevicePanel(); } },
    }, panel);
    return overlay;
  }

  #pairingCard(url: string): HTMLElement {
    return element("div", { className: "pairing-card" },
      element("strong", { text: "New device link" }),
      element("p", { text: "Scan with the phone camera, or use the link below." }),
      element("div", { className: "pairing-qr-wrap" },
        element("img", {
          className: "pairing-qr",
          attrs: {
            src: pairingQrDataUrl(url),
            alt: "QR code containing the one-use Luvus device pairing link",
            width: "220",
            height: "220",
          },
        }),
      ),
      element("input", { className: "pairing-link", attrs: { value: url, readonly: "", "aria-label": "One-use device pairing link" } }),
      element("div", { className: "pairing-actions" },
        button("Copy link", "primary", () => void this.#copyPairingLink(url)),
        typeof navigator.share === "function" ? button("Share", "ghost", () => void navigator.share({ title: "Connect to Luvus", url }).catch(() => {})) : undefined,
        button("Done", "ghost", () => {
          this.#pairingUrl = undefined;
          this.#render();
          void this.#refreshDevices();
        }),
      ),
    );
  }

  async #refreshDevices(): Promise<void> {
    if (this.#deviceLoading || this.#session.state !== "ready") return;
    this.#deviceLoading = true;
    let failure: unknown;
    try {
      this.#devices = asDeviceStatus(await this.#bridge.request("web.devices.status"));
      this.#render();
    } catch (error) {
      failure = error;
    } finally {
      this.#deviceLoading = false;
      if (this.#devicePanelOpen) this.#render();
    }
    if (failure) this.#showError(failure);
  }

  async #setDeviceLimit(limit: number): Promise<void> {
    if (this.#deviceLoading) return;
    this.#deviceLoading = true;
    let failure: unknown;
    try {
      this.#devices = asDeviceStatus(await this.#bridge.request("web.devices.set_limit", { limit }));
      this.#render();
    } catch (error) {
      failure = error;
    } finally {
      this.#deviceLoading = false;
      if (this.#devicePanelOpen) this.#render();
    }
    if (failure) this.#showError(failure);
  }

  async #createDevicePairing(): Promise<void> {
    if (this.#deviceLoading) return;
    this.#deviceLoading = true;
    this.#render();
    let failure: unknown;
    try {
      const pairing = asDevicePairing(await this.#bridge.request("web.devices.create_pairing"));
      this.#devices = pairing.devices;
      this.#pairingUrl = pairing.url ?? `${location.origin}${location.pathname}#pair=${encodeURIComponent(pairing.code)}`;
      this.#render();
    } catch (error) {
      failure = error;
    } finally {
      this.#deviceLoading = false;
      if (this.#devicePanelOpen) this.#render();
    }
    if (failure) this.#showError(failure);
  }

  async #copyPairingLink(url: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(url);
      this.#showMessage("Pairing link copied");
    } catch {
      const input = document.querySelector<HTMLInputElement>(".pairing-link");
      input?.select();
      this.#showMessage("Select and copy the pairing link");
    }
  }

  #closeDevicePanel(): void {
    this.#devicePanelOpen = false;
    this.#render();
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
    const streamCursor = this.#session.capabilities?.terminal?.features?.includes("stream_cursor") ?? false;
    const terminal = new TerminalView(this.#bridge, snapshot.server_generation, pane, control, streamCursor, () => {
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

  #showMessage(message: string): void {
    const toast = element("div", { className: "toast success", text: message });
    this.root.append(toast);
    setTimeout(() => toast.remove(), 3_000);
  }
}

function consumePairingFragment(): string | undefined {
  const params = new URLSearchParams(location.hash.slice(1));
  const pair = params.get("pair") || undefined;
  if (pair) history.replaceState(null, "", `${location.pathname}${location.search}`);
  return pair;
}

function asDeviceStatus(value: unknown): DeviceStatus {
  const status = value as Partial<DeviceStatus> | undefined;
  if (!status || status.type !== "browser_device_status"
    || !Number.isSafeInteger(status.paired_devices) || !Number.isSafeInteger(status.pending_pairings)
    || !Number.isSafeInteger(status.max_devices)) {
    throw new BridgeError("Invalid browser device status", "invalid_response");
  }
  return status as DeviceStatus;
}

function asDevicePairing(value: unknown): DevicePairing {
  const pairing = value as Partial<DevicePairing> | undefined;
  if (!pairing || pairing.type !== "browser_device_pairing" || typeof pairing.code !== "string"
    || !Number.isSafeInteger(pairing.expires_at) || !pairing.devices) {
    throw new BridgeError("Invalid browser pairing response", "invalid_response");
  }
  return { ...pairing, devices: asDeviceStatus(pairing.devices) } as DevicePairing;
}
