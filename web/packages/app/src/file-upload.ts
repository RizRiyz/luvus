import type { TerminalAction } from "./native-input.js";

const MAX_FILE_BYTES = 32 * 1024 * 1024;
const FALLBACK_CHUNK_BYTES = 160 * 1024;
const encoder = new TextEncoder();

type Dispatch = (action: TerminalAction, params: Record<string, unknown>) => Promise<unknown>;

type UploadStart = {
  type: "terminal_upload";
  upload_id: string;
  max_chunk_bytes: number;
};

export async function uploadTerminalFile(file: File, dispatch: Dispatch): Promise<void> {
  if (!file.size || file.size > MAX_FILE_BYTES) {
    throw new Error("Files must be between 1 byte and 32 MiB.");
  }
  const start = await dispatch("upload_start", {
    name: boundedName(file.name),
    size: file.size,
  }) as Partial<UploadStart>;
  if (start.type !== "terminal_upload" || !validUploadId(start.upload_id)) {
    throw new Error("The server did not create a terminal upload.");
  }
  const uploadId = start.upload_id;
  const chunkBytes = Number.isSafeInteger(start.max_chunk_bytes)
    ? Math.min(FALLBACK_CHUNK_BYTES, Number(start.max_chunk_bytes))
    : FALLBACK_CHUNK_BYTES;
  try {
    for (let offset = 0; offset < file.size; offset += chunkBytes) {
      const bytes = new Uint8Array(await file.slice(offset, offset + chunkBytes).arrayBuffer());
      await dispatch("upload_chunk", {
        upload_id: uploadId,
        offset,
        data_base64: bytesToBase64(bytes),
      });
    }
    await dispatch("upload_finish", { upload_id: uploadId });
  } catch (error) {
    await dispatch("upload_cancel", { upload_id: uploadId }).catch(() => undefined);
    throw error;
  }
}

function boundedName(name: string): string {
  const leaf = name.split(/[\\/]/).at(-1)?.trim() || "attachment";
  if (encoder.encode(leaf).byteLength <= 255) return leaf;
  let result = "";
  for (const character of leaf) {
    if (encoder.encode(result + character).byteLength > 240) break;
    result += character;
  }
  return result || "attachment";
}

function validUploadId(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{32}$/.test(value);
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
}
