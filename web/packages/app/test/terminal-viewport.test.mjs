import assert from "node:assert/strict";
import test from "node:test";

import { sameViewport, terminalViewportFor, viewportSizingAvailable } from "../dist/test/terminal-viewport.js";

test("viewport sizing needs a control stream and an advertised set_viewport", () => {
  assert.equal(viewportSizingAvailable(undefined, true), false);
  assert.equal(viewportSizingAvailable(["type_literal", "send_key"], true), false);
  assert.equal(viewportSizingAvailable(["type_literal", "set_viewport"], false), false);
  assert.equal(viewportSizingAvailable(["type_literal", "set_viewport"], true), true);
});

test("a viewport counts whole cells and refuses areas too small to use", () => {
  assert.deepEqual(terminalViewportFor(358, 480, 8.4, 21.6), { cols: 42, rows: 22 });
  // A cell that fits by less than a hundredth still counts.
  assert.deepEqual(terminalViewportFor(400, 200, 10, 10), { cols: 40, rows: 20 });
  assert.equal(terminalViewportFor(150, 480, 8.4, 21.6), undefined, "fewer than 20 columns");
  assert.equal(terminalViewportFor(358, 60, 8.4, 21.6), undefined, "fewer than 4 rows");
  assert.equal(terminalViewportFor(358, 480, 0, 21.6), undefined, "unmeasured cell");
  assert.deepEqual(terminalViewportFor(10_000, 10_000, 8, 16), { cols: 500, rows: 300 }, "protocol limits");
});

test("same viewport compares both dimensions and tolerates undefined", () => {
  assert.equal(sameViewport({ cols: 40, rows: 20 }, { cols: 40, rows: 20 }), true);
  assert.equal(sameViewport({ cols: 40, rows: 20 }, { cols: 40, rows: 21 }), false);
  assert.equal(sameViewport(undefined, undefined), true);
  assert.equal(sameViewport(undefined, { cols: 40, rows: 20 }), false);
});
