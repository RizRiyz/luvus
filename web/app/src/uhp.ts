import { currentCredentials, rememberCredentials } from "./security";
import { frameReader, readJsonFrame, writeJsonFrame } from "./transport";
import type { ByteStream, ByteTransportClient } from "./transport";
import type { TerminalAction, TerminalLocator } from "./terminal";

interface ResponseEnvelope {
  id: string;
  result?: unknown;
  error?: { code?: string; message?: string };
}

export interface PairResult {
  expiresAt: number;
  scopes: string[];
}

export class UhpClient {
  #nextId = 1;
  #eventStream: ByteStream | null = null;
  #terminalStream: ByteStream | null = null;
  #terminalActions = new Map<
    string,
    { resolve: () => void; reject: (error: Error) => void; timeout: number }
  >();

  constructor(
    private readonly transport: ByteTransportClient,
    private readonly port: number,
  ) {}

  async pair(code: string): Promise<PairResult> {
    const stream = await this.transport.dial(this.port);
    try {
      await writeJsonFrame(stream, { type: "pair", code });
      await stream.closeWrite();
      const response = (await readJsonFrame(stream)) as Record<string, unknown>;
      if (response.type !== "paired" || typeof response.token !== "string") {
        throw new Error("Pairing was rejected");
      }
      if (typeof response.expires_at !== "number") throw new Error("Pairing response is invalid");
      if (!Array.isArray(response.scopes) || !response.scopes.every((scope) => typeof scope === "string")) {
        throw new Error("Pairing authority is invalid");
      }
      rememberCredentials({ token: response.token, expiresAt: response.expires_at });
      return { expiresAt: response.expires_at, scopes: response.scopes };
    } finally {
      stream.close();
    }
  }

  async request(method: string, params: Record<string, unknown> = {}): Promise<unknown> {
    const stream = await this.transport.dial(this.port);
    const id = `web-${this.#nextId++}`;
    try {
      await writeJsonFrame(stream, {
        id,
        method,
        params,
        auth: currentCredentials().token,
      });
      await stream.closeWrite();
      const response = (await readJsonFrame(stream)) as ResponseEnvelope;
      if (response.id !== id) throw new Error("UHP response identity mismatch");
      if (response.error) throw new Error(response.error.message || response.error.code || "UHP request failed");
      return response.result;
    } finally {
      stream.close();
    }
  }

  async subscribe(
    afterSequence: number,
    onStarted: (sequence: number) => void,
    onEvent: (event: unknown) => void,
    onFailure: (error: unknown) => void,
  ): Promise<void> {
    this.closeEvents();
    const stream = await this.transport.dial(this.port);
    this.#eventStream = stream;
    const id = `web-events-${this.#nextId++}`;
    await writeJsonFrame(stream, {
      id,
      method: "events.subscribe",
      params: { after_sequence: afterSequence },
      auth: currentCredentials().token,
    });
    void (async () => {
      try {
        let started = false;
        for await (const value of frameReader(stream)) {
          if (!started) {
            const response = value as ResponseEnvelope;
            if (response.id !== id || response.error) throw new Error("Event subscription failed");
            const sequence = Number((response.result as Record<string, unknown>)?.sequence);
            if (!Number.isSafeInteger(sequence) || sequence < 0) throw new Error("Event fence is invalid");
            started = true;
            onStarted(sequence);
          } else {
            onEvent(value);
          }
        }
        if (this.#eventStream === stream) throw new Error("Event stream closed");
      } catch (error) {
        if (this.#eventStream === stream) onFailure(error);
      } finally {
        if (this.#eventStream === stream) this.#eventStream = null;
        stream.close();
      }
    })();
  }

  closeEvents(): void {
    this.#eventStream?.close();
    this.#eventStream = null;
  }

  async openTerminal(
    locator: TerminalLocator,
    control: boolean,
    onFrame: (frame: unknown) => void,
    onFailure: (error: unknown) => void,
  ): Promise<void> {
    this.closeTerminal();
    const stream = await this.transport.dial(this.port);
    this.#terminalStream = stream;
    const id = `web-terminal-${this.#nextId++}`;
    const frames = frameReader(stream);
    try {
      await writeJsonFrame(stream, {
        id,
        method: control ? "terminal.backend.control" : "terminal.backend.observe",
        params: {
          server_generation: locator.server_generation,
          terminal_id: locator.terminal_id,
          pane_id: locator.pane_id,
          mode: "visible",
          lines: 120,
          ansi: false,
        },
        auth: currentCredentials().token,
      });
      const first = await frames.next();
      if (first.done) throw new Error("Terminal stream closed before it started");
      const response = first.value as ResponseEnvelope;
      if (response.id !== id || response.error) {
        throw new Error(response.error?.message || response.error?.code || "Terminal stream failed");
      }
      const streamInfo = response.result as Record<string, unknown>;
      if (streamInfo?.type !== "terminal_backend_stream") {
        throw new Error("Terminal stream response is invalid");
      }
      void this.#readTerminal(stream, frames, onFrame, onFailure);
    } catch (error) {
      if (this.#terminalStream === stream) this.#terminalStream = null;
      stream.close();
      throw error;
    }
  }

  async terminalAction(action: TerminalAction, params: Record<string, string>): Promise<void> {
    const stream = this.#terminalStream;
    if (!stream) throw new Error("Open a terminal before sending input");
    const id = `web-action-${this.#nextId++}`;
    const completion = new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        this.#terminalActions.delete(id);
        reject(new Error("Terminal input acknowledgement timed out"));
      }, 10_000);
      this.#terminalActions.set(id, { resolve, reject, timeout });
    });
    try {
      await writeJsonFrame(stream, { id, action, params });
    } catch (error) {
      const pending = this.#terminalActions.get(id);
      if (pending) {
        window.clearTimeout(pending.timeout);
        this.#terminalActions.delete(id);
      }
      throw error;
    }
    return completion;
  }

  closeTerminal(): void {
    this.#terminalStream?.close();
    this.#terminalStream = null;
    for (const pending of this.#terminalActions.values()) {
      window.clearTimeout(pending.timeout);
      pending.reject(new Error("Terminal stream closed"));
    }
    this.#terminalActions.clear();
  }

  async #readTerminal(
    stream: ByteStream,
    frames: AsyncGenerator<unknown>,
    onFrame: (frame: unknown) => void,
    onFailure: (error: unknown) => void,
  ): Promise<void> {
    try {
      for await (const value of frames) {
        const response = value as ResponseEnvelope;
        if (typeof response?.id === "string" && this.#terminalActions.has(response.id)) {
          const pending = this.#terminalActions.get(response.id)!;
          window.clearTimeout(pending.timeout);
          this.#terminalActions.delete(response.id);
          if (response.error) {
            pending.reject(new Error(response.error.message || response.error.code || "Terminal input failed"));
          } else {
            pending.resolve();
          }
          continue;
        }
        onFrame(value);
      }
      if (this.#terminalStream === stream) throw new Error("Terminal stream closed");
    } catch (error) {
      if (this.#terminalStream === stream) onFailure(error);
    } finally {
      if (this.#terminalStream === stream) this.closeTerminal();
      stream.close();
    }
  }
}
