import assert from "node:assert/strict";
import test from "node:test";
import { accessProblem, pairingCredential, parsePairingInput } from "../dist/test/pairing.js";

const CODE = "T0oObuPJ_qx4MR4tUwEXH9OMwnxoPrXw";

test("a saved ticket is sent even when the tab also has a pairing code", () => {
  // Reopening a used link must not discard the ticket that still works.
  assert.deepEqual(pairingCredential("ticket-1", CODE), { ticket: "ticket-1", code: CODE });
  assert.deepEqual(pairingCredential("ticket-1", undefined), { ticket: "ticket-1" });
  assert.deepEqual(pairingCredential(null, CODE), { code: CODE });
  assert.deepEqual(pairingCredential(null, undefined), {});
});

test("a pasted link, fragment, or bare code yields only the code", () => {
  assert.equal(parsePairingInput(`http://127.0.0.1:4174/#pair=${CODE}`), CODE);
  assert.equal(parsePairingInput(`  https://phone.example/#pair=${CODE}\n`), CODE);
  assert.equal(parsePairingInput(`#pair=${CODE}`), CODE);
  assert.equal(parsePairingInput(CODE), CODE);
});

test("anything that is not a pairing code is rejected", () => {
  assert.equal(parsePairingInput(""), undefined);
  assert.equal(parsePairingInput("http://127.0.0.1:4174/"), undefined, "a link without a code");
  assert.equal(parsePairingInput("https://evil.example/login"), undefined);
  assert.equal(parsePairingInput("short"), undefined);
  assert.equal(parsePairingInput(`${CODE}<script>`), undefined);
  assert.equal(parsePairingInput(`http://x/#pair=${CODE}"onerror=`), undefined);
});

test("a rejection is explained by what the tab actually sent", () => {
  assert.match(accessProblem({ ticket: false, code: false }).title, /not paired yet/);
  assert.match(accessProblem({ ticket: false, code: true }).title, /already used/);
  assert.match(accessProblem({ ticket: true, code: false }).title, /access ended/);
  assert.match(accessProblem({ ticket: true, code: true }).title, /access ended/,
    "a rejected ticket is the problem even if a code was sent too");
  for (const sent of [{ ticket: false, code: false }, { ticket: false, code: true }, { ticket: true, code: false }]) {
    assert.match(accessProblem(sent).body, /Press Enter in the terminal running luvus web/);
  }
});
