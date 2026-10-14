import { BridgeClient, BridgeError, LiveSession, type PaneSnapshot, type SessionSnapshot } from "@luvus/uhp-client";
import { BrowserTickets, TICKET_KEY } from "./browser-tickets.js";
import { button, element } from "./dom.js";
import { accessProblem, pairingCredential, parsePairingInput, type SentCredential } from "./pairing.js";
import { pairingQrDataUrl } from "./pairing-qr.js";
import { RenderScheduler } from "./render-scheduler.js";
import { navigationBrand, navigationDot, refreshButton, trapNavigationFocus, type SidebarControls } from "./sidebar-controls.js";
import { supportsFileUpload } from "./terminal-capabilities.js";
import { viewportSizingAvailable } from "./terminal-viewport.js";
import { filterWorkspacePanes, TerminalPaneRecency, TerminalWorkspaceSelection, terminalWorkspaceOptions, type TerminalPaneOption } from "./terminal-pane-options.js";
import { TerminalPaneSidebar } from "./terminal-pane-sidebar.js";
import { TerminalWorkspaceSidebar } from "./terminal-workspace-sidebar.js";
import { TerminalView } from "./terminal-view.js";
import { markFieldSaved, rebuildPreservingView } from "./view-state.js";

type DeviceStatus = {
  type: "browser_device_status";
  paired_devices: number;
  pending_pairings: number;
  max_devices: number;
  /** The operator's --max-devices; a browser may not choose more. */
  limit_ceiling?: number;
  public_url: string | null;
};

type DevicePairing = {
  type: "browser_device_pairing";
  code: string;
  expires_at: number;
  url?: string;
  devices: DeviceStatus;
};

type BrowserSession = {
  name: string;
  default: boolean;
  running: boolean;
};

export class WebApp {
  #bridge: BridgeClient;
  #session: LiveSession;
  #terminal: TerminalView | undefined;
  #devices: DeviceStatus | undefined;
  #devicePanelOpen = false;
  #deviceLoading = false;
  #pairingCode: string | undefined;
  #pairingUrl: string | undefined;
  #sessions: BrowserSession[] | undefined;
  #sessionPanelOpen = false;
  #sessionLoading = false;
  #showShells = false;
  readonly #paneRecency = new TerminalPaneRecency();
  readonly #workspaceSelection = new TerminalWorkspaceSelection();
  readonly #dashboardWorkspaces: TerminalWorkspaceSidebar;
  readonly #dashboardPanes: TerminalPaneSidebar;
  readonly #overlays = element("div", { className: "web-overlays" });
  readonly #dashboardNavigation = element("div", { className: "terminal-navigation", attrs: { id: "dashboard-navigation", "aria-label": "Workspaces and panes" } });
  #dashboardNavigationOpen = false;
  readonly #dashboardOpenButton = navigationDot("Open navigation", "dashboard-navigation", () => this.#openDashboardNavigation());
  readonly #dashboardCloseButton: HTMLButtonElement;
  readonly #dashboardHeader: HTMLElement;
  readonly #desktopSidebars = matchMedia("(min-width: 1024px)");
  readonly #sidebarControls: SidebarControls = {
    collapsed: { workspaces: false, panes: false },
    toggle: (side) => {
      this.#sidebarControls.collapsed[side] = !this.#sidebarControls.collapsed[side];
      if (this.#terminal) this.#terminal.updateSidebarLayout();
      else this.#render();
    },
    refresh: () => void this.#session.refresh().catch((error) => this.#showError(error)),
    footer: () => this.#sidebarFooter(),
  };
  /** This tab's own pairing code, from its link or pasted on the access screen. */
  #tabCode: string | undefined;
  /** What this tab last presented, so a rejection can be explained truthfully. */
  #sent: SentCredential = { ticket: false, code: false };
  #pairError: string | undefined;
  readonly #tickets = new BrowserTickets();
  #sentTicket: string | null = null;
  /** Redraws caused by live updates, as opposed to the person's own actions. */
  readonly #renders = new RenderScheduler(() => this.#renderNow());

  constructor(private readonly root: HTMLElement) {
    const openPane = (pane: PaneSnapshot) => {
      const snapshot = this.#session.snapshot;
      if (snapshot) this.#openTerminal(snapshot, pane);
    };
    const overview = () => {
      this.#closeDashboardNavigation(true);
      this.root.querySelector<HTMLElement>(".dashboard")?.scrollTo({ top: 0, behavior: "smooth" });
      window.scrollTo({ top: 0, behavior: "smooth" });
    };
    this.#dashboardWorkspaces = new TerminalWorkspaceSidebar(openPane, overview, () => this.#sidebarControls.toggle("workspaces"));
    this.#dashboardPanes = new TerminalPaneSidebar(openPane, {
      showShells: this.#showShells,
      onChange: (showShells) => { this.#showShells = showShells; this.#render(); },
    }, () => this.#closeDashboardNavigation(true), () => this.#sidebarControls.toggle("panes"));
    this.#dashboardCloseButton = this.#dashboardPanes.closeButton;
    this.#dashboardCloseButton.setAttribute("aria-label", "Close navigation");
    this.#dashboardCloseButton.setAttribute("aria-controls", "dashboard-navigation");
    this.#dashboardCloseButton.title = "Close navigation";
    this.#dashboardOpenButton.setAttribute("aria-haspopup", "dialog");
    this.#dashboardHeader = element("header", { className: "dashboard-header" }, navigationBrand(overview), this.#dashboardOpenButton);
    this.#dashboardNavigation.append(
      element("header", { className: "terminal-navigation-header" }, navigationBrand(overview), this.#dashboardCloseButton),
      this.#dashboardWorkspaces.root, this.#dashboardPanes.root,
    );
    this.#desktopSidebars.addEventListener("change", () => {
      if (this.#desktopSidebars.matches) this.#dashboardNavigationOpen = false;
      this.#render();
    });
    document.addEventListener("keydown", (event) => {
      const dialog = this.#devicePanelOpen ? this.#overlays.querySelector<HTMLElement>(".device-panel")
        : !this.#terminal && this.#dashboardNavigationOpen ? this.#dashboardNavigation : undefined;
      if (!dialog) return;
      if (event.key === "Escape") {
        event.preventDefault();
        if (this.#devicePanelOpen) this.#closeDevicePanel();
        else this.#closeDashboardNavigation(true);
      } else trapNavigationFocus(event, dialog);
    });
    // Hold live redraws while a pointer is pressed, so the pressed element
    // is still there when it is released and the click is delivered.
    root.addEventListener("pointerdown", (event) => this.#renders.hold(event.pointerId), true);
    for (const type of ["pointerup", "pointercancel"] as const) {
      window.addEventListener(type, (event) => this.#renders.release(event.pointerId), true);
    }
    // A mouse that moves with no button pressed was released where this page
    // could not see it, for example outside the window.
    window.addEventListener("pointermove", (event) => {
      if (event.buttons === 0) this.#renders.release(event.pointerId);
    }, true);
    window.addEventListener("blur", () => this.#renders.releaseAll());
    document.addEventListener("visibilitychange", () => {
      if (document.hidden) this.#renders.releaseAll();
    });
    this.#tabCode = consumePairingFragment();
    const scheme = location.protocol === "https:" ? "wss:" : "ws:";
    this.#bridge = new BridgeClient(`${scheme}//${location.host}/bridge`, () => {
      this.#sentTicket = this.#tickets.get();
      const credential = pairingCredential(this.#sentTicket, this.#tabCode);
      this.#sent = { ticket: Boolean(credential.ticket), code: Boolean(credential.code) };
      return credential;
    }, (ticket) => {
      this.#tickets.set(ticket);
      this.#sentTicket = ticket;
      // The code was spent to issue this ticket.
      this.#tabCode = undefined;
    });
    this.#bridge.addEventListener("devices", (event) => {
      try {
        this.#devices = asDeviceStatus((event as CustomEvent).detail);
        if (this.#pairingCode) this.#pairingUrl = this.#pairingLink(this.#pairingCode);
        this.#renders.request();
      } catch (error) {
        this.#showError(error);
      }
    });
    this.#session = new LiveSession(this.#bridge);
    this.#session.addEventListener("state", () => {
      if (this.#session.state === "ready") this.#tabCode = undefined; // authorized; no longer needed
      if (this.#session.state === "expired" && this.#terminal) {
        // Access ended while a terminal was open. Leave it, or the terminal
        // would sit on a dead connection with no way forward.
        this.#terminal.destroy();
        this.#terminal = undefined;
      }
      this.#renders.request();
      if (this.#session.state === "ready" && !this.#devices) void this.#refreshDevices();
    });
    this.#session.addEventListener("snapshot", () => {
      const snapshot = this.#session.snapshot;
      if (snapshot) this.#terminal?.updateSnapshot(snapshot);
      this.#renders.request();
    });
    this.#session.addEventListener("titles", (event) => this.#updateTitles((event as CustomEvent<string[]>).detail));
    window.addEventListener("storage", (event) => {
      // Pairing or forgetting in another tab updates this tab too. Transient
      // disconnects do not erase access, and there is no idle polling.
      if ((event.key === TICKET_KEY || event.key === null) && this.#tickets.get() !== this.#sentTicket) {
        // Replace the old connection and any in-flight synchronization as one
        // lifecycle; late results from the old authority cannot restore its UI.
        location.reload();
      }
    });
  }

  async start(): Promise<void> {
    this.#render();
    try {
      await this.#session.start();
    } catch (error) {
      // The access screen already explains a rejection.
      if (this.#session.state !== "expired") this.#showError(error);
    }
  }

  /**
   * Redraw now, unless a pointer is pressed on the page: then the redraw is
   * queued until release, so a request that completes mid-press (device or
   * session lists, for example) cannot replace the pressed element.
   */
  #render(): void {
    if (this.#renders.holding) this.#renders.request();
    else this.#renderNow();
  }

  #renderNow(): void {
    if (this.#terminal) {
      this.#terminal.root.inert = this.#devicePanelOpen;
      this.#updateConnectionStatus();
      rebuildPreservingView(this.#overlays, () => this.#overlays.replaceChildren(...(this.#devicePanelOpen ? [this.#devicePanel()] : [])));
      return;
    }
    const snapshot = this.#session.snapshot;
    // Without authority nothing on the dashboard can act, so show the access
    // screen instead of a stale dashboard or a loading state that never ends.
    const expired = this.#session.state === "expired";
    if (expired) {
      this.#dashboardNavigationOpen = false;
      this.#devicePanelOpen = false;
    }
    rebuildPreservingView(this.root, () => {
      this.#overlays.replaceChildren(...(!expired && snapshot && this.#devicePanelOpen ? [this.#devicePanel()] : []));
      this.root.replaceChildren(
        element("div", { className: "shell" },
          expired
            ? element("div", { className: "dashboard loading-dashboard" }, this.#accessScreen())
            : snapshot
              ? this.#dashboardLayout(snapshot)
              : element("div", { className: "dashboard loading-dashboard" }, this.#connecting()),
          this.#overlays,
        ),
      );
    });
    const layout = this.root.querySelector<HTMLElement>(".dashboard-layout");
    if (layout) layout.inert = this.#devicePanelOpen;
  }

  /** Update changed agent titles in place, leaving every other element as it is. */
  #updateTitles(paneIds: string[]): void {
    const snapshot = this.#session.snapshot;
    if (!snapshot) return;
    if (this.#terminal) {
      this.#terminal.updateTitles(paneIds);
      return;
    }
    if ((this.#desktopSidebars.matches || this.#dashboardNavigationOpen) && this.#session.state !== "expired") {
      this.#dashboardPanes.update(this.#terminalPaneOptions(), undefined, this.#showShells);
    }
  }

  #connecting(): HTMLElement {
    return element("section", { className: "empty-state" },
      element("div", { className: "pulse" }),
      element("h1", { text: "Connecting to Luvus" }),
      element("p", { text: "Authenticating and reconciling the live session." }),
    );
  }

  /** Why this tab has no access, and a way to fix it without leaving the page. */
  #accessScreen(): HTMLElement {
    const problem = accessProblem(this.#sent);
    const input = element("input", {
      className: "pair-input",
      attrs: {
        type: "text",
        placeholder: "Paste a pairing link",
        "aria-label": "Pairing link or code",
        autocomplete: "off",
        autocapitalize: "off",
        spellcheck: "false",
      },
    });
    const form = element("form", {
      className: "pair-form",
      on: {
        submit: (event) => {
          event.preventDefault();
          this.#pairWith(input.value);
        },
      },
    },
      input,
      element("button", { className: "primary", text: "Connect", attrs: { type: "submit" } }),
    );
    return element("section", { className: "empty-state access-problem", attrs: { role: "alert" } },
      element("h1", { text: problem.title }),
      element("p", { text: problem.body }),
      form,
      this.#pairError ? element("p", { className: "pair-error", text: this.#pairError }) : undefined,
    );
  }

  #pairWith(value: string): void {
    const code = parsePairingInput(value);
    if (!code) {
      this.#pairError = "That is not a Luvus pairing link or code.";
      this.#render();
      return;
    }
    // A ticket the bridge just rejected can never work again.
    if (this.#sent.ticket) this.#tickets.clear(this.#sentTicket);
    this.#tabCode = code;
    this.#pairError = undefined;
    void this.start();
  }

  #devicePanel(): HTMLElement {
    const status = this.#devices;
    const used = status ? status.paired_devices + status.pending_pairings : 1;
    const select = element("select", {
      className: "device-limit",
      attrs: { "aria-label": "Maximum paired devices", ...(this.#deviceLoading ? { disabled: "" } : {}) },
      on: { change: (event) => void this.#setDeviceLimit(Number((event.currentTarget as HTMLSelectElement).value)) },
    });
    // The bridge refuses a limit above the operator's --max-devices, so only
    // offer what it will accept.
    const ceiling = status?.limit_ceiling ?? 8;
    for (let limit = 1; limit <= ceiling; limit += 1) {
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
    const publicUrl = element("input", {
      className: "device-url-input",
      attrs: {
        type: "url",
        inputmode: "url",
        autocomplete: "url",
        placeholder: location.origin,
        value: status?.public_url ?? "",
        "aria-label": "Public pairing address",
        ...(this.#deviceLoading ? { disabled: "" } : {}),
      },
      on: { keydown: (event) => {
        if ((event as KeyboardEvent).key === "Enter") void this.#setPublicUrl((event.currentTarget as HTMLInputElement).value);
      } },
    });
    const savePublicUrl = button("Save address", "ghost device-url-save", () => void this.#setPublicUrl(publicUrl.value));
    savePublicUrl.disabled = this.#deviceLoading || !status;
    const disconnectButton = button("Disconnect this browser", "ghost device-disconnect", () => void this.#disconnectBrowser());
    disconnectButton.disabled = this.#deviceLoading || !status;
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
      element("div", { className: "device-url-setting" },
        element("label", { className: "device-url-label" },
          element("span", { text: "Pairing address" }),
          publicUrl,
        ),
        savePublicUrl,
      ),
      element("p", { className: "device-help", text: "Use your HTTPS tunnel address for phone links. Clear it to use this browser's address. This changes links only, not network exposure or origin permissions." }),
      element("p", { className: "device-help", text: "Pair each browser once. Access is remembered across tabs and browser restarts. "
        + (this.#bridge.ready?.expires_at
          ? `This browser's access expires at ${new Date(this.#bridge.ready.expires_at * 1000).toLocaleString()}. `
          : "Access lasts until the bridge stops or this browser is revoked. ")
        + "Pairing links expire after five minutes." }),
      this.#pairingUrl ? this.#pairingCard(this.#pairingUrl) : pairButton,
      disconnectButton,
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
          this.#pairingCode = undefined;
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

  async #disconnectBrowser(): Promise<void> {
    if (this.#deviceLoading || !confirm("Disconnect this browser in every tab? You'll need a new QR/code to reconnect. Other browsers and running terminals are not affected.")) return;
    this.#deviceLoading = true;
    const ticket = this.#sentTicket;
    try {
      await this.#bridge.request("web.devices.forget");
      this.#tickets.clear(ticket);
      location.reload();
    } catch (error) {
      this.#showError(error);
    } finally {
      this.#deviceLoading = false;
      this.#render();
    }
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

  async #setPublicUrl(rawUrl: string): Promise<void> {
    if (this.#deviceLoading) return;
    this.#deviceLoading = true;
    let failure: unknown;
    try {
      const url = rawUrl.trim();
      this.#devices = asDeviceStatus(await this.#bridge.request("web.devices.set_public_url", { url: url || null }));
      if (this.#pairingCode) this.#pairingUrl = this.#pairingLink(this.#pairingCode);
      // The bridge may normalize the address; show what it saved rather than
      // carrying the typed spelling into the rebuilt field. Text typed after
      // this save was sent is newer than the save, so it is kept.
      const field = this.root.querySelector<HTMLInputElement>(".device-url-input");
      if (field) markFieldSaved(field, rawUrl);
      this.#render();
    } catch (error) {
      failure = error;
    } finally {
      this.#deviceLoading = false;
      if (this.#devicePanelOpen) this.#render();
    }
    if (failure) this.#showError(failure);
    else this.#showMessage("Pairing address updated");
  }

  async #createDevicePairing(): Promise<void> {
    if (this.#deviceLoading) return;
    this.#deviceLoading = true;
    this.#render();
    let failure: unknown;
    try {
      const pairing = asDevicePairing(await this.#bridge.request("web.devices.create_pairing"));
      this.#devices = pairing.devices;
      this.#pairingCode = pairing.code;
      this.#pairingUrl = pairing.url ?? this.#pairingLink(pairing.code);
      this.#render();
    } catch (error) {
      failure = error;
    } finally {
      this.#deviceLoading = false;
      if (this.#devicePanelOpen) this.#render();
    }
    if (failure) this.#showError(failure);
  }

  #pairingLink(code: string): string {
    const base = this.#devices?.public_url ?? location.origin;
    return `${base}/#pair=${encodeURIComponent(code)}`;
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
    this.root.querySelector<HTMLButtonElement>(this.#desktopSidebars.matches ? '[data-view-key="mission-nav:devices"]' : '[aria-label="Open navigation"]')?.focus({ preventScroll: true });
  }

  #openDevicePanel(): void {
    this.#dashboardNavigationOpen = false;
    this.#terminal?.closeNavigation();
    this.#devicePanelOpen = true;
    this.#render();
    this.#overlays.querySelector<HTMLButtonElement>(".device-close")?.focus({ preventScroll: true });
    void this.#refreshDevices();
  }

  #openSessionPanel(): void {
    this.#sessionPanelOpen = true;
    this.#sessions = undefined;
    this.#render();
    void this.#refreshSessions();
  }

  #closeSessionPanel(): void {
    this.#sessionPanelOpen = false;
    this.#render();
  }

  async #refreshSessions(): Promise<void> {
    if (this.#sessionLoading || this.#session.state !== "ready") return;
    this.#sessionLoading = true;
    this.#render();
    let failure: unknown;
    try {
      this.#sessions = asBrowserSessions(await this.#bridge.request("web.sessions.list"));
    } catch (error) {
      this.#sessionPanelOpen = false;
      failure = error;
    } finally {
      this.#sessionLoading = false;
      this.#render();
    }
    if (failure) this.#showError(failure);
  }

  async #switchSession(name: string): Promise<void> {
    const current = this.#session.snapshot?.session;
    if (this.#sessionLoading || name === current) return;
    this.#sessionLoading = true;
    this.#sessionPanelOpen = false;
    this.#sessions = undefined;
    this.#render();
    let failure: unknown;
    try {
      await this.#session.switchSession(name);
    } catch (error) {
      failure = error;
    } finally {
      this.#sessionLoading = false;
      this.#render();
    }
    if (failure) this.#showError(failure);
    else this.#showMessage(`Switched to ${name}`);
  }

  #sessionMenu(current: string): HTMLElement {
    const canStart = this.#session.capabilities?.access?.mode === "control";
    return element("div", { className: "session-menu", attrs: { role: "dialog", "aria-label": "Switch Luvus session" } },
      element("div", { className: "session-menu-head" },
        element("strong", { text: "Switch session" }),
        button("Close", "session-menu-close", () => this.#closeSessionPanel()),
      ),
      this.#sessionLoading || !this.#sessions
        ? element("p", { className: "session-menu-empty", text: "Loading sessions…" })
        : element("div", { className: "session-menu-list", attrs: { "data-scroll-key": "sessions" } }, ...this.#sessions.map((session) => {
          const active = session.name === current;
          const disabled = active || (!session.running && !canStart);
          const option = element("button", {
            className: `session-option${active ? " active" : ""}`,
            attrs: { type: "button", ...(disabled ? { disabled: "" } : {}) },
            on: { click: () => void this.#switchSession(session.name) },
          },
          element("span", { text: session.name }),
          element("small", { text: active ? "Current" : session.running ? "Running" : canStart ? "Start" : "Control required" }),
          );
          return option;
        })),
    );
  }

  #sidebarFooter(): HTMLElement {
    return element("nav", { className: "mission-dock", attrs: { "aria-label": "Devices and connection" } },
      missionNavButton("Devices", "devices", () => this.#openDevicePanel()),
      refreshButton(this.#sidebarControls.refresh),
      element("span", {
        className: `mission-nav-status ${this.#session.state}`,
        attrs: { role: "status", "aria-label": `Connection ${this.#session.state}`, title: this.#session.state },
      }, missionIcon("status")),
    );
  }

  #updateConnectionStatus(): void {
    for (const status of this.root.querySelectorAll<HTMLElement>(".mission-nav-status")) {
      status.className = `mission-nav-status ${this.#session.state}`;
      status.setAttribute("aria-label", `Connection ${this.#session.state}`);
      status.title = this.#session.state;
    }
  }

  #openDashboardNavigation(): void {
    if (this.#terminal || this.#desktopSidebars.matches || !this.#session.snapshot || this.#session.state === "expired") return;
    this.#dashboardNavigationOpen = true;
    this.#sessionPanelOpen = false;
    this.#render();
    this.#dashboardCloseButton.focus({ preventScroll: true });
  }

  #closeDashboardNavigation(restoreFocus = false): void {
    if (!this.#dashboardNavigationOpen) return;
    this.#dashboardNavigationOpen = false;
    this.#render();
    if (restoreFocus && !this.#desktopSidebars.matches) this.#dashboardOpenButton.focus({ preventScroll: true });
  }

  #dashboardLayout(snapshot: SessionSnapshot): HTMLElement {
    const desktop = this.#desktopSidebars.matches;
    const navigationVisible = desktop || this.#dashboardNavigationOpen;
    const selected = this.#workspaceSelection.resolve(snapshot);
    this.#dashboardWorkspaces.update(navigationVisible ? terminalWorkspaceOptions(snapshot, selected?.pane) : []);
    this.#dashboardWorkspaces.setFooter(this.#sidebarFooter());
    this.#dashboardWorkspaces.setCollapsed(this.#sidebarControls.collapsed.workspaces);
    this.#dashboardPanes.update(navigationVisible ? this.#terminalPaneOptions() : [], undefined, this.#showShells);
    this.#dashboardPanes.setCollapsed(this.#sidebarControls.collapsed.panes);
    const { workspaces, panes } = this.#sidebarControls.collapsed;
    if (this.#dashboardNavigationOpen) {
      this.#dashboardNavigation.setAttribute("role", "dialog");
      this.#dashboardNavigation.setAttribute("aria-modal", "true");
    } else {
      this.#dashboardNavigation.removeAttribute("role");
      this.#dashboardNavigation.removeAttribute("aria-modal");
    }
    this.#dashboardOpenButton.setAttribute("aria-expanded", String(this.#dashboardNavigationOpen));
    this.#dashboardHeader.inert = this.#dashboardNavigationOpen;
    const dashboard = this.#dashboard(snapshot);
    dashboard.inert = this.#dashboardNavigationOpen;
    const frame = element("div", { className: "web-content-frame" }, this.#dashboardHeader, dashboard);
    // Reuse the same sidebar nodes beside the desktop frame and in the mobile drawer.
    // Moving them preserves row identity, focus, and independent scroll state.
    if (!desktop && this.#dashboardPanes.root.parentElement !== this.#dashboardNavigation) this.#dashboardNavigation.append(this.#dashboardPanes.root);
    return element("div", { className: `dashboard-layout${workspaces ? " workspaces-collapsed" : ""}${panes ? " panes-collapsed" : ""}${this.#dashboardNavigationOpen ? " pane-navigation-open" : ""}` },
      this.#dashboardNavigation, frame, desktop ? this.#dashboardPanes.root : undefined,
    );
  }

  #dashboard(snapshot: SessionSnapshot): HTMLElement {
    const agents = snapshot.workspaces.flatMap((workspace) => workspace.tabs.flatMap((tab) => tab.panes))
      .filter((pane) => pane.kind === "terminal" && pane.is_agent === true);
    const agentCount = agents.length;
    const workingCount = agents.filter((pane) => pane.agent_status === "working").length;
    const tabCount = snapshot.workspaces.reduce((total, workspace) => total + workspace.tabs.length, 0);
    const paneCount = snapshot.workspaces.reduce((total, workspace) => total
      + workspace.tabs.reduce((tabTotal, tab) => tabTotal + tab.panes.length, 0), 0);
    return element("div", { className: "dashboard", attrs: { "data-scroll-key": "dashboard-main" } },
      element("section", { className: "mission-hero", attrs: { id: "mission-overview" } },
        element("div", { className: "hero-layout" },
          element("div", { className: "hero-stat-column stats-left" },
            missionStat(String(snapshot.workspaces.length).padStart(2, "0"), "Workspaces"),
            missionStat(String(tabCount).padStart(2, "0"), "Tabs"),
          ),
          element("div", { className: "hero-center" },
            element("h1", { text: "Live stats" }),
            element("div", { className: "mission-core", attrs: { "aria-label": "Luvus network online" } },
              element("div", { className: "orbit orbit-outer" }),
              element("div", { className: "orbit orbit-inner" }),
              element("div", { className: "core-mark" }, element("img", { attrs: { src: "/mark.svg", alt: "" } })),
              element("div", { className: "core-label" }, element("strong", { text: "SYSTEM ONLINE" }), element("small", { text: `${workingCount} executing` })),
            ),
            element("div", { className: "hero-session-wrap" },
              element("button", {
                className: "hero-session",
                attrs: {
                  type: "button",
                  "aria-haspopup": "dialog",
                  "aria-expanded": String(this.#sessionPanelOpen),
                },
                on: { click: () => this.#sessionPanelOpen ? this.#closeSessionPanel() : this.#openSessionPanel() },
              },
              element("span", { text: "Session" }),
              element("strong", { text: snapshot.session }),
              ),
              this.#sessionPanelOpen ? this.#sessionMenu(snapshot.session) : undefined,
            ),
          ),
          element("div", { className: "hero-stat-column stats-right" },
            missionStat(String(paneCount).padStart(2, "0"), "Live panes"),
            missionStat(String(agentCount).padStart(2, "0"), "Agents"),
          ),
        ),
      ),
    );
  }

  #openTerminal(snapshot: SessionSnapshot, pane: PaneSnapshot): void {
    const allowed = this.#session.allowedMethods;
    const control = allowed.has("terminal.backend.control");
    if (!control && !allowed.has("terminal.backend.observe")) return;
    this.#dashboardNavigationOpen = false;
    this.#workspaceSelection.select(snapshot, pane);
    this.#paneRecency.opened(snapshot, pane);
    this.#terminal?.destroy();
    const streamCursor = this.#session.capabilities?.terminal?.features?.includes("stream_cursor") ?? false;
    const canUploadFiles = control && supportsFileUpload(
      this.#session.capabilities?.terminal?.capabilities,
    );
    const viewportSizing = viewportSizingAvailable(this.#session.capabilities?.terminal?.capabilities, control);
    const terminal = new TerminalView(
      this.#bridge,
      snapshot,
      pane,
      control,
      canUploadFiles,
      streamCursor,
      viewportSizing,
      () => this.#terminalPaneOptions(),
      (selectedPane) => {
        const currentSnapshot = this.#session.snapshot;
        if (currentSnapshot) this.#openTerminal(currentSnapshot, selectedPane);
      },
      () => {
        terminal.destroy();
        this.#terminal = undefined;
        this.#render();
      },
      { showShells: this.#showShells, onChange: (showShells) => { this.#showShells = showShells; } },
      this.#sidebarControls,
    );
    this.#terminal = terminal;
    this.root.replaceChildren(terminal.root, this.#overlays);
    void terminal.start().catch((error) => this.#showError(error));
  }

  #terminalPaneOptions(): TerminalPaneOption[] {
    const snapshot = this.#session.snapshot;
    return snapshot ? filterWorkspacePanes(this.#paneRecency.options(snapshot), this.#workspaceSelection.resolve(snapshot)?.workspace) : [];
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
    || !Number.isSafeInteger(status.max_devices)
    || (status.public_url !== null && typeof status.public_url !== "string")
    || (status.limit_ceiling !== undefined && !Number.isSafeInteger(status.limit_ceiling))) {
    throw new BridgeError("Invalid browser device status", "invalid_response");
  }
  return status as DeviceStatus;
}

function asDevicePairing(value: unknown): DevicePairing {
  const pairing = value as Partial<DevicePairing> | undefined;
  if (!pairing || pairing.type !== "browser_device_pairing" || typeof pairing.code !== "string"
    || !Number.isSafeInteger(pairing.expires_at) || !pairing.devices
    || (pairing.url !== undefined && typeof pairing.url !== "string")) {
    throw new BridgeError("Invalid browser pairing response", "invalid_response");
  }
  return { ...pairing, devices: asDeviceStatus(pairing.devices) } as DevicePairing;
}

function asBrowserSessions(value: unknown): BrowserSession[] {
  const response = value as { type?: unknown; sessions?: unknown } | undefined;
  if (response?.type !== "browser_session_list" || !Array.isArray(response.sessions) || response.sessions.length > 256) {
    throw new BridgeError("Invalid browser session list", "invalid_response");
  }
  return response.sessions.map((entry) => {
    const session = entry as Partial<BrowserSession> | undefined;
    if (
      !session || typeof session.name !== "string" || !session.name
      || typeof session.default !== "boolean" || typeof session.running !== "boolean"
    ) throw new BridgeError("Invalid browser session entry", "invalid_response");
    return session as BrowserSession;
  });
}

type MissionIconName = "overview" | "workspaces" | "devices" | "status" | "arrow";
type MissionNavIconName = MissionIconName | "luvus";

function missionNavButton(label: string, icon: MissionNavIconName, onClick: () => void, active = false, disabled = false): HTMLButtonElement {
  const control = element("button", {
    className: `mission-nav-button${icon === "luvus" ? " logo" : ""}${active ? " active" : ""}`,
    attrs: { type: "button", "aria-label": label, title: label, "data-view-key": `mission-nav:${icon}`, ...(disabled ? { disabled: "" } : {}) },
    on: { click: onClick },
  }, icon === "luvus"
    ? element("img", { className: "mission-nav-logo", attrs: { src: "/mark.svg", alt: "" } })
    : missionIcon(icon), element("span", { className: "mission-nav-label", text: label }));
  return control;
}

function missionStat(value: string, label: string): HTMLElement {
  return element("div", { className: "mission-stat" },
    element("strong", { text: value }),
    element("div", {}, element("span", { text: label })),
  );
}

function missionIcon(name: MissionIconName): SVGSVGElement {
  const paths: Record<MissionIconName, string[]> = {
    overview: ["M4 4h6v6H4zM14 4h6v6h-6zM4 14h6v6H4zM14 14h6v6h-6z"],
    workspaces: ["M3 6.5h7l2 2h9v11H3z", "M3 6.5V4h7l2 2"],
    devices: ["M8 2h8a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2Z", "M10 18h4"],
    status: ["M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20Z", "m8 12 2.5 2.5L16 9"],
    arrow: ["M5 12h14m-5-5 5 5-5 5"],
  };
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "mission-icon");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  for (const data of paths[name]) {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", data);
    path.setAttribute("fill", name === "overview" ? "currentColor" : "none");
    path.setAttribute("stroke", "currentColor");
    path.setAttribute("stroke-width", "1.5");
    path.setAttribute("stroke-linecap", "round");
    path.setAttribute("stroke-linejoin", "round");
    svg.append(path);
  }
  return svg;
}
