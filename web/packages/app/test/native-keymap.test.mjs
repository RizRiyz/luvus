import assert from "node:assert/strict";
import test from "node:test";

import { deletionInputKey, logicalKey } from "../dist/test/native-keymap.js";

const key = (value, modifiers = {}) => ({
  key: value,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  shiftKey: false,
  ...modifiers,
});

test("desktop deletion chords preserve shell editing semantics", () => {
  assert.equal(logicalKey(key("Backspace", { altKey: true })), "ctrl-w");
  assert.equal(logicalKey(key("Backspace", { ctrlKey: true })), "ctrl-w");
  assert.equal(logicalKey(key("Backspace", { metaKey: true })), "ctrl-u");
  assert.equal(logicalKey(key("Delete", { altKey: true })), "alt-d");
  assert.equal(logicalKey(key("Delete", { ctrlKey: true })), "alt-d");
  assert.equal(logicalKey(key("Delete", { metaKey: true })), "ctrl-k");
  assert.equal(logicalKey(key("k", { ctrlKey: true })), "ctrl-k");
});

test("copy and paste remain browser-owned command shortcuts", () => {
  assert.equal(logicalKey(key("c", { metaKey: true })), undefined);
  assert.equal(logicalKey(key("v", { metaKey: true })), undefined);
});

test("beforeinput deletion intents cover words and both halves of a line", () => {
  assert.equal(deletionInputKey("deleteContentBackward"), "backspace");
  assert.equal(deletionInputKey("deleteContentForward"), "delete");
  assert.equal(deletionInputKey("deleteWordBackward"), "ctrl-w");
  assert.equal(deletionInputKey("deleteWordForward"), "alt-d");
  assert.equal(deletionInputKey("deleteSoftLineBackward"), "ctrl-u");
  assert.equal(deletionInputKey("deleteHardLineBackward"), "ctrl-u");
  assert.equal(deletionInputKey("deleteSoftLineForward"), "ctrl-k");
  assert.equal(deletionInputKey("deleteHardLineForward"), "ctrl-k");
});
