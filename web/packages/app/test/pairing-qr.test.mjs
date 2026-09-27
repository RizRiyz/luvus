import assert from "node:assert/strict";
import test from "node:test";
import { pairingQrDataUrl } from "../dist/test/pairing-qr.js";

test("pairing QR is rendered locally as an embeddable SVG data URL", () => {
  const result = pairingQrDataUrl("https://phone.example/#pair=temporary-secret");
  assert.ok(result.startsWith("data:image/svg+xml;charset=utf-8,"));
  const svg = decodeURIComponent(result.slice(result.indexOf(",") + 1));
  assert.match(svg, /^<svg /);
  assert.match(svg, /<rect fill="white"/);
  assert.match(svg, /<path fill="black"/);
});
