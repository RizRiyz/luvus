import assert from "node:assert/strict";
import test from "node:test";

import { uploadTerminalFile } from "../dist/test/file-upload.js";

const UPLOAD_ID = "0123456789abcdef0123456789abcdef";

async function chunkOffsets(maxChunkBytes, size) {
  const offsets = [];
  const dispatch = async (action, params) => {
    if (action === "upload_start") return { type: "terminal_upload", upload_id: UPLOAD_ID, max_chunk_bytes: maxChunkBytes };
    if (action === "upload_chunk") {
      offsets.push(params.offset);
      if (offsets.length > 64) throw new Error("upload did not advance");
    }
    return {};
  };
  await uploadTerminalFile(new File([new Uint8Array(size)], "a.bin"), dispatch);
  return offsets;
}

test("uploads in the server's chunk size, capped by the client limit", async () => {
  assert.deepEqual(await chunkOffsets(4, 10), [0, 4, 8]);
  assert.deepEqual(await chunkOffsets(1024 * 1024, 200 * 1024), [0, 160 * 1024]);
});

test("a zero or negative server chunk size falls back instead of looping", async () => {
  for (const limit of [0, -1, Number.NaN, "8"]) {
    assert.deepEqual(await chunkOffsets(limit, 200 * 1024), [0, 160 * 1024]);
  }
});
