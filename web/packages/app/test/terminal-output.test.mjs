import assert from "node:assert/strict";
import test from "node:test";
import { terminalFrameParts } from "../dist/test/terminal-output.js";

const plain = (parts) => parts.filter(({ kind }) => kind === "text").map(({ text }) => text).join("");
const beforeCursor = (parts) => plain(parts.slice(0, parts.findIndex(({ kind }) => kind === "cursor")));

test("terminal parts preserve indentation, hard newlines and long paths for browser wrapping", () => {
  const text = "  function hello() {\n    return '/very/long/path/'\n  }\n";
  assert.equal(plain(terminalFrameParts(text)), text);
});

test("terminal cursor offsets count Unicode characters across ANSI runs", () => {
  const parts = terminalFrameParts("\x1b[32m界🧭\x1b[0m input", 3);
  assert.equal(plain(parts), "界🧭 input");
  assert.equal(beforeCursor(parts), "界🧭 ");
  assert.equal(parts.filter(({ kind }) => kind === "cursor").length, 1);
  assert.equal(parts[0].style.color, "#9ed68a");
});

test("terminal cursor padding is inserted only at the cursor", () => {
  const parts = terminalFrameParts("prompt", 6, 2);
  assert.equal(beforeCursor(parts), "prompt  ");
  assert.equal(plain(parts), "prompt  ");
  assert.deepEqual(terminalFrameParts("", 0), [{ kind: "cursor" }]);
});

test("a missing or out-of-range cursor does not add a misleading caret", () => {
  assert.equal(terminalFrameParts("text").some(({ kind }) => kind === "cursor"), false);
  assert.equal(terminalFrameParts("text", 10).some(({ kind }) => kind === "cursor"), false);
});

test("ANSI styles remain scoped to their original text without turning output into HTML", () => {
  const parts = terminalFrameParts("plain \x1b[1;38;2;224;161;84mworking <script>\x1b[0m done");
  assert.equal(plain(parts), "plain working <script> done");
  assert.deepEqual(parts[1].style, { color: "rgb(224, 161, 84)", fontWeight: "700" });
  assert.equal(parts[2].style, undefined);
});
