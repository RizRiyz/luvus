import type { JsonObject } from "./types.js";

type ReadyFrame = {
  type: "ready";
  ticket?: string;
  expires_at: number;
  authority: { mode: "read_only" | "control"; scopes: string[] };
};

type ServerFrame =
  | ReadyFrame
  | { type: "response"; id: string; result?: unknown; error?: JsonObject }
  | { type: "stream.frame"; stream_id: string; frame: JsonObject }
  | { type: "stream.closed"; stream_id: string; reason: string }
  | { type: "devices"; devices: JsonObject }
  | { type: "error"; code: string; message: string }
  | { type: "pong" };

export class BridgeError extends Error {
  constructor(
    message: string,
    readonly code = "bridge_error",
  ) {
    super(message);
  }
}

export interface StreamHandle {
  readonly id: string;
  action(action: string, params: JsonObject): Promise<unknown>;
  close(): void;
}

type Pending = {
  resolve: (value: unknown) => void;
  reject: (reason: unknown) => void;
  timer: ReturnType<typeof setTimeout>;
  streamId?: string;
};

type StreamState = {
  onFrame: (frame: JsonObject) => void;
  onClose: (reason: string) => void;
};

const ID_PATTERN = /^[A-Za-z0-9._:-]{1,128}$/;

export class BridgeClient extends EventTarget {
  #socket: WebSocket | undefined;
  #pending = new Map<string, Pending>();
  #streams = new Map<string, StreamState>();
  #counter = 0;
  #ready: ReadyFrame | undefined;
  #connectPromise: Promise<ReadyFrame> | undefined;

  constructor(
    readonly url: string,
    private readonly credential: () => { ticket?: string; code?: string },
    private readonly storeTicket: (ticket: string) => void,
  ) {
    super();
  }

  get ready(): ReadyFrame | undefined {
    return this.#ready;
  }

  connect(signal?: AbortSignal): Promise<ReadyFrame> {
    if (this.#ready && this.#socket?.readyState === WebSocket.OPEN) {
      return Promise.resolve(this.#ready);
    }
    if (this.#connectPromise) return this.#connectPromise;
    this.#connectPromise = this.#connect(signal).finally(() => {
      this.#connectPromise = undefined;
    });
    return this.#connectPromise;
  }

  async #connect(signal?: AbortSignal): Promise<ReadyFrame> {
    this.close("replaced");
    const socket = new WebSocket(this.url);
    this.#socket = socket;
    const ready = await new Promise<ReadyFrame>((resolve, reject) => {
      const timer = setTimeout(() => fail(new BridgeError("Bridge connection timed out", "timeout")), 8_000);
      const onAbort = () => fail(new DOMException("Connection aborted", "AbortError"));
      const cleanup = () => {
        clearTimeout(timer);
        signal?.removeEventListener("abort", onAbort);
      };
      const fail = (error: unknown) => {
        cleanup();
        socket.close();
        reject(error);
      };
      signal?.addEventListener("abort", onAbort, { once: true });
      socket.addEventListener("open", () => {
        socket.send(JSON.stringify({ type: "authenticate", ...this.credential() }));
      }, { once: true });
      socket.addEventListener("error", () => fail(new BridgeError("Could not reach the bridge", "unavailable")), { once: true });
      socket.addEventListener("close", (event) => fail(new BridgeError(event.reason || "Bridge closed", "closed")), { once: true });
      socket.addEventListener("message", (event) => {
        const frame = parseFrame(event.data);
        if (frame.type === "ready") {
          cleanup();
          resolve(frame);
        } else if (frame.type === "error") {
          fail(new BridgeError(frame.message, frame.code));
        } else {
          fail(new BridgeError("Unexpected bridge handshake", "invalid_frame"));
        }
      }, { once: true });
    });
    if (ready.ticket) this.storeTicket(ready.ticket);
    this.#ready = ready;
    socket.addEventListener("message", (event) => this.#onMessage(event.data));
    socket.addEventListener("close", (event) => this.#onClose(event.reason || "connection closed"));
    return ready;
  }

  request(method: string, params: JsonObject = {}, timeoutMs = 10_000): Promise<unknown> {
    const id = this.#nextId("req");
    return this.#sendPending(id, { type: "request", id, method, params }, timeoutMs);
  }

  async openStream(
    method: string,
    params: JsonObject,
    onFrame: (frame: JsonObject) => void,
    onClose: (reason: string) => void,
  ): Promise<StreamHandle> {
    const id = this.#nextId("stream");
    this.#streams.set(id, { onFrame, onClose });
    try {
      await this.#sendPending(id, { type: "stream.open", id, method, params }, 10_000);
    } catch (error) {
      this.#streams.delete(id);
      throw error;
    }
    return {
      id,
      action: (action, actionParams) => {
        if (!this.#streams.has(id)) {
          return Promise.reject(new BridgeError("Terminal stream is closed", "stale_stream"));
        }
        const actionId = this.#nextId("action");
        return this.#sendPending(actionId, {
          type: "stream.action",
          stream_id: id,
          id: actionId,
          action,
          params: actionParams,
        }, 10_000, id);
      },
      close: () => {
        this.#streams.delete(id);
        this.#rejectStream(id, new BridgeError("Terminal stream is closed", "stale_stream"));
        this.#send({ type: "stream.close", stream_id: id });
      },
    };
  }

  close(reason = "client closed"): void {
    const socket = this.#socket;
    this.#socket = undefined;
    this.#ready = undefined;
    if (socket && socket.readyState < WebSocket.CLOSING) socket.close(1000, reason.slice(0, 120));
    this.#rejectAll(new BridgeError(reason, "closed"));
  }

  #sendPending(id: string, frame: JsonObject, timeoutMs: number, streamId?: string): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new BridgeError("Bridge request timed out", "timeout"));
      }, timeoutMs);
      this.#pending.set(id, { resolve, reject, timer, ...(streamId ? { streamId } : {}) });
      try {
        this.#send(frame);
      } catch (error) {
        clearTimeout(timer);
        this.#pending.delete(id);
        reject(error);
      }
    });
  }

  #send(frame: JsonObject): void {
    if (!this.#socket || this.#socket.readyState !== WebSocket.OPEN || !this.#ready) {
      throw new BridgeError("Bridge is not connected", "disconnected");
    }
    this.#socket.send(JSON.stringify(frame));
  }

  #onMessage(raw: unknown): void {
    let frame: ServerFrame;
    try {
      frame = parseFrame(raw);
    } catch {
      this.close("invalid bridge frame");
      return;
    }
    if (frame.type === "response") {
      const pending = this.#pending.get(frame.id);
      if (!pending) return;
      clearTimeout(pending.timer);
      this.#pending.delete(frame.id);
      if (frame.error) {
        pending.reject(new BridgeError(String(frame.error.message ?? "Request failed"), String(frame.error.code ?? "request_failed")));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (frame.type === "stream.frame") {
      this.#streams.get(frame.stream_id)?.onFrame(frame.frame);
      return;
    }
    if (frame.type === "stream.closed") {
      const stream = this.#streams.get(frame.stream_id);
      this.#streams.delete(frame.stream_id);
      this.#rejectStream(frame.stream_id, new BridgeError(frame.reason || "Terminal stream is closed", "stale_stream"));
      stream?.onClose(frame.reason);
      return;
    }
    if (frame.type === "devices") {
      this.dispatchEvent(new CustomEvent("devices", { detail: frame.devices }));
      return;
    }
    if (frame.type === "error") {
      this.dispatchEvent(new CustomEvent("protocol-error", { detail: frame }));
    }
  }

  #onClose(reason: string): void {
    if (!this.#socket) return;
    this.#socket = undefined;
    this.#ready = undefined;
    this.#rejectAll(new BridgeError(reason, "closed"));
    this.dispatchEvent(new CustomEvent("disconnected", { detail: reason }));
  }

  #rejectAll(error: Error): void {
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
    for (const stream of this.#streams.values()) stream.onClose(error.message);
    this.#streams.clear();
  }

  #rejectStream(streamId: string, error: Error): void {
    for (const [id, pending] of this.#pending) {
      if (pending.streamId !== streamId) continue;
      clearTimeout(pending.timer);
      pending.reject(error);
      this.#pending.delete(id);
    }
  }

  #nextId(prefix: string): string {
    const id = `${prefix}-${Date.now().toString(36)}-${(++this.#counter).toString(36)}`;
    if (!ID_PATTERN.test(id)) throw new BridgeError("Could not create request id");
    return id;
  }
}

function parseFrame(raw: unknown): ServerFrame {
  if (typeof raw !== "string") throw new BridgeError("Binary bridge frame rejected", "invalid_frame");
  const parsed: unknown = JSON.parse(raw);
  if (!parsed || typeof parsed !== "object" || typeof (parsed as { type?: unknown }).type !== "string") {
    throw new BridgeError("Invalid bridge frame", "invalid_frame");
  }
  return parsed as ServerFrame;
}
