import assert from "node:assert/strict";
import test from "node:test";

import { supportsFileUpload } from "../dist/test/terminal-capabilities.js";

const uploadActions = [
  "upload_start",
  "upload_chunk",
  "upload_finish",
  "upload_cancel",
];

test("file uploads require every advertised upload action", () => {
  assert.equal(supportsFileUpload(undefined), false);
  assert.equal(supportsFileUpload([]), false);
  assert.equal(supportsFileUpload(uploadActions.slice(0, -1)), false);
  assert.equal(supportsFileUpload(uploadActions), true);
  assert.equal(supportsFileUpload(["paste_text", ...uploadActions]), true);
});
