import assert from "node:assert/strict";
import test from "node:test";
import { BridgeError } from "@luvus/uhp-client";
import { recoverableConnectionError } from "../dist/test/terminal-reconnect.js";

test("a closing terminal control lease gets only four backoff retries", () => {
  const conflict = new BridgeError("Terminal already controlled", "control_conflict");
  assert.equal(recoverableConnectionError(conflict), false, "input/action failures do not retry control ownership");
  for (let retries = 0; retries < 4; retries += 1) assert.equal(recoverableConnectionError(conflict, retries), true);
  for (const retries of [4, 5, 100]) assert.equal(recoverableConnectionError(conflict, retries), false);
});

test("existing transport recovery is unchanged", () => {
  for (const code of ["bridge_error", "closed", "disconnected", "stale_server", "stale_stream", "timeout", "unavailable"]) {
    assert.equal(recoverableConnectionError(new BridgeError("Disconnected", code), 100), true);
  }
  assert.equal(recoverableConnectionError(new Error("Network failed"), 100), true);
});

test("authorization and validation failures never retry", () => {
  for (const code of ["unauthorized", "expired", "forbidden", "invalid_params", "limit_exceeded", "unknown_error"]) {
    assert.equal(recoverableConnectionError(new BridgeError("Rejected", code), 0), false);
  }
});
