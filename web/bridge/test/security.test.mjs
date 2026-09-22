import assert from "node:assert/strict";
import test from "node:test";
import { BrowserAuthority } from "../dist/auth.js";
import { originAllowed } from "../dist/config.js";

test("browser pairing is one-use and returns a reusable hashed ticket", () => {
  const authority = new BrowserAuthority(600);
  const paired = authority.authenticate({ code: authority.pairingCode });
  assert.equal(paired.accepted, true);
  assert.ok(paired.ticket);
  assert.equal(authority.authenticate({ code: authority.pairingCode }).accepted, false);
  assert.equal(authority.authenticate({ ticket: paired.ticket }).accepted, true);
  authority.revokeAll();
  assert.equal(authority.authenticate({ ticket: paired.ticket }).accepted, false);
});

test("origins require same host or an explicit allowlist", () => {
  assert.equal(originAllowed("http://127.0.0.1:4174", "127.0.0.1:4174", new Set()), true);
  assert.equal(originAllowed("https://evil.example", "127.0.0.1:4174", new Set()), false);
  assert.equal(originAllowed("https://phone.example", "internal:4174", new Set(["https://phone.example"])), true);
  assert.equal(originAllowed(undefined, "127.0.0.1:4174", new Set()), false);
});
