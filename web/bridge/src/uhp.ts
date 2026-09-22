import { spawn, type ChildProcessByStdio } from "node:child_process";
import net from "node:net";
import { once } from "node:events";
import type { Readable } from "node:stream";
import type { BridgeConfig } from "./config.js";
import { LineReader, readLine } from "./ndjson.js";

type Descriptor = {
  type: "luvus_uhp_access";
  protocol: { name: "luvus-uhp"; major: 1; minor: number };
  endpoint: { transport: "tcp"; host: "127.0.0.1"; port: number; framing: "ndjson" };
  pairing: { type: "one_use_code"; code: string; expires_at: number };
  authority: { mode: "read_only" | "control"; scopes: string[]; expires_at?: number; expires_on_close?: true };
};

type Paired = { type: "paired"; token: string; scopes: string[]; expires_at?: number; expires_on_close?: true };

export type BrowserSession = {
  name: string;
  default: boolean;
  running: boolean;
};

export interface UpstreamStream {
  write(frame: object): boolean;
  close(): void;
}

export class UhpAccess extends EventTarget {
  #child: ChildProcessByStdio<null, Readable, Readable> | undefined;
  #descriptor: Descriptor | undefined;
  #token = "";
  #capabilities: Record<string, unknown> | undefined;
  #allowed = new Set<string>();
  #config: BridgeConfig | undefined;
  #restartPromise: Promise<void> | undefined;
  #switchPromise: Promise<BrowserSession> | undefined;

  get authority(): Descriptor["authority"] {
    if (!this.#descriptor) throw new Error("UHP access has not started");
    return this.#descriptor.authority;
  }

  get capabilities(): Record<string, unknown> {
    if (!this.#capabilities) throw new Error("UHP capabilities unavailable");
    return this.#capabilities;
  }

  methodAllowed(method: string): boolean {
    return this.#allowed.has(method);
  }

  async start(config: BridgeConfig): Promise<void> {
    if (this.#child) throw new Error("UHP access already started");
    this.#config = config;
    const args = [
      ...(config.session ? ["--session", config.session] : []),
      "uhp", "access",
      ...(config.control ? ["--control"] : []),
      "--no-expiry",
    ];
    const env = { ...process.env };
    delete env.LUVUS_SOCKET_PATH;
    delete env.LUVUS_SESSION;
    if (config.luvusHome) env.LUVUS_HOME = config.luvusHome;
    const child = spawn(config.luvusBin, args, { env, stdio: ["ignore", "pipe", "pipe"] });
    this.#child = child;
    child.once("exit", () => {
      if (this.#child !== child) return;
      this.#child = undefined;
      this.#descriptor = undefined;
      this.#token = "";
      this.dispatchEvent(new Event("exit"));
    });
    child.stderr.setEncoding("utf8");
    let stderr = "";
    child.stderr.on("data", (chunk: string) => { stderr = (stderr + chunk).slice(-4096); });
    try {
      const line = await Promise.race([
        readChildLine(child, 8_000),
        once(child, "exit").then(() => { throw new Error(stderr.trim() || "luvus uhp access exited before startup"); }),
        once(child, "error").then(([error]) => { throw error; }),
      ]);
      const descriptor = parseDescriptor(line);
      this.#descriptor = descriptor;
      const paired = await pair(descriptor);
      this.#token = paired.token;
      const capabilities = await this.request("uhp.capabilities", {});
      if (!capabilities || typeof capabilities !== "object" || (capabilities as { type?: unknown }).type !== "uhp_capabilities") {
        throw new Error("UHP returned invalid capabilities");
      }
      this.#capabilities = capabilities as Record<string, unknown>;
      const allowed = (capabilities as { access?: { allowed_methods?: unknown } }).access?.allowed_methods;
      if (!Array.isArray(allowed) || !allowed.every((value) => typeof value === "string")) {
        throw new Error("UHP access capabilities omitted allowed methods");
      }
      this.#allowed = new Set(allowed);
    } catch (error) {
      this.#terminateChild();
      throw error;
    }
  }

  restart(): Promise<void> {
    if (this.#switchPromise) return this.#switchPromise.then(() => {});
    if (this.#restartPromise) return this.#restartPromise;
    const config = this.#config;
    if (!config) return Promise.reject(new Error("UHP access configuration unavailable"));
    this.#restartPromise = (async () => {
      this.#terminateChild();
      await new Promise((resolve) => setTimeout(resolve, 50));
      await this.start(config);
    })().finally(() => { this.#restartPromise = undefined; });
    return this.#restartPromise;
  }

  async sessions(): Promise<BrowserSession[]> {
    const result = await this.#hostRequest("session.list", {});
    if (!result || typeof result !== "object" || (result as { type?: unknown }).type !== "session_list") {
      throw new Error("Luvus returned an invalid session list");
    }
    const sessions = (result as { sessions?: unknown }).sessions;
    if (!Array.isArray(sessions) || sessions.length > 256) throw new Error("Luvus returned an invalid session list");
    return sessions.map((entry) => {
      if (!entry || typeof entry !== "object") throw new Error("Luvus returned an invalid session entry");
      const session = entry as { name?: unknown; default?: unknown; running?: unknown };
      if (
        typeof session.name !== "string" || !validSessionName(session.name)
        || typeof session.default !== "boolean" || typeof session.running !== "boolean"
      ) throw new Error("Luvus returned an invalid session entry");
      return { name: session.name, default: session.default, running: session.running };
    });
  }

  switchSession(name: string, allowStart: boolean): Promise<BrowserSession> {
    if (this.#switchPromise) return this.#switchPromise;
    this.#switchPromise = (async () => {
      if (this.#restartPromise) await this.#restartPromise;
      if (!validSessionName(name)) throw codedError("Invalid session name", "invalid_params");
      const sessions = await this.sessions();
      let target = sessions.find((session) => session.name === name);
      if (!target) throw codedError("Unknown Luvus session", "not_found");
      if (!target.running) {
        if (!allowStart) throw codedError("Starting a stopped session requires web control", "forbidden");
        const started = await this.#hostRequest("session.start", { name });
        const session = started && typeof started === "object"
          ? (started as { session?: { name?: unknown; default?: unknown; running?: unknown } }).session
          : undefined;
        if (
          !session || session.name !== name || typeof session.default !== "boolean"
          || session.running !== true
        ) throw new Error("Luvus returned an invalid started session");
        target = { name, default: session.default, running: true };
      }
      const previous = this.#config;
      if (!previous) throw new Error("UHP access configuration unavailable");
      const selected = previous.session ?? "default";
      if (selected === name) return target;
      const next: BridgeConfig = {
        ...previous,
        ...(name === "default" ? {} : { session: name }),
      };
      if (name === "default") delete next.session;
      this.#terminateChild();
      await new Promise((resolve) => setTimeout(resolve, 50));
      try {
        await this.start(next);
      } catch (error) {
        try { await this.start(previous); } catch { /* A later request retries the prior target. */ }
        throw error;
      }
      return target;
    })().finally(() => { this.#switchPromise = undefined; });
    return this.#switchPromise;
  }

  async request(method: string, params: object, id = requestId()): Promise<unknown> {
    if (!this.#descriptor || !this.#token) throw new Error("UHP access is unavailable");
    const socket = net.createConnection(this.#descriptor.endpoint.port, this.#descriptor.endpoint.host);
    await connect(socket);
    socket.end(`${JSON.stringify({ id, method, params, auth: this.#token })}\n`);
    const line = await readLine(socket, 12_000);
    const frame = parseObject(line);
    if (frame.id !== id) throw new Error("UHP response id mismatch");
    if (frame.error && typeof frame.error === "object") throw upstreamError(frame.error as Record<string, unknown>);
    return frame.result;
  }

  async stream(
    method: string,
    params: object,
    id: string,
    onFrame: (frame: Record<string, unknown>) => void,
    onClose: (reason: string) => void,
  ): Promise<UpstreamStream> {
    if (!this.#descriptor || !this.#token) throw new Error("UHP access is unavailable");
    const socket = net.createConnection(this.#descriptor.endpoint.port, this.#descriptor.endpoint.host);
    await connect(socket);
    let open = true;
    const reader = new LineReader((line) => {
      try { onFrame(parseObject(line)); }
      catch { socket.destroy(new Error("invalid upstream JSON")); }
    }, (error) => socket.destroy(error));
    socket.on("data", (chunk: Buffer) => reader.push(chunk));
    socket.once("close", () => {
      if (!open) return;
      open = false;
      onClose("upstream closed");
    });
    socket.once("error", () => {
      if (!open) return;
      open = false;
      onClose("upstream failed");
    });
    socket.write(`${JSON.stringify({ id, method, params, auth: this.#token })}\n`);
    return {
      write: (frame) => open && socket.write(`${JSON.stringify(frame)}\n`),
      close: () => {
        if (!open) return;
        open = false;
        socket.destroy();
      },
    };
  }

  stop(): void {
    this.#config = undefined;
    this.#token = "";
    this.#allowed.clear();
    this.#capabilities = undefined;
    this.#terminateChild();
  }

  #terminateChild(): void {
    const child = this.#child;
    this.#child = undefined;
    this.#descriptor = undefined;
    this.#token = "";
    child?.kill("SIGTERM");
  }

  async #hostRequest(method: "session.list" | "session.start", params: object): Promise<unknown> {
    const config = this.#config;
    if (!config) throw new Error("UHP access configuration unavailable");
    const env = { ...process.env };
    delete env.LUVUS_SOCKET_PATH;
    delete env.LUVUS_SESSION;
    if (config.luvusHome) env.LUVUS_HOME = config.luvusHome;
    const child = spawn(config.luvusBin, ["uhp", "proxy"], {
      env,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    child.stderr.setEncoding("utf8");
    let stderr = "";
    child.stderr.on("data", (chunk: string) => { stderr = (stderr + chunk).slice(-4096); });
    const id = requestId();
    child.stdin.end(`${JSON.stringify({ id, method, params })}\n`);
    try {
      const line = await Promise.race([
        readChildLine(child, 12_000),
        once(child, "error").then(([error]) => { throw error; }),
        once(child, "exit").then(([code]) => {
          throw new Error(stderr.trim() || `luvus uhp proxy exited with status ${String(code)}`);
        }),
      ]);
      const frame = parseObject(line);
      if (frame.id !== id) throw new Error("UHP host response id mismatch");
      if (frame.error && typeof frame.error === "object") throw upstreamError(frame.error as Record<string, unknown>);
      return frame.result;
    } finally {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    }
  }
}

async function pair(descriptor: Descriptor): Promise<Paired> {
  const socket = net.createConnection(descriptor.endpoint.port, descriptor.endpoint.host);
  await connect(socket);
  socket.end(`${JSON.stringify({ type: "pair", code: descriptor.pairing.code })}\n`);
  const frame = parseObject(await readLine(socket, 5_000));
  if (frame.type !== "paired" || typeof frame.token !== "string" || !frame.token) {
    throw new Error("UHP pairing failed");
  }
  return frame as unknown as Paired;
}

function parseDescriptor(line: string): Descriptor {
  const value = parseObject(line) as Partial<Descriptor>;
  if (
    value.type !== "luvus_uhp_access" || value.protocol?.name !== "luvus-uhp" || value.protocol.major !== 1 ||
    value.endpoint?.transport !== "tcp" || value.endpoint.host !== "127.0.0.1" ||
    value.endpoint.framing !== "ndjson" || !Number.isInteger(value.endpoint.port) ||
    value.pairing?.type !== "one_use_code" || typeof value.pairing.code !== "string" ||
    (value.authority?.mode !== "read_only" && value.authority?.mode !== "control")
  ) throw new Error("Invalid UHP access descriptor");
  return value as Descriptor;
}

function readChildLine(child: { stdout: Readable }, timeoutMs: number): Promise<string> {
  return new Promise((resolve, reject) => {
    let buffer = "";
    const timer = setTimeout(() => done(new Error("timed out waiting for UHP access descriptor")), timeoutMs);
    const done = (error?: Error, line?: string) => {
      clearTimeout(timer);
      child.stdout.off("data", onData);
      if (error) reject(error); else resolve(line ?? "");
    };
    const onData = (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      if (buffer.length > 64 * 1024) return done(new Error("UHP descriptor exceeded limit"));
      const newline = buffer.indexOf("\n");
      if (newline >= 0) done(undefined, buffer.slice(0, newline));
    };
    child.stdout.on("data", onData);
  });
}

function connect(socket: net.Socket): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => fail(new Error("upstream connection timed out")), 5_000);
    const fail = (error: Error) => { clearTimeout(timer); socket.destroy(); reject(error); };
    socket.once("connect", () => { clearTimeout(timer); socket.off("error", fail); resolve(); });
    socket.once("error", fail);
  });
}

function parseObject(line: string): Record<string, unknown> {
  const value: unknown = JSON.parse(line);
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid JSON object");
  return value as Record<string, unknown>;
}

function upstreamError(error: Record<string, unknown>): Error & { code?: string } {
  const result = new Error(typeof error.message === "string" ? error.message : "UHP request failed") as Error & { code?: string };
  if (typeof error.code === "string") result.code = error.code;
  return result;
}

function requestId(): string {
  return `bridge-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function validSessionName(name: string): boolean {
  return name.length > 0 && name.length <= 64 && name !== "." && name !== ".."
    && /^[A-Za-z0-9._-]+$/.test(name);
}

function codedError(message: string, code: string): Error & { code: string } {
  return Object.assign(new Error(message), { code });
}
