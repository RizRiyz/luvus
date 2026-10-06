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

test("default browser access is process-bound; finite access still expires", () => {
  const now = Date.now;
  let clock = now();
  Date.now = () => clock;
  try {
    const persistent = new BrowserAuthority(undefined, 2);
    const finite = new BrowserAuthority(600, 2);
    const browser = persistent.authenticate({ code: persistent.initialPairing.code });
    const limited = finite.authenticate({ code: finite.initialPairing.code });
    assert.equal(browser.expiresAt, undefined);
    assert.ok(limited.expiresAt);
    clock += 7 * 24 * 60 * 60 * 1000;
    assert.equal(persistent.authenticate({ ticket: browser.ticket }).accepted, true);
    assert.equal(persistent.authenticate({ ticket: "unknown-ticket" }).accepted, false);
    assert.equal(finite.authenticate({ ticket: limited.ticket }).accepted, false);
    persistent.revoke(browser.ticketDigest);
    assert.equal(persistent.authenticate({ ticket: browser.ticket }).accepted, false);
  } finally { Date.now = now; }
});

test("device limits and public pairing URLs are bounded configuration", () => {
  const config = loadConfig({
    LUVUS_WEB_MAX_DEVICES: "4",
    LUVUS_WEB_PUBLIC_URL: "https://phone.example/",
  });
  assert.equal(config.browserMaxDevices, 4);
  assert.equal(config.browserTicketSeconds, undefined);
  assert.equal(loadConfig({ LUVUS_WEB_TICKET_TTL: "600" }).browserTicketSeconds, 600);
  for (const ttl of ["0", "-1", "86401", "not-a-number"]) {
    assert.throws(() => loadConfig({ LUVUS_WEB_TICKET_TTL: ttl }), /1 through 86400/);
  }
  assert.equal(config.publicUrl, "https://phone.example");
  assert.throws(() => loadConfig({ LUVUS_WEB_MAX_DEVICES: "9" }), /1 through 8/);
  assert.throws(() => loadConfig({ LUVUS_WEB_PUBLIC_URL: "http://phone.example" }), /HTTPS URL/);
  assert.throws(() => loadConfig({ LUVUS_WEB_PUBLIC_URL: "https://user:secret@phone.example" }), /without credentials/);
  assert.throws(() => loadConfig({ LUVUS_WEB_PUBLIC_URL: "https://phone.example/luvus" }), /must not include a path prefix/);
});
