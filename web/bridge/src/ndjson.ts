import net from "node:net";

export const MAX_UPSTREAM_FRAME_BYTES = 1024 * 1024;

export function readLine(socket: net.Socket, timeoutMs: number, maxBytes = MAX_UPSTREAM_FRAME_BYTES): Promise<string> {
  return new Promise((resolve, reject) => {
    let settled = false;
    let chunks: Buffer[] = [];
    let bytes = 0;
    const timer = setTimeout(() => finish(new Error("upstream response timed out")), timeoutMs);
    const finish = (error?: Error, line?: string) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.off("data", onData);
      socket.off("error", onError);
      socket.off("close", onClose);
      chunks = [];
      if (error) reject(error);
      else resolve(line ?? "");
    };
    const onError = () => finish(new Error("upstream connection failed"));
    const onClose = () => finish(new Error("upstream closed before response"));
    const onData = (chunk: Buffer) => {
      const newline = chunk.indexOf(0x0a);
      const take = newline >= 0 ? chunk.subarray(0, newline) : chunk;
      bytes += take.length;
      if (bytes > maxBytes) return finish(new Error("upstream frame exceeded limit"));
      chunks.push(take);
      if (newline >= 0) finish(undefined, Buffer.concat(chunks, bytes).toString("utf8"));
    };
    socket.on("data", onData);
    socket.once("error", onError);
    socket.once("close", onClose);
  });
}

export class LineReader {
  #buffer = Buffer.alloc(0);
  constructor(private readonly onLine: (line: string) => void, private readonly onError: (error: Error) => void) {}

  push(chunk: Buffer): void {
    if (this.#buffer.length + chunk.length > MAX_UPSTREAM_FRAME_BYTES * 2) {
      this.onError(new Error("upstream buffer exceeded limit"));
      return;
    }
    this.#buffer = Buffer.concat([this.#buffer, chunk]);
    for (;;) {
      const newline = this.#buffer.indexOf(0x0a);
      if (newline < 0) {
        if (this.#buffer.length > MAX_UPSTREAM_FRAME_BYTES) this.onError(new Error("upstream frame exceeded limit"));
        return;
      }
      const line = this.#buffer.subarray(0, newline).toString("utf8");
      this.#buffer = this.#buffer.subarray(newline + 1);
      if (line.length) this.onLine(line);
    }
  }
}
