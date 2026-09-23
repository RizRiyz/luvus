import { createReadStream, existsSync } from "node:fs";
import { stat } from "node:fs/promises";
import http, { type IncomingMessage, type ServerResponse } from "node:http";
import path from "node:path";
import { WebSocketServer, WebSocket, type RawData } from "ws";
import type { BridgeConfig } from "./config.js";
import { normalizePublicUrl, originAllowed } from "./config.js";
import { BrowserAuthority } from "./auth.js";
import { UhpAccess, type UpstreamStream } from "./uhp.js";

const MAX_CLIENTS = 8;
const MAX_PAYLOAD = 256 * 1024;
const MAX_PENDING = 16;
const MAX_STREAMS = 3;
const MAX_BUFFERED_OUTBOUND = 1024 * 1024;
const REQUESTS_PER_MINUTE = 90;
const TERMINAL_ACTIONS_PER_MINUTE = 3_600;
const UPLOAD_CHUNKS_PER_MINUTE = 256;
const UPLOAD_ENCODED_BYTES_PER_MINUTE = 48 * 1024 * 1024;
const ID_PATTERN = /^[A-Za-z0-9._:-]{1,128}$/;
const METHOD_PATTERN = /^[a-z][a-z0-9_.]{0,127}$/;
const TERMINAL_ACTIONS = new Set([
  "type_literal", "paste_text", "paste_image", "submit_text", "send_key",
  "upload_start", "upload_chunk", "upload_finish", "upload_cancel",
]);

type ClientState = {
  authenticated: boolean;
  pending: number;
  requests: number;
  terminalActions: number;
  uploadChunks: number;
  uploadEncodedBytes: number;
  windowStarted: number;
  streams: Map<string, UpstreamStream>;
  terminalFrames: Map<string, object>;
  terminalFlush: ReturnType<typeof setTimeout> | undefined;
  expiryTimer?: ReturnType<typeof setTimeout>;
};

export class BridgeServer {
  readonly authority: BrowserAuthority;
  #http: http.Server | undefined;
  #wss: WebSocketServer | undefined;
  #clients = new Map<WebSocket, ClientState>();
  #sessionSwitching = false;
  #publicUrl: string | undefined;

  constructor(private readonly config: BridgeConfig, private readonly uhp: UhpAccess) {
    this.authority = new BrowserAuthority(config.browserTicketSeconds, config.browserMaxDevices);
    this.#publicUrl = config.publicUrl;
  }

  get port(): number {
    const address = this.#http?.address();
    if (!address || typeof address === "string") throw new Error("Bridge is not listening");
    return address.port;
  }

  async start(): Promise<void> {
    if (!existsSync(this.config.appDir)) throw new Error(`Web app build not found: ${this.config.appDir}`);
    const server = http.createServer((request, response) => void this.#serve(request, response));
    const wss = new WebSocketServer({ noServer: true, maxPayload: MAX_PAYLOAD, perMessageDeflate: false });
    this.#http = server;
    this.#wss = wss;
    server.on("upgrade", (request, socket, head) => {
      if (request.url !== "/bridge" || !originAllowed(request.headers.origin, request.headers.host, this.config.origins)) {
        socket.write("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
        socket.destroy();
        return;
      }
      if (wss.clients.size >= MAX_CLIENTS) {
        socket.write("HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n");
        socket.destroy();
        return;
      }
      wss.handleUpgrade(request, socket, head, (websocket) => wss.emit("connection", websocket, request));
    });
    wss.on("connection", (socket) => this.#client(socket));
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(this.config.port, this.config.host, () => {
        server.off("error", reject);
        resolve();
      });
    });
  }

  stop(): void {
    this.authority.revokeAll();
    for (const socket of this.#wss?.clients ?? []) socket.close(1001, "bridge stopping");
    this.#wss?.close();
    this.#http?.close();
  }

  #client(socket: WebSocket): void {
    const state: ClientState = {
      authenticated: false,
      pending: 0,
      requests: 0,
      terminalActions: 0,
      uploadChunks: 0,
      uploadEncodedBytes: 0,
      windowStarted: Date.now(),
      streams: new Map(),
      terminalFrames: new Map(),
      terminalFlush: undefined,
    };
    this.#clients.set(socket, state);
    const authTimer = setTimeout(() => socket.close(1008, "authentication timeout"), 5_000);
    socket.on("message", (raw, binary) => {
      if (binary) return socket.close(1003, "binary messages are not supported");
      void this.#message(socket, state, raw).catch(() => socket.close(1011, "bridge failure"));
    });
    socket.on("close", () => {
      this.#clients.delete(socket);
      clearTimeout(authTimer);
      if (state.expiryTimer) clearTimeout(state.expiryTimer);
      if (state.terminalFlush) clearTimeout(state.terminalFlush);
      for (const stream of state.streams.values()) stream.close();
      state.streams.clear();
    });
    socket.once("message", () => clearTimeout(authTimer));
  }

  async #message(socket: WebSocket, state: ClientState, raw: RawData): Promise<void> {
    const frame = parseClientFrame(raw);
    if (!state.authenticated) {
      if (frame.type !== "authenticate") return socket.close(1008, "authentication required");
      const result = this.authority.authenticate({
        ...(typeof frame.code === "string" ? { code: frame.code } : {}),
        ...(typeof frame.ticket === "string" ? { ticket: frame.ticket } : {}),
      });
      if (!result.accepted || !result.expiresAt) {
        send(socket, { type: "error", code: "forbidden", message: "Pairing or ticket was rejected" });
        return socket.close(1008, "authentication rejected");
      }
      state.authenticated = true;
      const expiresInMs = Math.max(1, result.expiresAt * 1000 - Date.now());
      state.expiryTimer = setTimeout(() => {
        state.authenticated = false;
        socket.close(1008, "browser ticket expired");
        this.#broadcastDevices(socket);
      }, expiresInMs);
      send(socket, {
        type: "ready",
        ...(result.ticket ? { ticket: result.ticket } : {}),
        expires_at: result.expiresAt,
        authority: this.uhp.authority,
      });
      this.#broadcastDevices(socket);
      return;
    }
    if (!rateAllowed(state, frame)) {
      const id = typeof frame.id === "string" && ID_PATTERN.test(frame.id) ? frame.id : undefined;
      send(socket, id
        ? { type: "response", id, error: { code: "rate_limited", message: "Browser request rate exceeded" } }
        : { type: "error", code: "rate_limited", message: "Browser request rate exceeded" });
      return;
    }
    switch (frame.type) {
      case "request":
        await this.#request(socket, state, frame);
        return;
      case "stream.open":
        await this.#openStream(socket, state, frame);
        return;
      case "stream.action":
        this.#streamAction(socket, state, frame);
        return;
      case "stream.close": {
        const streamId = requiredString(frame, "stream_id", ID_PATTERN);
        state.streams.get(streamId)?.close();
        state.streams.delete(streamId);
        state.terminalFrames.delete(streamId);
        return;
      }
      case "ping":
        send(socket, { type: "pong" });
        return;
      default:
        send(socket, { type: "error", code: "invalid_request", message: "Unknown bridge message" });
    }
  }

  async #request(socket: WebSocket, state: ClientState, frame: Record<string, unknown>): Promise<void> {
    const id = requiredString(frame, "id", ID_PATTERN);
    const method = requiredString(frame, "method", METHOD_PATTERN);
    const params = objectField(frame, "params");
    if (method.startsWith("web.devices.")) {
      return this.#deviceRequest(socket, id, method, params);
    }
    if (method.startsWith("web.sessions.")) {
      if (state.pending >= MAX_PENDING) {
        return send(socket, { type: "response", id, error: { code: "limit_exceeded", message: "Too many pending browser requests" } });
      }
      state.pending += 1;
      try {
        await this.#sessionRequest(socket, id, method, params);
      } finally {
        state.pending -= 1;
      }
      return;
    }
    if (!this.uhp.methodAllowed(method) || isStreaming(method)) {
      return send(socket, { type: "response", id, error: { code: "forbidden", message: "Method is not available through this bridge path" } });
    }
    if (state.pending >= MAX_PENDING) {
      return send(socket, { type: "response", id, error: { code: "limit_exceeded", message: "Too many pending browser requests" } });
    }
    state.pending += 1;
    try {
      const result = method === "uhp.capabilities" ? this.uhp.capabilities : await this.uhp.request(method, params, id);
      send(socket, { type: "response", id, result });
    } catch (error) {
      if (recoverable(error)) void this.#recoverUpstream();
      send(socket, { type: "response", id, error: publicError(error) });
    } finally {
      state.pending -= 1;
    }
  }

  #deviceRequest(socket: WebSocket, id: string, method: string, params: Record<string, unknown>): void {
    if (method === "web.devices.status") {
      return send(socket, { type: "response", id, result: deviceStatus(this.authority.status(), this.#publicUrl) });
    }
    if (method === "web.devices.create_pairing") {
      const pairing = this.authority.createPairing();
      if (!pairing) {
        return send(socket, { type: "response", id, error: { code: "device_limit", message: "Device limit reached or another pairing link is still pending" } });
      }
      const base = this.#publicUrl;
      send(socket, {
        type: "response",
        id,
        result: {
          type: "browser_device_pairing",
          code: pairing.code,
          expires_at: pairing.expiresAt,
          ...(base ? { url: `${base}/#pair=${encodeURIComponent(pairing.code)}` } : {}),
          devices: deviceStatus(this.authority.status(), this.#publicUrl),
        },
      });
      this.#broadcastDevices();
      return;
    }
    if (method === "web.devices.set_limit") {
      const limit = params.limit;
      if (!Number.isSafeInteger(limit) || (limit as number) < 1 || (limit as number) > MAX_CLIENTS) {
        return send(socket, { type: "response", id, error: { code: "invalid_params", message: `Device limit must be from 1 through ${MAX_CLIENTS}` } });
      }
      if (!this.authority.setMaxDevices(limit as number)) {
        return send(socket, { type: "response", id, error: { code: "device_limit", message: "Device limit cannot be lower than paired devices and pending links" } });
      }
      send(socket, { type: "response", id, result: deviceStatus(this.authority.status(), this.#publicUrl) });
      this.#broadcastDevices();
      return;
    }
    if (method === "web.devices.set_public_url") {
      if (Object.keys(params).length !== 1 || !("url" in params)
        || (params.url !== null && typeof params.url !== "string")) {
        return send(socket, { type: "response", id, error: { code: "invalid_params", message: "Public URL must be an HTTP(S) origin without a path" } });
      }
      try {
        this.#publicUrl = params.url === null ? undefined : normalizePublicUrl(params.url);
      } catch {
        return send(socket, { type: "response", id, error: { code: "invalid_params", message: "Public URL must be an HTTP(S) origin without a path" } });
      }
      send(socket, { type: "response", id, result: deviceStatus(this.authority.status(), this.#publicUrl) });
      this.#broadcastDevices();
      return;
    }
    send(socket, { type: "response", id, error: { code: "method_not_found", message: "Unknown web device method" } });
  }

  async #sessionRequest(socket: WebSocket, id: string, method: string, params: Record<string, unknown>): Promise<void> {
    if (method === "web.sessions.list") {
      if (Object.keys(params).length) {
        return send(socket, { type: "response", id, error: { code: "invalid_params", message: "Session listing takes no parameters" } });
      }
      try {
        const sessions = await this.uhp.sessions();
        return send(socket, { type: "response", id, result: { type: "browser_session_list", sessions } });
      } catch (error) {
        return send(socket, { type: "response", id, error: publicError(error) });
      }
    }
    if (method === "web.sessions.switch") {
      if (Object.keys(params).some((key) => key !== "name") || typeof params.name !== "string") {
        return send(socket, { type: "response", id, error: { code: "invalid_params", message: "A valid session name is required" } });
      }
      if (this.#sessionSwitching) {
        return send(socket, { type: "response", id, error: { code: "busy", message: "Another session switch is in progress" } });
      }
      this.#sessionSwitching = true;
      try {
        const session = await this.uhp.switchSession(params.name, this.config.control);
        return send(socket, { type: "response", id, result: { type: "browser_session_switch", session } });
      } catch (error) {
        return send(socket, { type: "response", id, error: publicError(error) });
      } finally {
        this.#sessionSwitching = false;
      }
    }
    send(socket, { type: "response", id, error: { code: "method_not_found", message: "Unknown web session method" } });
  }

  #broadcastDevices(except?: WebSocket): void {
    const devices = deviceStatus(this.authority.status(), this.#publicUrl);
    for (const [client, state] of this.#clients) {
      if (client !== except && state.authenticated) send(client, { type: "devices", devices });
    }
  }

  async #openStream(socket: WebSocket, state: ClientState, frame: Record<string, unknown>): Promise<void> {
    const id = requiredString(frame, "id", ID_PATTERN);
    const method = requiredString(frame, "method", METHOD_PATTERN);
    const params = objectField(frame, "params");
    if (!this.uhp.methodAllowed(method) || !isStreaming(method)) {
      return send(socket, { type: "response", id, error: { code: "forbidden", message: "Stream method is not allowed" } });
    }
    if (state.streams.size >= MAX_STREAMS || state.streams.has(id)) {
      return send(socket, { type: "response", id, error: { code: "limit_exceeded", message: "Browser stream capacity is full" } });
    }
    let acknowledged = false;
    try {
      const stream = await this.uhp.stream(method, params, id, (upstream) => {
        if (!acknowledged && upstream.id === id) {
          acknowledged = true;
          if (upstream.error) {
            send(socket, { type: "response", id, error: upstream.error });
            state.streams.get(id)?.close();
            state.streams.delete(id);
            if (recoverable(upstream.error)) void this.#recoverUpstream();
          } else {
            send(socket, { type: "response", id, result: upstream.result });
          }
          return;
        }
        if (typeof upstream.id === "string") {
          send(socket, {
            type: "response",
            id: upstream.id,
            ...(upstream.error ? { error: upstream.error } : { result: upstream.result }),
          });
          return;
        }
        const browserFrame = { type: "stream.frame", stream_id: id, frame: upstream };
        if (upstream.event === "terminal.frame") this.#queueTerminalFrame(socket, state, id, browserFrame);
        else send(socket, browserFrame);
      }, (reason) => {
        state.streams.delete(id);
        state.terminalFrames.delete(id);
        send(socket, { type: "stream.closed", stream_id: id, reason });
      });
      state.streams.set(id, stream);
    } catch (error) {
      if (recoverable(error)) void this.#recoverUpstream();
      send(socket, { type: "response", id, error: publicError(error) });
    }
  }

  async #recoverUpstream(): Promise<void> {
    try {
      await this.uhp.restart();
    } catch {
      // A later request or child-exit notification retries without polling.
    }
  }

  #streamAction(socket: WebSocket, state: ClientState, frame: Record<string, unknown>): void {
    const streamId = requiredString(frame, "stream_id", ID_PATTERN);
    const id = requiredString(frame, "id", ID_PATTERN);
    const action = requiredString(frame, "action", METHOD_PATTERN);
    const params = objectField(frame, "params");
    const stream = state.streams.get(streamId);
    if (!stream) return send(socket, { type: "response", id, error: { code: "stale_stream", message: "Terminal stream is closed" } });
    if (!terminalAction(action)) {
      return send(socket, { type: "response", id, error: { code: "invalid_params", message: "Unknown terminal action" } });
    }
    if (!stream.write({ id, action, params })) {
      send(socket, { type: "response", id, error: { code: "send_failed", message: "Terminal stream could not accept input" } });
    }
  }

  #queueTerminalFrame(socket: WebSocket, state: ClientState, streamId: string, frame: object): void {
    state.terminalFrames.set(streamId, frame);
    if (state.terminalFlush) return;
    const flush = () => {
      state.terminalFlush = undefined;
      if (socket.readyState !== WebSocket.OPEN) {
        state.terminalFrames.clear();
        return;
      }
      if (socket.bufferedAmount > MAX_BUFFERED_OUTBOUND / 2) {
        state.terminalFlush = setTimeout(flush, 16);
        return;
      }
      const frames = [...state.terminalFrames.values()];
      state.terminalFrames.clear();
      for (const pending of frames) send(socket, pending);
    };
    state.terminalFlush = setTimeout(flush, 0);
  }

  async #serve(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (request.method !== "GET" && request.method !== "HEAD") return reply(response, 405, "Method not allowed");
    let url: URL;
    try {
      url = new URL(request.url ?? "/", `http://${request.headers.host ?? "localhost"}`);
    } catch {
      return reply(response, 400, "Bad request");
    }
    if (url.pathname === "/healthz") {
      response.setHeader("content-type", "application/json");
      return reply(response, 200, JSON.stringify({ status: "ok" }));
    }
    let requested: string;
    try {
      requested = url.pathname === "/" ? "index.html" : decodeURIComponent(url.pathname.slice(1));
    } catch {
      return reply(response, 400, "Bad request");
    }
    const normalized = path.normalize(requested);
    if (path.isAbsolute(normalized) || normalized.startsWith("..") || normalized.includes(`..${path.sep}`)) {
      return reply(response, 404, "Not found");
    }
    let file = path.join(this.config.appDir, normalized);
    try {
      if (!(await stat(file)).isFile()) throw new Error("not a file");
    } catch {
      file = path.join(this.config.appDir, "index.html");
    }
    setSecurityHeaders(response);
    response.setHeader("content-type", contentType(file));
    response.setHeader("cache-control", normalized.startsWith(`assets${path.sep}`)
      ? "public, max-age=31536000, immutable"
      : "no-cache");
    response.statusCode = 200;
    if (request.method === "HEAD") {
      response.end();
      return;
    }
    createReadStream(file).on("error", () => reply(response, 500, "Read failed")).pipe(response);
  }
}

function parseClientFrame(raw: RawData): Record<string, unknown> {
  const text = Buffer.isBuffer(raw) ? raw.toString("utf8") : Buffer.concat(raw as Buffer[]).toString("utf8");
  const value: unknown = JSON.parse(text);
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid bridge message");
  return value as Record<string, unknown>;
}

function send(socket: WebSocket, frame: object): void {
  if (socket.readyState !== WebSocket.OPEN) return;
  if (socket.bufferedAmount > MAX_BUFFERED_OUTBOUND) {
    socket.close(1013, "outbound backpressure");
    return;
  }
  socket.send(JSON.stringify(frame));
}

function requiredString(frame: Record<string, unknown>, key: string, pattern: RegExp): string {
  const value = frame[key];
  if (typeof value !== "string" || !pattern.test(value)) throw new Error(`invalid ${key}`);
  return value;
}

function objectField(frame: Record<string, unknown>, key: string): Record<string, unknown> {
  const value = frame[key];
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`invalid ${key}`);
  return value as Record<string, unknown>;
}

function isStreaming(method: string): boolean {
  return new Set(["events.subscribe", "terminal.backend.events.subscribe", "terminal.backend.observe", "terminal.backend.control"]).has(method);
}

function rateAllowed(state: ClientState, frame: Record<string, unknown>): boolean {
  resetRateWindow(state);
  if (frame.type === "stream.action" && frame.action === "upload_chunk") {
    return uploadRateAllowed(state, frame);
  }
  if (frame.type === "stream.action" && typeof frame.action === "string" && terminalAction(frame.action)) {
    state.terminalActions += 1;
    return state.terminalActions <= TERMINAL_ACTIONS_PER_MINUTE;
  }
  state.requests += 1;
  return state.requests <= REQUESTS_PER_MINUTE;
}

function uploadRateAllowed(state: ClientState, frame: Record<string, unknown>): boolean {
  resetRateWindow(state);
  const params = frame.params;
  const encoded = params && typeof params === "object" && !Array.isArray(params)
    ? (params as Record<string, unknown>).data_base64
    : undefined;
  if (typeof encoded !== "string" || encoded.length > 218_456) return false;
  state.uploadChunks += 1;
  state.uploadEncodedBytes += encoded.length;
  return state.uploadChunks <= UPLOAD_CHUNKS_PER_MINUTE
    && state.uploadEncodedBytes <= UPLOAD_ENCODED_BYTES_PER_MINUTE;
}

function resetRateWindow(state: ClientState): void {
  const now = Date.now();
  if (now - state.windowStarted < 60_000) return;
  state.windowStarted = now;
  state.requests = 0;
  state.terminalActions = 0;
  state.uploadChunks = 0;
  state.uploadEncodedBytes = 0;
}

function terminalAction(action: string): boolean {
  return TERMINAL_ACTIONS.has(action);
}

function publicError(error: unknown): { code: string; message: string } {
  if (error && typeof error === "object") {
    const source = error as { code?: unknown; message?: unknown };
    return {
      code: typeof source.code === "string" ? source.code : "unavailable",
      message: typeof source.message === "string" ? source.message.slice(0, 512) : "Upstream request failed",
    };
  }
  return { code: "unavailable", message: "Upstream request failed" };
}

function recoverable(error: unknown): boolean {
  if (!error || typeof error !== "object") return true;
  const code = (error as { code?: unknown }).code;
  return code === undefined || code === "forbidden" || code === "unavailable" || code === "stale_server";
}

function deviceStatus(status: { pairedDevices: number; pendingPairings: number; maxDevices: number }, publicUrl?: string): object {
  return {
    type: "browser_device_status",
    paired_devices: status.pairedDevices,
    pending_pairings: status.pendingPairings,
    max_devices: status.maxDevices,
    public_url: publicUrl ?? null,
  };
}

function setSecurityHeaders(response: ServerResponse): void {
  response.setHeader("content-security-policy", "default-src 'self'; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'");
  response.setHeader("referrer-policy", "no-referrer");
  response.setHeader("x-content-type-options", "nosniff");
  response.setHeader("x-frame-options", "DENY");
  response.setHeader("permissions-policy", "camera=(), microphone=(), geolocation=()");
  response.setHeader("cross-origin-opener-policy", "same-origin");
}

function contentType(file: string): string {
  const extension = path.extname(file);
  return ({
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json; charset=utf-8",
    ".webmanifest": "application/manifest+json",
    ".svg": "image/svg+xml",
  } as Record<string, string>)[extension] ?? "application/octet-stream";
}

function reply(response: ServerResponse, status: number, body: string): void {
  if (response.headersSent && response.writableEnded) return;
  response.statusCode = status;
  response.end(body);
}
