import assert from "node:assert/strict";
import test from "node:test";

test("package exports are built", async () => {
  const module = await import("../dist/index.js");
  assert.equal(typeof module.BridgeClient, "function");
  assert.equal(typeof module.LiveSession, "function");
});
