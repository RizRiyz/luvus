import assert from "node:assert/strict";
import test from "node:test";

import { deletionInputKey, logicalKey, modifiedLogicalKey, shiftedText } from "../dist/test/native-keymap.js";

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

test("shell and agent control chords reach the terminal instead of the browser", () => {
  for (const letter of ["a", "b", "e", "f", "g", "l", "n", "o", "p", "r", "t", "y"]) {
    assert.equal(logicalKey(key(letter, { ctrlKey: true })), `ctrl-${letter}`);
  }
  assert.equal(logicalKey(key("R", { ctrlKey: true })), "ctrl-r");
  assert.equal(logicalKey(key("z", { ctrlKey: true })), undefined);
  assert.equal(logicalKey(key("s", { ctrlKey: true })), undefined);
  assert.equal(logicalKey(key("r", { ctrlKey: true, shiftKey: true })), undefined);
});

test("copy and paste remain browser-owned command shortcuts", () => {
  assert.equal(logicalKey(key("c", { metaKey: true })), undefined);
  assert.equal(logicalKey(key("v", { metaKey: true })), undefined);
  assert.equal(logicalKey(key("v", { ctrlKey: true })), undefined);
  assert.equal(logicalKey(key("c", { ctrlKey: true, shiftKey: true })), undefined);
});

test("toolbar modifiers preserve terminal chords without raw escape sequences", () => {
  const ctrl = { ctrl: true, shift: false }, shift = { ctrl: false, shift: true }, both = { ctrl: true, shift: true };
  for (const letter of "abcdefghijklmnopqrstuvwxyz") assert.equal(modifiedLogicalKey(letter, ctrl), `ctrl-${letter}`);
  assert.equal(modifiedLogicalKey("A", both), "ctrl-shift-a");
  assert.equal(modifiedLogicalKey("space", ctrl), "ctrl-space");
  assert.equal(modifiedLogicalKey("enter", shift), "shift-enter");
  assert.equal(modifiedLogicalKey("tab", shift), "backtab");
  assert.equal(modifiedLogicalKey("tab", both), "ctrl-shift-tab");
  assert.equal(modifiedLogicalKey("left", ctrl), "ctrl-left");
  assert.equal(modifiedLogicalKey("right", both), "ctrl-shift-right");
  assert.equal(modifiedLogicalKey("backspace", ctrl), "ctrl-w");
  assert.equal(modifiedLogicalKey("delete", ctrl), "alt-d");
  assert.equal(modifiedLogicalKey("ctrl-c", shift), "ctrl-c", "dedicated interrupt keeps its meaning");
  for (const value of ["abc", "日本語", "raw-bytes", "\x1b[A"]) assert.equal(modifiedLogicalKey(value, ctrl), undefined);
});

test("one-shot Shift affects one ASCII character and preserves native Unicode", () => {
  assert.equal(shiftedText("abc"), "Abc");
  assert.equal(shiftedText("1rest"), "!rest");
  assert.equal(shiftedText("["), "{");
  assert.equal(shiftedText("\\"), "|");
  assert.equal(shiftedText(";"), ":");
  assert.equal(shiftedText("'"), '"');
  assert.equal(shiftedText("🙂日本語"), "🙂日本語");
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
