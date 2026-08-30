/** Minimal byte-stream contract required by the UHP reference client. */
export interface ByteStream {
  read(): Promise<Uint8Array | null>;
  write(data: Uint8Array): Promise<void>;
  closeWrite(): Promise<void>;
  close(): void;
}

/** A transport provider only needs to dial the descriptor's loopback port. */
export interface ByteTransportClient {
  dial(port: number): Promise<ByteStream>;
  close(): void;
}

const MAX_FRAME_BYTES = 1024 * 1024;
const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

export async function writeJsonFrame(stream: ByteStream, value: unknown): Promise<void> {
  const data = textEncoder.encode(`${JSON.stringify(value)}\n`);
  if (data.byteLength > MAX_FRAME_BYTES) throw new Error("Request is too large");
  await stream.write(data);
}

export async function readJsonFrame(stream: ByteStream): Promise<unknown> {
  const reader = frameReader(stream);
  const next = await reader.next();
  if (next.done) throw new Error("Remote stream closed without a response");
  return next.value;
}

export async function* frameReader(stream: ByteStream): AsyncGenerator<unknown> {
  let buffered = new Uint8Array(0);
  while (true) {
    const newline = buffered.indexOf(10);
    if (newline >= 0) {
      const frame = buffered.slice(0, newline);
      buffered = buffered.slice(newline + 1);
      if (frame.byteLength === 0) throw new Error("Remote stream sent an empty frame");
      yield JSON.parse(textDecoder.decode(frame)) as unknown;
      continue;
    }
    const chunk = await stream.read();
    if (chunk === null) {
      if (buffered.byteLength !== 0) throw new Error("Remote stream ended mid-frame");
      return;
    }
    if (buffered.byteLength + chunk.byteLength > MAX_FRAME_BYTES) {
      throw new Error("Remote frame exceeds the one MiB limit");
    }
    const combined = new Uint8Array(buffered.byteLength + chunk.byteLength);
    combined.set(buffered);
    combined.set(chunk, buffered.byteLength);
    buffered = combined;
  }
}
