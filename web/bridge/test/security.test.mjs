import assert from "node:assert/strict";
import test from "node:test";
import { BrowserAuthority } from "../dist/auth.js";
import { loadConfig, originAllowed } from "../dist/config.js";

test("browser pairing gives each allowed device an independent reusable ticket", () => {
  const authority = new BrowserAuthority(600, 2);
  const paired = authority.authenticate({ code: authority.initialPairing.code });
  assert.equal(paired.accepted, true);
  assert.ok(paired.ticket);
  assert.equal(authority.authenticate({ code: authority.initialPairing.code }).accepted, false);
  assert.equal(authority.authenticate({ ticket: paired.ticket }).accepted, true);

  const phoneLink = authority.createPairing();
  assert.ok(phoneLink);
  const phone = authority.authenticate({ code: phoneLink.code });
  assert.equal(phone.accepted, true);
  assert.ok(phone.ticket);
  assert.notEqual(phone.ticket, paired.ticket);
  assert.deepEqual(authority.status(), { pairedDevices: 2, pendingPairings: 0, maxDevices: 2 });
  assert.equal(authority.createPairing(), undefined);
  assert.equal(authority.setMaxDevices(1), false);
  assert.equal(authority.setMaxDevices(3), true);
  assert.ok(authority.createPairing());

  authority.revokeAll();
  assert.equal(authority.authenticate({ ticket: paired.ticket }).accepted, false);
  assert.equal(authority.authenticate({ ticket: phone.ticket }).accepted, false);
});

test("origins require same host or an explicit allowlist", () => {
  assert.equal(originAllowed("http://127.0.0.1:4174", "127.0.0.1:4174", new Set()), true);
  assert.equal(originAllowed("https://evil.example", "127.0.0.1:4174", new Set()), false);
  assert.equal(originAllowed("https://phone.example", "internal:4174", new Set(["https://phone.example"])), true);
  assert.equal(originAllowed(undefined, "127.0.0.1:4174", new Set()), false);
});

test("device limits and public pairing URLs are bounded configuration", () => {
  const config = loadConfig({
    LUVUS_WEB_MAX_DEVICES: "4",
    LUVUS_WEB_PUBLIC_URL: "https://phone.example/luvus/",
  });
  assert.equal(config.browserMaxDevices, 4);
  assert.equal(config.publicUrl, "https://phone.example/luvus");
  assert.throws(() => loadConfig({ LUVUS_WEB_MAX_DEVICES: "9" }), /1 through 8/);
  assert.throws(() => loadConfig({ LUVUS_WEB_PUBLIC_URL: "https://user:secret@phone.example" }), /without credentials/);
});
