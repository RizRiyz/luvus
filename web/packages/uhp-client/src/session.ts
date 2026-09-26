import { BridgeClient, BridgeError, type StreamHandle } from "./client.js";
import type { Capabilities, ConnectionState, JsonObject, SessionSnapshot, UhpEvent } from "./types.js";

const STRUCTURAL_PREFIXES = ["workspace.", "tab.", "pane.", "agent.", "task.", "automation.", "orch."];
const SESSION_SWITCH_TIMEOUT_MS = 120_000;
const GATEWAY_RATE_WINDOW_MS = 60_000;

export class LiveSession extends EventTarget {
  #state: ConnectionState = "disconnected";
  #snapshot: SessionSnapshot | undefined;
  #capabilities: Capabilities | undefined;
  #events: StreamHandle | undefined;
  #lastSequence = 0;
  #buffer: UhpEvent[] = [];
  #syncing = false;
  #refreshing = false;
  #snapshotPromise: Promise<void> | undefined;
  #reconnecting = false;
  #stopped = true;
  #retry = 0;
  #refreshTimer: ReturnType<typeof setTimeout> | undefined;
  #titleRenderTimer: ReturnType<typeof setTimeout> | undefined;
  #refreshRetryAfter = 0;

  constructor(readonly bridge: BridgeClient) {
    super();
    bridge.addEventListener("disconnected", () => {
      if (!this.#stopped) void this.#reconnect();
    });
  }

  get state(): ConnectionState { return this.#state; }
  get snapshot(): SessionSnapshot | undefined { return this.#snapshot; }
  get capabilities(): Capabilities | undefined { return this.#capabilities; }
  get allowedMethods(): ReadonlySet<string> {
    return new Set(this.#capabilities?.access?.allowed_methods ?? this.#capabilities?.methods ?? []);
  }

  async start(): Promise<void> {
    this.#stopped = false;
    this.#retry = 0;
    try {
      await this.#synchronize("connecting");
    } catch (error) {
      if (this.#state !== "expired") void this.#reconnect();
      throw error;
    }
  }

  stop(): void {
    this.#stopped = true;
    if (this.#refreshTimer) clearTimeout(this.#refreshTimer);
    this.#refreshTimer = undefined;
    if (this.#titleRenderTimer) clearTimeout(this.#titleRenderTimer);
    this.#titleRenderTimer = undefined;
    this.#refreshRetryAfter = 0;
    this.#events?.close();
    this.#events = undefined;
    this.bridge.close();
    this.#setState("disconnected");
  }

  async refresh(): Promise<void> {
    if (this.#syncing) return;
    if (Date.now() < this.#refreshRetryAfter) return;
    try {
      await this.#takeSnapshot();
    } catch (error) {
      if (error instanceof BridgeError && error.code === "rate_limited") {
        this.#refreshRetryAfter = Date.now() + GATEWAY_RATE_WINDOW_MS;
        this.#scheduleRefresh();
      }
      throw error;
    }
  }

  async switchSession(name: string): Promise<void> {
    if (this.#syncing || this.#reconnecting || this.#state !== "ready") {
      throw new BridgeError("The live session is busy", "busy");
    }
    if (this.#snapshotPromise) await this.#snapshotPromise.catch(() => {});
    this.#stopped = true;
    if (this.#refreshTimer) clearTimeout(this.#refreshTimer);
    this.#refreshTimer = undefined;
    if (this.#titleRenderTimer) clearTimeout(this.#titleRenderTimer);
    this.#titleRenderTimer = undefined;
    this.#refreshRetryAfter = 0;
    this.#events?.close();
    this.#events = undefined;
    this.#snapshot = undefined;
    this.#capabilities = undefined;
    this.#lastSequence = 0;
    this.#buffer = [];
    this.#setState("synchronizing");
    let switchError: unknown;
    try {
      asSessionSwitch(await this.bridge.request("web.sessions.switch", { name }, SESSION_SWITCH_TIMEOUT_MS), name);
    } catch (error) {
      switchError = error;
    }
    this.bridge.close("session switched");
    this.#stopped = false;
    this.#retry = 0;
    try {
      await this.#synchronize("connecting");
    } catch (error) {
      void this.#reconnect();
      if (!switchError) throw error;
    }
    if (switchError) throw switchError;
  }

  async #synchronize(initial: ConnectionState): Promise<void> {
    if (this.#syncing || this.#stopped) return;
    this.#syncing = true;
    this.#setState(initial);
    try {
      await this.bridge.connect();
      this.#setState("authenticating");
      const capabilities = asCapabilities(await this.bridge.request("uhp.capabilities"));
      const priorGeneration = this.#capabilities?.server_generation ?? this.#snapshot?.server_generation;
      if (
        (priorGeneration !== undefined && capabilities.server_generation !== priorGeneration)
        || this.#lastSequence > capabilities.event_sequence
      ) {
        this.#lastSequence = 0;
        this.#snapshot = undefined;
      }
      this.#capabilities = capabilities;
      this.#setState("synchronizing");
      this.#buffer = [];
      this.#events?.close();
      this.#events = await this.bridge.openStream(
        "events.subscribe",
        this.#lastSequence > 0 ? { after_sequence: this.#lastSequence } : {},
        (frame) => this.#onEvent(frame),
        () => { if (!this.#stopped) void this.#reconnect(); },
      );
      await this.#takeSnapshot();
      this.#retry = 0;
      this.#setState("ready");
    } catch (error) {
      if (error instanceof BridgeError && (error.code === "forbidden" || error.code === "expired")) {
        this.#setState("expired");
        throw error;
      }
      throw error;
    } finally {
      this.#syncing = false;
    }
  }

  async #takeSnapshot(): Promise<void> {
    if (this.#snapshotPromise) return this.#snapshotPromise;
    const pending = this.#loadSnapshot();
    this.#snapshotPromise = pending;
    try {
      await pending;
    } finally {
      this.#snapshotPromise = undefined;
    }
  }

  async #loadSnapshot(): Promise<void> {
    this.#refreshing = true;
    try {
      const snapshot = asSnapshot(await this.bridge.request("session.snapshot"));
      if (this.#capabilities && snapshot.server_generation !== this.#capabilities.server_generation) {
        throw new BridgeError("Server generation changed during synchronization", "stale_server");
      }
      this.#snapshot = snapshot;
      this.#refreshRetryAfter = 0;
      if (this.#titleRenderTimer) clearTimeout(this.#titleRenderTimer);
      this.#titleRenderTimer = undefined;
      this.#lastSequence = snapshot.event_sequence;
      const buffered = this.#buffer;
      this.#buffer = [];
      for (const event of buffered) {
        if (event.sequence > snapshot.event_sequence) this.#applyEvent(event);
      }
      this.dispatchEvent(new CustomEvent("snapshot", { detail: snapshot }));
    } catch (error) {
      if (this.#snapshot && !this.#syncing) {
        const buffered = this.#buffer;
        this.#buffer = [];
        for (const event of buffered) this.#applyEvent(event);
      }
      throw error;
    } finally {
      this.#refreshing = false;
    }
  }

  #onEvent(raw: JsonObject): void {
    const event = asEvent(raw);
    if (!event) return;
    if (event.event === "events.resync_required") {
      this.#events?.close();
      this.#events = undefined;
      void this.#reconnect(true);
      return;
    }
    if (this.#syncing || this.#refreshing || !this.#snapshot) {
      this.#buffer.push(event);
      return;
    }
    this.#applyEvent(event);
  }

  #applyEvent(event: UhpEvent): void {
    if (event.sequence <= this.#lastSequence) return;
    if (this.#lastSequence > 0 && event.sequence !== this.#lastSequence + 1) {
      void this.#reconnect(true);
      return;
    }
    this.#lastSequence = event.sequence;
    this.dispatchEvent(new CustomEvent("event", { detail: event }));
    if (event.event === "agent.title_changed" && this.#applyAgentTitle(event)) return;
    if (STRUCTURAL_PREFIXES.some((prefix) => event.event.startsWith(prefix))) {
      this.#scheduleRefresh();
    }
  }

  #applyAgentTitle(event: UhpEvent): boolean {
    const data = event.data;
    if (!data || typeof data !== "object" || Array.isArray(data)) return false;
    const paneId = data.pane;
    const title = data.title;
    if (typeof paneId !== "string" || !(typeof title === "string" || title === null)) return false;
    for (const workspace of this.#snapshot?.workspaces ?? []) {
      for (const tab of workspace.tabs) {
        const pane = tab.panes.find((candidate) => candidate.pane_id === paneId);
        if (!pane) continue;
        pane.agent_session_title = title;
        if (!this.#titleRenderTimer) {
          this.#titleRenderTimer = setTimeout(() => {
            this.#titleRenderTimer = undefined;
            if (!this.#stopped) this.dispatchEvent(new CustomEvent("snapshot", { detail: this.#snapshot }));
          }, 60);
        }
        return true;
      }
    }
    return false;
  }

  #scheduleRefresh(): void {
    if (this.#stopped) return;
    if (this.#refreshTimer) clearTimeout(this.#refreshTimer);
    this.#refreshTimer = setTimeout(() => {
      this.#refreshTimer = undefined;
      void this.refresh().catch((error) => {
        if (!(error instanceof BridgeError && error.code === "rate_limited")) {
          void this.#reconnect(true);
        }
      });
    }, Math.max(60, this.#refreshRetryAfter - Date.now()));
  }

  async #reconnect(immediate = false): Promise<void> {
    if (this.#stopped || this.#syncing || this.#reconnecting) return;
    this.#reconnecting = true;
    let skipDelay = immediate;
    try {
      while (!this.#stopped && this.#state !== "expired") {
        this.#events?.close();
        this.#events = undefined;
        this.bridge.close("reconnecting");
        this.#setState("reconnecting");
        const delay = skipDelay ? 0 : Math.min(10_000, 250 * 2 ** Math.min(this.#retry++, 5));
        skipDelay = false;
        if (delay) await new Promise((resolve) => setTimeout(resolve, delay + Math.random() * 150));
        if (this.#stopped) return;
        try {
          await this.#synchronize("reconnecting");
          return;
        } catch (error) {
          if (error instanceof BridgeError && error.code === "rate_limited") {
            await new Promise((resolve) => setTimeout(resolve, GATEWAY_RATE_WINDOW_MS));
          }
          // The next bounded iteration retries unless authority expired.
        }
      }
    } finally {
      this.#reconnecting = false;
    }
  }

  #setState(state: ConnectionState): void {
    if (state === this.#state) return;
    this.#state = state;
    this.dispatchEvent(new CustomEvent("state", { detail: state }));
  }
}

function asCapabilities(value: unknown): Capabilities {
  if (!value || typeof value !== "object" || (value as { type?: unknown }).type !== "uhp_capabilities") {
    throw new BridgeError("Invalid capabilities response", "invalid_response");
  }
  return value as Capabilities;
}

function asSnapshot(value: unknown): SessionSnapshot {
  if (!value || typeof value !== "object" || (value as { type?: unknown }).type !== "session_snapshot") {
    throw new BridgeError("Invalid snapshot response", "invalid_response");
  }
  return value as SessionSnapshot;
}

function asSessionSwitch(value: unknown, expected: string): void {
  const response = value as { type?: unknown; session?: { name?: unknown } } | undefined;
  if (response?.type !== "browser_session_switch" || response.session?.name !== expected) {
    throw new BridgeError("Invalid session switch response", "invalid_response");
  }
}

function asEvent(value: JsonObject): UhpEvent | undefined {
  return typeof value.event === "string" && Number.isSafeInteger(value.sequence) && value.sequence as number >= 0
    ? value as unknown as UhpEvent
    : undefined;
}
