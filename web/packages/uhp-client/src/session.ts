import { BridgeClient, BridgeError, type StreamHandle } from "./client.js";
import type { Capabilities, ConnectionState, JsonObject, SessionSnapshot, UhpEvent } from "./types.js";

const STRUCTURAL_PREFIXES = ["workspace.", "tab.", "pane.", "agent.", "task.", "automation.", "orch."];

export class LiveSession extends EventTarget {
  #state: ConnectionState = "disconnected";
  #snapshot: SessionSnapshot | undefined;
  #capabilities: Capabilities | undefined;
  #events: StreamHandle | undefined;
  #lastSequence = 0;
  #buffer: UhpEvent[] = [];
  #syncing = false;
  #reconnecting = false;
  #stopped = true;
  #retry = 0;
  #refreshTimer: ReturnType<typeof setTimeout> | undefined;

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
    this.#events?.close();
    this.#events = undefined;
    this.bridge.close();
    this.#setState("disconnected");
  }

  async refresh(): Promise<void> {
    if (this.#syncing) return;
    await this.#takeSnapshot();
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
    const snapshot = asSnapshot(await this.bridge.request("session.snapshot"));
    if (this.#capabilities && snapshot.server_generation !== this.#capabilities.server_generation) {
      throw new BridgeError("Server generation changed during synchronization", "stale_server");
    }
    this.#snapshot = snapshot;
    this.#lastSequence = snapshot.event_sequence;
    const buffered = this.#buffer;
    this.#buffer = [];
    for (const event of buffered) {
      if (event.sequence > snapshot.event_sequence) this.#applyEvent(event);
    }
    this.dispatchEvent(new CustomEvent("snapshot", { detail: snapshot }));
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
    if (this.#syncing || !this.#snapshot) {
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
    if (STRUCTURAL_PREFIXES.some((prefix) => event.event.startsWith(prefix))) {
      if (this.#refreshTimer) clearTimeout(this.#refreshTimer);
      this.#refreshTimer = setTimeout(() => {
        this.#refreshTimer = undefined;
        void this.refresh().catch(() => this.#reconnect(true));
      }, 60);
    }
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
        } catch {
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

function asEvent(value: JsonObject): UhpEvent | undefined {
  return typeof value.event === "string" && Number.isSafeInteger(value.sequence) && value.sequence as number >= 0
    ? value as unknown as UhpEvent
    : undefined;
}
